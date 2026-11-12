//! api.js/auth.js clear their explicit abort timer after response headers.
//! The CLI's global fetch dispatcher still bounds each hop and body idle time.
use super::WeixinClientError;
use crate::runtime::model::http::{
    HyperModelTransport, ModelBody, ModelHttpTransport, TransportFailure, TransportFuture, fetch,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use std::time::Duration;

const TRANSPORT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Default)]
pub(super) struct Network<T = HyperModelTransport>(T);
impl<T: ModelHttpTransport> ModelHttpTransport for Network<T> {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let exchange = self.0.send(request);
        TransportFuture::io(async move {
            tokio::time::timeout(TRANSPORT_TIMEOUT, exchange)
                .await
                .map_err(|_| TransportFailure::fetch_failed(None))?
        })
    }
}
impl<T: ModelHttpTransport> Network<T> {
    pub(super) async fn send(
        &self,
        request: Request<Full<Bytes>>,
        abort_after: Option<Duration>,
    ) -> Result<Response<ModelBody>, WeixinClientError> {
        let (parts, body) = request.into_parts();
        let body = body
            .collect()
            .await
            .expect("Full body is infallible")
            .to_bytes();
        let url = url::Url::parse(&parts.uri.to_string())
            .map_err(|_| WeixinClientError::fetch_failed())?;
        let send = fetch::fetch(self, url, parts.method, parts.headers, body);
        let response = match abort_after {
            Some(timeout) => tokio::time::timeout(timeout, send)
                .await
                .map_err(|_| WeixinClientError::abort())?,
            None => send.await,
        };
        response.map_err(|_| WeixinClientError::fetch_failed())
    }
}
pub(super) async fn read_body(response: Response<ModelBody>) -> Result<Bytes, WeixinClientError> {
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = tokio::time::timeout(TRANSPORT_TIMEOUT, body.frame())
        .await
        .map_err(|_| terminated())?
    {
        let frame = frame.map_err(|_| terminated())?;
        if let Some(chunk) = frame.data_ref() {
            if bytes.len().saturating_add(chunk.len()) > 0x1fff_ffe8 {
                return Err(WeixinClientError {
                    name: "RangeError".into(),
                    message: "Cannot create a string longer than 0x1fffffe8 characters".into(),
                });
            }
            bytes.extend_from_slice(chunk);
        }
    }
    Ok(Bytes::from(bytes))
}
fn terminated() -> WeixinClientError {
    WeixinClientError {
        name: "TypeError".into(),
        message: "terminated".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use hyper::body::Frame;

    struct Delayed {
        headers: u64,
        chunks: Vec<u64>,
    }
    impl ModelHttpTransport for Delayed {
        fn send(&self, _: Request<Full<Bytes>>) -> TransportFuture {
            let headers = self.headers;
            let chunks = self.chunks.clone();
            TransportFuture::io(async move {
                tokio::time::sleep(Duration::from_secs(headers)).await;
                let frames = futures_util::stream::iter(chunks).then(|delay| async move {
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    Ok::<_, crate::runtime::model::http::BodyFailure>(Frame::data(
                        Bytes::from_static(b"x"),
                    ))
                });
                Ok(Response::new(BodyExt::boxed(
                    http_body_util::StreamBody::new(frames),
                )))
            })
        }
    }
    fn request() -> Request<Full<Bytes>> {
        Request::builder()
            .uri("http://isolated.invalid/qr")
            .body(Full::new(Bytes::new()))
            .unwrap()
    }
    #[tokio::test(start_paused = true)]
    async fn initial_qr_has_no_poll_abort_but_poll_headers_do() {
        let network = Network(Delayed {
            headers: 36,
            chunks: vec![],
        });
        let started = tokio::time::Instant::now();
        assert!(network.send(request(), None).await.is_ok());
        assert!(started.elapsed() >= Duration::from_secs(36));
        let error = network
            .send(request(), Some(Duration::from_secs(35)))
            .await
            .unwrap_err();
        assert_eq!(error.name, "AbortError");
        assert_eq!(error.message, "This operation was aborted");
    }
    #[tokio::test(start_paused = true)]
    async fn poll_abort_ends_at_headers_and_body_budget_resets_per_frame() {
        let network = Network(Delayed {
            headers: 0,
            chunks: vec![400, 400],
        });
        let response = network
            .send(request(), Some(Duration::from_secs(35)))
            .await
            .unwrap();
        let started = tokio::time::Instant::now();
        assert_eq!(
            read_body(response).await.unwrap(),
            Bytes::from_static(b"xx")
        );
        assert!(started.elapsed() >= Duration::from_secs(800));
        let network = Network(Delayed {
            headers: 0,
            chunks: vec![601],
        });
        let response = network.send(request(), None).await.unwrap();
        let error = read_body(response).await.unwrap_err();
        assert_eq!(error.formatted(), "TypeError: terminated");
    }
}
