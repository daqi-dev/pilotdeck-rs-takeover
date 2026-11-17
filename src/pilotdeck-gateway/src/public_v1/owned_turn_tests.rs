//! D20 (2026-11-17): `public_v1` run submissions go through
//! `TurnSubmitter::submit_owned`, with the store-assigned v1 `runId` passed
//! explicitly in `input.runId`. The tests here first pin the submit
//! machinery's input/output assumptions the integration depends on (identity
//! propagation, in-order event stream to EOF, cancellation), then drive the
//! full `run.submit` -> owned turn -> SQLite projection loop end to end.
//! The scripted live session is the §7.2 test adapter driving events without
//! model keys; the `OfflineRunDriver` stays the no-submitter fallback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::metadata::object;
use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
use crate::public_v1::runtime::{OfflineRunDriver, PublicV1Runtime};
use crate::public_v1::store::{InstanceMeta, V1Store};
use crate::public_v1::test_support::TempDir;
use crate::runtime::project::{DisposeFuture, RuntimeError, RuntimeResult};
use crate::runtime::refresh::BoxFuture;
use crate::runtime::session::{
    LiveSession, LiveSessionHandle, LiveTurn, SessionContext, SessionFactory, SessionNext,
    SessionOwnerOptions,
};
use crate::runtime::turn::coordinator::TurnEventCoordinator;
use crate::runtime::turn::projector::{
    GatewayAgentEventProjector, GatewayAgentEventProjectorOptions,
};
use crate::runtime::turn::submit::{SubmitError, TurnSubmitter};
use crate::runtime::turn::submit_host::{
    GatewaySubmitHost, SubmitAdmission, SubmitAttachmentLease, SubmitHostServices,
    SubmitReplacement,
};
use crate::storage::RunRef;
use pilotdeck_protocol::{JsString, JsValue};

fn js(value: &Value) -> JsValue {
    pilotdeck_protocol::parse_json(&value.to_string()).unwrap()
}

fn js_to_value(event: &JsValue) -> Value {
    let text = pilotdeck_protocol::stringify_json(event)
        .unwrap()
        .expect("stringifiable event");
    serde_json::from_str(&text).unwrap()
}

fn js_string(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

/// Owned snapshot of what the turn machine derived from the inputs, so no
/// `MutexGuard` is ever held across an await point.
fn observed_snapshot(observed: &Observed) -> (Vec<Value>, Vec<String>) {
    (
        observed.runs.lock().unwrap().clone(),
        observed.messages.lock().unwrap().clone(),
    )
}

/// Abort latch shared between the scripted session and its open turn: abort
/// releases a held stream so the owned pump observes a normal EOF.
#[derive(Default)]
struct AbortLatch {
    fired: AtomicBool,
    notify: Notify,
}

impl AbortLatch {
    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    fn is_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

/// What the turn machine derived from a `submit_owned` input.
#[derive(Default)]
struct Observed {
    /// RunRef identity recorded at admission preflight.
    runs: Mutex<Vec<Value>>,
    /// The `message` field handed to `prepare_attachment_turn`.
    messages: Mutex<Vec<String>>,
}

struct RecordingAdmission {
    observed: Arc<Observed>,
}

impl SubmitAdmission for RecordingAdmission {
    fn normalize<'a>(
        &'a self,
        _input: &'a mut JsValue,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn route_input<'a>(
        &'a self,
        _input: &'a mut JsValue,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn preflight<'a>(
        &'a self,
        _input: &'a JsValue,
        run: &'a RunRef,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        self.observed.runs.lock().unwrap().push(json!({
            "sessionKey": js_string(run.session()),
            "projectKey": run.project_key.as_ref().map(js_string),
            "runId": js_string(&run.run_id),
        }));
        Box::pin(async { Ok(vec![]) })
    }
    fn admitted_preflight<'a>(
        &'a self,
        _input: &'a JsValue,
        _run: &'a RunRef,
        _completion: &'a crate::runtime::session::TurnCompletionHandle,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<Vec<JsValue>, SubmitError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn observe_event<'a>(
        &'a self,
        _input: &'a JsValue,
        _event: &'a JsValue,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<(), SubmitError>> {
        Box::pin(async { Ok(()) })
    }
    fn has_sdk_config(&self) -> bool {
        false
    }
    fn validate_sdk_config(&self, _config: &JsValue) -> Result<(), SubmitError> {
        Ok(())
    }
    fn set_sdk_config<'a>(
        &'a self,
        _input: &'a JsValue,
        _completion: &'a crate::runtime::session::TurnCompletionHandle,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<bool, SubmitError>> {
        Box::pin(async { Ok(false) })
    }
    fn reject_pending(&self, _session: &JsString, _reason: &str) -> Result<(), SubmitError> {
        Ok(())
    }
}

