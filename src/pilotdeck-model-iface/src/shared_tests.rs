use super::*;
use agentcpm_iface::env::http_pull::*;
use pilotdeck_model_core::codec::normalize_error::JsError;
use pilotdeck_model_runtime::{
    driver_transport::HostOperationError,
    headers::{
        HeaderMaterializeError, HeaderMaterializeResult, HeaderProjection, HeaderValidateError,
        ModelHeaderService, PreparedModelHeaders, RequestMode,
    },
    services::ModelHostServices,
};
use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};

struct Host {
    issuer: HttpPreparationIssuer,
    deny: bool,
}
struct Prepared {
    token: HttpRequestPreparation,
}
impl ModelHeaderService for Host {
    type Rejection = HttpFailure;
    type Preparation = HttpRequestPreparation;
    fn materialize(
        &self,
        _: &JsValue,
        _: &JsValue,
        _: RequestMode,
    ) -> HeaderMaterializeResult<HttpFailure, HttpRequestPreparation> {
        if self.deny {
            return Err(HeaderMaterializeError::Rejected(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Denied,
                "test denied",
            )));
        }
        Ok(Box::new(Prepared {
            token: self.issuer.issue(()),
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
        let (url, headers) = pilotdeck_model_runtime::headers::build_http_request(url, &[])
            .map_err(HeaderValidateError::Original)?;
        Ok(HeaderProjection {
            url,
            headers,
            preparation: self.token.clone(),
        })
    }
}
impl ModelHostServices for Host {
    fn dump_request_if_enabled(
        &self,
        _: &JsValue,
        _: &JsValue,
    ) -> Result<(), HostOperationError<JsError, HttpFailure>> {
        Ok(())
    }
    fn parse_retry_after_date(&self, _: &JsString) -> Result<Option<f64>, JsString> {
        Ok(None)
    }
    fn format_epoch_iso(&self, _: i64) -> String {
        "1970-01-01T00:00:00.000Z".into()
    }
    fn streaming_duration_limit(&self) -> Option<String> {
        None
    }
    fn diagnostic(&self, _: &str) {}
}
struct Jobs {
    deny: bool,
}
impl ModelJobServicesProvider for Jobs {
    fn supports_binding(&self, binding: &ModelBinding) -> bool {
        binding.endpoint_ref.as_deref() == Some("p") && binding.model == "m"
    }
    fn for_job(
        &self,
        ctx: &ModelExecutionContext,
        _: &ModelBinding,
        _: ConfigRevision,
    ) -> Result<ModelJobServices, HttpFailure> {
        assert_eq!(ctx.principal(), "principal");
        Ok(ModelJobServices {
            safe_config: parse_json(r#"{"providers":{"p":{"id":"p","protocol":"openai","url":"http://localhost","models":{"m":{"id":"m","capabilities":{"supportsStreaming":true},"multimodal":{"input":["text"]}}}}}}"#).unwrap(),
            host: Arc::new(Host { issuer: HttpPreparationIssuer::new(), deny: self.deny }),
            options: Default::default(),
        })
    }
}
struct Control(bool);
impl HttpExchangeControl for Control {
    fn stop_local(&self) {}
    fn snapshot(&self) -> HttpExchangeReceipt {
        HttpExchangeReceipt {
            dispatch: HttpDispatch::MayHaveDispatched,
            end: Some(HttpExchangeEnd::Eof),
            resources: if self.0 {
                HttpResources::Released
            } else {
                HttpResources::Unknown
            },
        }
    }
    fn released(&self) -> BoxFuture<'_, HttpExchangeReceipt> {
        Box::pin(async { self.snapshot() })
    }
}
struct Cleanup;
impl HttpScopeCleanup for Cleanup {
    fn resources(&self) -> HttpResources {
        HttpResources::Released
    }
    fn released(&self) -> BoxFuture<'_, HttpResources> {
        Box::pin(async { self.resources() })
    }
}
struct Body(Option<Vec<u8>>, bool);
impl HttpBody for Body {
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<HttpFrame, HttpFailure>>> {
        if self.1 {
            return Poll::Pending;
        }
        Poll::Ready(self.0.take().map(|bytes| Ok(HttpFrame::Data(bytes))))
    }
}
#[derive(Default)]
struct Port {
    calls: AtomicUsize,
    closes: AtomicUsize,
    unknown: bool,
    head_denied: bool,
    pending: bool,
}
impl HttpPullTransport for Port {
    fn start(
        &self,
        request: HttpPullRequest,
        context: HttpPullContext,
    ) -> Result<HttpExchange, HttpFailure> {
        assert_eq!(context.principal, "principal");
        assert_eq!(context.job_id, "job");
        assert!(request.preparation.is_some());
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.head_denied {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Denied,
                "dispatch denied",
            ));
        }
        let body = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec();
        let pending = self.pending;
        Ok(HttpExchange {
            response: Box::pin(async move {
                Ok(HttpPullResponse {
                    head: HttpPullHead {
                        status: 200,
                        reason: None,
                        headers: vec![],
                    },
                    body: Box::pin(Body(Some(body), pending)),
                })
            }),
            control: Arc::new(Control(!self.unknown)),
        })
    }
    fn close_scope(&self, _: HttpPullContext) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Cleanup))
    }
}
async fn run(
    deny: bool,
    unknown: bool,
    head_denied: bool,
    pending: bool,
) -> (Vec<EventKind>, Arc<Port>) {
    let port = Arc::new(Port {
        unknown,
        head_denied,
        pending,
        ..Port::default()
    });
    let factory = SharedPilotDeckModelFactory {
        jobs: Arc::new(Jobs { deny }),
        transport: port.clone(),
        executor: tokio::runtime::Handle::current(),
    };
    let session = factory
        .open(ModelSessionConfig {
            binding: ModelBinding::new("m".into(), Some("p".into()), vec![]),
            mode: SessionMode::Request,
            input_formats: vec![],
            output_formats: vec![],
            tool_catalog_ref: None,
            generation: GenerationSettings::default(),
            limits: SessionLimits::default(),
            timeouts: SessionTimeouts::default(),
            extensions: vec![
                (
                    REQUIRED_MODEL_REQUEST_PROFILE.into(),
                    MODEL_REQUEST_PROFILE_V1.into(),
                ),
                (
                    REQUIRED_MODEL_EVENT_PROFILE.into(),
                    MODEL_EVENT_PROFILE_V1.into(),
                ),
            ],
            expected_revision: None,
        })
        .await
        .unwrap();
    session.next_event().await.unwrap().unwrap();
    session
        .append_input(InputEnvelope {
            model_session_id: session.model_session_id().clone(),
            client_message_id: "input".into(),
            expected_config_revision: None,
            input_id: InputId("input".into()),
            chunk_seq: 0,
            last_chunk: false,
            kind: InputKind::RequestSnapshot,
            content_type: MODEL_REQUEST_PROFILE_V1.into(),
            content: br#"{"provider":"p","model":"m","messages":[]}"#.to_vec(),
            media_seq: None,
            media_time_ms: None,
            media_duration_ms: None,
        })
        .await
        .unwrap();
    session
        .inspect_input(InputId("input".into()), ConfigRevision(0))
        .await
        .unwrap();
    let cancel = HttpStreamCancel::default();
    session
        .commit_input_scoped(
            InputId("input".into()),
            ModelExecutionContext::new(
                "session".into(),
                session.model_session_id().0.clone(),
                "job".into(),
                "principal".into(),
                None,
                cancel.clone(),
            ),
        )
        .await
        .unwrap();
    if pending {
        let observed = port.clone();
        tokio::spawn(async move {
            while observed.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            cancel.cancel();
        });
    }
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let done = matches!(
            event.kind,
            EventKind::ResponseCompleted | EventKind::ResponseFailed { .. }
        );
        events.push(event.kind);
        if done {
            return (events, port);
        }
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}
#[test]
fn real_shared_driver_streams_and_waits_for_scope_receipt() {
    let (events, port) = runtime().block_on(run(false, false, false, false));
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert_eq!(port.closes.load(Ordering::SeqCst), 1);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventKind::Structured { .. }))
    );
    assert!(matches!(events.last(), Some(EventKind::ResponseCompleted)));
}
#[test]
fn typed_header_denial_is_not_retried_and_has_no_attempt_to_release() {
    let (events, port) = runtime().block_on(run(true, false, false, false));
    assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    assert_eq!(port.closes.load(Ordering::SeqCst), 1);
    assert!(
        matches!(events.last(), Some(EventKind::ResponseFailed { message, retryable: false, resource_released: true, .. }) if message == "test denied")
    );
}

#[test]
fn unknown_attempt_receipt_never_becomes_success() {
    let (events, port) = runtime().block_on(run(false, true, false, false));
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert_eq!(port.closes.load(Ordering::SeqCst), 1);
    assert!(matches!(
        events.last(),
        Some(EventKind::ResponseFailed {
            resource_released: false,
            ..
        })
    ));
}

#[test]
fn typed_transport_denial_does_not_become_provider_retry() {
    let (events, port) = runtime().block_on(run(false, false, true, false));
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert_eq!(port.closes.load(Ordering::SeqCst), 1);
    assert!(
        matches!(events.last(), Some(EventKind::ResponseFailed { message, retryable: false, resource_released: true, .. }) if message == "dispatch denied")
    );
}

#[test]
fn hard_cancel_wakes_pending_driver_and_runs_cleanup() {
    let (events, port) = runtime().block_on(run(false, false, false, true));
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert_eq!(port.closes.load(Ordering::SeqCst), 1);
    assert!(
        matches!(events.last(), Some(EventKind::ResponseFailed { message, retryable: false, .. }) if message == "model job cancelled")
    );
}
