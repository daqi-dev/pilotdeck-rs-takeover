//! Adapter for the admitted, lazy pull transport. It owns no credentials and
//! never treats cancellation or the dropping of a body as a release receipt.
use agentcpm_iface::env::http_pull::{
    HttpBody, HttpExchangeControl, HttpFailure, HttpFailureKind, HttpFailurePhase, HttpFrame,
    HttpPullContext, HttpPullRequest, HttpPullTransport, HttpRequestPreparation, HttpResources,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::{Body, Frame, SizeHint},
};
use pilotdeck_model_runtime::{
    driver_transport::{
        DriverBody, DriverBodyFailure, DriverHttpTransport, DriverNetworkFailure,
        DriverTransportFuture, HostOperationError,
    },
    network::ExchangeFuture,
};
use std::{
    borrow::Cow,
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

fn head_failure(error: HttpFailure) -> HostOperationError<DriverNetworkFailure, HttpFailure> {
    if matches!(
        error.phase,
        HttpFailurePhase::Dns
            | HttpFailurePhase::Connect
            | HttpFailurePhase::Tls
            | HttpFailurePhase::ResponseHead
    ) && error.kind == HttpFailureKind::Network
    {
        // The trusted host retains the product's outer network/encoding message.
        // An unknown or absent code is still a network error, not a policy refusal.
        HostOperationError::Original(DriverNetworkFailure {
            cause_code: error
                .network_cause
                .and_then(|cause| cause.code)
                .map(Cow::Owned),
            message: error.detail,
        })
    } else {
        HostOperationError::Rejected(error)
    }
}
fn body_failure(error: HttpFailure) -> HostOperationError<DriverBodyFailure, HttpFailure> {
    if error.phase == HttpFailurePhase::ResponseBody {
        if error.kind == HttpFailureKind::ResponseBodyTooLarge {
            return HostOperationError::Original(DriverBodyFailure::StringTooLong);
        }
        if error.kind == HttpFailureKind::Network {
            return HostOperationError::Original(DriverBodyFailure::Terminated);
        }
        if error.kind == HttpFailureKind::Decode
            && let Some(cause) = &error.network_cause
        {
            return HostOperationError::Original(DriverBodyFailure::Decode(Cow::Owned(
                cause.message.clone(),
            )));
        }
    }
    // Resource is not a byte-limit discriminator; only the explicit public
    // response-body-too-large kind above can become the original V8 failure.
    HostOperationError::Rejected(error)
}
struct PullBody(Pin<Box<dyn HttpBody>>);
impl Body for PullBody {
    type Data = Bytes;
    type Error = HostOperationError<DriverBodyFailure, HttpFailure>;
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

#[cfg(test)]
#[path = "pull_error_tests.rs"]
mod error_tests;