struct RecordingServices {
    observed: Arc<Observed>,
}

impl SubmitHostServices for RecordingServices {
    fn workspace_id(&self, _input: &JsValue) -> Result<Option<JsString>, SubmitError> {
        Ok(None)
    }
    fn snapshots(&self) -> Option<Arc<dyn crate::runtime::turn::submit::SubmitSnapshots>> {
        None
    }
    fn snapshots_required(&self) -> bool {
        false
    }
    fn cwd(&self) -> Option<Arc<dyn crate::runtime::turn::submit_host::SubmitCwdService>> {
        None
    }
    fn refresh(&self) -> crate::runtime::turn::submit::SubmitFuture<'_, Result<(), SubmitError>> {
        Box::pin(async { Ok(()) })
    }
    fn run_policy(&self) -> Option<JsValue> {
        None
    }
    fn resolve_attachments<'a>(
        &'a self,
        _input: &'a JsValue,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<SubmitAttachmentLease, SubmitError>>
    {
        Box::pin(async {
            Ok(SubmitAttachmentLease {
                attachments: vec![],
                release: Box::new(|| Box::pin(async { Ok(()) })),
            })
        })
    }
    fn prepare_attachment_turn<'a>(
        &'a self,
        message: &'a JsValue,
        _attachments: Vec<JsValue>,
        _project: Option<&JsString>,
        _session: Option<&JsString>,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<JsValue, SubmitError>> {
        let observed = self.observed.clone();
        let message = message.clone();
        Box::pin(async move {
            observed.messages.lock().unwrap().push(
                js_to_value(&message)
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            Ok(object([("type", "text".into()), ("text", message)]))
        })
    }
    fn resolve_model_selection<'a>(
        &'a self,
        _input: &'a JsValue,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<JsValue, SubmitError>> {
        Box::pin(async { Ok(object([("source", "default".into())])) })
    }
    fn record_failure<'a>(
        &'a self,
        _run: &'a RunRef,
        _status: &'a JsValue,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<(), SubmitError>> {
        Box::pin(async { Ok(()) })
    }
    fn after_turn(&self, _run: &RunRef) -> Result<(), SubmitError> {
        Ok(())
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn abort_turn_timeout_ms(&self) -> u64 {
        60_000
    }
}

struct NoopReplacement;
impl SubmitReplacement for NoopReplacement {
    fn commit_accepted_input<'a>(
        &'a self,
        _session: &'a JsString,
        _run: &'a JsString,
    ) -> crate::runtime::turn::submit::SubmitFuture<'a, Result<(), SubmitError>> {
        Box::pin(async { Ok(()) })
    }
    fn release_claim(&self, _session: &JsString, _run: &JsString) {}
}

/// Scripted live session: replays `events`, then either EOFs or holds the
/// stream open until aborted.
struct ScriptedSession {
    events: Vec<JsValue>,
    hold_open: bool,
    entered_hold: Arc<Notify>,
    abort: Arc<AbortLatch>,
}

impl LiveSession for Arc<ScriptedSession> {
    fn snapshot(&self) -> JsValue {
        object([
            ("messages", JsValue::Array(vec![])),
            ("cwd", "/unused".into()),
        ])
    }
    fn abort(&self, _reason: &JsString) -> RuntimeResult {
        self.abort.fire();
        Ok(())
    }
    fn dispose(&self) -> DisposeFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn submit(
        &self,
        _input: JsValue,
        _options: JsValue,
    ) -> Result<Box<dyn LiveTurn>, RuntimeError> {
        Ok(Box::new(ScriptedTurn {
            events: self.events.clone(),
            index: 0,
            hold_open: self.hold_open,
            entered_hold: self.entered_hold.clone(),
            abort: self.abort.clone(),
        }))
    }
}

struct ScriptedTurn {
    events: Vec<JsValue>,
    index: usize,
    hold_open: bool,
    entered_hold: Arc<Notify>,
    abort: Arc<AbortLatch>,
}

impl LiveTurn for ScriptedTurn {
    fn next(&mut self) -> SessionNext<'_> {
        Box::pin(async {
            if let Some(event) = self.events.get(self.index) {
                self.index += 1;
                return Ok(Some(event.clone()));
            }
            if self.hold_open {
                self.entered_hold.notify_one();
                let released = self.abort.notify.notified();
                if self.abort.is_fired() {
                    return Ok(None);
                }
                released.await;
                return Ok(None);
            }
            Ok(None)
        })
    }
}

