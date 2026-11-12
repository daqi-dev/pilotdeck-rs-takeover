//! Node-compatible `fetch` for native CLI consumers of the shared transport.
//!
//! The caller owns the abort/deadline and decides whether to consume the body.
//! Returning after headers is necessary for health probes and QR polling;
//! collecting the body here would change both their timing and failure rules.
use super::{ModelBody, ModelHttpTransport, TransportFailure};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;
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
        |request| send_fetch(transport, request),
    )
    .await
}

async fn send_fetch(
    transport: &dyn ModelHttpTransport,
    mut request: Request<Full<Bytes>>,
) -> Result<Response<ModelBody>, TransportFailure> {
    let headers = crate::fetch_redirect::node_fetch_wire_headers(request.uri(), request.headers());
    *request.headers_mut() = headers;
    // Node 22 fetch rejects an unsolicited 100 with UND_ERR_SOCKET, while
    // Hyper otherwise consumes it and waits for the final response. 102 and
    // 103 remain valid informational responses. Register on every redirect.
    let bad_response = Arc::new(AtomicBool::new(false));
    let notified = Arc::new(Notify::new());
    let seen = bad_response.clone();
    let wake = notified.clone();
    hyper::ext::on_informational(&mut request, move |response| {
        if response.status() == StatusCode::CONTINUE {
            seen.store(true, Ordering::Release);
            wake.notify_one();
        }
    });
    let result = tokio::select! {
        biased;
        _ = notified.notified() => Err(TransportFailure::fetch_failed(Some("UND_ERR_SOCKET"))),
        response = transport.send(request) => response,
    };
    // A single poll can parse both 100 and 200. In that case the send future
    // may finish before select polls the notification again; the 100 still
    // wins. The rejected send future/response is dropped on both paths.
    if bad_response.load(Ordering::Acquire) {
        Err(TransportFailure::fetch_failed(Some("UND_ERR_SOCKET")))
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::http::HyperModelTransport;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn unsolicited_continue_rejects_without_a_final_response_or_probe_timer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("C1 100-only mock bind");
        let address = listener.local_addr().expect("C1 100-only mock address");
        let server = tokio::spawn(async move {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(3), listener.accept()).await
            else {
                return false;
            };
            let mut request = Vec::new();
            let mut buffer = [0; 512];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let Ok(Ok(count)) =
                    tokio::time::timeout(Duration::from_secs(3), socket.read(&mut buffer)).await
                else {
                    return false;
                };
                assert_ne!(count, 0, "C1 100-only complete request");
                request.extend_from_slice(&buffer[..count]);
            }
            assert!(request.starts_with(b"GET /health HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .expect("C1 100-only response");
            // Never send final headers and keep the peer open. Cancellation
            // must drop the pending exchange, not wait for a 200 or the CLI's
            // 200 ms timer (which this test never calls).
            matches!(
                tokio::time::timeout(Duration::from_secs(3), socket.read(&mut buffer)).await,
                Ok(Ok(0))
            )
        });
        let transport = HyperModelTransport::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            fetch(
                &transport,
                Url::parse(&format!("http://{address}/health")).expect("C1 100-only URL"),
                Method::GET,
                HeaderMap::new(),
                Bytes::new(),
            ),
        )
        .await;
        // Join before asserting so a failure still releases the owned mock.
        let peer_closed = server.await.expect("C1 100-only mock joined");
        let Err(error) = outcome.expect("100 must reject without a final response") else {
            panic!("100 must be rejected");
        };
        assert_eq!(error.message, "fetch failed");
        assert_eq!(error.cause_code, Some("UND_ERR_SOCKET"));
        assert!(
            peer_closed,
            "rejected fetch releases its pending connection"
        );
    }
}
