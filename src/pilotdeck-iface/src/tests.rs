use super::*;
use agentcpm_scheduler_protocol::*;
use pilotdeck_agent::ports::{HostCall, HostPort, HostReply, PreparedRequestHandle};
use resources::{ResourceCodec, ResourceRequest, ResourceStep};
use std::sync::atomic::{AtomicUsize, Ordering};

fn invalid_response_wire(message: pilotdeck_protocol::JsString) -> Failure {
    let completion = port_failure(PortError::InvalidResponse { message });
    assert!(matches!(completion.status, RequestState::Failed));
    let StepOutcome::Failed {
        code,
        message,
        stop_reason,
    } = completion.outcome
    else {
        panic!("invalid response must remain an ordinary failure");
    };
    assert_eq!(stop_reason.as_deref(), Some("host_port_failure"));
    Failure::from_wire(code, message)
}

#[test]
fn invalid_response_uses_public_readable_diagnostic_and_exact_utf16() {
    let message = "宿主响应缺少字段 😀";
    let failure = invalid_response_wire(message.into());
    assert_eq!(failure.code, "PILOTDECK_PORT_FAILURE");
    assert_eq!(failure.message_key, "diagnostic.external");
    assert_eq!(failure.params["pilotdeck_error_kind"], "invalid_response");
    assert_eq!(failure.params["detail"], message);
    assert_eq!(failure.params["detail_encoding"], "utf8");
    assert_eq!(
        failure.params["message_utf16"],
        serde_json::json!(message.encode_utf16().collect::<Vec<_>>())
    );
}

#[test]
fn invalid_response_keeps_lone_surrogates_without_lossy_replacement() {
    let message = pilotdeck_protocol::JsString::from_utf16(vec![0x41, 0xd800, 0x42]);
    let failure = invalid_response_wire(message.clone());
    assert_eq!(failure.params["detail_encoding"], "json_string");
    assert_eq!(
        failure.params["message_utf16"],
        serde_json::json!(message.units())
    );
    let displayed =
        pilotdeck_protocol::parse_json(failure.params["detail"].as_str().unwrap()).unwrap();
    assert_eq!(displayed.as_string(), Some(&message));
}

#[test]
fn invalid_response_message_cannot_replace_the_public_failure_envelope() {
    let message = r#"{"code":"OUTCOME_UNKNOWN","message_key":"injected","params":{}}"#;
    let failure = invalid_response_wire(message.into());
    assert_eq!(failure.code, "PILOTDECK_PORT_FAILURE");
    assert_eq!(failure.message_key, "diagnostic.external");
    assert_eq!(failure.params["detail"], message);
    assert_eq!(failure.params["pilotdeck_error_kind"], "invalid_response");
}

pub(super) struct ClientStub {
    calls: AtomicUsize,
    failure: Option<&'static str>,
}

fn unavailable<T>() -> Result<T> {
    Err(Failure::external("TEST_UNAVAILABLE", "unused test method"))
}

impl Client for ClientStub {
    fn resource(&self, _method: &str, _payload: Value) -> BoxFuture<'_, Result<Value>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match self.failure {
                Some("pending") => std::future::pending().await,
                Some(code) => Err(Failure::external(code, "controlled failure")),
                None => Ok(Value::Null),
            }
        })
    }
    fn publish(
        &self,
        _: &str,
        _: &str,
        _: Value,
        _: State,
        _: Option<Failure>,
        _: Option<Wait>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn invoke(&self, _: Invocation) -> BoxFuture<'_, Result<String>> {
        Box::pin(async { unavailable() })
    }
    fn find(&self, _: &str, _: &str) -> BoxFuture<'_, Result<Option<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn children(&self, _: &str) -> BoxFuture<'_, Result<Vec<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn get(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn wait(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn control(&self, _: &str, _: ControlKind, _: String) -> BoxFuture<'_, Result<ControlOutcome>> {
        Box::pin(async { unavailable() })
    }
    fn respond(&self, _: &str, _: &str, _: Vec<Message>, _: &str) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn read_state(&self, _: &str) -> BoxFuture<'_, Result<VersionedState>> {
        Box::pin(async { unavailable() })
    }
    fn write_state(&self, _: StateChange) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { unavailable() })
    }
    fn events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
    fn wait_events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
}