struct ScriptedFactory(Arc<ScriptedSession>);
impl SessionFactory for ScriptedFactory {
    fn create(
        &self,
        _context: SessionContext,
        _previous: Option<LiveSessionHandle>,
    ) -> BoxFuture<Result<LiveSessionHandle, RuntimeError>> {
        let session = Arc::new(self.0.clone());
        Box::pin(async { Ok(LiveSessionHandle::new(session, None)) })
    }
}

struct OwnedFixture {
    submitter: Arc<TurnSubmitter>,
    observed: Arc<Observed>,
    session_key: &'static str,
}

impl OwnedFixture {
    fn new(events: Vec<Value>, hold_open: bool) -> Self {
        let observed = Arc::new(Observed::default());
        let entered_hold = Arc::new(Notify::new());
        let session = Arc::new(ScriptedSession {
            events: events.iter().map(js).collect(),
            hold_open,
            entered_hold: entered_hold.clone(),
            abort: Arc::default(),
        });
        let mut options = SessionOwnerOptions::new(Arc::new(ScriptedFactory(session)));
        options.idle_sweep_interval_ms = 0;
        let owner = crate::runtime::session::SessionOwner::new(options);
        let submitter = Arc::new(TurnSubmitter {
            owner,
            coordinator: Arc::new(TurnEventCoordinator::new()),
            projector: Arc::new(
                GatewayAgentEventProjector::new(GatewayAgentEventProjectorOptions::default())
                    .unwrap(),
            ),
            registry: None,
            host: Arc::new(GatewaySubmitHost::new(
                Arc::new(RecordingServices {
                    observed: observed.clone(),
                }),
                Arc::new(NoopReplacement),
                Arc::new(RecordingAdmission {
                    observed: observed.clone(),
                }),
            )),
            workspace_admissions: Default::default(),
        });
        Self {
            submitter,
            observed,
            session_key: "sess-owned",
        }
    }

    async fn clean(self) {
        let _ = self.submitter.owner.close(&self.session_key.into()).await;
    }
}

/// Characterization: `submit_owned` derives the run identity from the input's
/// explicit `sessionKey`/`projectKey`/`runId` (never generating a runId of
/// its own) and forwards the `message` to the turn host — the contract the
/// public_v1 integration relies on to hand the v1-assigned runId through.
#[tokio::test]
async fn owned_submit_respects_the_explicit_run_and_session_identity() {
    let fixture = OwnedFixture::new(
        vec![
            json!({"type": "turn_started"}),
            json!({"type": "turn_completed", "result": {"type": "success", "stopReason": "completed"}}),
        ],
        false,
    );
    let input = js(&json!({
        "sessionKey": "sess-owned",
        "projectKey": "proj-owned",
        "runId": "v1-run-explicit",
        "channelKey": "agentcpm-pd",
        "message": "hello owned turn",
    }));
    let mut submission = fixture.submitter.submit_owned(input);
    let mut completed = false;
    while let Some(event) = submission.next().await.unwrap() {
        let event = js_to_value(&event);
        if event["type"] == "turn_completed" {
            completed = true;
        }
    }
    assert!(completed, "scripted terminal event must reach the consumer");

    let (runs, messages) = observed_snapshot(&fixture.observed);
    assert_eq!(runs.len(), 1, "exactly one admitted run");
    assert_eq!(runs[0]["runId"], "v1-run-explicit");
    assert_eq!(runs[0]["sessionKey"], "sess-owned");
    assert_eq!(runs[0]["projectKey"], "proj-owned");
    assert_eq!(
        messages,
        ["hello owned turn"],
        "the message field reaches the turn host"
    );
    fixture.clean().await;
}

/// Characterization: the owned stream delivers the projected events in order
/// and EOFs (repeatedly) after the terminal frame — the projection loop reads
/// exactly this contract.
#[tokio::test]
async fn owned_submit_streams_events_in_order_until_eof() {
    let fixture = OwnedFixture::new(
        vec![
            json!({"type": "turn_started"}),
            json!({"type": "agent_status", "event": "warning", "detail": {"code": "x"}}),
            json!({"type": "turn_completed", "result": {"type": "success", "stopReason": "completed"}}),
        ],
        false,
    );
    let input = js(&json!({"sessionKey": "sess-owned", "runId": "v1-run-order", "message": "m"}));
    let mut submission = fixture.submitter.submit_owned(input);
    let mut types = Vec::new();
    while let Some(event) = submission.next().await.unwrap() {
        types.push(js_to_value(&event)["type"].as_str().unwrap().to_owned());
    }
    assert_eq!(types, ["turn_started", "agent_status", "turn_completed"]);
    assert!(submission.next().await.unwrap().is_none(), "EOF is stable");
    let (runs, _messages) = observed_snapshot(&fixture.observed);
    assert_eq!(runs[0]["runId"], "v1-run-order");
    fixture.clean().await;
}

