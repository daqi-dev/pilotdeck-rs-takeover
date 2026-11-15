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

struct DelayedControl {
    released: tokio::sync::watch::Receiver<bool>,
}
impl HttpExchangeControl for DelayedControl {
    fn stop_local(&self) {}
    fn snapshot(&self) -> HttpExchangeReceipt {
        HttpExchangeReceipt {
            dispatch: HttpDispatch::MayHaveDispatched,
            end: Some(HttpExchangeEnd::StoppedLocally),
            resources: if *self.released.borrow() {
                HttpResources::Released
            } else {
                HttpResources::Held
            },
        }
    }
    fn released(&self) -> BoxFuture<'_, HttpExchangeReceipt> {
        Box::pin(async move {
            let mut wait = self.released.clone();
            while !*wait.borrow_and_update() {
                if wait.changed().await.is_err() {
                    return HttpExchangeReceipt {
                        dispatch: HttpDispatch::MayHaveDispatched,
                        end: Some(HttpExchangeEnd::StoppedLocally),
                        resources: HttpResources::Unknown,
                    };
                }
            }
            self.snapshot()
        })
    }
}
struct DelayedScope {
    released: tokio::sync::watch::Receiver<bool>,
}
impl HttpScopeCleanup for DelayedScope {
    fn resources(&self) -> HttpResources {
        if *self.released.borrow() {
            HttpResources::Released
        } else {
            HttpResources::Held
        }
    }
    fn released(&self) -> BoxFuture<'_, HttpResources> {
        Box::pin(async move {
            let mut wait = self.released.clone();
            while !*wait.borrow_and_update() {
                if wait.changed().await.is_err() {
                    return HttpResources::Unknown;
                }
            }
            HttpResources::Released
        })
    }
}
struct DelayedPort {
    signal: tokio::sync::watch::Sender<bool>,
    calls: AtomicUsize,
    closes: AtomicUsize,
    unsupported: bool,
}
impl HttpPullTransport for DelayedPort {
    fn start(&self, _: HttpPullRequest, _: HttpPullContext) -> Result<HttpExchange, HttpFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(HttpExchange {
            response: Box::pin(async {
                Ok(HttpPullResponse {
                    head: HttpPullHead {
                        status: 200,
                        reason: None,
                        headers: vec![],
                    },
                    body: Box::pin(Body(None, true)),
                })
            }),
            control: Arc::new(DelayedControl {
                released: self.signal.subscribe(),
            }),
        })
    }
    fn close_scope(&self, _: HttpPullContext) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if self.unsupported {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Unsupported,
                "cleanup unsupported",
            ));
        }
        Ok(Arc::new(DelayedScope {
            released: self.signal.subscribe(),
        }))
    }
}
async fn release_session(port: Arc<dyn HttpPullTransport>) -> Arc<dyn ModelSession> {
    release_session_with_jobs(port, Arc::new(Jobs { deny: false })).await
}
async fn release_session_with_jobs(
    port: Arc<dyn HttpPullTransport>,
    jobs: Arc<dyn ModelJobServicesProvider>,
) -> Arc<dyn ModelSession> {
    let factory = SharedPilotDeckModelFactory {
        jobs,
        transport: port,
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
                (
                    REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE.into(),
                    MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1.into(),
                ),
            ],
            expected_revision: None,
        })
        .await
        .unwrap();
    session.next_event().await.unwrap().unwrap();
    session
}
async fn commit_release_job(
    session: &dyn ModelSession,
    input: &str,
    job: &str,
    token: HttpStreamCancel,
) {
    session
        .append_input(InputEnvelope {
            model_session_id: session.model_session_id().clone(),
            client_message_id: input.into(),
            expected_config_revision: None,
            input_id: InputId(input.into()),
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
        .inspect_input(InputId(input.into()), ConfigRevision(0))
        .await
        .unwrap();
    session
        .commit_input_scoped(
            InputId(input.into()),
            ModelExecutionContext::new(
                "session".into(),
                session.model_session_id().0.clone(),
                job.into(),
                "principal".into(),
                None,
                token,
            ),
        )
        .await
        .unwrap();
}
#[test]
fn interrupted_generation_reports_late_release_before_unknown_session_end() {
    runtime().block_on(async {
        let (signal, _) = tokio::sync::watch::channel(false);
        let port = Arc::new(DelayedPort {
            signal,
            calls: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            unsupported: false,
        });
        let session = release_session(port.clone()).await;
        let token = HttpStreamCancel::default();
        commit_release_job(session.as_ref(), "input", "job", token.clone()).await;
        let started = session.next_event().await.unwrap().unwrap();
        assert!(matches!(started.kind, EventKind::ResponseStarted));
        let response = started.response_id.unwrap();
        assert!(matches!(
            session.next_event().await.unwrap().unwrap().kind,
            EventKind::Structured { .. }
        ));
        let reader = {
            let session = session.clone();
            tokio::spawn(async move { session.next_event().await.unwrap().unwrap() })
        };
        while port.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        session.interrupt(&response).await.unwrap();
        let interrupted = reader.await.unwrap();
        assert!(matches!(
            interrupted.kind,
            EventKind::ResponseInterrupted {
                backend_stop: BackendStopStatus::Unknown,
                ..
            }
        ));
        session.close().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), session.next_event())
                .await
                .is_err()
        );
        assert_eq!(port.closes.load(Ordering::SeqCst), 1);
        port.signal.send_replace(true);
        let release = tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(release.input_id, Some(InputId("input".into())));
        assert_eq!(release.response_id, Some(response));
        let EventKind::LocalResourcesReleased { release } = release.kind else {
            panic!("missing release fact")
        };
        assert_eq!(release.context().job_id(), "job");
        assert!(release.context().cancel().same_source(&token));
        let terminal = session.next_event().await.unwrap().unwrap();
        assert!(matches!(
            terminal.kind,
            EventKind::SessionClosed {
                terminal: SessionTerminal::Unknown
            }
        ));
        assert!(session.next_event().await.unwrap().is_none());
    });
}
#[test]
fn unsupported_cleanup_never_becomes_release_fact() {
    runtime().block_on(async {
        let (signal, _) = tokio::sync::watch::channel(false);
        let port = Arc::new(DelayedPort {
            signal,
            calls: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            unsupported: true,
        });
        let session = release_session(port.clone()).await;
        commit_release_job(
            session.as_ref(),
            "input",
            "job",
            HttpStreamCancel::default(),
        )
        .await;
        let response = session
            .next_event()
            .await
            .unwrap()
            .unwrap()
            .response_id
            .unwrap();
        session.next_event().await.unwrap().unwrap();
        let reader = {
            let session = session.clone();
            tokio::spawn(async move { session.next_event().await.unwrap().unwrap() })
        };
        while port.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        session.interrupt(&response).await.unwrap();
        reader.await.unwrap();
        session.close().await.unwrap();
        let terminal =
            tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert!(matches!(
            terminal.kind,
            EventKind::SessionClosed {
                terminal: SessionTerminal::Unknown
            }
        ));
        assert!(session.next_event().await.unwrap().is_none());
    });
}

