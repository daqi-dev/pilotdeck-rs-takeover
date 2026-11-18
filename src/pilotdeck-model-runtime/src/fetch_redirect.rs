//! Undici `fetch` redirect policy (`redirect: "follow"`), shared by every
//! Rust port of a TS call site that uses the default `fetch`: the provider
//! count transport (calibrated by `tests/fixtures/token-accounting`) and the
//! HTTP hook executor (`tests/fixtures/hooks-runtime`).
//!
//! Only the single HTTP exchange is injected (`send`): a per-dispatch timer
//! (undici `headersTimeout`) belongs inside `send`, an abort signal around the
//! returned future.
use bytes::Bytes;
use http_body_util::Full;
use hyper::{HeaderMap, Method, Request, Response, body::Body};
use std::{future::Future, time::Duration};
use url::Url;

/// `buffer.constants.MAX_STRING_LENGTH` on Node 22.22 (V8 12.x): `text()` /
/// `json()` reject a body with more bytes than this.
pub const MAX_STRING_LENGTH: usize = 0x1fffffe8;
/// V8 / Node `ERR_STRING_TOO_LONG` message for [`MAX_STRING_LENGTH`].
pub use crate::transport::STRING_TOO_LONG;

/// Undici's redirect status set, shared with the content-decoding boundary.
pub fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[derive(Debug, PartialEq, Eq)]
pub enum BodyError {
    /// More than the limit's bytes arrived.
    TooLong,
    /// A frame failed, or no data arrived within the idle timeout.
    Terminated,
    /// The caller's signal fired before the next frame/idle timeout.
    Aborted,
}

/// Reads a response body like `response.text()` before decoding: bytes are
/// counted frame by frame and the read stops as soon as `limit` would be
/// exceeded, so a never-ending body cannot grow memory without bound.
/// `idle` is undici's `bodyTimeout`: the longest gap between two body frames
/// (including the wait for the first), not a deadline for the whole body.
pub async fn read_body<B>(
    body: B,
    limit: usize,
    idle: Option<Duration>,
) -> Result<Vec<u8>, BodyError>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Send,
{
    read_body_with_signal(body, limit, idle, None, false).await
}

/// Keep each frame's idle deadline visible to the same event dispatcher used
/// for headers. Wrapping the entire read in another race would lose the timer
/// and incorrectly report a later parent abort instead of `terminated`.
pub async fn read_body_with_signal<B>(
    body: B,
    limit: usize,
    idle: Option<Duration>,
    signal: Option<&crate::network::Cancellation>,
    mut continuation_before_abort: bool,
) -> Result<Vec<u8>, BodyError>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Send,
{
    use crate::network::{Exchange, body_frame};
    let mut body = Box::pin(body);
    let mut buffer = Vec::new();
    loop {
        // Ledger: specs/gateway-parity/diffs/A6.4-NETWORK-TIMING-PARENT-TIE-001.md
        let frame = match body_frame(&mut body, idle, signal, &mut continuation_before_abort).await
        {
            Exchange::Response(frame, _) => frame,
            Exchange::Timeout { .. } => return Err(BodyError::Terminated),
            Exchange::Aborted => return Err(BodyError::Aborted),
        };
        let Some(frame) = frame else {
            return Ok(buffer);
        };
        let frame = frame.map_err(|_| BodyError::Terminated)?;
        if let Some(data) = frame.data_ref() {
            if buffer.len().saturating_add(data.len()) > limit {
                return Err(BodyError::TooLong);
            }
            buffer.extend_from_slice(data);
        }
    }
}

/// [`read_body_with_signal`] with `Response.text()`'s over-limit semantics:
/// once the received bytes pass `limit` the buffer is dropped and the body is
/// still drained to EOF, so a body failure wins over the string error and
/// exactly `limit` bytes stay valid (real TS probes,
/// `_tmp/20261025-issue-641/RESULT.md` and
/// `_tmp/20261025-issue-647-probe/RESULT.md`). Kept separate from
/// [`read_body_with_signal`], whose callers (the HTTP hook executor, the
/// user-dialog store) bound an arbitrary configured cap and stop at the first
/// over-limit frame.
pub async fn read_text_body<B>(
    body: B,
    limit: usize,
    idle: Option<Duration>,
) -> Result<Vec<u8>, BodyError>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Send,
{
    read_text_body_with_signal(body, limit, idle, None, false).await
}

/// The signal-aware half of [`read_text_body`]; see its doc for the
/// over-limit draining contract.
pub async fn read_text_body_with_signal<B>(
    body: B,
    limit: usize,
    idle: Option<Duration>,
    signal: Option<&crate::network::Cancellation>,
    continuation_before_abort: bool,
) -> Result<Vec<u8>, BodyError>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Send,
{
    read_text_body_ordered(body, limit, idle, signal, continuation_before_abort, None).await
}

