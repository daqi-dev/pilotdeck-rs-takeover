//! Trusted per-preparation redirect verification; token binding is not wired yet.
use agentcpm_iface::env::http_pull::{
    HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPullRequest,
};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{HeaderMap, Method, Request};
use pilotdeck_model_runtime::fetch_redirect::{RedirectState, advance_redirect};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub(crate) enum WireHeaders {
    Undici,
    Unchanged,
}
impl WireHeaders {
    fn apply(self, uri: &hyper::Uri, headers: &HeaderMap) -> HeaderMap {
        match self {
            Self::Undici => {
                pilotdeck_model_runtime::fetch_redirect::undici_wire_headers(uri, headers)
            }
            Self::Unchanged => headers.clone(),
        }
    }
}

struct State {
    projected: RedirectState,
    full: RedirectState,
    // A single chain is sequential. A lost/failed attempt terminates it.
    in_flight: bool,
    terminal: bool,
}

/// Created only by an approved preparation after URL/header validation. The
/// projection and private header states use the exact same redirect transition.
pub(crate) struct ChainState {
    state: Mutex<State>,
    wire: WireHeaders,
    follow: bool,
}
fn denied() -> HttpFailure {
    HttpFailure::new(
        HttpFailurePhase::Scope,
        HttpFailureKind::Denied,
        "invalid prepared request chain",
    )
}
fn equal_headers(left: &HeaderMap, right: &HeaderMap) -> bool {
    left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| a == b)
}
impl ChainState {
    pub(crate) fn new(
        mut projected: RedirectState,
        mut full: RedirectState,
        wire: WireHeaders,
        follow: bool,
    ) -> Result<Arc<Self>, HttpFailure> {
        if projected.method != full.method
            || projected.body != full.body
            || projected.redirects != 0
            || full.redirects != 0
        {
            return Err(denied());
        }
        // The original follow removes Host before dispatch.
        if follow {
            projected.headers.remove(hyper::header::HOST);
            full.headers.remove(hyper::header::HOST);
        }
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                projected,
                full,
                in_flight: false,
                terminal: false,
            }),
            wire,
            follow,
        }))
    }

    /// Atomically reserves one hop; the caller must already have checked the
    /// Host-minted token, original binding and scope. The caller must authorize
    /// the returned *actual* URI against the approved origin before sending.
    pub(crate) fn reserve(
        self: &Arc<Self>,
        incoming: &HttpPullRequest,
    ) -> Result<(Request<Full<Bytes>>, Hop), HttpFailure> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.in_flight || state.terminal || incoming.caller_follows_redirects != self.follow {
            return Err(denied());
        }
        let mut expected_url = state.projected.url.clone();
        if self.follow
            && (!matches!(expected_url.scheme(), "http" | "https")
                || expected_url
                    .port()
                    .is_some_and(pilotdeck_model_runtime::fetch_redirect::blocked_port))
        {
            return Err(denied());
        }
        expected_url.set_fragment(None);
        let mut full_url = state.full.url.clone();
        if self.follow
            && (!matches!(full_url.scheme(), "http" | "https")
                || full_url
                    .port()
                    .is_some_and(pilotdeck_model_runtime::fetch_redirect::blocked_port))
        {
            return Err(denied());
        }
        full_url.set_fragment(None);
        let full_uri: hyper::Uri = full_url.as_str().parse().map_err(|_| denied())?;
        let uri: hyper::Uri = expected_url.as_str().parse().map_err(|_| denied())?;
        let supplied_uri: hyper::Uri = incoming.url.parse().map_err(|_| denied())?;
        let method: Method = incoming.method.parse().map_err(|_| denied())?;
        let mut headers = HeaderMap::new();
        for (name, value) in &incoming.headers {
            let name =
                hyper::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| denied())?;
            let value = hyper::header::HeaderValue::from_bytes(value).map_err(|_| denied())?;
            headers.append(name, value);
        }
        let expected = self.wire.apply(&uri, &state.projected.headers);
        if supplied_uri != uri
            || method != state.projected.method
            || incoming.body.as_slice() != state.projected.body.as_ref()
            || !equal_headers(&headers, &expected)
        {
            return Err(denied());
        }
        let full_headers = self.wire.apply(&full_uri, &state.full.headers);
        let mut request = Request::builder()
            .method(method)
            .uri(full_uri)
            .body(Full::new(state.full.body.clone()))
            .map_err(|_| denied())?;
        *request.headers_mut() = full_headers;
        state.in_flight = true;
        drop(state);
        Ok((
            request,
            Hop {
                chain: self.clone(),
                finished: false,
            },
        ))
    }
}

/// Reservation commits the transition only on the real received response head.
pub(crate) struct Hop {
    chain: Arc<ChainState>,
    finished: bool,
}
impl Hop {
    pub(crate) fn receive(
        mut self,
        status: u16,
        raw_headers: &HeaderMap,
        visible_headers: &HeaderMap,
    ) {
        let mut state = self.chain.state.lock().unwrap_or_else(|p| p.into_inner());
        if !self.chain.follow {
            state.terminal = true;
        } else {
            // Driver navigation sees the sanitized head; private navigation
            // follows the actual Location. Both use the same transition.
            let projection = advance_redirect(&mut state.projected, status, visible_headers);
            let full = advance_redirect(&mut state.full, status, raw_headers);
            // Transition failure is not returned early: the ordinary follow
            // sees the same head and reports its error at its original point.
            let sensitive_presence_agrees =
                ["authorization", "proxy-authorization", "cookie", "host"]
                    .iter()
                    .all(|name| {
                        state.projected.headers.contains_key(*name)
                            == state.full.headers.contains_key(*name)
                    });
            if !matches!((projection, full), (Ok(true), Ok(true))) || !sensitive_presence_agrees {
                // A redacted Location must not silently change credential
                // stripping. Reject any subsequent hop if it does.
                state.terminal = true;
            }
        }
        state.in_flight = false;
        self.finished = true;
    }
}
impl Drop for Hop {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.chain.state.lock().unwrap_or_else(|p| p.into_inner());
            state.in_flight = false;
            state.terminal = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_model_runtime::fetch_redirect::RedirectState;

