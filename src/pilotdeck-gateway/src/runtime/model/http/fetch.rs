//! Node-compatible `fetch` for native CLI consumers of the shared transport.
//!
//! The caller owns the abort/deadline and decides whether to consume the body.
//! Returning after headers is necessary for health probes and QR polling;
//! collecting the body here would change both their timing and failure rules.
use super::{ModelBody, ModelHttpTransport, TransportFailure};
use bytes::Bytes;
use hyper::{HeaderMap, Method, Response};
use url::Url;

/// Follow the fixed baseline's default redirect policy and apply the Node
/// global-fetch headers at every hop. Drops and cancellation retain the
/// supplied transport's resource ownership semantics.
pub async fn fetch(
    transport: &dyn ModelHttpTransport,
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<ModelBody>, TransportFailure> {
    crate::fetch_redirect::follow(
        url,
        method,
        headers,
        body,
        || TransportFailure::fetch_failed(None),
        |mut request| {
            let headers =
                crate::fetch_redirect::node_fetch_wire_headers(request.uri(), request.headers());
            *request.headers_mut() = headers;
            transport.send(request)
        },
    )
    .await
}