/// Characterization: `cancel_and_drain` on an in-flight owned turn resolves
/// after the live activity is aborted (the stream observes the abort latch),
/// which is how the v1 `run.cancel` path stops a submitter-driven run.
#[tokio::test]
async fn owned_cancel_and_drain_releases_a_held_live_turn() {
    let fixture = OwnedFixture::new(vec![json!({"type": "turn_started"})], true);
    let input = js(&json!({"sessionKey": "sess-owned", "runId": "v1-run-hold", "message": "m"}));
    let mut submission = fixture.submitter.submit_owned(input);
    let started = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = submission.next().await.unwrap().expect("live turn");
            if js_to_value(&event)["type"] == "turn_started" {
                return;
            }
        }
    })
    .await;
    assert!(started.is_ok(), "turn started within the deadline");
    // The held stream parks on the abort latch; the scripted session's abort
    // (fired by cancel_and_drain through the owned control) releases it, so
    // the drain resolves instead of waiting on the held turn forever.
    tokio::time::timeout(
        Duration::from_secs(5),
        submission.cancel_and_drain("stop".into()),
    )
    .await
    .expect("cancel_and_drain resolves after abort")
    .unwrap();
    fixture.clean().await;
}

/// Integration (D20): `run.submit` on a submitter-backed runtime hands the
/// store-assigned runId to `submit_owned` (`input.runId`) and projects the
/// turn machine's frames into the authoritative SQLite store.
#[tokio::test]
async fn run_submit_drives_the_v1_run_through_submit_owned() {
    let tmp = TempDir::new("owned-success");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
    let iid = file.instance_id.clone();
    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-test-1".to_owned(),
        server_revision: "srv-test-1".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };
    let store = V1Store::open_and_recover(&root_dir, meta).unwrap();
    let fixture = OwnedFixture::new(
        vec![
            json!({"type": "turn_started"}),
            json!({"type": "turn_completed", "result": {"type": "success", "stopReason": "completed"}}),
        ],
        false,
    );
    let rt = PublicV1Runtime::with_submitter(
        store,
        Arc::new(OfflineRunDriver),
        fixture.submitter.clone(),
    );

    rt.dispatch(
        "project.register",
        json!({
            "instanceId": iid,
            "projectId": "proj-owned",
            "name": "Owned",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
        }),
        None,
        None,
    )
    .await
    .unwrap();
    rt.dispatch(
        "session.create",
        json!({"instanceId": iid, "sessionId": "sess-owned", "projectId": "proj-owned"}),
        None,
        None,
    )
    .await
    .unwrap();

    let (_, submitted) = rt
        .dispatch(
            "run.submit",
            json!({"instanceId": iid, "sessionId": "sess-owned", "input": [{"kind": "text", "text": "hello from v1"}]}),
            Some("key-owned-turn-1"),
            None,
        )
        .await
        .unwrap();
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    assert!(!run_id.is_empty());

    let (_, waited) = rt
        .dispatch(
            "run.wait",
            json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(waited["run"]["state"], "succeeded");

    // The turn machine received the v1-assigned runId explicitly.
    let (runs, messages) = observed_snapshot(&fixture.observed);
    assert_eq!(runs.len(), 1, "run.submit goes through submit_owned once");
    assert_eq!(runs[0]["runId"], run_id, "input.runId is the v1 runId");
    assert_eq!(runs[0]["sessionKey"], "sess-owned");
    assert_eq!(runs[0]["projectKey"], "proj-owned");
    assert_eq!(
        messages,
        ["hello from v1"],
        "the v1 input parts reach the turn machine as the message"
    );

    // The store recorded the projected lifecycle events.
    let (_, events) = rt
        .dispatch(
            "run.events",
            json!({"instanceId": iid, "runId": run_id}),
            None,
            None,
        )
        .await
        .unwrap();
    let types: Vec<&str> = events["frame"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    let queued = types
        .iter()
        .position(|t| *t == "run.queued")
        .expect("run.queued");
    let started = types
        .iter()
        .position(|t| *t == "run.started")
        .expect("run.started");
    let completed = types
        .iter()
        .position(|t| *t == "run.completed")
        .expect("run.completed");
    assert!(
        queued < started && started < completed,
        "event order {types:?}"
    );
    let (_, got) = rt
        .dispatch(
            "run.get",
            json!({"instanceId": iid, "runId": run_id}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(got["run"]["state"], "succeeded");
    assert_eq!(got["run"]["outcomeCode"], "SUCCEEDED");
    fixture.clean().await;
}

/// Integration (D20): a turn-machine failure projects to `failed` and pauses
/// the session queue, matching the offline driver's error semantics.
#[tokio::test]
async fn owned_turn_failure_pauses_the_session_queue() {
    let tmp = TempDir::new("owned-failure");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
    let iid = file.instance_id.clone();
    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-test-1".to_owned(),
        server_revision: "srv-test-1".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };
    let store = V1Store::open_and_recover(&root_dir, meta).unwrap();
    let fixture = OwnedFixture::new(
        vec![
            json!({"type": "turn_started"}),
            json!({"type": "turn_failed", "error": {"code": "DRIVER_TEST_FAILURE", "message": "boom"}}),
        ],
        false,
    );
    let rt = PublicV1Runtime::with_submitter(
        store,
        Arc::new(OfflineRunDriver),
        fixture.submitter.clone(),
    );

    rt.dispatch(
        "project.register",
        json!({
            "instanceId": iid,
            "projectId": "proj-owned",
            "name": "Owned",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
        }),
        None,
        None,
    )
    .await
    .unwrap();
    rt.dispatch(
        "session.create",
        json!({"instanceId": iid, "sessionId": "sess-owned", "projectId": "proj-owned"}),
        None,
        None,
    )
    .await
    .unwrap();

    let (_, submitted) = rt
        .dispatch(
            "run.submit",
            json!({"instanceId": iid, "sessionId": "sess-owned", "input": [{"kind": "text", "text": "will fail"}]}),
            Some("key-owned-turn-fail-1"),
            None,
        )
        .await
        .unwrap();
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    let (_, waited) = rt
        .dispatch(
            "run.wait",
            json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(waited["run"]["state"], "failed");
    assert_eq!(waited["run"]["outcomeCode"], "DRIVER_TEST_FAILURE");

    let (_, queue) = rt
        .dispatch(
            "queue.get",
            json!({"instanceId": iid, "sessionId": "sess-owned"}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(queue["queue"]["paused"], true);
    assert_eq!(queue["queue"]["pauseReason"], "run_failed");
    fixture.clean().await;
}

/// Integration (D20): `run.cancel` on an in-flight submitter-driven run stops
/// the owned turn and records the durable Cancelled state.
#[tokio::test]
async fn owned_turn_cancel_stops_the_turn_and_records_cancelled() {
    let tmp = TempDir::new("owned-cancel");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
    let iid = file.instance_id.clone();
    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-test-1".to_owned(),
        server_revision: "srv-test-1".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };
    let store = V1Store::open_and_recover(&root_dir, meta).unwrap();
    let fixture = OwnedFixture::new(vec![json!({"type": "turn_started"})], true);
    let rt = PublicV1Runtime::with_submitter(
        store,
        Arc::new(OfflineRunDriver),
        fixture.submitter.clone(),
    );

    rt.dispatch(
        "project.register",
        json!({
            "instanceId": iid,
            "projectId": "proj-owned",
            "name": "Owned",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
        }),
        None,
        None,
    )
    .await
    .unwrap();
    rt.dispatch(
        "session.create",
        json!({"instanceId": iid, "sessionId": "sess-owned", "projectId": "proj-owned"}),
        None,
        None,
    )
    .await
    .unwrap();

    let (_, submitted) = rt
        .dispatch(
            "run.submit",
            json!({"instanceId": iid, "sessionId": "sess-owned", "input": [{"kind": "text", "text": "hold"}]}),
            Some("key-owned-turn-cancel-1"),
            None,
        )
        .await
        .unwrap();
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    // Wait for the owned turn to actually start, then cancel it mid-flight.
    let running = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, got) = rt
                .dispatch(
                    "run.get",
                    json!({"instanceId": iid, "runId": run_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["run"]["state"] == "running" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(running.is_ok(), "owned turn reached running");

    let (_, cancelled) = rt
        .dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": run_id}),
            Some("key-owned-turn-cancel-req-1"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(cancelled["receipt"]["state"], "cancelling");

    let (_, waited) = rt
        .dispatch(
            "run.wait",
            json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(waited["run"]["state"], "cancelled");
    fixture.clean().await;
}