/// A prearmed absolute deadline and the per-frame idle timeout compete in
/// the same dispatcher. The earlier timer wins even after a late poll; the
/// prearmed deadline wins ties. Existing signal-only callers are unchanged.
pub async fn read_text_body_with_deadline<B>(
    body: B,
    limit: usize,
    idle: Option<Duration>,
    deadline: Option<tokio::time::Instant>,
) -> Result<Vec<u8>, BodyError>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Send,
{
    read_text_body_ordered(body, limit, idle, None, false, deadline).await
}

async fn read_text_body_ordered<B>(
    body: B,
    limit: usize,
    idle: Option<Duration>,
    signal: Option<&crate::network::Cancellation>,
    mut continuation_before_abort: bool,
    deadline: Option<tokio::time::Instant>,
) -> Result<Vec<u8>, BodyError>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Send,
{
    use crate::network::{Exchange, body_frame_at};
    let mut body = Box::pin(body);
    let mut buffer = Vec::new();
    let mut too_long = false;
    loop {
        // Ledger: specs/gateway-parity/diffs/A6.4-NETWORK-TIMING-PARENT-TIE-001.md
        let now = tokio::time::Instant::now();
        let deadline_first = deadline.is_some_and(|at| idle.is_none_or(|idle| at <= now + idle));
        let next = if deadline_first {
            deadline
        } else {
            idle.map(|idle| now + idle)
        };
        let frame =
            match body_frame_at(&mut body, next, signal, &mut continuation_before_abort).await {
                Exchange::Response(frame, _) => frame,
                Exchange::Timeout { .. } => {
                    return Err(if deadline_first {
                        BodyError::Aborted
                    } else {
                        BodyError::Terminated
                    });
                }
                Exchange::Aborted => return Err(BodyError::Aborted),
            };
        let Some(frame) = frame else {
            return if too_long {
                Err(BodyError::TooLong)
            } else {
                Ok(buffer)
            };
        };
        let frame = frame.map_err(|_| BodyError::Terminated)?;
        if let Some(data) = frame.data_ref() {
            if too_long {
                continue;
            }
            if buffer.len().saturating_add(data.len()) > limit {
                // The conversion has not run yet; keep draining to EOF so a
                // body failure still decides, and retain none of the
                // over-limit prefix.
                too_long = true;
                buffer = Vec::new();
                continue;
            }
            buffer.extend_from_slice(data);
        }
    }
}

/// Internal request metadata; never sent as a wire header.
#[derive(Clone, Copy, Debug)]
pub struct FollowRedirects;

/// Follow 301/302/303/307/308 like undici and return the first non-redirect
/// response (its body unread). Every network-level refusal maps to `failed()`
/// (TS `TypeError: fetch failed`).
pub async fn follow<B, E, F, Fut>(
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
    failed: impl Fn() -> E,
    mut send: F,
) -> Result<Response<B>, E>
where
    F: FnMut(Request<Full<Bytes>>) -> Fut,
    Fut: Future<Output = Result<Response<B>, E>>,
{
    follow_with_context(url, method, headers, body, (), failed, |request, ()| {
        send(request)
    })
    .await
}

/// The explicit preparation identity is carried without a private header or
/// request extension. It grants no authority; managed transports recheck it.
pub async fn follow_with_context<B, E, F, Fut, P: Clone>(
    url: Url,
    method: Method,
    mut headers: HeaderMap,
    body: Bytes,
    preparation: P,
    failed: impl Fn() -> E,
    mut send: F,
) -> Result<Response<B>, E>
where
    F: FnMut(Request<Full<Bytes>>, P) -> Fut,
    Fut: Future<Output = Result<Response<B>, E>>,
{
    // Undici discards user Host on every hop; the HTTP client supplies the
    // authority of the current URL, including a redirected port.
    headers.remove(hyper::header::HOST);
    let mut state = RedirectState {
        url,
        method,
        headers,
        body,
        redirects: 0,
    };
    loop {
        // Port validation remains at the beginning of each actual hop.
        if !matches!(state.url.scheme(), "http" | "https")
            || state.url.port().is_some_and(blocked_port)
        {
            return Err(failed());
        }
        state.url.set_fragment(None);
        let mut request = Request::builder()
            .method(state.method.clone())
            .uri(state.url.as_str())
            .body(Full::new(state.body.clone()))
            .map_err(|_| failed())?;
        *request.headers_mut() = state.headers.clone();
        request.extensions_mut().insert(FollowRedirects);
        let response = send(request, preparation.clone()).await?;
        if !advance_redirect(&mut state, response.status().as_u16(), response.headers())
            .map_err(|_| failed())?
        {
            return Ok(response);
        }
        // The original loop never collects an intermediate redirect body.
        drop(response);
    }
}

