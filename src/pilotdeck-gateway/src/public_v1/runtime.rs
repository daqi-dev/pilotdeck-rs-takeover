//! `public_v1` operation dispatch and the S1+S2 runtime core. The 28
//! operations' handlers live in `domains/` (one file per domain, registered
//! through [`crate::public_v1::domains::DomainRegistry`]); the offline run
//! driver lives in `run_driver.rs` and is re-exported below.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::runtime::turn::submit::TurnSubmitter;
use agentcpm_pd_protocol::contract::{OPERATION_VERSION, ResponseOutcome, RunState};
use agentcpm_pd_protocol::envelope::bind_instance_id;
use agentcpm_pd_protocol::errors::{
    CODE_CAPABILITY_UNAVAILABLE, CODE_UNAVAILABLE, CODE_UNSUPPORTED_OPERATION, ContractError,
    ERR_CAPABILITY_UNAVAILABLE, ERR_MISSING_IDEMPOTENCY_KEY, EffectStatus,
    JSONRPC_METHOD_NOT_FOUND, RetrySpec,
};
use agentcpm_pd_protocol::registry::{
    ExecutionMode, OperationRegistry, RetryKind, is_implemented,
    validation_errors_to_contract_error,
};
use agentcpm_pd_protocol::wire::MAX_FRAME_BYTES;
use futures_util::FutureExt;
use pilotdeck_protocol::JsValue;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::run_driver::{
    cancel_run_now, extract_prompt_text, fail_run_now, fail_run_now_async, succeed_run_now,
};
use crate::public_v1::store::{RunTransition, StoredRun, V1Store};

pub use crate::public_v1::run_driver::{
    DriverFuture, OfflineRunDriver, RunDriver, RunDriverContext,
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

    fn clear_interaction_waiter(&self, interaction_id: &str) {
        self.waiters
            .lock()
            .expect("interaction waiters")
            .remove(interaction_id);
    }
}

/// The next queued run of an unpaused session, skipping the run this worker
/// just failed to claim (another worker owns it).
fn next_queued_run_except(
    rt: &Arc<PublicV1Runtime>,
    session_id: &str,
    skip: &str,
) -> Option<String> {
    match rt.store.get_queue(session_id, None) {
        Ok(q) if q["queue"]["paused"].as_bool() == Some(false) => q["queue"]["items"]
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|item| item["runId"].as_str() != Some(skip))
                    .and_then(|item| item["runId"].as_str())
            })
            .map(ToOwned::to_owned),
        _ => None,
    }
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

const AUTH_CACHE_MAX_ENTRIES: usize = 256;

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
    draining: AtomicBool,
    auth_revocation: watch::Sender<u64>,
    auth_cache_epoch: AtomicU64,
    auth_cache: Mutex<(u64, HashMap<String, bool>)>,
    terminals: crate::public_v1::domains::terminals_pty::TerminalRegistry,
    #[cfg(test)]
    forced_unimplemented: HashSet<&'static str>,
}

impl Drop for PublicV1Runtime {
    fn drop(&mut self) {
        self.terminals.close_all_live_terminals(false);
    }
}

struct ClaimedRunGuard {
    rt: Arc<PublicV1Runtime>,
    run_id: String,
    session_id: String,
}

impl Drop for ClaimedRunGuard {
    fn drop(&mut self) {
        self.rt.waiters.clear_run_cancel(&self.run_id);
    }
}

impl PublicV1Runtime {
    pub fn new(store: Arc<V1Store>) -> Arc<Self> {
        Self::with_driver(store, Arc::new(OfflineRunDriver))
    }

    pub fn with_driver(store: Arc<V1Store>, driver: Arc<dyn RunDriver>) -> Arc<Self> {
        // S3 substrate: constructing a runtime builds the domain table once
        // per process, which marks every registered operation as implemented
        // (the availability set `dispatch` and `sub.open` admission read).
        let _ = DomainRegistry::global();
        let (auth_revocation, _) = watch::channel(0u64);
        Arc::new(Self {
            store,
            driver,
            waiters: InteractionWaiterRegistry::default(),
            submitter: None,
            draining: AtomicBool::new(false),
            auth_revocation,
            auth_cache_epoch: AtomicU64::new(0),
            auth_cache: Mutex::new((0, HashMap::new())),
            terminals: crate::public_v1::domains::terminals_pty::TerminalRegistry::default(),
            #[cfg(test)]
            forced_unimplemented: HashSet::new(),
        })
    }