pub(super) fn client(failure: Option<&'static str>) -> Arc<ClientStub> {
    Arc::new(ClientStub {
        calls: AtomicUsize::new(0),
        failure,
    })
}

pub(super) fn context() -> CallContext {
    CallContext {
        call_ref: "run-1".into(),
        session_ref: "session-1".into(),
        binding: Binding {
            id: "pilotdeck".into(),
            revision: "test".into(),
            kind: Kind::Loop,
            effect: Effect::State,
            permissions: Permissions::calls([]),
        },
        invocation: Invocation::new(
            "pilotdeck",
            "context-1",
            "invoke-1",
            "correlation-1",
            Value::Null,
        ),
        state: VersionedState::default(),
        attempt: 1,
        origin: InputOrigin::default(),
        interaction: InputOrigin {
            turn: 7,
            generation: 3,
            ..InputOrigin::default()
        },
        security: SecurityRefs::default(),
    }
}

pub(super) fn input() -> TurnInput {
    TurnInput::new(
        vec![],
        agentcpm_iface::types::ExecutionIdentity::new("run-1", "step-1")
            .with_idempotency_key("step-1"),
    )
}

struct CleanupAdmission;
struct CleanupCodec {
    live: bool,
}
impl ResourceCodec for CleanupCodec {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        (module, operation) == ("model", "close_stream")
    }
    fn begin(&mut self, _: &HostCall) -> PortResult<ResourceStep> {
        Ok(ResourceStep::Request(ResourceRequest {
            method: "model.session_abort",
            payload: serde_json::json!({"model_session_id":"owned"}),
        }))
    }
    fn resume(&mut self, _: &HostCall, _: Value) -> PortResult<ResourceStep> {
        self.live = false;
        Ok(ResourceStep::Reply(Box::new(HostReply::ModelClosed)))
    }
    fn cleanup_calls(&self) -> Vec<HostCall> {
        if self.live {
            vec![HostCall::ModelClose {
                handle: PreparedRequestHandle("owned".into()),
            }]
        } else {
            vec![]
        }
    }
    fn resource_failed(&mut self, _: &HostCall, _: &PortError) {}
}
impl AdmittedTurnFactory for CleanupAdmission {
    fn resource_codec(&self, _: &CallContext, _: &TurnInput) -> PortResult<Box<dyn ResourceCodec>> {
        Ok(Box::new(CleanupCodec { live: true }))
    }
    fn prepare(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: &mut dyn HostPort,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<AdmittedTurn> {
        Err(PortError::InvalidResponse {
            message: "controlled preparation failure after acquiring a resource".into(),
        })
    }
}
#[tokio::test]
async fn native_settles_resources_even_when_preparation_fails() {
    for failure in [None, Some("OUTCOME_UNKNOWN")] {
        let client = client(failure);
        let factory = NativeFactory::new(Arc::new(CleanupAdmission));
        let loop_ = factory.create(context(), client.clone()).await.unwrap();
        let outcome = loop_.step(&"context-1".into(), 7, input()).await.unwrap();
        if failure.is_none() {
            assert!(matches!(outcome, StepOutcome::Failed { .. }));
        } else {
            assert!(matches!(outcome, StepOutcome::Unknown { reconcile: true }));
        }
        assert_eq!(
            client.calls.load(Ordering::SeqCst),
            1,
            "cleanup must be attempted exactly once"
        );
    }
}

struct RejectAdmission {
    calls: AtomicUsize,
    codecs: AtomicUsize,
    delay: Duration,
}

impl AdmittedTurnFactory for RejectAdmission {
    fn resource_codec(&self, _: &CallContext, _: &TurnInput) -> PortResult<Box<dyn ResourceCodec>> {
        self.codecs.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CloseCodec))
    }
    fn prepare(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: &mut dyn HostPort,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<AdmittedTurn> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Err(PortError::NotAdvertised {
            module: "model",
            operation: "prepare",
        })
    }
}