/// Pure state used by both the ordinary driver and trusted per-hop verification.
#[derive(Clone, Debug)]
pub struct RedirectState {
    pub url: Url,
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub redirects: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedirectFailure;

/// Apply the original post-response transition. `false` means the response is
/// terminal. Invalid next targets are not dispatched here; port validation and
/// fragment removal remain at the next loop's original pre-send point.
pub fn advance_redirect(
    state: &mut RedirectState,
    status: u16,
    response_headers: &HeaderMap,
) -> Result<bool, RedirectFailure> {
    if !(is_redirect_status(status) && response_headers.contains_key(hyper::header::LOCATION)) {
        return Ok(false);
    }
    let location = response_headers
        .get_all(hyper::header::LOCATION)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()))
        .collect::<Vec<_>>()
        .join(", ");
    let next = state.url.join(&location).map_err(|_| RedirectFailure)?;
    if state.redirects == 20
        || !matches!(next.scheme(), "http" | "https")
        || !next.username().is_empty()
        || next.password().is_some()
    {
        return Err(RedirectFailure);
    }
    if (matches!(status, 301 | 302) && state.method == Method::POST)
        || (status == 303 && state.method != Method::GET && state.method != Method::HEAD)
    {
        state.method = Method::GET;
        state.body = Bytes::new();
        for name in [
            "content-encoding",
            "content-language",
            "content-location",
            "content-type",
            "content-length",
        ] {
            state.headers.remove(name);
        }
    }
    if state.url.origin() != next.origin() {
        for name in ["authorization", "proxy-authorization", "cookie", "host"] {
            state.headers.remove(name);
        }
    }
    state.redirects += 1;
    state.url = next;
    Ok(true)
}

/// The header block undici's `fetch` puts on the wire for one hop, in wire
/// order: `host`, `connection: keep-alive`, the caller's headers in insertion
/// order, then the Fetch defaults for whichever of them the caller did not
/// set (`accept`, `accept-language`, `sec-fetch-mode`, `user-agent`,
/// `accept-encoding`). `content-length` is left to the HTTP client, which
/// writes it last, as undici does.
///
/// Observed from the baseline (Node 22) against a recording provider:
/// `host, connection, content-type, authorization, accept, accept-language,
/// sec-fetch-mode, user-agent, accept-encoding, content-length`.
pub fn undici_wire_headers(uri: &hyper::Uri, headers: &HeaderMap) -> HeaderMap {
    fetch_wire_headers(uri, headers, "undici")
}

/// The same block for Node's global `fetch`, whose default `user-agent` is
/// `node` (observed on the baseline's HTTP hook requests).
pub fn node_fetch_wire_headers(uri: &hyper::Uri, headers: &HeaderMap) -> HeaderMap {
    fetch_wire_headers(uri, headers, "node")
}

fn fetch_wire_headers(
    uri: &hyper::Uri,
    headers: &HeaderMap,
    user_agent: &'static str,
) -> HeaderMap {
    use hyper::header::{HeaderName, HeaderValue};
    let mut wire = HeaderMap::with_capacity(headers.len() + 7);
    if let Some(authority) = uri.authority()
        && let Ok(host) = HeaderValue::from_str(authority.as_str())
    {
        wire.insert(hyper::header::HOST, host);
    }
    wire.insert(
        hyper::header::CONNECTION,
        HeaderValue::from_static("keep-alive"),
    );
    for (name, value) in headers {
        if name == hyper::header::HOST || name == hyper::header::CONNECTION {
            continue;
        }
        wire.append(name.clone(), value.clone());
    }
    let https = uri.scheme_str() == Some("https");
    let defaults: [(HeaderName, &'static str, bool); 5] = [
        (hyper::header::ACCEPT, "*/*", false),
        (hyper::header::ACCEPT_LANGUAGE, "*", false),
        // `headersList.set`: replaces a caller value in place.
        (HeaderName::from_static("sec-fetch-mode"), "cors", true),
        (hyper::header::USER_AGENT, user_agent, false),
        (
            hyper::header::ACCEPT_ENCODING,
            if https {
                "br, gzip, deflate"
            } else {
                "gzip, deflate"
            },
            false,
        ),
    ];
    for (name, value, replace) in defaults {
        if replace || !wire.contains_key(&name) {
            wire.insert(name, HeaderValue::from_static(value));
        }
    }
    wire
}

/// Fetch bad-port table (Node 22); shared by every fetch-shaped client.
pub fn blocked_port(port: u16) -> bool {
    // Node 22 / Fetch's bad-port table. Check before any network exchange.
    matches!(
        port,
        1 | 7
            | 9
            | 11
            | 13
            | 15
            | 17
            | 19
            | 20
            | 21
            | 22
            | 23
            | 25
            | 37
            | 42
            | 43
            | 53
            | 69
            | 77
            | 79
            | 87
            | 95
            | 101
            | 102
            | 103
            | 104
            | 109
            | 110
            | 111
            | 113
            | 115
            | 117
            | 119
            | 123
            | 135
            | 137
            | 139
            | 143
            | 161
            | 179
            | 389
            | 427
            | 465
            | 512
            | 513
            | 514
            | 515
            | 526
            | 530
            | 531
            | 532
            | 540
            | 548
            | 554
            | 556
            | 563
            | 587
            | 601
            | 636
            | 989
            | 990
            | 993
            | 995
            | 1719
            | 1720
            | 1723
            | 2049
            | 3659
            | 4045
            | 4190
            | 5060
            | 5061
            | 6000
            | 6566
            | 6665
            | 6666
            | 6667
            | 6668
            | 6669
            | 6679
            | 6697
            | 10080
    )
}
