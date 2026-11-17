//! `public_v1` operation dispatch, deterministic run driver, and 28-op S1+S2 runtime.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runtime::turn::submit::TurnSubmitter;
use agentcpm_pd_protocol::contract::{OPERATION_VERSION, ResponseOutcome, RunState};
use agentcpm_pd_protocol::envelope::bind_instance_id;
use agentcpm_pd_protocol::errors::{
    CODE_CAPABILITY_UNAVAILABLE, CODE_UNSUPPORTED_OPERATION, ContractError,
    ERR_CAPABILITY_UNAVAILABLE, ERR_MISSING_IDEMPOTENCY_KEY, EffectStatus,
    JSONRPC_METHOD_NOT_FOUND, RetrySpec,
};
use agentcpm_pd_protocol::registry::{
    ExecutionMode, OperationRegistry, RetryKind, validation_errors_to_contract_error,
};
use agentcpm_pd_protocol::wire::MAX_FRAME_BYTES;
use pilotdeck_protocol::JsValue;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::public_v1::store::{
    EventsQueryResult, RunTransition, StoredRun, V1Store, is_terminal_run_state,
};

#[derive(Clone, Debug)]
pub enum DispatchError {
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    Application(ContractError),
}

impl From<ContractError> for DispatchError {
    fn from(value: ContractError) -> Self {
        Self::Application(value)
    }
}

pub type DriverFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

pub trait RunDriver: Send + Sync {
    fn execute(&self, ctx: RunDriverContext) -> DriverFuture;
}

#[derive(Clone)]
pub struct RunDriverContext {
    pub store: Arc<V1Store>,
    pub run: StoredRun,
    pub runtime: Arc<PublicV1Runtime>,
    pub cancel_rx: watch::Receiver<bool>,
}

#[derive(Default)]
struct InteractionWaiterRegistry {
    waiters: Mutex<HashMap<String, watch::Sender<Option<Value>>>>,
    run_cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl InteractionWaiterRegistry {
    fn register_run_cancel(&self, run_id: &str) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        self.run_cancels
            .lock()
            .expect("cancel map")
            .insert(run_id.to_owned(), tx);
        rx
    }

    fn clear_run_cancel(&self, run_id: &str) {
        self.run_cancels.lock().expect("cancel map").remove(run_id);
    }

    fn signal_run_cancel(&self, run_id: &str) {
        if let Some(tx) = self.run_cancels.lock().expect("cancel map").get(run_id) {
            let _ = tx.send(true);
        }
    }

    fn register_interaction_waiter(&self, interaction_id: &str) -> watch::Receiver<Option<Value>> {
        let (tx, rx) = watch::channel(None);
        self.waiters
            .lock()
            .expect("interaction waiters")
            .insert(interaction_id.to_owned(), tx);
        rx
    }

    fn notify_interaction_resolved(&self, interaction_id: &str, answer: Value) {
        if let Some(tx) = self
            .waiters
            .lock()
            .expect("interaction waiters")
            .remove(interaction_id)
        {
            let _ = tx.send(Some(answer));
        }
    }
}

/// Offline deterministic driver supporting real run progression, cancellation,
/// failures with queue pause, approval/question interaction waits, and event bursts.
pub struct OfflineRunDriver;

fn cancel_run_now(store: &V1Store, run_id: &str, payload: &Value) {
    let _ = store.transition_run_state(
        run_id,
        RunTransition::new(RunState::Cancelled, "run.cancelled", payload).outcome_code("CANCELLED"),
    );
}

fn succeed_run_now(store: &V1Store, run_id: &str, payload: &Value) {
    let _ = store.transition_run_state(
        run_id,
        RunTransition::new(RunState::Succeeded, "run.completed", payload).outcome_code("SUCCEEDED"),
    );
}

/// Why `drive_owned_turn` stopped polling the owned submission.
enum DriveStop {
    /// A terminal turn event was projected durably.
    Terminal,
    /// The stream ended cleanly without a terminal event.
    Eof,
    /// The submission reported an error.
    Error(crate::runtime::turn::submit::SubmitError),
    /// `run.cancel` fired for this run.
    Cancelled,
}

