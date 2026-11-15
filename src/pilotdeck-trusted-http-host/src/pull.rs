//! Per-exchange projection of the shared trusted HTTP implementation.
//! Not wired until the host-verified provider binding is available.
use crate::{
    attempt::{Attempt, AttemptOwner},
    body::{BodyEnd, BodyHooks, PullBody},
    prepared::PreparedGrant,
    scope::ScopeManager,
};
use agentcpm_core::guard::SecretRegistry;
use agentcpm_host::TrustedModelBindingResolver;
use agentcpm_iface::{
    BoxFuture,
    env::http_pull::{
        HttpExchange, HttpExchangeControl, HttpExchangeEnd, HttpFailure, HttpFailureKind,
        HttpFailurePhase, HttpPreparationIssuer, HttpPullContext, HttpPullHead, HttpPullRequest,
        HttpPullResponse, HttpPullTransport, HttpScopeCleanup,
    },
};
use pilotdeck_http_transport::transport::{ExchangeError, cause_code, encoding::DecodedProgress};
use std::sync::Arc;

pub(crate) struct PullTransport {
    pub(crate) scopes: Arc<ScopeManager>,
    pub(crate) issuer: Arc<HttpPreparationIssuer>,
    pub(crate) bindings: TrustedModelBindingResolver,
}