#[tokio::test]
async fn repeated_step_reuses_result_and_rejects_changed_identity() {
    let admission = Arc::new(RejectAdmission {
        calls: AtomicUsize::new(0),
        codecs: AtomicUsize::new(0),
        delay: Duration::ZERO,
    });
    let factory = NativeFactory::new(admission.clone());
    let loop_ = factory.create(context(), client(None)).await.unwrap();
    let name = "context-1".to_owned();
    let first = loop_.step(&name, 7, input()).await.unwrap();
    assert!(matches!(first, StepOutcome::Failed { .. }));
    assert_eq!(loop_.step(&name, 7, input()).await.unwrap(), first);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    let mut changed = input();
    changed.execution.idempotency_key = Some("different".into());
    assert!(loop_.step(&name, 7, changed).await.is_err());
    assert!(loop_.step(&name, 1, input()).await.is_err());
    assert!(
        loop_
            .step(&"other-context".into(), 7, input())
            .await
            .is_err()
    );
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    assert!(factory.create(context(), client(None)).await.is_err());
}

#[tokio::test]
async fn cancelling_before_admission_prevents_work() {
    let admission = Arc::new(RejectAdmission {
        calls: AtomicUsize::new(0),
        codecs: AtomicUsize::new(0),
        delay: Duration::ZERO,
    });
    let factory = NativeFactory::new(admission.clone());
    let client = client(None);
    let loop_ = factory.create(context(), client.clone()).await.unwrap();
    let name = "context-1".to_owned();
    assert_eq!(
        loop_
            .interrupt(&name, InterruptKind::Steer, "new instruction")
            .await
            .unwrap(),
        InterruptOutcome::Unsupported
    );
    assert_eq!(
        loop_
            .interrupt(&name, InterruptKind::Cancel, "cancel")
            .await
            .unwrap(),
        InterruptOutcome::Accepted
    );
    assert!(matches!(
        loop_.step(&name, 7, input()).await.unwrap(),
        StepOutcome::Finished {
            outcome: TurnOutcome {
                completed: false,
                ..
            }
        }
    ));
    assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.codecs.load(Ordering::SeqCst), 0);
    assert_eq!(client.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        loop_.status(&name).await.unwrap().state,
        RequestState::Cancelled
    );
    assert_eq!(
        loop_
            .interrupt(&name, InterruptKind::Cancel, "again")
            .await
            .unwrap(),
        InterruptOutcome::AlreadyTerminal
    );
}

#[tokio::test]
async fn dropping_step_future_does_not_repeat_an_admitted_worker() {
    let admission = Arc::new(RejectAdmission {
        calls: AtomicUsize::new(0),
        codecs: AtomicUsize::new(0),
        delay: Duration::from_millis(40),
    });
    let factory = NativeFactory::new(admission.clone());
    let loop_ = factory.create(context(), client(None)).await.unwrap();
    let copy = loop_.clone();
    let first = tokio::spawn(async move { copy.step(&"context-1".into(), 7, input()).await });
    while admission.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    first.abort();
    let outcome = loop_.step(&"context-1".into(), 7, input()).await.unwrap();
    assert!(matches!(outcome, StepOutcome::Failed { .. }));
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
}

struct CloseCodec;
impl ResourceCodec for CloseCodec {
    fn resource_failed(&mut self, _: &HostCall, _: &PortError) {}
    fn cleanup_calls(&self) -> Vec<HostCall> {
        vec![]
    }
    fn advertised(&self, module: &str, operation: &str) -> bool {
        (module, operation) == ("model", "close_stream")
    }
    fn begin(&mut self, _: &HostCall) -> PortResult<ResourceStep> {
        Ok(ResourceStep::Request(ResourceRequest {
            method: "model.session_close",
            payload: serde_json::json!({"model_session_id":"model-1","idempotency_key":"close-1"}),
        }))
    }
    fn resume(&mut self, _: &HostCall, _: Value) -> PortResult<ResourceStep> {
        Ok(ResourceStep::Reply(Box::new(HostReply::ModelClosed)))
    }
}