    /// D20 wiring: submit runs through the real turn machine (`submit_owned`),
    /// keeping `driver` as the no-key fallback/test adapter.
    pub fn with_submitter(
        store: Arc<V1Store>,
        driver: Arc<dyn RunDriver>,
        submitter: Arc<TurnSubmitter>,
    ) -> Arc<Self> {
        let _ = DomainRegistry::global();
        let (auth_revocation, _) = watch::channel(0u64);
        Arc::new(Self {
            store,
            driver,
            waiters: InteractionWaiterRegistry::default(),
            submitter: Some(submitter),
            draining: AtomicBool::new(false),
            auth_revocation,
            auth_cache_epoch: AtomicU64::new(0),
            auth_cache: Mutex::new((0, HashMap::new())),
            terminals: crate::public_v1::domains::terminals_pty::TerminalRegistry::default(),
            #[cfg(test)]
            forced_unimplemented: HashSet::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_forced_unimplemented(
        store: Arc<V1Store>,
        forced: Vec<&'static str>,
    ) -> Arc<Self> {
        let _ = DomainRegistry::global();
        let (auth_revocation, _) = watch::channel(0u64);
        Arc::new(Self {
            store,
            driver: Arc::new(OfflineRunDriver),
            waiters: InteractionWaiterRegistry::default(),
            submitter: None,
            draining: AtomicBool::new(false),
            auth_revocation,
            auth_cache_epoch: AtomicU64::new(0),
            auth_cache: Mutex::new((0, HashMap::new())),
            terminals: crate::public_v1::domains::terminals_pty::TerminalRegistry::default(),
            forced_unimplemented: forced.into_iter().collect(),
        })
    }

    fn is_operation_implemented(&self, method_id: &str) -> bool {
        #[cfg(test)]
        if self.forced_unimplemented.contains(method_id) {
            return false;
        }
        is_implemented(method_id)
    }

    pub fn store(&self) -> &Arc<V1Store> {
        &self.store
    }

    pub(crate) fn terminal_registry(
        &self,
    ) -> &crate::public_v1::domains::terminals_pty::TerminalRegistry {
        &self.terminals
    }

    pub(crate) fn enter_draining(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    pub(crate) fn subscribe_auth_revocation(&self) -> watch::Receiver<u64> {
        self.auth_revocation.subscribe()
    }

    pub(crate) fn bump_auth_epoch(&self) {
        let next = self.auth_cache_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut guard) = self.auth_cache.lock() {
            guard.0 = next;
            guard.1.clear();
        }
    }

    pub(crate) fn notify_auth_revocation(&self) {
        self.bump_auth_epoch();
        let next = self.auth_revocation.borrow().wrapping_add(1);
        let _ = self.auth_revocation.send(next);
    }

    pub(crate) fn cached_auth_decision(&self, digest: &str) -> (u64, Option<bool>) {
        let epoch = self.auth_cache_epoch.load(Ordering::SeqCst);
        let cached = self.auth_cache.lock().ok().and_then(|g| {
            if g.0 == epoch {
                g.1.get(digest).copied()
            } else {
                None
            }
        });
        (epoch, cached)
    }

    pub(crate) fn record_auth_decision(&self, epoch: u64, digest: String, valid: bool) {
        if self.auth_cache_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        if let Ok(mut guard) = self.auth_cache.lock() {
            if self.auth_cache_epoch.load(Ordering::SeqCst) != epoch {
                return;
            }
            if guard.0 != epoch {
                guard.0 = epoch;
                guard.1.clear();
            }
            if guard.1.len() >= AUTH_CACHE_MAX_ENTRIES {
                guard.1.clear();
            }
            guard.1.insert(digest, valid);
        }
    }

    pub(crate) async fn cancel_all_for_stop(self: &Arc<Self>) {
        self.enter_draining();
        self.terminals.close_all_live_terminals(false);
        let store = self.store.clone();
        let run_ids = tokio::task::spawn_blocking(move || {
            let _ = store.cancel_non_terminal_operations_for_stop();
            store.list_non_terminal_run_ids()
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        for rid in run_ids {
            let store_c = self.store.clone();
            let rid_c = rid.clone();
            let outcome =
                tokio::task::spawn_blocking(move || store_c.cancel_run(&rid_c, None)).await;
            if let Ok(Ok(out)) = outcome
                && out.should_notify_worker
            {
                self.signal_run_cancel(&rid);
            }
            self.signal_run_cancel(&rid);
        }
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
                    current = next_queued_run_except(&rt, &session_id, &run_id);
                    continue;
                }
                // P1 (wp4 review): exactly one worker may drive a run. The
                // atomic claim fails for a run another worker already owns or
                // for one that a racing `run.cancel` terminalized or paused.
                if !rt.store.claim_run_execution(&run_id).unwrap_or(false) {
                    current = next_queued_run_except(&rt, &session_id, &run_id);
                    continue;
                }
                let guard = ClaimedRunGuard {
                    rt: rt.clone(),
                    run_id: run_id.clone(),
                    session_id: session_id.clone(),
                };
                let rt_exec = rt.clone();
                let run_id_exec = run_id.clone();
                let exec_res = AssertUnwindSafe(async move {
                    if let Some(submitter) = rt_exec.submitter.clone() {
                        rt_exec.drive_owned_turn(&submitter, run).await;
                    } else {
                        let cancel_rx = rt_exec.waiters.register_run_cancel(&run_id_exec);
                        if let Ok(now) = rt_exec.store.get_run(&run_id_exec)
                            && (now.terminal || now.state == RunState::Cancelling)
                        {
                            if !now.terminal {
                                cancel_run_now(
                                    &rt_exec.store,
                                    &run_id_exec,
                                    &json!({"runId": run_id_exec}),
                                );
                            }
                            return;
                        }
                        let ctx = RunDriverContext {
                            store: rt_exec.store.clone(),
                            run,
                            runtime: rt_exec.clone(),
                            cancel_rx,
                        };
                        rt_exec.driver.execute(ctx).await;

                        if let Ok(after) = rt_exec.store.get_run(&run_id_exec)
                            && !after.terminal
                        {
                            succeed_run_now(
                                &rt_exec.store,
                                &run_id_exec,
                                &json!({"runId": run_id_exec}),
                            );
                        }
                        if let Ok(after) = rt_exec.store.get_run(&run_id_exec) {
                            crate::public_v1::domains::snapshots::capture_run_boundary_logged(
                                &rt_exec.store,
                                &after,
                            );
                        }
                    }
                })
                .catch_unwind()
                .await;

                if exec_res.is_err() {
                    if let Ok(after) = rt.store.get_run(&guard.run_id)
                        && !after.terminal
                    {
                        fail_run_now_async(
                            &rt.store,
                            &guard.run_id,
                            "DRIVER_PANICKED",
                            &json!({"runId": guard.run_id, "errorCode": "DRIVER_PANICKED"}),
                        )
                        .await;
                    }
                    let _ = rt
                        .store
                        .clear_active_run_if_matching(&guard.session_id, &guard.run_id);
                }
                drop(guard);

                // Check if session queue is unpaused and still has pending queued runs.
                current = next_queued_run_except(&rt, &session_id, &run_id);
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
        let resolved_runtime_prefs = if let Ok(conn) = self.store.conn.lock() {
            let init_rev = &self.store.meta().initialization_revision;
            let cfg_map = crate::public_v1::domains::configuration::resolve_effective_map_in_tx(
                &conn,
                &run.project_id,
                &run.session_id,
            )
            .ok();
            let model = crate::public_v1::domains::models::resolve_effective_model_for_run_in_tx(
                &conn,
                init_rev,
                &run.project_id,
                &run.session_id,
            )
            .ok()
            .flatten();
            let ctx = crate::public_v1::domains::context::resolve_effective_context_for_run_in_tx(
                &conn,
                init_rev,
                &run.project_id,
                &run.session_id,
            )
            .ok();
            Some((cfg_map, model, ctx))
        } else {
            None
        };
        let mut turn_input = json!({
            "sessionKey": run.session_id,
            "projectKey": run.project_id,
            "runId": run.run_id,
            "channelKey": "agentcpm-pd",
            "message": extract_prompt_text(&run.input),
        });
        if let Some((cfg_map, model, ctx)) = resolved_runtime_prefs
            && let Some(obj) = turn_input.as_object_mut()
        {
            if let Some(map) = cfg_map
                && !map.is_empty()
            {
                obj.insert("effectiveConfig".to_owned(), json!(map));
            }
            if let Some((provider_id, model_id)) = model {
                obj.insert("providerId".to_owned(), json!(provider_id));
                obj.insert("modelId".to_owned(), json!(model_id));
            }
            if let Some(ctx_prefs) = ctx {
                if let Some(thinking) = ctx_prefs.thinking {
                    obj.insert("thinking".to_owned(), json!(thinking));
                }
                if let Some(style_id) = ctx_prefs.output_style_id {
                    obj.insert("outputStyleId".to_owned(), json!(style_id));
                }
            }
        }
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
        // P2 (wp4 review): a cancel that landed between the worker's claim and
        // this registration must not be lost — re-check the durable state
        // before submitting the owned turn.
        if let Ok(now) = self.store.get_run(&run_id)
            && (now.terminal || now.state == RunState::Cancelling)
        {
            if !now.terminal {
                cancel_run_now(&self.store, &run_id, &json!({"runId": run_id}));
            }
            self.waiters.clear_run_cancel(&run_id);
            return;
        }
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
                if let Ok(after) = self.store.get_run(&run_id)
                    && !after.terminal
                {
                    let payload = json!({
                        "runId": run_id,
                        "errorCode": error.code,
                        "message": error.message,
                    });
                    fail_run_now(&self.store, &run_id, &error.code, &payload);
                }
            }
            DriveStop::Terminal => {
                // The terminal transition is already durable; dropping the
                // submission lets the owned pump drain on its own completion
                // fence (submit.rs's documented "drain or drop" contract).
            }
        }
        // A2 (snap-wire §5): every stop branch (Terminal/Cancelled/Eof/Error)
        // ends with the run terminal here — aggregate its capture ledger
        // entries into one run-owned snapshot (failed/cancelled runs keep
        // their side effects too). Capture failure is absorbed into a
        // diagnostic progress event; it must not flip the terminal run.
        // (A1 attribution is explicit since dsh r3 P1#3 — the D20 turn
        // machine's bridge passes the run id at registration time.)
        crate::public_v1::domains::snapshots::capture_run_boundary_logged(&self.store, &run);
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
                if let Some(usage) = value.get("usage").or_else(|| result.get("usage"))
                    && let Some(obj) = usage.as_object()
                {
                    let finite_f64 = |keys: &[&str]| -> Option<f64> {
                        keys.iter()
                            .find_map(|k| obj.get(*k).and_then(Value::as_f64))
                            .filter(|v| v.is_finite() && *v >= 0.0)
                    };
                    if let (Some(input_tokens), Some(output_tokens)) = (
                        finite_f64(&["inputTokens", "input_tokens", "promptTokens"]),
                        finite_f64(&["outputTokens", "output_tokens", "completionTokens"]),
                    ) {
                        let cost = finite_f64(&["cost", "costUsd", "totalCost"]);
                        let model = value
                            .get("providerId")
                            .and_then(Value::as_str)
                            .zip(value.get("modelId").and_then(Value::as_str))
                            .map(|(p, m)| (p.to_owned(), m.to_owned()));
                        let _ = self.store.record_usage(
                            crate::public_v1::domains::usage::UsageRecordIn::actual_tokens(
                                crate::public_v1::store::now_rfc3339(),
                                crate::public_v1::domains::usage::UsageScope::Session {
                                    project_id: run.project_id.clone(),
                                    session_id: run.session_id.clone(),
                                },
                                model,
                                input_tokens,
                                output_tokens,
                                cost,
                                "USD",
                                "turn_completed",
                            ),
                        );
                    }
                }
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
        // S3 substrate: the reported set derives from what this build actually
        // registered (the domain table), not from the JSON `currentRound`.
        let domains = DomainRegistry::global();
        let meta = self.store.meta();
        let limits_pair = vec![json!({"key": "maxFrameBytes", "value": MAX_FRAME_BYTES})];
        let registered: Vec<&agentcpm_pd_protocol::registry::OperationSpec> = reg
            .operations()
            .iter()
            .filter(|op| domains.is_registered(&op.id))
            .collect();
        let mut caps = Vec::with_capacity(registered.len() + 2);
        for op in registered {
            if op.id == "context.compact" {
                caps.push(json!({
                    "methodId": op.id,
                    "available": false,
                    "reason": "IMPLEMENTATION_UNAVAILABLE",
                    "schemaVersion": OPERATION_VERSION,
                    "limits": limits_pair,
                    "supported": true,
                    "configured": false,
                    "authorized": Value::Null,
                    "requiredPermission": [],
                }));
                continue;
            }
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
        // Surface an explicit S3 stream entry (`terminals.output`) so callers
        // can inspect its status — but only while no domain has registered
        // it; once one does (t015b) the loop above already reports the real
        // availability, and this hardcoded fallback would duplicate the
        // methodId with a contradictory `available:false` (dsh boundary
        // review P2).
        if !domains.is_registered("terminals.output") {
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
        }

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

        // Availability gate (S3 substrate): "implemented" = a domain
        // registered the operation. The predicate replaces the JSON
        // `currentRound` check; the error and its position (before the
        // idempotency-key check and input validation) are unchanged.
        let domains = DomainRegistry::global();
        if !self.is_operation_implemented(method_id) {
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

        if self.is_draining()
            && matches!(
                method_id,
                "run.submit" | "commands.execute" | "terminals.create"
            )
        {
            return Err(DispatchError::Application(ContractError::new(
                CODE_UNAVAILABLE,
                "gateway is draining; refusing new submissions",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }

        // Domain dispatch table: the handler bodies live in `domains/`.
        let Some(handler) = domains.handler_for(method_id) else {
            // Unreachable today: every registered remote operation carries a
            // handler and CLI-local ops are refused above. Kept as the -32004
            // safety net (identical to the pre-substrate catch-all) so a
            // mis-registered domain fails loudly instead of succeeding.
            let err = ContractError::capability_unavailable(
                method_id,
                "not implemented in S1+S2 round (deferred to S3)",
            );
            return Err(DispatchError::Rpc {
                code: ERR_CAPABILITY_UNAVAILABLE,
                message: format!("capability unavailable in S1+S2: `{method_id}`"),
                data: Some(serde_json::to_value(err).unwrap_or(Value::Null)),
            });
        };
        handler(
            self.clone(),
            input,
            idempotency_key.map(ToOwned::to_owned),
            expected_revision.map(ToOwned::to_owned),
        )
        .await
    }

    pub fn notify_all_events(&self) -> Arc<tokio::sync::Notify> {
        self.store.event_notifier()
    }

    // -----------------------------------------------------------------
    // Waiter-registry accessors (S3 substrate): domain handlers and the
    // offline driver drive interactions/cancels through these instead of
    // reaching into the private registry.
    // -----------------------------------------------------------------

    pub(crate) fn register_interaction_waiter(
        &self,
        interaction_id: &str,
    ) -> watch::Receiver<Option<Value>> {
        self.waiters.register_interaction_waiter(interaction_id)
    }

    pub(crate) fn clear_interaction_waiter(&self, interaction_id: &str) {
        self.waiters.clear_interaction_waiter(interaction_id);
    }

    pub(crate) fn notify_interaction_resolved(&self, interaction_id: &str, answer: Value) {
        self.waiters
            .notify_interaction_resolved(interaction_id, answer);
    }

    pub(crate) fn signal_run_cancel(&self, run_id: &str) {
        self.waiters.signal_run_cancel(run_id);
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
