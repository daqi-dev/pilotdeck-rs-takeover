//! Trusted, two-stage header preparation for one host-verified model job.
//! Construction is deliberately private until the real project-lease assembly
//! supplies the approved snapshot, registered secrets, and endpoint candidates.
use crate::{
    chain::{ChainState, WireHeaders},
    prepared::{PreparedFacts, PreparedGrant, ScopeOwned, TrustedProjectLease},
    pull::redact_failure,
    scope::{CreationReservation, Scope, ScopeManager},
};
use agentcpm_core::guard::SecretRegistry;
use agentcpm_host::{TrustedModelBinding, TrustedModelBindingResolver};
use agentcpm_iface::env::http_pull::{
    HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPreparationIssuer, HttpPullContext,
    HttpRequestPreparation,
};
use bytes::Bytes;
use hyper::{HeaderMap, Method};
use pilotdeck_model_core::codec::{self, normalize_error::NetworkFetchError};
use pilotdeck_model_runtime::{
    ModelStreamError,
    fetch_redirect::RedirectState,
    headers::{
        HeaderMaterializeError, HeaderMaterializeResult, HeaderProjection, HeaderValidateError,
        ModelHeaderService, PreparedModelHeaders, RequestMode, build_http_request,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;

pub(crate) use crate::prepared::OriginCheck;

/// Exact private generation snapshot supplied only by trusted assembly.
/// Endpoint candidates are generated from it using the original core policy.
pub(crate) struct JobFacts {
    pub(crate) private_provider: JsValue,
    pub(crate) provider_view: JsValue,
    /// Canonical request model from the admitted binding/config snapshot.
    pub(crate) canonical_model: JsValue,
    /// Explicitly approved final wire model after provider extraBody override.
    /// None is an explicit approval of an absent model field.
    pub(crate) approved_wire_model: Option<JsValue>,
    pub(crate) registry: Arc<SecretRegistry>,
    pub(crate) lease: Arc<dyn TrustedProjectLease>,
    pub(crate) origin: OriginCheck,
}
pub struct JobHeaders {
    context: HttpPullContext,
    binding: TrustedModelBinding,
    resolver: TrustedModelBindingResolver,
    secrets: Arc<ScopeOwned<JobFacts>>,
    scope: Arc<Scope>,
    issuer: Arc<HttpPreparationIssuer>,
}
struct HeaderFacts {
    full_headers: Vec<(JsString, JsValue)>,
    body: JsValue,
    wire: WireHeaders,
    private_provider: JsValue,
    canonical_model: JsValue,
    mode: RequestMode,
    registry: Arc<SecretRegistry>,
    lease: Arc<dyn TrustedProjectLease>,
    origin: OriginCheck,
}
struct Prepared {
    context: HttpPullContext,
    binding: TrustedModelBinding,
    resolver: TrustedModelBindingResolver,
    secrets: Arc<ScopeOwned<HeaderFacts>>,
    scope: Arc<Scope>,
    issuer: Arc<HttpPreparationIssuer>,
}
impl JobHeaders {
    /// Called by trusted assembly after Host admitted the job and supplied the
    /// exact project lease/snapshot. Registration completes before publication.
    pub(crate) fn new(
        context: HttpPullContext,
        binding: TrustedModelBinding,
        resolver: TrustedModelBindingResolver,
        facts: JobFacts,
        scopes: &ScopeManager,
        issuer: Arc<HttpPreparationIssuer>,
        reservation: CreationReservation,
    ) -> Result<Arc<Self>, HttpFailure> {
        check_binding(&resolver, &context, &binding)
            .map_err(|error| redact_failure(&facts.registry, error))?;
        let (scope, _creation) = scopes
            .scope_reserved(context.clone(), reservation)
            .map_err(|error| redact_failure(&facts.registry, error))?;
        let secrets = ScopeOwned::new(facts);
        scope.register_revocable(&secrets)?;
        Ok(Arc::new(Self {
            context,
            binding,
            resolver,
            secrets,
            scope,
            issuer,
        }))
    }
}
/// Exact private snapshot comparison; the general JsValue equality intentionally
/// omits object metadata and treats NaN as unequal, neither fits this guard.
fn same_snapshot(left: &JsValue, right: &JsValue) -> bool {
    let mut pending = vec![(left, right)];
    while let Some((left, right)) = pending.pop() {
        match (left, right) {
            (JsValue::Undefined, JsValue::Undefined) | (JsValue::Null, JsValue::Null) => {}
            (JsValue::Bool(a), JsValue::Bool(b)) if a == b => {}
            (JsValue::Number(a), JsValue::Number(b)) if a.to_bits() == b.to_bits() => {}
            (JsValue::String(a), JsValue::String(b)) if a == b => {}
            (JsValue::Array(a), JsValue::Array(b)) if a.len() == b.len() => {
                pending.extend(a.iter().zip(b));
            }
            (JsValue::Object(a), JsValue::Object(b))
                if a.native_tag() == b.native_tag() && a.entries().len() == b.entries().len() =>
            {
                match (a.prototype(), b.prototype()) {
                    (None, None) => {}
                    (Some(a), Some(b)) => pending.push((a, b)),
                    _ => return false,
                }
                for ((ak, av), (bk, bv)) in a.entries().iter().zip(b.entries()) {
                    if ak != bk {
                        return false;
                    }
                    pending.push((av, bv));
                }
            }
            _ => return false,
        }
    }
    true
}

fn same_optional(left: Option<&JsValue>, right: Option<&JsValue>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => same_snapshot(a, b),
        _ => false,
    }
}

/// The model capability supplies the body, but its wire model must remain
/// within the route approved from the original provider snapshot. Derive the
/// override with the same core final_body function, never with a second codec.
fn authorize_wire_model(
    job: &JobFacts,
    final_body: &JsValue,
    google: bool,
) -> Result<(), HttpFailure> {
    if google {
        return Ok(());
    }
    let mut input = JsObject::new();
    input.insert("model".into(), job.canonical_model.clone());
    let derived =
        pilotdeck_model_core::request::final_body(&JsValue::Object(input), &job.private_provider);
    let actual = final_body.as_object().and_then(|body| body.get("model"));
    let derived = derived.as_object().and_then(|body| body.get("model"));
    if same_optional(derived, job.approved_wire_model.as_ref())
        && same_optional(actual, job.approved_wire_model.as_ref())
    {
        Ok(())
    } else {
        Err(denied())
    }
}
fn candidate_urls(
    provider: &JsValue,
    canonical_model: &JsValue,
    mode: RequestMode,
) -> Result<Vec<String>, codec::CodecError> {
    let google = provider
        .get("protocol")
        .and_then(JsValue::as_string)
        .is_some_and(|value| value.equals_str("google"));
    if google {
        Ok(vec![codec::providers::google::endpoint_with_stream(
            provider,
            canonical_model,
            mode == RequestMode::Stream,
        )?])
    } else {
        Ok(codec::providers::chat_endpoint_candidates(provider))
    }
}

fn denied() -> HttpFailure {
    HttpFailure::new(
        HttpFailurePhase::Scope,
        HttpFailureKind::Denied,
        "unapproved model preparation",
    )
}
fn check_binding(
    resolver: &TrustedModelBindingResolver,
    context: &HttpPullContext,
    binding: &TrustedModelBinding,
) -> Result<(), HttpFailure> {
    if &resolver.resolve(context)? == binding {
        Ok(())
    } else {
        Err(denied())
    }
}
fn filtered_string(registry: &SecretRegistry, value: &JsString) -> JsString {
    let text = String::from_utf16_lossy(value.units());
    let filtered = registry.redact(&text);
    if filtered == text {
        value.clone()
    } else {
        filtered.into()
    }
}
fn filtered_value(registry: &SecretRegistry, value: &JsValue) -> JsValue {
    match value {
        JsValue::String(text) => JsValue::String(filtered_string(registry, text)),
        JsValue::Array(values) => {
            JsValue::Array(values.iter().map(|v| filtered_value(registry, v)).collect())
        }
        JsValue::Object(object) => {
            let mut out = JsObject::new();
            for (key, value) in object.entries() {
                out.insert(
                    filtered_string(registry, key),
                    filtered_value(registry, value),
                );
            }
            if let Some(prototype) = object.prototype() {
                out.set_prototype(filtered_value(registry, prototype));
            }
            if let Some(tag) = object.native_tag() {
                out.set_native_tag(tag);
            }
            JsValue::Object(out)
        }
        _ => value.clone(),
    }
}
fn filtered_model_error(registry: &SecretRegistry, error: codec::CodecError) -> ModelStreamError {
    match error {
        codec::CodecError::Request {
            code,
            message,
            details,
        } => ModelStreamError::Request {
            code: filtered_string(registry, &code),
            message: filtered_string(registry, &message),
            details: details.map(|value| Box::new(filtered_value(registry, &value))),
        },
        codec::CodecError::Provider(object) => {
            let JsValue::Object(ref object) = filtered_value(registry, &JsValue::Object(object))
            else {
                unreachable!()
            };
            ModelStreamError::Provider(object.clone())
        }
        codec::CodecError::Thrown { name, message } => ModelStreamError::Thrown {
            name: filtered_string(registry, &name),
            message: filtered_string(registry, &message),
        },
    }
}
fn filtered_network_error(
    registry: &SecretRegistry,
    mut error: NetworkFetchError,
) -> NetworkFetchError {
    error.message = filtered_string(registry, &error.message);
    error.cause = filtered_value(registry, &error.cause);
    error
}
fn project_headers(registry: &SecretRegistry, full: &HeaderMap) -> Result<HeaderMap, HttpFailure> {
    let mut projected = HeaderMap::new();
    for (name, value) in full {
        // A secret in a field name cannot be safely represented while
        // preserving the original redirect/header semantics.
        if registry.redact(name.as_str()) != name.as_str() {
            return Err(denied());
        }
        let mut filter = registry.stream_redactor();
        let mut safe = filter.push(value.as_bytes());
        safe.extend(filter.finish());
        let value = hyper::header::HeaderValue::from_bytes(&safe).map_err(|_| denied())?;
        projected.append(name.clone(), value);
    }
    Ok(projected)
}
impl ModelHeaderService for JobHeaders {
    type Rejection = HttpFailure;
    type Preparation = HttpRequestPreparation;
    fn materialize(
        &self,
        provider_view: &JsValue,
        final_body: &JsValue,
        mode: RequestMode,
    ) -> HeaderMaterializeResult<Self::Rejection, Self::Preparation> {
        let _owner = self
            .scope
            .begin_preparation()
            .map_err(HeaderMaterializeError::Rejected)?;
        let job = self
            .secrets
            .checkout()
            .map_err(HeaderMaterializeError::Rejected)?;
        check_binding(&self.resolver, &self.context, &self.binding).map_err(|error| {
            HeaderMaterializeError::Rejected(redact_failure(&job.registry, error))
        })?;
        if !same_snapshot(provider_view, &job.provider_view) {
            return Err(HeaderMaterializeError::Rejected(denied()));
        }
        let headers = codec::providers::request_headers_with_credentials(
            &job.private_provider,
            final_body,
            mode == RequestMode::Stream,
        );
        // Revocation during synchronous coercion wins before any error or
        // prepared value leaves the trusted host.
        check_binding(&self.resolver, &self.context, &self.binding).map_err(|error| {
            HeaderMaterializeError::Rejected(redact_failure(&job.registry, error))
        })?;
        let headers = headers.map_err(|error| {
            HeaderMaterializeError::Original(filtered_model_error(&job.registry, error))
        })?;
        let google = job
            .private_provider
            .get("protocol")
            .and_then(JsValue::as_string)
            .is_some_and(|value| value.equals_str("google"));
        authorize_wire_model(&job, final_body, google).map_err(HeaderMaterializeError::Rejected)?;
        let secrets = ScopeOwned::new(HeaderFacts {
            full_headers: headers,
            body: final_body.clone(),
            wire: if google {
                WireHeaders::Unchanged
            } else {
                WireHeaders::Undici
            },
            private_provider: job.private_provider.clone(),
            canonical_model: job.canonical_model.clone(),
            mode,
            registry: job.registry.clone(),
            lease: job.lease.clone(),
            origin: job.origin.clone(),
        });
        // The scope may have closed while materialization ran. It must not
        // return an opaque object unless registration still succeeds.
        self.scope
            .register_revocable(&secrets)
            .map_err(HeaderMaterializeError::Rejected)?;
        Ok(Box::new(Prepared {
            context: self.context.clone(),
            binding: self.binding.clone(),
            resolver: self.resolver.clone(),
            secrets,
            scope: self.scope.clone(),
            issuer: self.issuer.clone(),
        }))
    }
}
impl PreparedModelHeaders for Prepared {
    type Rejection = HttpFailure;
    type Preparation = HttpRequestPreparation;
    fn validate_url(
        &self,
        url: &str,
    ) -> Result<HeaderProjection<Self::Preparation>, HeaderValidateError<Self::Rejection>> {
        let _owner = self
            .scope
            .begin_preparation()
            .map_err(HeaderValidateError::Rejected)?;
        let facts = self
            .secrets
            .checkout()
            .map_err(HeaderValidateError::Rejected)?;
        check_binding(&self.resolver, &self.context, &self.binding).map_err(|error| {
            HeaderValidateError::Rejected(redact_failure(&facts.registry, error))
        })?;
        // Derive endpoint candidates from the approved provider snapshot via
        // the same core rules as the ordinary driver, at its original point.
        let candidates =
            candidate_urls(&facts.private_provider, &facts.canonical_model, facts.mode)
                .map_err(|_| HeaderValidateError::Rejected(denied()))?;
        let mut matching = candidates
            .iter()
            .filter(|real| facts.registry.redact(real) == url);
        let real = matching
            .next()
            .ok_or_else(|| HeaderValidateError::Rejected(denied()))?;
        if matching.any(|other| other != real) {
            return Err(HeaderValidateError::Rejected(denied()));
        }
        // This runs only at the original post-encoding/post-dump point.
        let validated = build_http_request(real, &facts.full_headers);
        check_binding(&self.resolver, &self.context, &self.binding).map_err(|error| {
            HeaderValidateError::Rejected(redact_failure(&facts.registry, error))
        })?;
        let (full_url, full_headers) = validated.map_err(|error| {
            HeaderValidateError::Original(filtered_network_error(&facts.registry, error))
        })?;
        let projected_headers = project_headers(&facts.registry, &full_headers)
            .map_err(HeaderValidateError::Rejected)?;
        let visible = facts.registry.redact(full_url.as_str());
        let projected_url =
            url::Url::parse(&visible).map_err(|_| HeaderValidateError::Rejected(denied()))?;
        let encoded = codec::js::json_stringify(&facts.body)
            .map_err(|_| HeaderValidateError::Rejected(denied()))?
            .unwrap_or_default();
        // The public port may not carry any registered secret in its body.
        if facts.registry.redact(&encoded) != encoded {
            return Err(HeaderValidateError::Rejected(denied()));
        }
        let body = Bytes::from(encoded);
        let projected = RedirectState {
            url: projected_url.clone(),
            method: Method::POST,
            headers: projected_headers.clone(),
            body: body.clone(),
            redirects: 0,
        };
        let full = RedirectState {
            url: full_url,
            method: Method::POST,
            headers: full_headers,
            body,
            redirects: 0,
        };
        let chain = ChainState::new(projected, full, facts.wire, true)
            .map_err(HeaderValidateError::Rejected)?;
        let grant = PreparedGrant::new(
            self.context.clone(),
            self.binding.clone(),
            PreparedFacts {
                chain,
                registry: facts.registry.clone(),
                _lease: facts.lease.clone(),
                authorize_origin: facts.origin.clone(),
            },
        );
        let preparation = self
            .scope
            .issue_grant(&self.issuer, grant)
            .map_err(HeaderValidateError::Rejected)?;
        Ok(HeaderProjection {
            preparation,
            url: projected_url,
            headers: projected_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_comparison_includes_nonfinite_numbers_and_object_metadata() {
        let mut original = JsObject::new();
        original.insert("n".into(), JsValue::Number(f64::NAN));
        original.set_prototype(JsValue::Null);
        original.set_native_tag("Map");
        let left = JsValue::Object(original.clone());
        assert!(same_snapshot(&left, &left.clone()));
        let mut changed_prototype = original.clone();
        changed_prototype.set_prototype(JsValue::Bool(false));
        assert!(!same_snapshot(&left, &JsValue::Object(changed_prototype)));
        let mut changed_tag = original;
        changed_tag.set_native_tag("Set");
        assert!(!same_snapshot(&left, &JsValue::Object(changed_tag)));
        assert!(!same_snapshot(
            &JsValue::Number(0.0),
            &JsValue::Number(-0.0)
        ));
    }
    #[test]
    fn original_url_then_header_error_order_is_retained_without_secret_disclosure() {
        let secret = "synthetic-secret";
        let registry = SecretRegistry::new([("fixture/key".into(), secret.into())]);
        let headers = vec![(
            format!("bad {secret}").into(),
            JsValue::String(secret.into()),
        )];
        let bad_url = format!("not-a-url-{secret}");
        let first = build_http_request(&bad_url, &headers).unwrap_err();
        assert!(String::from_utf16_lossy(first.message.units()).starts_with("Failed to parse URL"));
        let first = filtered_network_error(&registry, first);
        assert!(!String::from_utf16_lossy(first.message.units()).contains(secret));
        let second = build_http_request("https://example.test/", &headers).unwrap_err();
        assert!(String::from_utf16_lossy(second.message.units()).contains("invalid header name"));
        let second = filtered_network_error(&registry, second);
        assert!(!String::from_utf16_lossy(second.message.units()).contains(secret));
    }
    struct TestLease;
    impl TrustedProjectLease for TestLease {}
    fn route_job(provider: JsValue, canonical: &str, approved: Option<JsValue>) -> JobFacts {
        JobFacts {
            private_provider: provider,
            provider_view: JsValue::Null,
            canonical_model: canonical.into(),
            approved_wire_model: approved,
            registry: Arc::new(SecretRegistry::new(Vec::<(String, String)>::new())),
            lease: Arc::new(TestLease),
            origin: Arc::new(|_| Ok(())),
        }
    }
    #[test]
    fn route_check_uses_original_extrabody_override_not_display_name() {
        let provider = pilotdeck_protocol::parse_json(
            r#"{"protocol":"openai","extraBody":{"model":"wire-override"}}"#,
        )
        .unwrap();
        let approved = route_job(provider, "display-model", Some("wire-override".into()));
        let matching = pilotdeck_protocol::parse_json(r#"{"model":"wire-override"}"#).unwrap();
        let wrong = pilotdeck_protocol::parse_json(r#"{"model":"display-model"}"#).unwrap();
        assert!(authorize_wire_model(&approved, &matching, false).is_ok());
        assert!(authorize_wire_model(&approved, &wrong, false).is_err());
        let unapproved = route_job(
            approved.private_provider.clone(),
            "display-model",
            Some("other".into()),
        );
        assert!(authorize_wire_model(&unapproved, &matching, false).is_err());
    }
    #[test]
    fn google_candidates_use_canonical_model_and_requested_mode() {
        let provider = pilotdeck_protocol::parse_json(r#"{"protocol":"google","url":"https://api.test","extraBody":{"model":"ignored-route"}}"#).unwrap();
        let model: JsValue = "models/canonical".into();
        let stream = candidate_urls(&provider, &model, RequestMode::Stream).unwrap();
        let complete = candidate_urls(&provider, &model, RequestMode::Complete).unwrap();
        assert!(stream[0].contains("models/canonical:streamGenerateContent?alt=sse"));
        assert!(complete[0].contains("models/canonical:generateContent"));
        assert!(!stream[0].contains("ignored-route"));
    }
}