struct MultiPort;
impl HttpPullTransport for MultiPort {
    fn start(&self, _: HttpPullRequest, _: HttpPullContext) -> Result<HttpExchange, HttpFailure> {
        let wire = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec();
        Ok(HttpExchange {
            response: Box::pin(async move {
                Ok(HttpPullResponse {
                    head: HttpPullHead {
                        status: 200,
                        reason: None,
                        headers: vec![],
                    },
                    body: Box::pin(Body(Some(wire), false)),
                })
            }),
            control: Arc::new(Control(true)),
        })
    }
    fn close_scope(&self, _: HttpPullContext) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        Ok(Arc::new(Cleanup))
    }
}
#[test]
fn sequential_generations_keep_distinct_original_release_tokens() {
    runtime().block_on(async {
        let session = release_session(Arc::new(MultiPort)).await;
        let first = HttpStreamCancel::default();
        let second = HttpStreamCancel::default();
        for (input, job, expected, other) in [
            ("one", "job-one", first.clone(), second.clone()),
            ("two", "job-two", second.clone(), first.clone()),
        ] {
            commit_release_job(session.as_ref(), input, job, expected.clone()).await;
            let mut releases = 0;
            loop {
                let event =
                    tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                match event.kind {
                    EventKind::LocalResourcesReleased { release } => {
                        releases += 1;
                        assert_eq!(event.input_id, Some(InputId(input.into())));
                        assert_eq!(release.context().job_id(), job);
                        assert!(release.context().cancel().same_source(&expected));
                        assert!(!release.context().cancel().same_source(&other));
                    }
                    EventKind::ResponseCompleted => break,
                    EventKind::ResponseFailed { message, .. } => {
                        panic!("unexpected failure: {message}")
                    }
                    _ => {}
                }
            }
            assert_eq!(releases, 1);
        }
    });
}