#[tokio::test]
async fn effectful_resource_failure_is_unknown_and_is_not_replayed() {
    for code in ["PROVIDER_CLOSED", "OUTCOME_UNKNOWN"] {
        let client = client(Some(code));
        let runtime = tokio::runtime::Handle::current();
        let observed = client.clone();
        let error = tokio::task::spawn_blocking(move || {
            let mut host = KernelHostPort::new(client, context(), runtime, Box::new(CloseCodec));
            host.call(
                &CallControl {
                    cancellation: Cancellation::default(),
                    deadline: None,
                },
                HostCall::ModelClose {
                    handle: PreparedRequestHandle("model-1".into()),
                },
            )
            .unwrap_err()
        })
        .await
        .unwrap();
        assert!(matches!(error, PortError::AcceptedUnknown { .. }));
        assert_eq!(observed.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            port_failure(error).outcome,
            StepOutcome::Unknown { reconcile: true }
        ));
    }
}

#[tokio::test]
async fn affirmative_resource_denial_remains_a_known_failure() {
    let runtime = tokio::runtime::Handle::current();
    let error = tokio::task::spawn_blocking(move || {
        let mut host = KernelHostPort::new(
            client(Some("CAPABILITY_DENIED")),
            context(),
            runtime,
            Box::new(CloseCodec),
        );
        host.call(
            &CallControl {
                cancellation: Cancellation::default(),
                deadline: None,
            },
            HostCall::ModelClose {
                handle: PreparedRequestHandle("model-1".into()),
            },
        )
        .unwrap_err()
    })
    .await
    .unwrap();
    assert!(matches!(error, PortError::HostFailure { .. }));
}

#[tokio::test]
async fn cancelling_an_unacknowledged_effect_does_not_claim_cancelled() {
    let runtime = tokio::runtime::Handle::current();
    let client = client(Some("pending"));
    let observed = client.clone();
    let cancellation = Cancellation::default();
    let control = CallControl {
        cancellation: cancellation.clone(),
        deadline: None,
    };
    let running = tokio::task::spawn_blocking(move || {
        let mut host = KernelHostPort::new(client, context(), runtime, Box::new(CloseCodec));
        host.call(
            &control,
            HostCall::ModelClose {
                handle: PreparedRequestHandle("model-1".into()),
            },
        )
        .unwrap_err()
    });
    while observed.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    cancellation.cancel(CancelCause::Requested("stop".into()));
    let error = tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(error, PortError::AcceptedUnknown { .. }));
    assert_eq!(observed.calls.load(Ordering::SeqCst), 1);
}

struct InvalidReplyCodec;
impl ResourceCodec for InvalidReplyCodec {
    fn resource_failed(&mut self, _: &HostCall, _: &PortError) {}
    fn cleanup_calls(&self) -> Vec<HostCall> {
        vec![]
    }
    fn advertised(&self, module: &str, operation: &str) -> bool {
        CloseCodec.advertised(module, operation)
    }
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep> {
        CloseCodec.begin(call)
    }
    fn resume(&mut self, _: &HostCall, _: Value) -> PortResult<ResourceStep> {
        Err(PortError::InvalidResponse {
            message: "malformed close receipt".into(),
        })
    }
}

#[tokio::test]
async fn malformed_effect_receipt_requires_reconciliation() {
    let runtime = tokio::runtime::Handle::current();
    let error = tokio::task::spawn_blocking(move || {
        let mut host = KernelHostPort::new(
            client(None),
            context(),
            runtime,
            Box::new(InvalidReplyCodec),
        );
        host.call(
            &CallControl {
                cancellation: Cancellation::default(),
                deadline: None,
            },
            HostCall::ModelClose {
                handle: PreparedRequestHandle("model-1".into()),
            },
        )
        .unwrap_err()
    })
    .await
    .unwrap();
    assert!(matches!(error, PortError::AcceptedUnknown { .. }));
}

#[tokio::test]
async fn unavailable_operation_is_rejected_before_kernel_call() {
    let runtime = tokio::runtime::Handle::current();
    let client = client(None);
    let observed = client.clone();
    let error = tokio::task::spawn_blocking(move || {
        let mut host = KernelHostPort::new(client, context(), runtime, Box::new(CloseCodec));
        host.call(
            &CallControl {
                cancellation: Cancellation::default(),
                deadline: None,
            },
            HostCall::ListTools,
        )
        .unwrap_err()
    })
    .await
    .unwrap();
    assert!(matches!(
        error,
        PortError::NotAdvertised {
            module: "capability",
            operation: "list_tools"
        }
    ));
    assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
}

