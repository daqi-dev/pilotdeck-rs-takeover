use super::*;
use agentcpm_scheduler_protocol::*;
use pilotdeck_agent::ports::{HostCall, HostPort, HostReply, PreparedRequestHandle};
use resources::{ResourceCodec, ResourceRequest, ResourceStep};
use std::sync::atomic::{AtomicUsize, Ordering};

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