    fn state(url: &str, headers: HeaderMap) -> RedirectState {
        RedirectState {
            url: url.parse().unwrap(),
            method: Method::GET,
            headers,
            body: Bytes::new(),
            redirects: 0,
        }
    }
    fn incoming(url: &str, headers: &HeaderMap) -> HttpPullRequest {
        HttpPullRequest {
            method: "GET".into(),
            url: url.into(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
                .collect(),
            body: Vec::new(),
            caller_follows_redirects: true,
            preparation: None,
        }
    }
    #[test]
    fn same_origin_redacted_query_dispatches_real_query() {
        let headers = HeaderMap::new();
        let chain = ChainState::new(
            state("https://api.test/a", headers.clone()),
            state("https://api.test/a", headers.clone()),
            WireHeaders::Unchanged,
            true,
        )
        .unwrap();
        let (_, first) = chain
            .reserve(&incoming("https://api.test/a", &headers))
            .unwrap();
        let mut raw = HeaderMap::new();
        raw.insert(
            hyper::header::LOCATION,
            "?key=synthetic-secret".parse().unwrap(),
        );
        let mut visible = HeaderMap::new();
        visible.insert(hyper::header::LOCATION, "?key=***".parse().unwrap());
        first.receive(302, &raw, &visible);
        let (request, _) = chain
            .reserve(&incoming("https://api.test/a?key=***", &headers))
            .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "https://api.test/a?key=synthetic-secret"
        );
    }
    #[test]
    fn redacted_origin_disagreement_forbids_another_hop() {
        let mut headers = HeaderMap::new();
        headers.insert(hyper::header::AUTHORIZATION, "placeholder".parse().unwrap());
        let chain = ChainState::new(
            state("https://api.test/a", headers.clone()),
            state("https://api.test/a", headers.clone()),
            WireHeaders::Unchanged,
            true,
        )
        .unwrap();
        let (_, first) = chain
            .reserve(&incoming("https://api.test/a", &headers))
            .unwrap();
        let mut raw = HeaderMap::new();
        raw.insert(
            hyper::header::LOCATION,
            "https://other.test/b".parse().unwrap(),
        );
        let mut visible = HeaderMap::new();
        visible.insert(
            hyper::header::LOCATION,
            "https://api.test/b".parse().unwrap(),
        );
        first.receive(302, &raw, &visible);
        assert!(
            chain
                .reserve(&incoming("https://api.test/b", &headers))
                .is_err()
        );
    }
    #[test]
    fn repeated_hop_and_duplicate_header_reordering_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.append("x-test", "first".parse().unwrap());
        headers.append("x-test", "second".parse().unwrap());
        let chain = ChainState::new(
            state("https://api.test/a", headers.clone()),
            state("https://api.test/a", headers.clone()),
            WireHeaders::Unchanged,
            true,
        )
        .unwrap();
        let mut reordered = incoming("https://api.test/a", &headers);
        reordered.headers.reverse();
        assert!(chain.reserve(&reordered).is_err());
        let (_, held) = chain
            .reserve(&incoming("https://api.test/a", &headers))
            .unwrap();
        assert!(
            chain
                .reserve(&incoming("https://api.test/a", &headers))
                .is_err()
        );
        drop(held);
        assert!(
            chain
                .reserve(&incoming("https://api.test/a", &headers))
                .is_err()
        );
    }
    #[test]
    fn post_redirect_get_and_cross_origin_credentials_do_not_return() {
        let mut projected = HeaderMap::new();
        projected.insert(hyper::header::AUTHORIZATION, "placeholder".parse().unwrap());
        projected.insert(
            hyper::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let mut full = projected.clone();
        full.insert(
            hyper::header::AUTHORIZATION,
            "Bearer synthetic-secret".parse().unwrap(),
        );
        let mut a = state("https://api.test/a", projected.clone());
        let mut b = state("https://api.test/a", full);
        a.method = Method::POST;
        b.method = Method::POST;
        a.body = Bytes::from_static(b"{}");
        b.body = Bytes::from_static(b"{}");
        let chain = ChainState::new(a, b, WireHeaders::Unchanged, true).unwrap();
        let mut first = incoming("https://api.test/a", &projected);
        first.method = "POST".into();
        first.body = b"{}".to_vec();
        let (_, first_hop) = chain.reserve(&first).unwrap();
        let mut redirect = HeaderMap::new();
        redirect.insert(
            hyper::header::LOCATION,
            "https://other.test/b".parse().unwrap(),
        );
        first_hop.receive(302, &redirect, &redirect);
        let no_headers = HeaderMap::new();
        let (second, second_hop) = chain
            .reserve(&incoming("https://other.test/b", &no_headers))
            .unwrap();
        assert_eq!(second.method(), Method::GET);
        assert!(!second.headers().contains_key(hyper::header::AUTHORIZATION));
        redirect.insert(
            hyper::header::LOCATION,
            "https://api.test/c".parse().unwrap(),
        );
        second_hop.receive(307, &redirect, &redirect);
        let (third, _) = chain
            .reserve(&incoming("https://api.test/c", &no_headers))
            .unwrap();
        assert!(!third.headers().contains_key(hyper::header::AUTHORIZATION));
    }
}