struct DemandScript {
    calls: std::sync::Mutex<Vec<(String, Value)>>,
    observation: std::sync::Mutex<Option<Result<Value>>>,
    pending: bool,
}
impl Client for DemandScript {
    fn resource(&self, method: &str, payload: Value) -> BoxFuture<'_, Result<Value>> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_owned(), payload));
        if method == "model.events_read" && self.pending {
            return Box::pin(std::future::pending());
        }
        let result = if method == "model.delivery_advance" {
            Err(
                Failure::new("KERNEL_REJECTED", "scheduler.resource.rejected")
                    .with("rpc_error", serde_json::json!({"reason":"BAD_STATE"})),
            )
        } else if method == "model.events_read" {
            self.observation
                .lock()
                .unwrap()
                .take()
                .expect("one observation only")
        } else {
            panic!("unexpected resource {method}")
        };
        Box::pin(async move { result })
    }
    fn publish(
        &self,
        _: &str,
        _: &str,
        _: Value,
        _: State,
        _: Option<Failure>,
        _: Option<Wait>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn invoke(&self, _: Invocation) -> BoxFuture<'_, Result<String>> {
        Box::pin(async { unavailable() })
    }
    fn find(&self, _: &str, _: &str) -> BoxFuture<'_, Result<Option<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn children(&self, _: &str) -> BoxFuture<'_, Result<Vec<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn get(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn wait(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn control(&self, _: &str, _: ControlKind, _: String) -> BoxFuture<'_, Result<ControlOutcome>> {
        Box::pin(async { unavailable() })
    }
    fn respond(&self, _: &str, _: &str, _: Vec<Message>, _: &str) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn read_state(&self, _: &str) -> BoxFuture<'_, Result<VersionedState>> {
        Box::pin(async { unavailable() })
    }
    fn write_state(&self, _: StateChange) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { unavailable() })
    }
    fn events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
    fn wait_events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
}

#[tokio::test]
async fn demand_failed_next_reconciles_once_without_reissuing_credit() {
    use crate::model_session_tests::{demand_after_first_yield, event, page, pull_call};
    for case in [
        "terminal", "local", "denied", "gap", "foreign", "empty", "pending",
    ] {
        let observation = match case {
            "denied" => Err(Failure::new("AUTHORITY_REVOKED", "test.denied")),
            "pending" | "empty" => Ok(page(vec![], "c1", false, false)),
            "local" => {
                let mut failed = event(3, "response_failed");
                failed["code"] = serde_json::json!(500);
                failed["message"] = serde_json::json!("failed");
                let mut released = event(4, "local_resources_released");
                released["input_id"] = serde_json::json!("pd-input-1");
                released["response_id"] = serde_json::json!("response-1");
                released["job_id"] = serde_json::json!("job-1");
                released["config_revision"] = serde_json::json!(0);
                Ok(page(vec![failed, released], "c4", false, false))
            }
            _ => {
                let mut failed = event(if case == "gap" { 3 } else { 2 }, "response_failed");
                if case == "foreign" {
                    failed["response_id"] = serde_json::json!("other");
                }
                failed["code"] = serde_json::json!(500);
                failed["message"] = serde_json::json!("failed");
                Ok(page(vec![failed], "c2", false, false))
            }
        };
        let client = Arc::new(DemandScript {
            calls: std::sync::Mutex::new(Vec::new()),
            observation: std::sync::Mutex::new(Some(observation)),
            pending: case == "pending",
        });
        let observed = client.clone();
        let runtime = tokio::runtime::Handle::current();
        let (error, cause, cleanup) = tokio::task::spawn_blocking(move || {
            let mut host = KernelHostPort::new(
                client,
                context(),
                runtime,
                Box::new(demand_after_first_yield(case == "local")),
            );
            let error = host
                .call(
                    &CallControl {
                        cancellation: Cancellation::default(),
                        deadline: None,
                    },
                    pull_call(),
                )
                .unwrap_err();
            (
                error,
                host.diagnostic_for_test(),
                host.cleanup_count_for_test(),
            )
        })
        .await
        .unwrap();
        let PortError::AcceptedUnknown {
            cause: Some(returned),
            ..
        } = error
        else {
            panic!("{case}: expected uncertain outcome with cause");
        };
        assert_eq!(returned.code, "KERNEL_REJECTED");
        assert_eq!(returned.params["rpc_error"]["reason"], "BAD_STATE");
        let cause = cause.expect("original structured failure retained");
        assert_eq!(cause.code, "KERNEL_REJECTED");
        assert_eq!(cause.params["rpc_error"]["reason"], "BAD_STATE");
        assert_eq!(cleanup, 1);
        let calls = observed.calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            vec!["model.delivery_advance", "model.events_read"]
        );
        assert_eq!(
            calls[0].1["previous_event_seq"],
            if case == "local" { "2" } else { "1" }
        );
        assert_eq!(
            calls[1].1["cursor"],
            if case == "local" { "c2" } else { "c1" }
        );
    }
}

