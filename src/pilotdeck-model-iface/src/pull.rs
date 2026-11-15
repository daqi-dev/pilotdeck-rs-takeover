//! Adapter for the admitted, lazy pull transport. It owns no credentials and
//! never treats cancellation or the dropping of a body as a release receipt.
use agentcpm_iface::env::http_pull::{
    HttpBody, HttpExchangeControl, HttpFailure, HttpFailureKind, HttpFrame, HttpPullContext,
    HttpPullRequest, HttpPullTransport, HttpRequestPreparation, HttpResources,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::{Body, Frame, SizeHint},
};
use pilotdeck_model_runtime::{
    driver_transport::{
        DriverBody, DriverHttpTransport, DriverTransportFuture, HostOperationError,
    },
    network::ExchangeFuture,
    transport::{BodyFailure, TransportFailure},
};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

/// All controls remain owned through scope cleanup, including failed heads and
/// locally stopped redirects. No per-frame task or buffer is introduced.
pub(crate) struct PullAdapter {
    pub port: Arc<dyn HttpPullTransport>,
    pub context: HttpPullContext,
    controls: Arc<Mutex<Vec<Arc<dyn HttpExchangeControl>>>>,
}
impl PullAdapter {
    pub fn new(port: Arc<dyn HttpPullTransport>, context: HttpPullContext) -> Self {
        Self {
            port,
            context,
            controls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Called once the driver will not start more attempts. Unknown and Held
    /// are never promoted to Released, including cleanup failures.
    pub async fn cleanup(&self) -> bool {
        let controls = self.controls.lock().unwrap().clone();
        let Ok(scope) = self.port.close_scope(self.context.clone()) else {
            return false;
        };
        for control in controls {
            if control.released().await.resources != HttpResources::Released {
                return false;
            }
        }
        scope.released().await == HttpResources::Released
    }
}

fn static_code(code: Option<&str>) -> Option<&'static str> {
    match code {
        Some("ECONNREFUSED") => Some("ECONNREFUSED"),
        Some("ECONNRESET") => Some("ECONNRESET"),
        Some("ENOTFOUND") => Some("ENOTFOUND"),
        Some("EAI_AGAIN") => Some("EAI_AGAIN"),
        Some("ETIMEDOUT") => Some("ETIMEDOUT"),
        Some("UND_ERR_CONNECT_TIMEOUT") => Some("UND_ERR_CONNECT_TIMEOUT"),
        Some("UND_ERR_SOCKET") => Some("UND_ERR_SOCKET"),
        _ => None,
    }
}
fn head_failure(error: HttpFailure) -> HostOperationError<TransportFailure, HttpFailure> {
    if error.kind == HttpFailureKind::Network {
        if let Some(cause) = &error.network_cause {
            // Unknown codes cannot be faithfully represented by the legacy
            // &'static str type; retain them as typed host failures instead.
            let code = static_code(cause.code.as_deref());
            if cause.code.is_none() || code.is_some() {
                return HostOperationError::Original(TransportFailure::fetch_failed(code));
            }
        }
    }
    HostOperationError::Rejected(error)
}
fn body_failure(error: HttpFailure) -> HostOperationError<BodyFailure, HttpFailure> {
    if error.kind == HttpFailureKind::Network {
        HostOperationError::Original(BodyFailure::Terminated)
    } else {
        HostOperationError::Rejected(error)
    }
}
struct PullBody(Pin<Box<dyn HttpBody>>);
impl Body for PullBody {
    type Data = Bytes;
    type Error = HostOperationError<BodyFailure, HttpFailure>;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0.as_mut().poll_frame(cx).map(|result| {
            result.map(|result| {
                result
                    .map(|frame| {
                        Frame::data(match frame {
                            HttpFrame::Data(bytes) => Bytes::from(bytes),
                            HttpFrame::Progress => Bytes::new(),
                        })
                    })
                    .map_err(body_failure)
            })
        })
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}
impl DriverHttpTransport for PullAdapter {
    type Rejection = HttpFailure;
    type Preparation = HttpRequestPreparation;
    fn send(
        &self,
        request: Request<Full<Bytes>>,
        preparation: Self::Preparation,
    ) -> DriverTransportFuture<HttpFailure> {
        let port = self.port.clone();
        let context = self.context.clone();
        let controls = self.controls.clone();
        ExchangeFuture::io(async move {
            let (parts, body) = request.into_parts();
            let body = body
                .collect()
                .await
                .expect("Full body is infallible")
                .to_bytes()
                .to_vec();
            let request = HttpPullRequest {
                method: parts.method.to_string(),
                url: parts.uri.to_string(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
                    .collect(),
                body,
                caller_follows_redirects: true,
                preparation: Some(preparation),
            };
            let exchange = port.start(request, context).map_err(head_failure)?;
            controls.lock().unwrap().push(exchange.control);
            let response = exchange.response.await.map_err(head_failure)?;
            let mut builder = Response::builder().status(response.head.status);
            if let Some(reason) = response.head.reason {
                let reason = hyper::ext::ReasonPhrase::try_from(reason).map_err(|_| {
                    HostOperationError::Rejected(HttpFailure::new(
                        agentcpm_iface::env::http_pull::HttpFailurePhase::ResponseHead,
                        HttpFailureKind::Protocol,
                        "invalid HTTP reason phrase",
                    ))
                })?;
                builder = builder.extension(reason);
            }
            for (name, value) in response.head.headers {
                builder = builder.header(name, value);
            }
            let body: DriverBody<HttpFailure> = PullBody(response.body).boxed();
            builder.body(body).map_err(|_| {
                HostOperationError::Rejected(HttpFailure::new(
                    agentcpm_iface::env::http_pull::HttpFailurePhase::ResponseHead,
                    HttpFailureKind::Protocol,
                    "invalid HTTP response head",
                ))
            })
        })
    }
}