fn fail_run_now(store: &V1Store, run_id: &str, code: &str, payload: &Value) {
    let _ = store.transition_run_state(
        run_id,
        RunTransition::new(RunState::Failed, "run.failed", payload)
            .outcome_code(code)
            .pause_session_reason("run_failed"),
    );
}

fn is_turn_event(event: &JsValue, event_type: &str) -> bool {
    event
        .get("type")
        .and_then(JsValue::as_string)
        .is_some_and(|value| value.equals_str(event_type))
}

fn js_event_to_value(event: &JsValue) -> Option<Value> {
    let text = pilotdeck_protocol::stringify_json(event).ok()??;
    serde_json::from_str(&text).ok()
}

impl RunDriver for OfflineRunDriver {
    fn execute(&self, mut ctx: RunDriverContext) -> DriverFuture {
        Box::pin(async move {
            let run_id = ctx.run.run_id.clone();
            if *ctx.cancel_rx.borrow() {
                cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                return;
            }

            if ctx
                .store
                .transition_run_state(
                    &run_id,
                    RunTransition::new(
                        RunState::Running,
                        "run.started",
                        &json!({"runId": run_id, "sessionId": ctx.run.session_id}),
                    ),
                )
                .is_err()
            {
                return;
            }

            let prompt_text = extract_prompt_text(&ctx.run.input);
            let directive = parse_driver_directive(&prompt_text);

            match directive {
                DriverDirective::BurstEvents(count) => {
                    for i in 0..count {
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(
                                &ctx.store,
                                &run_id,
                                &json!({"runId": run_id, "seq": i}),
                            );
                            return;
                        }
                        let _ = ctx.store.append_run_progress_event(
                            &run_id,
                            &ctx.run.session_id,
                            "run.output.delta",
                            &json!({"runId": run_id, "chunkIndex": i, "delta": format!("chunk-{i:05}-{}", "x".repeat(48))}),
                        );
                        if i % 64 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    succeed_run_now(
                        &ctx.store,
                        &run_id,
                        &json!({"runId": run_id, "emittedEvents": count}),
                    );
                }
                DriverDirective::Fail {
                    reason,
                    pause_reason,
                } => {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    let _ = ctx.store.transition_run_state(
                        &run_id,
                        RunTransition::new(
                            RunState::Failed,
                            "run.failed",
                            &json!({"runId": run_id, "errorCode": reason}),
                        )
                        .outcome_code(&reason)
                        .pause_session_reason(&pause_reason),
                    );
                }
                DriverDirective::Approval { prompt } => {
                    let Ok(ia) = ctx.store.create_interaction(
                        &run_id,
                        "approval",
                        &prompt,
                        vec!["allow".to_owned(), "deny".to_owned()],
                        Some("agentcpm-pd.approval.response/1".to_owned()),
                        json!({"type": "object", "properties": {"decision": {"enum": ["allow", "deny"]}}, "required": ["decision"]}),
                    ) else {
                        return;
                    };
                    let mut rx = ctx
                        .runtime
                        .waiters
                        .register_interaction_waiter(&ia.interaction_id);
                    loop {
                        let resolved = rx.borrow_and_update().clone();
                        if let Some(ans) = resolved {
                            let decision = ans
                                .get("decision")
                                .and_then(Value::as_str)
                                .unwrap_or("deny");
                            if decision == "allow" {
                                succeed_run_now(
                                    &ctx.store,
                                    &run_id,
                                    &json!({"runId": run_id, "approved": true}),
                                );
                            } else {
                                let _ = ctx.store.transition_run_state(
                                    &run_id,
                                    RunTransition::new(
                                        RunState::Failed,
                                        "run.failed",
                                        &json!({"runId": run_id, "errorCode": "APPROVAL_DENIED"}),
                                    )
                                    .outcome_code("APPROVAL_DENIED"),
                                );
                            }
                            return;
                        }
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                }
                DriverDirective::Question { prompt } => {
                    let Ok(ia) = ctx.store.create_interaction(
                        &run_id,
                        "question",
                        &prompt,
                        vec![],
                        Some("agentcpm-pd.question.response/1".to_owned()),
                        json!({"type": "object"}),
                    ) else {
                        return;
                    };
                    let mut rx = ctx
                        .runtime
                        .waiters
                        .register_interaction_waiter(&ia.interaction_id);
                    loop {
                        let resolved = rx.borrow_and_update().clone();
                        if let Some(ans) = resolved {
                            succeed_run_now(
                                &ctx.store,
                                &run_id,
                                &json!({"runId": run_id, "answer": ans}),
                            );
                            return;
                        }
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                }
                DriverDirective::Sleep(ms) => {
                    let deadline = Instant::now() + Duration::from_millis(ms);
                    while Instant::now() < deadline {
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        let step = (deadline.saturating_duration_since(Instant::now()))
                            .min(Duration::from_millis(20));
                        tokio::select! {
                            _ = tokio::time::sleep(step) => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                    let _ = ctx.store.append_run_progress_event(
                        &run_id,
                        &ctx.run.session_id,
                        "run.output.delta",
                        &json!({"runId": run_id, "text": format!("completed: {prompt_text}")}),
                    );
                    succeed_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                }
            }
        })
    }
}

enum DriverDirective {
    Sleep(u64),
    Fail {
        reason: String,
        pause_reason: String,
    },
    Approval {
        prompt: String,
    },
    Question {
        prompt: String,
    },
    BurstEvents(usize),
}

fn extract_prompt_text(input: &Value) -> String {
    input
        .get("input")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn parse_driver_directive(text: &str) -> DriverDirective {
    if let Some(rest) = text.split("[pd-driver:events=").nth(1)
        && let Some(num) = rest.split(']').next()
        && let Ok(count) = num.trim().parse::<usize>()
    {
        return DriverDirective::BurstEvents(count.clamp(1, 20_000));
    }
    if let Some(rest) = text.split("[pd-driver:sleep=").nth(1)
        && let Some(num) = rest.split(']').next()
        && let Ok(ms) = num.trim().parse::<u64>()
    {
        return DriverDirective::Sleep(ms);
    }
    if text.contains("[pd-driver:fail]") {
        return DriverDirective::Fail {
            reason: "DRIVER_INJECTED_FAILURE".to_owned(),
            pause_reason: "run_failed".to_owned(),
        };
    }
    if text.contains("[pd-driver:approval]") {
        return DriverDirective::Approval {
            prompt: text.to_owned(),
        };
    }
    if text.contains("[pd-driver:question]") {
        return DriverDirective::Question {
            prompt: text.to_owned(),
        };
    }
    DriverDirective::Sleep(25)
}

pub struct PublicV1Runtime {
    store: Arc<V1Store>,
    driver: Arc<dyn RunDriver>,
    waiters: InteractionWaiterRegistry,
    /// D20: when an embedding mounts `public_v1` with the production turn
    /// machine, every `run.submit` is driven through
    /// [`TurnSubmitter::submit_owned`] with the v1-assigned `runId` passed
    /// explicitly in `input.runId`. `None` keeps the offline
    /// [`OfflineRunDriver`] test adapter (007 §7.2's no-model-key path).
    submitter: Option<Arc<TurnSubmitter>>,
}

impl PublicV1Runtime {
    pub fn new(store: Arc<V1Store>) -> Arc<Self> {
        Self::with_driver(store, Arc::new(OfflineRunDriver))
    }

    pub fn with_driver(store: Arc<V1Store>, driver: Arc<dyn RunDriver>) -> Arc<Self> {
        Arc::new(Self {
            store,
            driver,
            waiters: InteractionWaiterRegistry::default(),
            submitter: None,
        })
    }

    /// D20 wiring: submit runs through the real turn machine (`submit_owned`),
    /// keeping `driver` as the no-key fallback/test adapter.
    pub fn with_submitter(
        store: Arc<V1Store>,
        driver: Arc<dyn RunDriver>,
        submitter: Arc<TurnSubmitter>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            driver,
            waiters: InteractionWaiterRegistry::default(),
            submitter: Some(submitter),
        })
    }

    pub fn store(&self) -> &Arc<V1Store> {
        &self.store
    }

    pub fn start_session_worker(self: &Arc<Self>, initial_run_id: String) {
        let rt = self.clone();
        tokio::spawn(async move {
            let mut current = Some(initial_run_id);
            while let Some(run_id) = current {
                let Ok(run) = rt.store.get_run(&run_id) else {
                    break;
                };
                let session_id = run.session_id.clone();
                if run.terminal {
                    let next = rt
                        .store
                        .list_pending_runs_for_session(&run.session_id)
                        .ok()
                        .and_then(|v| v.into_iter().next());
                    current = next;
                    continue;
                }
                if let Some(submitter) = rt.submitter.clone() {
                    rt.drive_owned_turn(&submitter, run).await;
                } else {
                    let cancel_rx = rt.waiters.register_run_cancel(&run_id);
                    let ctx = RunDriverContext {
                        store: rt.store.clone(),
                        run,
                        runtime: rt.clone(),
                        cancel_rx,
                    };
                    rt.driver.execute(ctx).await;
                    rt.waiters.clear_run_cancel(&run_id);

                    // Check whether driver already called transition_run_state to terminal; if not, finalize safely.
                    if let Ok(after) = rt.store.get_run(&run_id)
                        && !after.terminal
                    {
                        succeed_run_now(&rt.store, &run_id, &json!({"runId": run_id}));
                    }
                }

                // Check if session queue is unpaused and still has pending queued runs.
                current = match rt.store.get_queue(&session_id, None) {
                    Ok(q) if q["queue"]["paused"].as_bool() == Some(false) => {
                        let next_id = q["queue"]["items"]
                            .as_array()
                            .and_then(|arr| arr.first())
                            .and_then(|item| item["runId"].as_str())
                            .map(ToOwned::to_owned);
                        if let Some(ref next_rid) = next_id {
                            // Keep active_run_id pointing cleanly to the next run.
                            let _ = rt.store.get_run(next_rid);
                        }
                        next_id
                    }
                    _ => None,
                };
            }
        });
    }

    /// D20: drive one v1 run through the real turn machine. The submission is
    /// channel-owned (`submit_owned`), so the run's lifecycle is independent
    /// of any `/v1/ws` connection: this task is the sole consumer, the turn
    /// input carries the store-assigned `runId` explicitly, and every turn
    /// event is projected into the authoritative SQLite store.
    async fn drive_owned_turn(self: &Arc<Self>, submitter: &Arc<TurnSubmitter>, run: StoredRun) {
        let run_id = run.run_id.clone();
        let turn_input = json!({
            "sessionKey": run.session_id,
            "projectKey": run.project_id,
            "runId": run.run_id,
            "channelKey": "agentcpm-pd",
            "message": extract_prompt_text(&run.input),
        });
        let js_input = match pilotdeck_protocol::parse_json(&turn_input.to_string()) {
            Ok(value) => value,
            Err(error) => {
                let payload = json!({
                    "runId": run_id,
                    "errorCode": "TURN_INPUT_INVALID",
                    "message": error.to_string(),
                });
                fail_run_now(&self.store, &run_id, "TURN_INPUT_INVALID", &payload);
                return;
            }
        };

        let mut cancel_rx = self.waiters.register_run_cancel(&run_id);
        let mut submission = submitter.submit_owned(js_input);
        // The drive future borrows `submission`; scoping it here releases that
        // borrow before the cancellation path calls `cancel_and_drain`.
        let stop = {
            let rt = self.clone();
            let mut drive = Box::pin(async {
                let mut started = false;
                loop {
                    match submission.next().await {
                        Ok(Some(event)) => {
                            if is_turn_event(&event, "turn_started") && !started {
                                started = true;
                                let _ = rt.store.transition_run_state(
                                    &run_id,
                                    RunTransition::new(
                                        RunState::Running,
                                        "run.started",
                                        &json!({"runId": run_id, "sessionId": run.session_id}),
                                    ),
                                );
                            } else if rt.project_turn_event(&run, &event) {
                                return DriveStop::Terminal;
                            }
                        }
                        Ok(None) => return DriveStop::Eof,
                        Err(error) => return DriveStop::Error(error),
                    }
                }
            });
            let mut cancel_fired = Box::pin(async {
                loop {
                    if *cancel_rx.borrow() {
                        return;
                    }
                    if cancel_rx.changed().await.is_err() {
                        // The cancel sender was replaced or cleared for this
                        // run; never fire instead of busy-looping on a dead
                        // watch channel.
                        return std::future::pending::<()>().await;
                    }
                }
            });
            tokio::select! {
                stop = &mut drive => stop,
                () = &mut cancel_fired => DriveStop::Cancelled,
            }
        };
        match stop {
            DriveStop::Cancelled => {
                let drained = submission.cancel_and_drain("run.cancelled".into()).await;
                // `store.cancel_run` already moved a non-terminal run to
                // `cancelling`; the drain ending without a terminal
                // transition still owes the durable Cancelled state (or the
                // failure the drain itself reported).
                if let Ok(after) = self.store.get_run(&run_id)
                    && !after.terminal
                {
                    match drained {
                        Ok(()) => {
                            cancel_run_now(&self.store, &run_id, &json!({"runId": run_id}));
                        }
                        Err(error) => {
                            let payload = json!({
                                "runId": run_id,
                                "errorCode": error.code,
                                "message": error.message,
                            });
                            fail_run_now(&self.store, &run_id, &error.code, &payload);
                        }
                    }
                }
            }
            DriveStop::Eof => {
                // A clean EOF without a terminal turn event is an abnormal
                // end for a real turn machine; the session pauses like any
                // other failure (007 §7.2 keeps error-pause semantics here).
                if let Ok(after) = self.store.get_run(&run_id)
                    && !after.terminal
                {
                    let payload = json!({"runId": run_id, "errorCode": "TURN_STREAM_ENDED"});
                    fail_run_now(&self.store, &run_id, "TURN_STREAM_ENDED", &payload);
                }
            }
            DriveStop::Error(error) => {
                let payload = json!({
                    "runId": run_id,
                    "errorCode": error.code,
                    "message": error.message,
                });
                fail_run_now(&self.store, &run_id, &error.code, &payload);
            }
            DriveStop::Terminal => {
                // The terminal transition is already durable; dropping the
                // submission lets the owned pump drain on its own completion
                // fence (submit.rs's documented "drain or drop" contract).
            }
        }
        self.waiters.clear_run_cancel(&run_id);
    }

    /// Project one turn-machine event into the authoritative store. Returns
    /// `true` once a terminal outcome has been durably recorded.
    fn project_turn_event(&self, run: &StoredRun, event: &JsValue) -> bool {
        let Some(value) = js_event_to_value(event) else {
            return false;
        };
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "turn_started" => false,
            "turn_completed" => {
                let result = value.get("result").cloned().unwrap_or(Value::Null);
                let finish_reason = value
                    .get("finishReason")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let success = result.get("type").and_then(Value::as_str) == Some("success")
                    || finish_reason == "completed";
                if success {
                    let payload =
                        json!({"runId": run.run_id, "stopReason": finish_reason, "result": result});
                    let _ = self.store.transition_run_state(
                        &run.run_id,
                        RunTransition::new(RunState::Succeeded, "run.completed", &payload)
                            .outcome_code("SUCCEEDED"),
                    );
                } else if finish_reason == "aborted_streaming" {
                    cancel_run_now(&self.store, &run.run_id, &json!({"runId": run.run_id}));
                } else {
                    let payload = json!({"runId": run.run_id, "finishReason": finish_reason, "result": result});
                    fail_run_now(&self.store, &run.run_id, "TURN_FAILED", &payload);
                }
                true
            }
            "error" => {
                let code = value
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("TURN_ERROR");
                let payload = json!({
                    "runId": run.run_id,
                    "errorCode": code,
                    "message": value.get("message").cloned().unwrap_or(Value::Null),
                });
                fail_run_now(&self.store, &run.run_id, code, &payload);
                true
            }
            _ => {
                let _ = self.store.append_run_progress_event(
                    &run.run_id,
                    &run.session_id,
                    "run.progress",
                    &json!({"runId": run.run_id, "event": value}),
                );
                false
            }
        }
    }

    pub fn capabilities_json(&self) -> Value {
        let reg = OperationRegistry::global();
        let current_round: std::collections::BTreeSet<&str> =
            reg.current_round_ids().iter().map(String::as_str).collect();
        let meta = self.store.meta();
        let limits_pair = vec![json!({"key": "maxFrameBytes", "value": MAX_FRAME_BYTES})];
        let mut caps = Vec::with_capacity(current_round.len() + 2);
        for op in reg.operations() {
            if current_round.contains(op.id.as_str()) {
                let is_remote = op.execution != ExecutionMode::Local;
                let available = is_remote || op.id == "schema.list" || op.id == "schema.get";
                if available {
                    caps.push(json!({
                        "methodId": op.id,
                        "available": true,
                        "reason": Value::Null,
                        "schemaVersion": OPERATION_VERSION,
                        "limits": limits_pair,
                        "supported": true,
                        "configured": true,
                        "authorized": true,
                        "requiredPermission": [],
                    }));
                } else {
                    caps.push(json!({
                        "methodId": op.id,
                        "available": false,
                        "reason": "IMPLEMENTATION_UNAVAILABLE",
                        "schemaVersion": OPERATION_VERSION,
                        "limits": limits_pair,
                        "supported": true,
                        "configured": Value::Null,
                        "authorized": Value::Null,
                        "requiredPermission": [],
                    }));
                }
            }
        }
        // Surface an explicit S3 stream entry (`terminals.output`) so callers can inspect its status.
        caps.push(json!({
            "methodId": "terminals.output",
            "available": false,
            "reason": "IMPLEMENTATION_UNAVAILABLE",
            "schemaVersion": OPERATION_VERSION,
            "limits": [],
            "supported": false,
            "configured": Value::Null,
            "authorized": Value::Null,
            "requiredPermission": [],
        }));

        json!({
            "identity": {
                "instanceId": meta.instance_id,
                "ownerId": meta.owner_id,
                "product": meta.product,
                "protocolVersion": meta.protocol_version,
                "nodeId": meta.node_id,
            },
            "generation": meta.generation,
            "runtimeKind": "native_rust",
            "configurationState": "ready",
            "capabilities": caps,
        })
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        method_id: &str,
        mut input: Value,
        idempotency_key: Option<&str>,
        expected_revision: Option<&str>,
    ) -> Result<(ResponseOutcome, Value), DispatchError> {
        let reg = OperationRegistry::global();
        let Some(spec) = reg.get(method_id) else {
            return Err(DispatchError::Rpc {
                code: JSONRPC_METHOD_NOT_FOUND,
                message: format!("unknown operation `{method_id}`"),
                data: None,
            });
        };

        if spec.execution == ExecutionMode::Local
            && method_id != "schema.list"
            && method_id != "schema.get"
        {
            return Err(DispatchError::Rpc {
                code: JSONRPC_METHOD_NOT_FOUND,
                message: format!(
                    "operation `{method_id}` is execution='local' CLI-only and not served over /v1/ws"
                ),
                data: Some(
                    serde_json::to_value(ContractError::new(
                        CODE_UNSUPPORTED_OPERATION,
                        format!("`{method_id}` is a local CLI command"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ))
                    .unwrap_or(Value::Null),
                ),
            });
        }

        bind_instance_id(
            &mut input,
            &self.store.meta().instance_id,
            spec.declares_instance_id,
        )?;

        if !spec.is_current_round {
            return Err(capability_unavailable_rpc_error(method_id));
        }

        if spec.retry == RetryKind::IdempotentSubmit && idempotency_key.unwrap_or("").is_empty() {
            return Err(DispatchError::Rpc {
                code: ERR_MISSING_IDEMPOTENCY_KEY,
                message: format!("operation `{method_id}` requires idempotencyKey"),
                data: None,
            });
        }

        reg.validate_input(method_id, &input)
            .map_err(|e| DispatchError::Application(validation_errors_to_contract_error(e)))?;

        match method_id {
            "schema.list" => Ok((ResponseOutcome::Succeeded, reg.handle_schema_list(&input)?)),
            "schema.get" => Ok((ResponseOutcome::Succeeded, reg.handle_schema_get(&input)?)),
            "system.capabilities" => Ok((ResponseOutcome::Succeeded, self.capabilities_json())),
            "system.health" => Ok((
                ResponseOutcome::Succeeded,
                json!({
                    "alive": true,
                    "ready": true,
                    "diagnostics": self.store.diagnostics(),
                }),
            )),
            "requests.lookup" => {
                let target_method = input["methodId"].as_str().unwrap_or_default();
                let key = input["idempotencyKey"].as_str().unwrap_or_default();
                Ok((
                    ResponseOutcome::Succeeded,
                    self.store.lookup_request(target_method, key)?,
                ))
            }
            "project.register" => Ok((
                ResponseOutcome::Succeeded,
                self.store.register_project(&input)?,
            )),
            "project.get" => Ok((
                ResponseOutcome::Succeeded,
                self.store
                    .get_project(input["projectId"].as_str().unwrap_or_default())?,
            )),
            "project.list" => Ok((
                ResponseOutcome::Succeeded,
                self.store.list_projects(input.get("page"))?,
            )),
            "session.create" => Ok((
                ResponseOutcome::Succeeded,
                self.store.create_session(&input)?,
            )),
            "session.get" => Ok((
                ResponseOutcome::Succeeded,
                self.store
                    .get_session(input["sessionId"].as_str().unwrap_or_default())?,
            )),
            "session.list" => Ok((
                ResponseOutcome::Succeeded,
                self.store.list_sessions(
                    input["projectId"].as_str().unwrap_or_default(),
                    input.get("sessionState").and_then(Value::as_str),
                    input.get("page"),
                )?,
            )),
            "queue.get" => Ok((
                ResponseOutcome::Succeeded,
                self.store.get_queue(
                    input["sessionId"].as_str().unwrap_or_default(),
                    input.get("page"),
                )?,
            )),
            "queue.resume" => {
                let rev = expected_revision
                    .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
                    .unwrap_or_default();
                let (queue_json, next_run_id) = self.store.resume_queue(
                    input["sessionId"].as_str().unwrap_or_default(),
                    rev,
                    input["acknowledgedPauseReason"]
                        .as_str()
                        .unwrap_or_default(),
                )?;
                if let Some(run_id) = next_run_id {
                    self.start_session_worker(run_id);
                }
                Ok((ResponseOutcome::Succeeded, queue_json))
            }
            "run.submit" => match self.store.submit_run(&input, idempotency_key)? {
                crate::public_v1::store::SubmitDisposition::Deduplicated { receipt } => {
                    Ok((ResponseOutcome::Accepted, receipt))
                }
                crate::public_v1::store::SubmitDisposition::NewRun {
                    receipt,
                    run_id,
                    start_immediately,
                    ..
                } => {
                    if start_immediately {
                        self.start_session_worker(run_id);
                    }
                    Ok((ResponseOutcome::Accepted, receipt))
                }
            },
            "run.get" => {
                let run = self
                    .store
                    .get_run(input["runId"].as_str().unwrap_or_default())?;
                let snap_cursor = run.final_cursor.clone();
                Ok((
                    ResponseOutcome::Succeeded,
                    json!({
                        "run": run.to_contract_json(),
                        "snapshotCursor": snap_cursor,
                    }),
                ))
            }
            "run.list" => {
                let states = input.get("states").and_then(Value::as_array).map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                });
                Ok((
                    ResponseOutcome::Succeeded,
                    self.store
                        .list_runs(input.get("scope"), states.as_ref(), input.get("page"))?,
                ))
            }
            "run.wait" => {
                let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
                let timeout_ms = input
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(25_000);
                let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                let notifier = self.store.state_notifier();
                loop {
                    let notified = notifier.notified();
                    let run = self.store.get_run(&run_id)?;
                    if is_terminal_run_state(run.state) {
                        return Ok((
                            ResponseOutcome::Succeeded,
                            json!({
                                "conditionMet": true,
                                "timedOut": false,
                                "run": run.to_contract_json(),
                            }),
                        ));
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok((
                            ResponseOutcome::Succeeded,
                            json!({
                                "conditionMet": false,
                                "timedOut": true,
                                "run": run.to_contract_json(),
                            }),
                        ));
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep(remaining) => {}
                    }
                }
            }
            "run.events" => {
                let run_id = input["runId"].as_str().unwrap_or_default();
                let after = input.get("afterCursor").and_then(Value::as_str);
                let limit = input
                    .get("page")
                    .and_then(|p| p.get("limit"))
                    .and_then(Value::as_u64)
                    .unwrap_or(100)
                    .clamp(1, 500) as usize;
                let res = self.store.query_run_events_page(run_id, after, limit)?;
                let val = match res {
                    EventsQueryResult::Page(p) => p,
                    EventsQueryResult::Gap(g) => g,
                };
                Ok((ResponseOutcome::Succeeded, val))
            }
            "run.snapshot" => {
                let run_id = input["runId"].as_str().unwrap_or_default();
                Ok((
                    ResponseOutcome::Succeeded,
                    self.store.build_run_snapshot(run_id)?,
                ))
            }
            "run.cancel" => {
                let run_id = input["runId"].as_str().unwrap_or_default();
                let outcome = self.store.cancel_run(run_id, idempotency_key)?;
                if outcome.should_notify_worker {
                    self.waiters.signal_run_cancel(run_id);
                }
                Ok((ResponseOutcome::Succeeded, outcome.receipt))
            }
            "interaction.list" => Ok((
                ResponseOutcome::Succeeded,
                self.store.list_interactions(
                    input.get("scope"),
                    input.get("kind").and_then(Value::as_str),
                    input.get("state").and_then(Value::as_str),
                    input.get("page"),
                )?,
            )),
            "interaction.get" => {
                let ia = self
                    .store
                    .get_interaction(input["interactionId"].as_str().unwrap_or_default())?;
                Ok((
                    ResponseOutcome::Succeeded,
                    json!({
                        "interaction": ia.to_contract_json(),
                        "responseSchema": ia.response_schema,
                    }),
                ))
            }
            "approval.respond" => {
                let rev = expected_revision
                    .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
                    .unwrap_or_default();
                let decision = input["decision"].as_str().unwrap_or("deny");
                let remember = input
                    .get("rememberScope")
                    .and_then(Value::as_str)
                    .unwrap_or("none");
                let res = self.store.respond_interaction(
                    input["interactionId"].as_str().unwrap_or_default(),
                    Some("approval"),
                    rev,
                    json!({"decision": decision, "rememberScope": remember}),
                    Some(remember),
                )?;
                if res.should_wake_run {
                    self.waiters.notify_interaction_resolved(
                        &res.interaction.interaction_id,
                        res.decision_or_answer.clone(),
                    );
                }
                Ok((
                    ResponseOutcome::Succeeded,
                    json!({
                        "interaction": res.interaction.to_contract_json(),
                        "policyRevision": res.policy_revision,
                    }),
                ))
            }
            "interaction.respond" => {
                let rev = expected_revision
                    .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
                    .unwrap_or_default();
                let answer = input.get("answer").cloned().unwrap_or(Value::Null);
                let res = self.store.respond_interaction(
                    input["interactionId"].as_str().unwrap_or_default(),
                    None,
                    rev,
                    answer,
                    None,
                )?;
                if res.should_wake_run {
                    self.waiters.notify_interaction_resolved(
                        &res.interaction.interaction_id,
                        res.decision_or_answer.clone(),
                    );
                }
                Ok((
                    ResponseOutcome::Succeeded,
                    json!({
                        "interaction": res.interaction.to_contract_json(),
                    }),
                ))
            }
            other => {
                let err = ContractError::capability_unavailable(
                    other,
                    "not implemented in S1+S2 round (deferred to S3)",
                );
                Err(DispatchError::Rpc {
                    code: ERR_CAPABILITY_UNAVAILABLE,
                    message: format!("capability unavailable in S1+S2: `{other}`"),
                    data: Some(serde_json::to_value(&err).unwrap_or(Value::Null)),
                })
            }
        }
    }

    pub fn notify_all_events(&self) -> Arc<tokio::sync::Notify> {
        self.store.event_notifier()
    }
}

pub fn capability_unavailable_rpc_error(method_id: &str) -> DispatchError {
    let err = ContractError::new(
        CODE_CAPABILITY_UNAVAILABLE,
        format!("capability unavailable in current stage: `{method_id}`"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    );
    DispatchError::Rpc {
        code: ERR_CAPABILITY_UNAVAILABLE,
        message: err.message.clone(),
        data: Some(serde_json::to_value(err).unwrap_or(Value::Null)),
    }
}