#[test]
fn host_token_can_stop_driver_before_interrupt_is_polled_without_duplicate_terminal() {
    runtime().block_on(async {
        let session = release_session(Arc::new(MultiPort)).await;
        let token = HttpStreamCancel::default();
        commit_release_job(session.as_ref(), "first", "job-first", token.clone()).await;
        let response = session
            .next_event()
            .await
            .unwrap()
            .unwrap()
            .response_id
            .unwrap();
        assert!(matches!(
            session.next_event().await.unwrap().unwrap().kind,
            EventKind::Structured { .. }
        ));
        // Deterministically establish the token-first branch before calling
        // interrupt; this is not a substitute for a Host monitor barrier test.
        token.cancel();
        let mut failures = 0;
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            if matches!(&event.kind, EventKind::ResponseFailed { .. }) {
                failures += 1;
                break;
            }
            assert!(!matches!(&event.kind, EventKind::ResponseCompleted));
        }
        assert_eq!(failures, 1);
        let ack = session.interrupt(&response).await.unwrap();
        assert!(ack.delivery_stopped);
        assert_eq!(ack.backend_stop, BackendStopStatus::Unknown);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), session.next_event())
                .await
                .is_err()
        );
        // An older response cannot interrupt or acknowledge a new active job.
        let other = HttpStreamCancel::default();
        commit_release_job(session.as_ref(), "second", "job-second", other.clone()).await;
        let next = session
            .next_event()
            .await
            .unwrap()
            .unwrap()
            .response_id
            .unwrap();
        assert_ne!(next, response);
        assert!(session.interrupt(&response).await.is_err());
        assert!(!other.is_cancelled());
        session.interrupt(&next).await.unwrap();
    });
}
#[test]
fn naturally_completed_response_is_never_acknowledged_as_interrupted() {
    runtime().block_on(async {
        let session = release_session(Arc::new(MultiPort)).await;
        commit_release_job(
            session.as_ref(),
            "complete",
            "job-complete",
            HttpStreamCancel::default(),
        )
        .await;
        let response = session
            .next_event()
            .await
            .unwrap()
            .unwrap()
            .response_id
            .unwrap();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            if matches!(event.kind, EventKind::ResponseCompleted) {
                break;
            }
            assert!(!matches!(event.kind, EventKind::ResponseFailed { .. }));
        }
        assert!(session.interrupt(&response).await.is_err());
    });
}