#[tokio::test]
async fn unknown_diagnostic_is_bound_to_original_call_and_never_changes_outcome() {
    let factory = NativeFactory::new(Arc::new(CleanupAdmission));
    let ctx = context();
    let runtime = factory.create(ctx.clone(), client(None)).await.unwrap();
    let cause = Failure::new("KERNEL_REJECTED", "scheduler.resource.rejected").with(
        "rpc_error",
        serde_json::json!({"reason":"BAD_STATE","state":"finished","reconcile":false,"identity":"forged"}),
    );
    let error = PortError::AcceptedUnknown {
        identity: Box::new(ExecutionIdentity {
            session_id: ctx.session_ref.clone().into(),
            turn_id: ctx.interaction.turn.to_string().into(),
            run_id: ctx.call_ref.clone().into(),
            operation_id: ctx.invocation.correlation_id.clone().into(),
            request_id: ctx.call_ref.clone().into(),
            idempotency_key: None,
            module_instance_id: ctx.binding.id.clone().into(),
            connection_generation: ctx.interaction.generation.to_string().into(),
        }),
        last_applied_sequence: None,
        cause: Some(Box::new(pilotdeck_protocol::model::HostDiagnosticCause {
            code: cause.code.clone(),
            message_key: cause.message_key.clone(),
            params: cause.params.clone(),
        })),
    };
    let completion = port_failure(error);
    assert!(matches!(
        completion.outcome,
        StepOutcome::Unknown { reconcile: true }
    ));
    assert_eq!(completion.cause.as_ref(), Some(&cause));
    let shared = factory
        .calls
        .lock()
        .unwrap()
        .get(&(ctx.session_ref.clone(), ctx.call_ref.clone()))
        .unwrap()
        .upgrade()
        .unwrap();
    shared.completion.send_replace(Some(completion));
    let outcome = StepOutcome::Unknown { reconcile: true };
    assert_eq!(
        factory.diagnostic_cause(&ctx, runtime.as_ref(), &outcome),
        Some(cause)
    );
    let mut other_context = ctx.clone();
    other_context.call_ref = "another-call".into();
    let other_runtime = factory.create(other_context, client(None)).await.unwrap();
    assert!(
        factory
            .diagnostic_cause(&ctx, other_runtime.as_ref(), &outcome)
            .is_none()
    );
    let mut foreign = ctx.clone();
    foreign.session_ref = "other-session".into();
    let foreign_runtime = factory.create(foreign.clone(), client(None)).await.unwrap();
    assert!(
        factory
            .diagnostic_cause(&foreign, foreign_runtime.as_ref(), &outcome)
            .is_none()
    );
    assert!(
        factory
            .diagnostic_cause(&ctx, runtime.as_ref(), &outcome)
            .is_some()
    );
    assert!(
        factory
            .diagnostic_cause(&foreign, runtime.as_ref(), &outcome)
            .is_none()
    );
    let mut foreign = ctx.clone();
    foreign.interaction.generation += 1;
    assert!(
        factory
            .diagnostic_cause(&foreign, runtime.as_ref(), &outcome)
            .is_none()
    );
}