struct HeadOwner {
    attempt: Arc<Attempt>,
    owner: Option<AttemptOwner>,
    done: bool,
}
impl Drop for HeadOwner {
    fn drop(&mut self) {
        if !self.done {
            self.attempt.stop_local();
        }
        self.owner.take();
    }
}
fn managed_failure(error: &(dyn std::error::Error + 'static)) -> Option<HttpFailure> {
    if let Some(failure) = error.downcast_ref::<HttpFailure>() {
        return Some(failure.clone());
    }
    // io::Error::source skips the custom inner error itself on supported Rust
    // versions. Inspect it explicitly before walking ordinary source links.
    if let Some(io) = error.downcast_ref::<std::io::Error>()
        && let Some(inner) = io.get_ref()
        && let Some(failure) = managed_failure(inner)
    {
        return Some(failure);
    }
    error.source().and_then(managed_failure)
}

fn map_exchange_error(error: ExchangeError) -> HttpFailure {
    match error {
        ExchangeError::Cancelled => HttpFailure::new(
            HttpFailurePhase::Scope,
            HttpFailureKind::Cancelled,
            "attempt stopped",
        ),
        ExchangeError::InvalidTarget => HttpFailure::new(
            HttpFailurePhase::Request,
            HttpFailureKind::Protocol,
            "invalid target authority",
        ),
        ExchangeError::Dial(error) | ExchangeError::Network(error) => {
            if let Some(failure) = managed_failure(error.as_ref()) {
                return failure;
            }
            HttpFailure {
                phase: HttpFailurePhase::ResponseHead,
                kind: HttpFailureKind::Network,
                detail: "fetch failed".into(),
                network_cause: Some(agentcpm_iface::env::http_pull::HttpNetworkCause {
                    code: cause_code(error.as_ref()).map(str::to_owned),
                    message: "fetch failed".into(),
                }),
            }
        }
        ExchangeError::Encoding(failure) => HttpFailure {
            phase: HttpFailurePhase::ResponseHead,
            kind: HttpFailureKind::Network,
            detail: failure.message,
            network_cause: Some(agentcpm_iface::env::http_pull::HttpNetworkCause {
                code: failure.cause_code.map(str::to_owned),
                message: "fetch failed".into(),
            }),
        },
    }
}

pub(crate) fn redact_failure(registry: &SecretRegistry, mut failure: HttpFailure) -> HttpFailure {
    failure.detail = registry.redact(&failure.detail);
    if let Some(cause) = &mut failure.network_cause {
        cause.message = registry.redact(&cause.message);
        if let Some(code) = &mut cause.code {
            *code = registry.redact(code);
        }
    }
    failure
}
fn redact_bytes(registry: &SecretRegistry, value: &[u8]) -> Vec<u8> {
    let mut filter = registry.stream_redactor();
    let mut safe = filter.push(value);
    safe.extend(filter.finish());
    safe
}
fn visible_header_map(headers: &[(String, Vec<u8>)]) -> Result<hyper::HeaderMap, HttpFailure> {
    let fail = || {
        HttpFailure::new(
            HttpFailurePhase::ResponseHead,
            HttpFailureKind::Protocol,
            "invalid filtered response header",
        )
    };
    let mut map = hyper::HeaderMap::new();
    for (name, value) in headers {
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| fail())?;
        let value = hyper::header::HeaderValue::from_bytes(value).map_err(|_| fail())?;
        map.append(name, value);
    }
    Ok(map)
}
impl HttpPullTransport for PullTransport {
    fn start(
        &self,
        request: HttpPullRequest,
        context: HttpPullContext,
    ) -> Result<HttpExchange, HttpFailure> {
        // pool() checks the real host scope but performs no network I/O.
        let (scope, _creation) = self.scopes.scope(context.clone())?;
        let client = scope.client().ok_or_else(|| {
            HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Cancelled,
                "model scope stopped",
            )
        })?;
        let token = request.preparation.as_ref().ok_or_else(|| {
            HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Denied,
                "missing HTTP preparation",
            )
        })?;
        let grant = self
            .issuer
            .inspect::<Arc<PreparedGrant>>(token)
            .cloned()
            .ok_or_else(|| {
                HttpFailure::new(
                    HttpFailurePhase::Scope,
                    HttpFailureKind::Denied,
                    "foreign HTTP preparation",
                )
            })?;
        let binding = self.bindings.resolve(&context)?;
        let facts = grant.checkout(&context, &binding)?;
        // Registration at issuance is mandatory; this also serializes raw
        // start against closure and rejects an already stopped scope.
        scope.register_grant(&grant)?;
        let attempt = Attempt::new();
        attempt.retain_facts(facts.clone());
        // Owner must exist before register races scope.stop().
        let owner = attempt.owner();
        scope.register(attempt.clone())?;
        let mut stopped = attempt.stop_receiver();
        let bindings = self.bindings.clone();
        let control: Arc<dyn HttpExchangeControl> = attempt.clone();
        let head = HeadOwner {
            attempt: attempt.clone(),
            owner: Some(owner),
            done: false,
        };
        let response: BoxFuture<'static, Result<HttpPullResponse, HttpFailure>> = Box::pin(
            async move {
                let mut head = head;
                let registry = facts.registry.clone();
                let already_stopped = *stopped.borrow();
                let send = async {
                    if already_stopped {
                        return Err(HttpFailure::new(
                            HttpFailurePhase::Scope,
                            HttpFailureKind::Cancelled,
                            "attempt stopped",
                        ));
                    }
                    let follow = request.caller_follows_redirects;
                    if bindings.resolve(&context)? != binding {
                        return Err(HttpFailure::new(
                            HttpFailurePhase::Scope,
                            HttpFailureKind::Denied,
                            "model binding changed",
                        ));
                    }
                    let (mut built, hop) = facts.chain.reserve(&request)?;
                    // The real URI may differ from its redacted projection.
                    (facts.authorize_origin)(built.uri())?;
                    if bindings.resolve(&context)? != binding {
                        return Err(HttpFailure::new(
                            HttpFailurePhase::Scope,
                            HttpFailureKind::Denied,
                            "model binding changed",
                        ));
                    }
                    if follow {
                        built
                            .extensions_mut()
                            .insert(pilotdeck_model_runtime::fetch_redirect::FollowRedirects);
                    }
                    let response = client
                        .send_checked(built, || attempt.may_dispatch())
                        .await
                        .map_err(map_exchange_error)?;
                    Ok((response, hop))
                };
                let result = tokio::select! {
                    biased;
                    _ = stopped.changed() => Err(HttpFailure::new(HttpFailurePhase::Scope, HttpFailureKind::Cancelled, "attempt stopped")),
                    result = send => result,
                };
                let (response, hop) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        let end = attempt.finish(HttpExchangeEnd::Failed);
                        head.done = true;
                        let error = if end == HttpExchangeEnd::StoppedLocally {
                            HttpFailure::new(
                                HttpFailurePhase::Scope,
                                HttpFailureKind::Cancelled,
                                "attempt stopped",
                            )
                        } else {
                            error
                        };
                        return Err(redact_failure(&registry, error));
                    }
                };
                // A local stop that won while Hyper produced the head must not
                // publish a fresh body after the attempt is already terminal.
                if attempt.is_stopped() {
                    head.done = true;
                    return Err(HttpFailure::new(
                        HttpFailurePhase::Scope,
                        HttpFailureKind::Cancelled,
                        "attempt stopped",
                    ));
                }
                let decoded_progress = response
                    .extensions()
                    .get::<DecodedProgress>()
                    .map(|marker| marker.0.clone());
                let status = response.status().as_u16();
                let reason = response
                    .extensions()
                    .get::<hyper::ext::ReasonPhrase>()
                    .map(|phrase| redact_bytes(&registry, phrase.as_bytes()));
                let headers: Vec<(String, Vec<u8>)> = response
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            registry.redact(name.as_str()),
                            redact_bytes(&registry, value.as_bytes()),
                        )
                    })
                    .collect();
                let visible_headers = match visible_header_map(&headers) {
                    Ok(headers) => headers,
                    Err(error) => {
                        let end = attempt.finish(HttpExchangeEnd::Failed);
                        head.done = true;
                        return Err(if end == HttpExchangeEnd::StoppedLocally {
                            HttpFailure::new(
                                HttpFailurePhase::Scope,
                                HttpFailureKind::Cancelled,
                                "attempt stopped",
                            )
                        } else {
                            error
                        });
                    }
                };
                hop.receive(status, response.headers(), &visible_headers);
                let (parts, body) = response.into_parts();
                drop(parts);
                let end_attempt = attempt.clone();
                let on_end = Arc::new(move |end| match end {
                    BodyEnd::Eof => end_attempt.finish(HttpExchangeEnd::Eof),
                    BodyEnd::Failed => end_attempt.finish(HttpExchangeEnd::Failed),
                    BodyEnd::Stopped => {
                        end_attempt.stop_local();
                        end_attempt
                            .snapshot()
                            .end
                            .expect("stop sets terminal outcome")
                    }
                });
                let check_pool = scope.pool.clone();
                let check = Arc::new(move || check_pool.check_existing());
                let filter_registry = registry.clone();
                let redact = Arc::new(move |value: &str| filter_registry.redact(value));
                let owner = head
                    .owner
                    .take()
                    .expect("head owns attempt until body transfer");
                head.done = true;
                Ok(HttpPullResponse {
                    head: HttpPullHead {
                        status,
                        reason,
                        headers,
                    },
                    body: Box::pin(PullBody::new(
                        body,
                        owner,
                        attempt.clone(),
                        registry.stream_redactor(),
                        check,
                        on_end,
                        BodyHooks {
                            decoded_progress,
                            redact_detail: redact,
                        },
                    )),
                })
            },
        );
        Ok(HttpExchange { response, control })
    }

    fn close_scope(
        &self,
        context: HttpPullContext,
    ) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        self.scopes.cleanup_scope(&context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_io_error_preserves_scope_denial() {
        let denied = HttpFailure::new(
            HttpFailurePhase::Scope,
            HttpFailureKind::Denied,
            "scope denied",
        );
        let wrapped = std::io::Error::other(denied.clone());
        let result = map_exchange_error(ExchangeError::Network(Box::new(wrapped)));
        assert_eq!(result, denied);
    }
}
