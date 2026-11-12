//! Owned-turn controls reuse the existing submit host and real FileRegistry.
//! Gates record causal boundaries; no timing sleep grants ownership.
use super::*;
use crate::channel::{
    protocol::{ChannelKey, TurnGateway, TurnRequest},
    turn_submitter::SubmitterTurnGateway,
};
use tokio::sync::Notify;

struct Admission {
    base: Host,
    interactions: Arc<crate::runtime::interaction::GatewayInteractionCoordinator>,
    phase: &'static str,
    entered: Arc<Notify>,
    inputs: Arc<Mutex<Vec<JsValue>>>,
}
impl SubmitAdmission for Admission {
    fn normalize<'a>(
        &'a self,
        input: &'a mut JsValue,
    ) -> SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        self.inputs.lock().unwrap().push(input.clone());
        if self.phase == "record" {
            self.entered.notify_one();
        }
        if self.phase == "normalize" {
            Box::pin(async move {
                self.entered.notify_one();
                std::future::pending().await
            })
        } else {
            self.base.normalize(input)
        }
    }
    fn preflight<'a>(
        &'a self,
        input: &'a JsValue,
        run: &'a RunRef,
    ) -> SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        if self.phase == "preflight" {
            Box::pin(async move {
                self.entered.notify_one();
                std::future::pending().await
            })
        } else {
            self.base.preflight(input, run)
        }
    }
    fn route_input<'a>(
        &'a self,
        input: &'a mut JsValue,
    ) -> SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        self.base.route_input(input)
    }
    fn admitted_preflight<'a>(
        &'a self,
        input: &'a JsValue,
        run: &'a RunRef,
        completion: &'a TurnCompletionHandle,
    ) -> SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        self.base.admitted_preflight(input, run, completion)
    }
    fn observe_event<'a>(
        &'a self,
        input: &'a JsValue,
        event: &'a JsValue,
    ) -> SubmitFuture<'a, Result<(), SubmitError>> {
        self.base.observe_event(input, event)
    }
    fn has_sdk_config(&self) -> bool {
        self.base.has_sdk_config()
    }
    fn validate_sdk_config(&self, value: &JsValue) -> Result<(), SubmitError> {
        self.base.validate_sdk_config(value)
    }
    fn set_sdk_config<'a>(
        &'a self,
        input: &'a JsValue,
        completion: &'a TurnCompletionHandle,
    ) -> SubmitFuture<'a, Result<bool, SubmitError>> {
        self.base.set_sdk_config(input, completion)
    }
    fn reject_pending(&self, session: &JsString, reason: &str) -> Result<(), SubmitError> {
        self.interactions
            .reject_pending_turn(session, &reason.into());
        self.base.reject_pending(session, reason)
    }
}
struct RegistryGate {
    inner: Arc<Registry>,
    entered: Arc<Notify>,
    hold: Option<Gate>,
}
impl TurnRunRegistry for RegistryGate {
    fn accept(&self, input: &RunAcceptInput) -> RegistryFuture<bool> {
        let accepted = self.inner.accept(input);
        let entered = self.entered.clone();
        let hold = self.hold.clone();
        Box::pin(async move {
            if let Some(hold) = hold {
                entered.notify_one();
                hold.wait().await;
            }
            accepted.await
        })
    }
    fn events(&self, run: &RunRef) -> RegistryFuture<Vec<JsValue>> {
        self.inner.events(run)
    }
    fn append(&self, run: &RunRef, event: &JsValue) -> RegistryFuture<()> {
        self.inner.append(run, event)
    }
}
struct TailSession {
    session: Arc<Session>,
    entered: Arc<Notify>,
    gate: Gate,
    hold: bool,
}
struct TailStream {
    inner: Box<dyn LiveTurn>,
    entered: Arc<Notify>,
    gate: Gate,
    hold: bool,
}
impl LiveTurn for TailStream {
    fn next(&mut self) -> SessionNext<'_> {
        Box::pin(async move {
            let value = self.inner.next().await?;
            if value.is_none() && self.hold {
                self.entered.notify_one();
                self.gate.wait().await;
            }
            Ok(value)
        })
    }
}
impl LiveSession for TailSession {
    fn snapshot(&self) -> JsValue {
        self.session.snapshot()
    }
    fn abort(&self, reason: &JsString) -> RuntimeResult {
        self.session.abort(reason)
    }
    fn dispose(&self) -> DisposeFuture<'_> {
        self.session.dispose()
    }
    fn submit(&self, input: JsValue, options: JsValue) -> Result<Box<dyn LiveTurn>, RuntimeError> {
        Ok(Box::new(TailStream {
            inner: self.session.submit(input, options)?,
            entered: self.entered.clone(),
            gate: self.gate.clone(),
            hold: self.hold,
        }))
    }
}
struct TailFactory(Arc<TailSession>);
impl SessionFactory for TailFactory {
    fn create(
        &self,
        _: SessionContext,
        _: Option<LiveSessionHandle>,
    ) -> BoxFuture<Result<LiveSessionHandle, RuntimeError>> {
        let session = self.0.clone();
        Box::pin(async move { Ok(LiveSessionHandle::new(session, None)) })
    }
    fn can_recreate(&self) -> bool {
        false
    }
}
struct Fixture {
    submitter: Arc<TurnSubmitter>,
    interactions: Arc<crate::runtime::interaction::GatewayInteractionCoordinator>,
    session: Arc<Session>,
    log: Arc<Log>,
    entered: Arc<Notify>,
    inputs: Arc<Mutex<Vec<JsValue>>>,
    accept_gate: Gate,
    tail_gate: Gate,
    tail_entered: Arc<Notify>,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new(phase: &'static str, setup: Value) -> Self {
        let root = std::env::temp_dir().join(format!("h1-owned-submit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let log = Arc::new(Log::default());
        let coordinator = Arc::new(TurnEventCoordinator::new());
        let session = Arc::new(Session {
            setup: setup.clone(),
            status_recorder: None,
            native: vec![
                js(&json!({"type":"turn_started","sessionId":"s","turnId":"r"})),
                js(
                    &json!({"type":"turn_completed","result":{"type":"success","stopReason":"completed","usage":{}}}),
                ),
            ],
            submits: AtomicUsize::new(0),
            log: log.clone(),
            coordinator: coordinator.clone(),
            accepted: Gate::new(),
            terminal: Gate::new(),
            end: Gate::new(),
            aborted: AtomicBool::new(false),
            failure_gate: Gate::new(),
            pump_error_gate: Gate::new(),
        });
        let tail_gate = Gate::new();
        let tail_entered = Arc::new(Notify::new());
        let factory = Arc::new(TailFactory(Arc::new(TailSession {
            session: session.clone(),
            entered: tail_entered.clone(),
            gate: tail_gate.clone(),
            hold: setup["ownedTailGate"] == true,
        })));
        let mut options = SessionOwnerOptions::new(factory);
        options.idle_sweep_interval_ms = 0;
        let owner = SessionOwner::new(options);
        let entered = Arc::new(Notify::new());
        let interactions =
            Arc::new(crate::runtime::interaction::GatewayInteractionCoordinator::default());
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let admission = Arc::new(Admission {
            interactions: interactions.clone(),
            base: Host {
                session: session.clone(),
                sdk: Gate::new(),
                recovery: Gate::new(),
            },
            phase,
            entered: entered.clone(),
            inputs: inputs.clone(),
        });
        let services = Arc::new(Services {
            session: session.clone(),
            refreshed: AtomicBool::new(false),
            prepare: Gate::new(),
        });
        let host = Arc::new(GatewaySubmitHost::new(
            services,
            Arc::new(FakeReplacement {
                session: session.clone(),
            }),
            admission,
        ));
        let accept_gate = Gate::new();
        let registry = Arc::new(RegistryGate {
            inner: Arc::new(Registry {
                port: FileRegistry::new(FileRunRegistry::open(root.join("registry.json")).unwrap())
                    .unwrap(),
                log: log.clone(),
                setup,
                pending: Arc::default(),
                append_gate: Gate::new(),
            }),
            entered: entered.clone(),
            hold: (phase == "accept").then(|| accept_gate.clone()),
        });
        let submitter = Arc::new(TurnSubmitter {
            owner,
            coordinator,
            registry: Some(registry),
            host,
            workspace_admissions: WorkspaceAdmissions::default(),
            projector: Arc::new(
                GatewayAgentEventProjector::new(GatewayAgentEventProjectorOptions::default())
                    .unwrap(),
            ),
        });
        Self {
            submitter,
            interactions,
            session,
            log,
            entered,
            inputs,
            accept_gate,
            tail_gate,
            tail_entered,
            root,
        }
    }
    fn input() -> JsValue {
        js(
            &json!({"sessionKey":"s","projectKey":"p","runId":"r","message":"hello","channelKey":"signal"}),
        )
    }
    async fn clean(self) {
        self.submitter.owner.close(&"s".into()).await.unwrap();
        drop(self.submitter);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("owned lifecycle failed to settle")
}

#[tokio::test]
async fn h1_owned_cancel_interrupts_normalize_and_preflight_without_model() {
    for phase in ["normalize", "preflight"] {
        let fixture = Fixture::new(phase, json!({}));
        let mut submission = fixture.submitter.submit_owned(Fixture::input());
        bounded(fixture.entered.notified()).await;
        bounded(submission.cancel_and_drain("channel stopped".into()))
            .await
            .unwrap();
        assert_eq!(fixture.session.submits.load(Ordering::SeqCst), 0);
        assert!(!fixture.submitter.owner.has_active_turn(&"s".into()));
        if phase == "preflight" {
            let events = fixture
                .submitter
                .registry
                .as_ref()
                .unwrap()
                .events(&RunRef {
                    session_key: Some("s".into()),
                    project_key: Some("p".into()),
                    run_id: "r".into(),
                })
                .await
                .unwrap();
            assert!(
                events
                    .iter()
                    .any(|event| is(event, "finishReason", "aborted_streaming"))
            );
        }
        fixture.clean().await;
    }
}

#[tokio::test]
async fn h1_owned_cancel_waits_for_accept_then_persists_terminal_without_admission() {
    let fixture = Fixture::new("accept", json!({}));
    let mut submission = fixture.submitter.submit_owned(Fixture::input());
    bounded(fixture.entered.notified()).await;
    let mut cancel = Box::pin(submission.cancel_and_drain("stop".into()));
    std::future::poll_fn(|cx| {
        assert!(cancel.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(fixture.session.submits.load(Ordering::SeqCst), 0);
    assert!(!fixture.submitter.owner.has_active_turn(&"s".into()));
    fixture.accept_gate.open();
    bounded(cancel).await.unwrap();
    let events = fixture
        .submitter
        .registry
        .as_ref()
        .unwrap()
        .events(&RunRef {
            session_key: Some("s".into()),
            project_key: Some("p".into()),
            run_id: "r".into(),
        })
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| is(event, "finishReason", "aborted_streaming"))
    );
    assert_eq!(fixture.session.submits.load(Ordering::SeqCst), 0);
    fixture.clean().await;
}

#[tokio::test]
async fn h1_owned_cancel_persistence_failure_is_not_success() {
    let fixture = Fixture::new("preflight", json!({"registryAppendFailure":true}));
    let mut submission = fixture.submitter.submit_owned(Fixture::input());
    bounded(fixture.entered.notified()).await;
    assert!(
        bounded(submission.cancel_and_drain("stop".into()))
            .await
            .is_err()
    );
    assert!(
        bounded(submission.cancel_and_drain("retry".into()))
            .await
            .is_err()
    );
    assert_eq!(fixture.session.submits.load(Ordering::SeqCst), 0);
    fixture.clean().await;
}

#[tokio::test]
async fn h1_owned_cancel_drains_live_activity_and_does_not_abort_successor() {
    let fixture = Fixture::new("live", json!({}));
    let mut submission = fixture.submitter.submit_owned(Fixture::input());
    // The first native event proves the live activity is owned; its next
    // input/terminal remains behind the original host's gates.
    loop {
        if let Some(event) = bounded(submission.next()).await.unwrap() {
            if is(&event, "type", "turn_started") {
                break;
            }
        } else {
            panic!("missing live turn");
        }
    }
    bounded(submission.cancel_and_drain("stop".into()))
        .await
        .unwrap();
    assert_eq!(host_count(&fixture.log, "session_abort"), 1);
    let new_reservation = fixture
        .submitter
        .owner
        .begin_turn(&"s".into(), &"r".into())
        .unwrap();
    let successor = fixture
        .submitter
        .owner
        .begin_completion(&"s".into(), &"r".into());
    bounded(submission.cancel_and_drain("late".into()))
        .await
        .unwrap();
    assert!(!successor.is_cancelled());
    assert_eq!(host_count(&fixture.log, "session_abort"), 1);
    fixture
        .submitter
        .owner
        .end_turn(&"s".into(), Some(&"r".into()), Some(&new_reservation));
    fixture
        .submitter
        .owner
        .complete_turn(&"s".into(), &successor);
    fixture.clean().await;
}

#[tokio::test]
async fn h1_unpolled_channel_submit_never_starts_a_turn() {
    let fixture = Fixture::new("live", json!({}));
    let gateway = SubmitterTurnGateway::new_owned(fixture.submitter.clone());
    drop(gateway.submit(TurnRequest {
        session_key: "s".into(),
        channel_key: ChannelKey::new("signal"),
        message: "hello".into(),
        ..Default::default()
    }));
    tokio::task::yield_now().await;
    assert!(fixture.inputs.lock().unwrap().is_empty());
    assert!(fixture.log.registry.lock().unwrap().is_empty());
    assert_eq!(fixture.session.submits.load(Ordering::SeqCst), 0);
    fixture.clean().await;
}

#[tokio::test]
async fn channel_bridge_preserves_optional_fields_for_both_submission_modes() {
    for owned in [false, true] {
        for case in 0..3 {
            let present = case != 0;
            let attachments = if case == 2 {
                vec![js(&json!({"type":"image", "path":"/fixture.png"}))]
            } else {
                vec![]
            };
            let fixture = Fixture::new("record", json!({}));
            let gateway = if owned {
                SubmitterTurnGateway::new_owned(fixture.submitter.clone())
            } else {
                SubmitterTurnGateway::new(fixture.submitter.clone())
            };
            let request = TurnRequest {
                session_key: "s".into(),
                channel_key: ChannelKey::new("wecom"),
                message: "hello".into(),
                attachments: present.then(|| attachments.clone()),
                allow_plan_mode_tools: present.then_some(false),
                synthetic_messages: present
                    .then(|| vec![js(&json!({"text":"hint", "purpose":"fixture"}))]),
                project_key: present.then(|| "project-fixture".into()),
            };
            let mut stream = bounded(gateway.submit(request)).await.unwrap();
            bounded(fixture.entered.notified()).await;
            let input = fixture.inputs.lock().unwrap()[0].clone();
            assert_eq!(field(&input, "sessionKey"), &JsValue::from("s"));
            assert_eq!(field(&input, "channelKey"), &JsValue::from("wecom"));
            assert_eq!(field(&input, "message"), &JsValue::from("hello"));
            let fields = input.as_object().unwrap();
            if present {
                assert_eq!(
                    fields.get("attachments"),
                    Some(&JsValue::Array(attachments))
                );
                assert_eq!(
                    fields.get("allowPlanModeTools"),
                    Some(&JsValue::Bool(false))
                );
                assert_eq!(
                    fields.get("syntheticMessages"),
                    Some(&JsValue::Array(vec![js(
                        &json!({"text":"hint", "purpose":"fixture"})
                    )]))
                );
                assert_eq!(
                    fields.get("projectKey"),
                    Some(&JsValue::from("project-fixture"))
                );
            } else {
                for key in [
                    "attachments",
                    "allowPlanModeTools",
                    "syntheticMessages",
                    "projectKey",
                ] {
                    assert!(fields.get(key).is_none(), "unexpected {key}");
                }
            }
            // Exhaust the real submission even if a later validation rejects
            // the synthetic fixture; the bridge assertion is at admission.
            while matches!(bounded(stream.next()).await, Ok(Some(_))) {}
            drop(stream);
            fixture.clean().await;
        }
    }
}

#[tokio::test]
async fn legacy_channel_submit_starts_before_its_future_is_polled() {
    let fixture = Fixture::new("record", json!({}));
    let gateway = SubmitterTurnGateway::new(fixture.submitter.clone());
    let future = gateway.submit(TurnRequest {
        session_key: "s".into(),
        channel_key: ChannelKey::new("signal"),
        message: "hello".into(),
        ..Default::default()
    });
    bounded(fixture.entered.notified()).await;
    assert_eq!(fixture.inputs.lock().unwrap().len(), 1);
    let mut stream = bounded(future).await.unwrap();
    while matches!(bounded(stream.next()).await, Ok(Some(_))) {}
    drop(stream);
    fixture.clean().await;
}

#[tokio::test]
async fn owned_channel_cancel_drains_the_real_admission() {
    let fixture = Fixture::new("normalize", json!({}));
    let gateway = SubmitterTurnGateway::new_owned(fixture.submitter.clone());
    let mut stream = bounded(gateway.submit(TurnRequest {
        session_key: "s".into(),
        channel_key: ChannelKey::new("signal"),
        message: "hello".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    bounded(fixture.entered.notified()).await;
    bounded(stream.cancel_and_drain("channel stop".into()))
        .await
        .unwrap();
    assert_eq!(fixture.session.submits.load(Ordering::SeqCst), 0);
    assert!(!fixture.submitter.owner.has_active_turn(&"s".into()));
    drop(stream);
    fixture.clean().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h1_owned_cancel_after_external_close_leaves_new_same_id_and_pending_untouched() {
    let fixture = Fixture::new("live", json!({}));
    let mut submission = fixture.submitter.submit_owned(Fixture::input());
    loop {
        let event = bounded(submission.next())
            .await
            .unwrap()
            .expect("live turn");
        if is(&event, "type", "turn_started") {
            break;
        }
    }
    // External close completes the old handle independently of the consumer
    // receipt, permitting reuse before the old channel resumes its drain.
    bounded(fixture.submitter.owner.close(&"s".into()))
        .await
        .unwrap();
    let reservation = fixture
        .submitter
        .owner
        .begin_turn(&"s".into(), &"r".into())
        .unwrap();
    let successor = fixture
        .submitter
        .owner
        .begin_completion(&"s".into(), &"r".into());
    let pending = fixture
        .interactions
        .permission_bus()
        .register(
            &"s".into(),
            crate::runtime::interaction::PendingEntry {
                request_id: "new-request".into(),
                tool_call_id: "new-call".into(),
                tool_name: "new-tool".into(),
                resolve: Arc::new(|_| panic!("old cancellation resolved successor permission")),
                reject: Arc::new(|_| panic!("old cancellation rejected successor permission")),
            },
            None,
        )
        .unwrap();
    bounded(submission.cancel_and_drain("old channel stopped".into()))
        .await
        .unwrap();
    assert!(!successor.is_cancelled());
    assert!(
        fixture
            .submitter
            .owner
            .is_current_completion(&"s".into(), &successor)
    );
    assert!(pending.active());
    assert_eq!(
        fixture
            .interactions
            .permission_bus()
            .pending_count(&"s".into()),
        1
    );
    pending.dispose();
    fixture
        .submitter
        .owner
        .end_turn(&"s".into(), Some(&"r".into()), Some(&reservation));
    fixture
        .submitter
        .owner
        .complete_turn(&"s".into(), &successor);
    fixture.clean().await;
}

#[tokio::test]
async fn h1_cancel_before_admission_does_not_abort_other_active_run() {
    let fixture = Fixture::new("preflight", json!({}));
    let reservation = fixture
        .submitter
        .owner
        .begin_turn(&"s".into(), &"other".into())
        .unwrap();
    let other = fixture
        .submitter
        .owner
        .begin_completion(&"s".into(), &"other".into());
    let mut submission = fixture.submitter.submit_owned(Fixture::input());
    bounded(fixture.entered.notified()).await;
    bounded(submission.cancel_and_drain("stop".into()))
        .await
        .unwrap();
    assert!(!other.is_cancelled());
    assert!(fixture.submitter.owner.has_active_turn(&"s".into()));
    assert_eq!(host_count(&fixture.log, "session_abort"), 0);
    fixture
        .submitter
        .owner
        .end_turn(&"s".into(), Some(&"other".into()), Some(&reservation));
    fixture.submitter.owner.complete_turn(&"s".into(), &other);
    fixture.clean().await;
}

#[tokio::test]
async fn h1_owned_timeout_does_not_report_drained_before_worker_tail() {
    let fixture = Fixture::new("live", json!({"ownedTailGate":true}));
    let mut input = Fixture::input();
    set(&mut input, "timeoutMs", JsValue::Number(10.0));
    let mut submission = fixture.submitter.submit_owned(input);
    loop {
        let event = bounded(submission.next())
            .await
            .unwrap()
            .expect("live turn");
        if is(&event, "type", "turn_started") {
            break;
        }
    }
    // The timeout abort opens the native gates; this receipt proves its
    // actual EOF/lifecycle tail was polled and remains held by this stream.
    bounded(fixture.tail_entered.notified()).await;
    let mut cancel = Box::pin(submission.cancel_and_drain("stop after timeout".into()));
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut cancel)
            .await
            .is_err(),
        "owned timeout reported success while its proven live tail was still blocked"
    );
    assert!(fixture.submitter.owner.has_active_turn(&"s".into()));
    fixture.tail_gate.open();
    bounded(cancel).await.unwrap();
    assert!(!fixture.submitter.owner.has_active_turn(&"s".into()));
    fixture.clean().await;
}

#[tokio::test]
async fn h1_owned_timeout_persistence_failure_survives_worker_drain() {
    let fixture = Fixture::new("live", json!({"statusRecordFailure":true}));
    let mut input = Fixture::input();
    set(&mut input, "timeoutMs", JsValue::Number(10.0));
    let mut submission = fixture.submitter.submit_owned(input);
    loop {
        match bounded(submission.next()).await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("owned persistence failure was reported as success"),
            Err(_) => break,
        }
    }
    assert!(
        submission
            .cancel_and_drain("after timeout".into())
            .await
            .is_err()
    );
    fixture.clean().await;
}