struct GuardCancelledJobs;
impl ModelJobServicesProvider for GuardCancelledJobs {
    fn supports_binding(&self, binding: &ModelBinding) -> bool {
        binding.model == "m" && binding.endpoint_ref.as_deref() == Some("p")
    }
    fn for_job(
        &self,
        _: &ModelExecutionContext,
        _: &ModelBinding,
        _: ConfigRevision,
    ) -> Result<ModelJobServices, HttpFailure> {
        Err(HttpFailure::new(
            HttpFailurePhase::Scope,
            HttpFailureKind::Cancelled,
            "guard cancelled before token",
        ))
    }
}
#[test]
fn typed_guard_cancel_before_token_still_allows_late_ack_only() {
    runtime().block_on(async {
        let port = Arc::new(Port::default());
        let session = release_session_with_jobs(port.clone(), Arc::new(GuardCancelledJobs)).await;
        let token = HttpStreamCancel::default();
        commit_release_job(session.as_ref(), "guard", "job-guard", token.clone()).await;
        let response = session
            .next_event()
            .await
            .unwrap()
            .unwrap()
            .response_id
            .unwrap();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            if matches!(&event.kind, EventKind::ResponseFailed { .. }) {
                break;
            }
            assert!(!matches!(&event.kind, EventKind::ResponseCompleted));
        }
        assert!(
            !token.is_cancelled(),
            "fixture must cover guard-before-token ordering"
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
        let ack = session.interrupt(&response).await.unwrap();
        assert!(ack.delivery_stopped);
        assert_eq!(ack.backend_stop, BackendStopStatus::Unknown);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), session.next_event())
                .await
                .is_err()
        );
        session.close().await.unwrap();
        assert!(
            session.interrupt(&response).await.is_err(),
            "a closed session cannot accept a stale ACK"
        );
    });
}

#[test]
fn non_cancelled_host_failure_is_not_acknowledged_as_interruption() {
    runtime().block_on(async {
        let session =
            release_session_with_jobs(Arc::new(Port::default()), Arc::new(Jobs { deny: true }))
                .await;
        commit_release_job(
            session.as_ref(),
            "deny",
            "job-deny",
            HttpStreamCancel::default(),
        )
        .await;
        let response = session
            .next_event()
            .await
            .unwrap()
            .unwrap()
            .response_id
            .unwrap();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            if matches!(&event.kind, EventKind::ResponseFailed { .. }) {
                break;
            }
            assert!(!matches!(&event.kind, EventKind::ResponseCompleted));
        }
        assert!(session.interrupt(&response).await.is_err());
    });
}

struct GuardCancelledPort {
    calls: AtomicUsize,
}
impl HttpPullTransport for GuardCancelledPort {
    fn start(&self, _: HttpPullRequest, _: HttpPullContext) -> Result<HttpExchange, HttpFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(HttpFailure::new(
            HttpFailurePhase::Scope,
            HttpFailureKind::Cancelled,
            "guard revoked before token",
        ))
    }
    fn close_scope(&self, _: HttpPullContext) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        Ok(Arc::new(Cleanup))
    }
}
#[test]
fn guard_cancel_at_real_driver_dispatch_before_token_still_gives_late_ack() {
    runtime().block_on(async {
        let port = Arc::new(GuardCancelledPort {
            calls: AtomicUsize::new(0),
        });
        let session = release_session(port.clone()).await;
        let token = HttpStreamCancel::default();
        commit_release_job(
            session.as_ref(),
            "guard-dispatch",
            "job-guard-dispatch",
            token.clone(),
        )
        .await;
        let response = session
            .next_event()
            .await
            .unwrap()
            .unwrap()
            .response_id
            .unwrap();
        let mut saw_source = false;
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(3), session.next_event())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            match event.kind {
                EventKind::Structured { .. } => saw_source = true,
                EventKind::ResponseFailed { .. } => break,
                EventKind::ResponseCompleted => panic!("cancelled dispatch became success"),
                _ => {}
            }
        }
        assert!(
            saw_source,
            "real shared driver must reach its dispatch boundary"
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 1);
        assert!(!token.is_cancelled(), "guard must win before monitor token");
        let ack = session.interrupt(&response).await.unwrap();
        assert!(ack.delivery_stopped);
        assert_eq!(ack.backend_stop, BackendStopStatus::Unknown);
    });
}
