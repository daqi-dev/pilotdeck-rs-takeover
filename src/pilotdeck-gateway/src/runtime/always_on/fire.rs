//! Port of TS `src/always-on/runtime/DiscoveryFire.ts` — the five session-key
//! derivations, the `wx` discovery lock (`acquireDiscoveryLock` /
//! `releaseDiscoveryLock`), the shared turn plumbing (`emitEvent`,
//! `drainTurn`, the per-phase session override) and the three entry points.
//!
//! `runApplyPhase` is the full baseline phase. `run` and `rerunPlan` keep
//! the fail-closed seam past their real pre-checks: `run` answers an
//! `internal` AlwaysOnError ("not implemented in this embedding"), and
//! `rerunPlan` keeps `plan_not_found` / `plan_body_missing`
//! (DiscoveryFire.ts:339-361) and the `updateStatus({status: "ready"})`
//! write. A workspace-less cycle answers the standalone-fallback envelope
//! (`missing_workspace`, createApplyHandler.ts:93) instead of the TS runtime
//! path's TypeError — reading `.strategy` of `undefined` is a JS engine
//! artifact.
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
};

use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};

use crate::{
    config::{
        session_overrides::{SessionConfigOverrides, UNATTENDED_SESSION_EXCLUDED_TOOLS},
        session_value::SessionValue,
    },
    runtime::cron::{iso_from_epoch_ms, stringify_pretty},
};

use super::{
    AlwaysOnError, AlwaysOnPaths, DiscoveryPlanRecord, DiscoveryPlanStore, DiscoveryStateStore,
    WorkCycleRecord, WorkCycleStore,
    apply::{ApplyPromptInput, build_apply_prompt, generate_workspace_diff},
    manager::{AlwaysOnAgentGateway, AlwaysOnRunContextRegistry, ControlError},
    paths::run_events_path,
    phases::{HistoryBase, TailPlan},
    workspace::WorkspaceRegistry,
};

/// `onTurnEvent(sessionKey, channelKey, event)` (DiscoveryFire.ts:68): the
/// host broadcasts every always-on turn event as `always-on:turn-event`.
pub(crate) type TurnEventHandler = Arc<dyn Fn(&JsString, &JsString, &JsValue) + Send + Sync>;

/// `DiscoveryFire.deriveDiscoverySessionKey` (DiscoveryFire.ts:244-246) and
/// its four siblings: `always-on/<phase>:project=<key>:run=<runId>`.
pub(crate) fn derive_discovery_session_key(project_key: &str, run_id: &str) -> String {
    format!("always-on/discovery:project={project_key}:run={run_id}")
}

pub(crate) fn derive_workspace_session_key(project_key: &str, run_id: &str) -> String {
    format!("always-on/workspace:project={project_key}:run={run_id}")
}

pub(crate) fn derive_execution_session_key(project_key: &str, run_id: &str) -> String {
    format!("always-on/execute:project={project_key}:run={run_id}")
}

pub(crate) fn derive_report_session_key(project_key: &str, run_id: &str) -> String {
    format!("always-on/report:project={project_key}:run={run_id}")
}

pub(crate) fn derive_apply_session_key(project_key: &str, run_id: &str) -> String {
    format!("always-on/apply:project={project_key}:run={run_id}")
}

/// The channel keys of the five phases (DiscoveryFire.ts:78-82).
const APPLY_CHANNEL: &str = "always-on/apply";

/// `acquireDiscoveryLock` (DiscoveryFire.ts:1145-1159): `mkdir -p`, then a
/// `wx` (fail-if-exists) write of the pretty-printed payload — key order
/// `pid`, `runId`, `startedAt`, two-space indent, no trailing newline
/// (golden `l1`). EEXIST reads as `false`; other errors propagate.
pub(crate) fn acquire_discovery_lock(
    paths: &AlwaysOnPaths,
    pid: u32,
    run_id: &str,
    started_at: &str,
) -> io::Result<bool> {
    if let Some(parent) = paths.discovery_lock_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut payload = JsObject::new();
    payload.insert("pid".into(), JsValue::Number(f64::from(pid)));
    payload.insert("runId".into(), JsValue::String(JsString::from(run_id)));
    payload.insert(
        "startedAt".into(),
        JsValue::String(JsString::from(started_at)),
    );
    let bytes = stringify_pretty(&JsValue::Object(payload));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&paths.discovery_lock_file)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(bytes.as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// `releaseDiscoveryLock` (DiscoveryFire.ts:1161-1163): the unlink swallows
/// every error, a missing lock included.
pub(crate) fn release_discovery_lock(paths: &AlwaysOnPaths) {
    let _ = std::fs::remove_file(&paths.discovery_lock_file);
}

/// The `DiscoveryFireResult` slice the scheduler reads: `outcome`, which
/// re-arms the dormancy watcher when the run produced no plan
/// (DiscoveryScheduler.ts:154-156).
#[derive(Debug)]
pub(crate) struct FireOutcome {
    pub outcome: String,
}

/// `run` input — the scheduler's `{ runId, startedAt }` pair.
pub(crate) struct FireRunInput<'a> {
    pub run_id: &'a str,
    pub started_at_ms: f64,
}

/// `rerunPlan` input (DiscoveryFire.ts:332-336).
pub(crate) struct RerunFireInput<'a> {
    pub plan_id: &'a str,
    pub run_id: &'a str,
    pub started_at_ms: f64,
}

/// `runApplyPhase` input (DiscoveryFire.ts:264-270): the cycle, its
/// `{ id, title }` plan projection and the apply context.
pub(crate) struct ApplyPhaseInput<'a> {
    pub run_id: &'a str,
    pub cycle: &'a WorkCycleRecord,
    pub plans: &'a [DiscoveryPlanRecord],
    /// `String(input.projectName)` — `None` is `undefined`.
    pub project_name: Option<&'a str>,
    pub project_root: &'a str,
}

/// `rerunPlan`'s resolved result (the runtime echoes `runId` and `error`).
pub(crate) struct RerunFireResult {
    pub run_id: String,
    pub error: Option<ControlError>,
}

/// `runApplyPhase`'s resolved result.
pub(crate) struct ApplyPhaseResult {
    pub session_key: String,
    pub error: Option<ControlError>,
}

/// The fire seam as the scheduler and the control surface see it. The
/// futures are boxed so a tick can hold one across an await — which is what
/// makes `stop` wait for an in-flight fire instead of aborting it — and so
/// test fakes can block on their own schedule.
pub(crate) trait DiscoveryFirePort: Send + Sync {
    fn run<'a>(
        &'a self,
        input: FireRunInput<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<FireOutcome, AlwaysOnError>> + Send + 'a>>;

    fn rerun_plan<'a>(
        &'a self,
        input: RerunFireInput<'a>,
    ) -> Pin<Box<dyn Future<Output = io::Result<RerunFireResult>> + Send + 'a>>;

    fn run_apply_phase<'a>(
        &'a self,
        input: ApplyPhaseInput<'a>,
    ) -> Pin<Box<dyn Future<Output = io::Result<ApplyPhaseResult>> + Send + 'a>>;
}

/// `DiscoveryFireDependencies` (DiscoveryFire.ts:48-70) as the ported phases
/// read them.
pub(crate) struct FireDeps {
    /// `config.language` — the prompt language.
    pub language: Option<String>,
    pub paths: AlwaysOnPaths,
    pub project_key: String,
    pub gateway: Arc<dyn AlwaysOnAgentGateway>,
    pub session_overrides: Arc<Mutex<SessionConfigOverrides>>,
    pub plan_store: Arc<DiscoveryPlanStore>,
    pub cycle_store: Arc<WorkCycleStore>,
    pub state_store: Arc<DiscoveryStateStore>,
    pub run_contexts: Arc<AlwaysOnRunContextRegistry>,
    pub workspace_registry: WorkspaceRegistry,
    pub uuid: Arc<dyn Fn() -> String + Send + Sync>,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub on_turn_event: Option<TurnEventHandler>,
}

pub(crate) struct DiscoveryFire {
    pub(super) deps: FireDeps,
}

/// `JSON.stringify(value) + "\n"` appended to `file` after `mkdir -p` of its
/// directory (AlwaysOnEventStore.ts:9-12, DiscoveryReportStore.ts:16-25).
pub(crate) fn append_json_line(file: &std::path::Path, value: &JsValue) -> io::Result<()> {
    use std::io::Write;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = stringify_json(value)
        .map_err(|error| io::Error::other(error.to_string()))?
        .unwrap_or_default();
    let mut handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    handle.write_all(format!("{line}\n").as_bytes())
}

/// `pickFirstError` (DiscoveryFire.ts:1165-1172): the first `error` event's
/// `{ code, message }`.
pub(crate) fn pick_first_error(events: &[JsValue]) -> Option<(Option<JsValue>, JsValue)> {
    events.iter().find_map(|event| {
        let is_error = event
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|kind| kind.equals_str("error"));
        is_error.then(|| {
            (
                event.get("code").cloned(),
                event.get("message").cloned().unwrap_or(JsValue::Undefined),
            )
        })
    })
}

/// JS `String(value)`.
pub(crate) fn js_display(value: &JsValue) -> String {
    String::from_utf16_lossy(&crate::js_value::to_string_units(value).unwrap_or_default())
}

/// `{ code: error.code ?? fallback, message: error.message }`.
pub(crate) fn control_error(error: &(Option<JsValue>, JsValue), fallback: &str) -> ControlError {
    let code = match &error.0 {
        None | Some(JsValue::Undefined) | Some(JsValue::Null) => fallback.to_string(),
        Some(code) => js_display(code),
    };
    ControlError {
        code,
        message: js_display(&error.1),
    }
}

pub(crate) fn error_value(error: &ControlError) -> JsValue {
    let mut out = JsObject::new();
    out.insert("code".into(), JsValue::from(error.code.as_str()));
    out.insert("message".into(), JsValue::from(error.message.as_str()));
    JsValue::Object(out)
}

/// The per-phase `sessionOverrides.set` (DiscoveryFire.ts:281-287 and its
/// siblings): the phase's cwd, `bypassPermissions`, no prompting and the
/// unattended tool exclusions.
pub(crate) fn unattended_override(cwd: &str) -> SessionValue {
    let value = SessionValue::object();
    value.set("cwd", JsValue::from(cwd));
    value.set("permissionMode", JsValue::from("bypassPermissions"));
    value.set("bypassAvailable", JsValue::Bool(true));
    value.set("canPrompt", JsValue::Bool(false));
    value.set(
        "excludeTools",
        SessionValue::array(
            UNATTENDED_SESSION_EXCLUDED_TOOLS
                .iter()
                .map(|tool| SessionValue::from(JsValue::from(*tool)))
                .collect(),
        ),
    );
    value
}

impl DiscoveryFire {
    pub(crate) fn new(deps: FireDeps) -> Self {
        Self { deps }
    }

    /// `emitEvent` (DiscoveryFire.ts:188-242): the phase event appended to
    /// `events.jsonl`; the extra fields follow the fixed head in the call
    /// site's key order. The append is fire-and-forget in TS — its failure
    /// is swallowed.
    pub(super) fn emit_event(&self, run_id: &str, phase: &str, extra: Vec<(&str, JsValue)>) {
        let mut event = JsObject::new();
        event.insert("schemaVersion".into(), JsValue::Number(1.0));
        event.insert("eventId".into(), JsValue::from((self.deps.uuid)().as_str()));
        event.insert("runId".into(), JsValue::from(run_id));
        event.insert(
            "projectKey".into(),
            JsValue::from(self.deps.project_key.as_str()),
        );
        event.insert("phase".into(), JsValue::from(phase));
        event.insert(
            "timestamp".into(),
            JsValue::from(iso_from_epoch_ms((self.deps.now)()).as_str()),
        );
        for (key, value) in extra {
            event.insert(key.into(), value);
        }
        let _ = append_json_line(&self.deps.paths.events_file, &JsValue::Object(event));
    }

    pub(super) fn set_override(&self, session_key: &str, value: &SessionValue) {
        self.deps
            .session_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set(&JsString::from(session_key), value);
    }

    /// The `finally` of every phase: drop the override, then
    /// `closeSession({ reason: "always-on/done" }).catch(() => undefined)`.
    pub(super) async fn release_session(&self, session_key: &str) {
        self.deps
            .session_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .delete(&JsString::from(session_key));
        self.deps.gateway.close_session(session_key).await;
    }

    /// `drainTurn` (DiscoveryFire.ts:1063-1097): submit, forward every event
    /// to `onTurnEvent`, and with `persist_events` append it to
    /// `runs/<runId>.events.jsonl` (append failures swallowed). A stream
    /// failure is the iterator's throw.
    pub(super) async fn drain_turn(
        &self,
        session_key: &str,
        channel_key: &str,
        run_id: &str,
        message: JsString,
        persist_events: bool,
    ) -> io::Result<Vec<JsValue>> {
        let mut telemetry = JsObject::new();
        telemetry.insert("ownerModule".into(), JsValue::from("always_on"));
        telemetry.insert("executionKind".into(), JsValue::from("always_on"));
        telemetry.insert(
            "phase".into(),
            JsValue::from(
                channel_key
                    .strip_prefix("always-on/")
                    .unwrap_or(channel_key),
            ),
        );
        let mut input = JsObject::new();
        input.insert("sessionKey".into(), JsValue::from(session_key));
        input.insert("channelKey".into(), JsValue::from(channel_key));
        input.insert("message".into(), JsValue::String(message));
        input.insert("mode".into(), JsValue::from("bypassPermissions"));
        input.insert("runId".into(), JsValue::from(run_id));
        input.insert(
            "projectKey".into(),
            JsValue::from(self.deps.project_key.as_str()),
        );
        input.insert("telemetry".into(), JsValue::Object(telemetry));
        let mut stream = self.deps.gateway.submit_turn(JsValue::Object(input));
        let session = JsString::from(session_key);
        let channel = JsString::from(channel_key);
        let events_file = run_events_path(&self.deps.paths, &JsString::from(run_id));
        let mut events = Vec::new();
        while let Some(event) = stream.next_event().await.map_err(io::Error::other)? {
            if let Some(forward) = &self.deps.on_turn_event {
                forward(&session, &channel, &event);
            }
            if persist_events {
                let _ = append_json_line(&events_file, &event);
            }
            events.push(event);
        }
        Ok(events)
    }

    /// `runApplyPhase` (DiscoveryFire.ts:264-330).
    async fn apply_phase(&self, input: ApplyPhaseInput<'_>) -> io::Result<ApplyPhaseResult> {
        let Some(workspace) = &input.cycle.workspace else {
            return Ok(ApplyPhaseResult {
                session_key: String::new(),
                error: Some(ControlError {
                    code: "missing_workspace".into(),
                    message: "Cycle has no associated workspace to apply".into(),
                }),
            });
        };
        let diff =
            generate_workspace_diff(&workspace.strategy, &workspace.cwd, input.project_root).await;
        let session_key = derive_apply_session_key(&self.deps.project_key, input.run_id);
        self.emit_event(
            input.run_id,
            "apply_started",
            vec![("outcome", JsValue::from("executed"))],
        );
        self.set_override(&session_key, &unattended_override(input.project_root));
        // `plans.map((p) => p.title).join("; ")` — a missing title joins as "".
        let title = input
            .plans
            .iter()
            .map(|plan| plan.title.clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("; ");
        let branch_name = workspace
            .metadata
            .get("branchName")
            .and_then(JsValue::as_string)
            .map(|branch| String::from_utf16_lossy(branch.units()));
        let message = build_apply_prompt(&ApplyPromptInput {
            plan_title: &title,
            plan_id: &input.cycle.id,
            workspace_cwd: &workspace.cwd,
            workspace_strategy: &workspace.strategy,
            project_name: input.project_name.unwrap_or("undefined"),
            project_root: input.project_root,
            diff: &diff,
            branch_name: branch_name.as_deref(),
            language: self.deps.language.as_deref(),
        });
        let drained = self
            .drain_turn(
                &session_key,
                APPLY_CHANNEL,
                &format!("{}.apply", input.run_id),
                message,
                true,
            )
            .await;
        let result = drained.map(|events| {
            let error =
                pick_first_error(&events).map(|error| control_error(&error, "apply_failed"));
            match &error {
                Some(error) => self.emit_event(
                    input.run_id,
                    "run_failed",
                    vec![
                        ("outcome", JsValue::from("failed")),
                        ("error", error_value(error)),
                    ],
                ),
                None => self.emit_event(
                    input.run_id,
                    "apply_completed",
                    vec![("outcome", JsValue::from("executed"))],
                ),
            }
            ApplyPhaseResult {
                session_key: session_key.clone(),
                error,
            }
        });
        self.release_session(&session_key).await;
        result
    }
}

impl DiscoveryFirePort for DiscoveryFire {
    fn run<'a>(
        &'a self,
        input: FireRunInput<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<FireOutcome, AlwaysOnError>> + Send + 'a>> {
        Box::pin(async move {
            let outcome = self
                .discovery_run(input.run_id, input.started_at_ms)
                .await?;
            Ok(FireOutcome {
                outcome: outcome.into(),
            })
        })
    }

    fn rerun_plan<'a>(
        &'a self,
        input: RerunFireInput<'a>,
    ) -> Pin<Box<dyn Future<Output = io::Result<RerunFireResult>> + Send + 'a>> {
        Box::pin(async move {
            let plan_store = &self.deps.plan_store;
            // DiscoveryFire.ts:339-349.
            if plan_store.get_record(input.plan_id)?.is_none() {
                return Ok(RerunFireResult {
                    run_id: input.run_id.into(),
                    error: Some(ControlError {
                        code: "plan_not_found".into(),
                        message: format!("Plan {} not found", input.plan_id),
                    }),
                });
            }
            // DiscoveryFire.ts:351-361.
            if plan_store.read_plan_markdown(input.plan_id)?.is_none() {
                return Ok(RerunFireResult {
                    run_id: input.run_id.into(),
                    error: Some(ControlError {
                        code: "plan_body_missing".into(),
                        message: format!("Plan markdown for {} not found on disk", input.plan_id),
                    }),
                });
            }
            // DiscoveryFire.ts:363.
            plan_store.update_status(input.plan_id, "ready")?;
            let record = plan_store
                .get_record(input.plan_id)?
                .ok_or_else(|| io::Error::other("plan vanished"))?;
            let markdown = plan_store
                .read_plan_markdown(input.plan_id)?
                .unwrap_or_default();
            let state = self.deps.state_store.read(input.started_at_ms)?;
            let tail = self
                .plan_tail(
                    input.run_id,
                    input.started_at_ms,
                    &state,
                    &TailPlan { record, markdown },
                    &HistoryBase {
                        run_id: input.run_id.into(),
                        started_at: iso_from_epoch_ms(input.started_at_ms),
                        plan_first: true,
                    },
                )
                .await?;
            Ok(RerunFireResult {
                run_id: input.run_id.into(),
                error: tail.error,
            })
        })
    }

    fn run_apply_phase<'a>(
        &'a self,
        input: ApplyPhaseInput<'a>,
    ) -> Pin<Box<dyn Future<Output = io::Result<ApplyPhaseResult>> + Send + 'a>> {
        Box::pin(self.apply_phase(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negative_assert_eq;
    use std::path::{Path, PathBuf};

    const GOLDEN: &str = include_str!("../../../tests/fixtures/always-on-2294/oracle.json");

    fn golden(id: &str) -> serde_json::Value {
        let records: Vec<serde_json::Value> =
            serde_json::from_str(GOLDEN).expect("#2294 golden parses");
        records
            .iter()
            .find(|record| record["id"].as_str() == Some(id))
            .map(|record| record["value"].clone())
            .unwrap_or_else(|| panic!("#2294 golden record {id} missing"))
    }

    fn scratch(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("always-on-2294-fire-{tag}"));
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    fn paths(home: &Path) -> AlwaysOnPaths {
        AlwaysOnPaths {
            project_key: "/proj/a".into(),
            state_file: home.join("state.json"),
            plans_dir: home.join("plans"),
            plan_index_file: home.join("plans").join("index.json"),
            cycle_index_file: home.join("cycles").join("index.json"),
            reports_dir: home.join("reports"),
            discovery_lock_file: home.join("locks").join("discovery.lock"),
            ..Default::default()
        }
    }

    // Golden `l1`: the wx lock writes exactly these bytes, a second acquire
    // while held fails, and release is an unlink that tolerates repeats.
    #[test]
    fn replays_the_lock_golden() {
        let home = scratch("l1");
        let paths = paths(&home);
        let first = acquire_discovery_lock(&paths, 4242, "lock-run-1", "2026-01-01T00:05:00.000Z")
            .expect("#2294 first acquire");
        let bytes = std::fs::read_to_string(&paths.discovery_lock_file).expect("#2294 read");
        let second = acquire_discovery_lock(&paths, 99, "other", "x").expect("#2294 second");
        let bytes_after =
            std::fs::read_to_string(&paths.discovery_lock_file).expect("#2294 read after");
        release_discovery_lock(&paths);
        let after_release = paths.discovery_lock_file.exists();
        release_discovery_lock(&paths);
        negative_assert_eq!(
            serde_json::json!({
                "first": first,
                "bytes": bytes,
                "second": second,
                "bytesAfter": bytes_after,
                "afterRelease": after_release,
                "doubleReleaseOk": true,
            }),
            golden("l1-acquire-release"),
            "#2294 the lock contract matches the TS baseline"
        );
    }

    // The five derivations (DiscoveryFire.ts:244-262), pinned against the
    // session keys the r1 golden carries.
    #[test]
    fn derives_the_five_session_keys() {
        negative_assert_eq!(
            derive_discovery_session_key("x", "y"),
            "always-on/discovery:project=x:run=y",
            "#2294 discovery key"
        );
        negative_assert_eq!(
            derive_workspace_session_key("x", "y"),
            "always-on/workspace:project=x:run=y",
            "#2294 workspace key"
        );
        negative_assert_eq!(
            derive_execution_session_key("x", "y"),
            "always-on/execute:project=x:run=y",
            "#2294 execution key"
        );
        negative_assert_eq!(
            derive_report_session_key("x", "y"),
            "always-on/report:project=x:run=y",
            "#2294 report key"
        );
        negative_assert_eq!(
            derive_apply_session_key("x", "y"),
            "always-on/apply:project=x:run=y",
            "#2294 apply key"
        );
    }

    fn plan_fixture(home: &Path) {
        let index = home.join("plans").join("index.json");
        std::fs::create_dir_all(index.parent().expect("#2294 parent")).expect("#2294 mkdir");
        std::fs::write(
            &index,
            "{\n  \"schemaVersion\": 1,\n  \"plans\": [\n    {\n      \"id\": \"p1\",\n      \
             \"title\": \"T\",\n      \"createdAt\": \"2026-01-01T00:00:00.000Z\",\n      \
             \"status\": \"ready\",\n      \"summary\": \"s\",\n      \"rationale\": \"r\",\n      \
             \"dedupeKey\": \"d\",\n      \"sourceRunId\": \"r0\",\n      \"planFilePath\": \
             \"plans/p1.md\"\n    }\n  ]\n}",
        )
        .expect("#2294 plan index");
    }

    fn as_error_json(error: Option<super::super::manager::ControlError>) -> serde_json::Value {
        match error {
            Some(error) => serde_json::json!({"code": error.code, "message": error.message}),
            None => serde_json::Value::Null,
        }
    }

    // The rerun seam keeps its two real checks byte-identical to the r1
    // golden records and fail-closes past them.
    #[tokio::test]
    async fn rerun_seam_keeps_the_real_checks_then_fail_closes() {
        let home = scratch("r1-rerun");
        let paths = paths(&home);
        plan_fixture(&home);
        let fire = fire(paths.clone());

        // r1 `rerunMissing`: plan_not_found with the runId echo.
        let missing = fire
            .rerun_plan(RerunFireInput {
                plan_id: "p-missing",
                run_id: "probe-run-7",
                started_at_ms: 0.0,
            })
            .await
            .expect("#2294 rerun");
        negative_assert_eq!(
            serde_json::json!({"runId": missing.run_id, "error": as_error_json(missing.error)}),
            golden("r1-control-envelopes")["rerunMissing"],
            "#2294 plan_not_found envelope"
        );

        // r1 `rerunNoBody`: the record exists, the markdown does not.
        let no_body = fire
            .rerun_plan(RerunFireInput {
                plan_id: "p1",
                run_id: "probe-run-8",
                started_at_ms: 0.0,
            })
            .await
            .expect("#2294 rerun");
        negative_assert_eq!(
            serde_json::json!({"runId": no_body.run_id, "error": as_error_json(no_body.error)}),
            golden("r1-control-envelopes")["rerunNoBody"],
            "#2294 plan_body_missing envelope"
        );

        // Past the checks: the pre-seam `updateStatus({status: "ready"})`
        // write lands, then the seam fail-closes.
        std::fs::create_dir_all(&paths.plans_dir).expect("#2294 mkdir");
        std::fs::write(paths.plans_dir.join("p1.md"), "# plan").expect("#2294 markdown");
        let seam = fire
            .rerun_plan(RerunFireInput {
                plan_id: "p1",
                run_id: "probe-run-9",
                started_at_ms: 0.0,
            })
            .await
            .expect("#2294 rerun");
        negative_assert_eq!(
            as_error_json(seam.error),
            serde_json::json!({
                "code": "workspace_unavailable",
                "message": "no workspace provider applicable for /proj/a."
            }),
            "#2294 past the checks the workspace phase runs (no project dir: no provider)"
        );
        let index = std::fs::read_to_string(&paths.plan_index_file).expect("#2294 index");
        negative_assert_eq!(
            serde_json::from_str::<serde_json::Value>(&index).expect("#2294 parse")["plans"][0]["status"],
            "ready",
            "#2294 the pre-seam status write ran"
        );
    }

    /// A turn facade that records the submitted input and replays a fixed
    /// event list.
    struct ScriptedGateway {
        events: Vec<JsValue>,
        submitted: Mutex<Vec<JsValue>>,
        closed: Mutex<Vec<String>>,
    }
    struct ScriptedTurn(std::collections::VecDeque<JsValue>);
    impl super::super::AlwaysOnTurnStream for ScriptedTurn {
        fn next_event(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, String>> + Send + '_>> {
            let next = self.0.pop_front();
            Box::pin(async move { Ok(next) })
        }
    }
    impl AlwaysOnAgentGateway for ScriptedGateway {
        fn abort_turn<'a>(
            &'a self,
            _input: super::super::GatewayAbortInput<'a>,
        ) -> Pin<Box<dyn Future<Output = Result<(), JsString>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn submit_turn(&self, input: JsValue) -> Box<dyn super::super::AlwaysOnTurnStream> {
            self.submitted.lock().unwrap().push(input);
            Box::new(ScriptedTurn(self.events.clone().into()))
        }
        fn close_session<'a>(
            &'a self,
            session_key: &'a str,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            self.closed.lock().unwrap().push(session_key.to_string());
            Box::pin(async {})
        }
    }

    fn event(fields: &[(&str, &str)]) -> JsValue {
        let mut out = JsObject::new();
        for (key, value) in fields {
            out.insert((*key).into(), JsValue::from(*value));
        }
        JsValue::Object(out)
    }

    fn fire_with(paths: AlwaysOnPaths, gateway: Arc<dyn AlwaysOnAgentGateway>) -> DiscoveryFire {
        DiscoveryFire::new(FireDeps {
            language: None,
            plan_store: Arc::new(DiscoveryPlanStore::new(paths.clone())),
            cycle_store: Arc::new(WorkCycleStore::new(paths.clone())),
            state_store: Arc::new(DiscoveryStateStore::new(paths.clone())),
            run_contexts: Arc::new(AlwaysOnRunContextRegistry::new()),
            workspace_registry: WorkspaceRegistry {
                worktrees_dir: paths.worktrees_dir.clone(),
                snapshots_dir: paths.snapshots_dir.clone(),
                on_worktree_created: None,
            },
            paths,
            project_key: "/proj/a".into(),
            gateway,
            session_overrides: Arc::default(),
            uuid: Arc::new(|| "u".into()),
            now: Arc::new(|| 0.0),
            on_turn_event: None,
        })
    }

    fn fire(paths: AlwaysOnPaths) -> DiscoveryFire {
        fire_with(
            paths,
            Arc::new(ScriptedGateway {
                events: Vec::new(),
                submitted: Mutex::new(Vec::new()),
                closed: Mutex::new(Vec::new()),
            }),
        )
    }

    // runApplyPhase (DiscoveryFire.ts:264-330): a workspace-less cycle
    // answers the standalone-fallback `missing_workspace` envelope (the
    // documented deviation from the TS runtime path's TypeError); a
    // workspace-bearing cycle submits the apply turn and its first `error`
    // event becomes the result error and a `run_failed` phase event.
    #[tokio::test]
    async fn apply_phase_runs_the_turn_and_reports_the_first_error() {
        let home = scratch("r1-apply");
        let mut paths = paths(&home);
        paths.events_file = home.join("events.jsonl");
        paths.runs_dir = home.join("runs");
        let gateway = Arc::new(ScriptedGateway {
            events: vec![
                event(&[("type", "turn_started")]),
                event(&[("type", "error"), ("message", "boom")]),
                event(&[("type", "error"), ("code", "second"), ("message", "x")]),
            ],
            submitted: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        });
        let fire = fire_with(paths.clone(), gateway.clone());
        let workspaceless = WorkCycleRecord {
            id: "c1".into(),
            project_key: "/proj/a".into(),
            status: "active".into(),
            workspace: None,
            plan_ids: Vec::new(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            created_by_run_id: "r0".into(),
        };
        let result = fire
            .run_apply_phase(ApplyPhaseInput {
                run_id: "run-1",
                cycle: &workspaceless,
                plans: &[],
                project_name: Some("P"),
                project_root: "/proj/a",
            })
            .await
            .expect("#2294 apply");
        negative_assert_eq!(
            serde_json::json!({
                "sessionKey": result.session_key,
                "error": as_error_json(result.error),
            }),
            serde_json::json!({
                "sessionKey": "",
                "error": {
                    "code": "missing_workspace",
                    "message": "Cycle has no associated workspace to apply"
                },
            }),
            "#2294 the workspace-less apply envelope"
        );
        let workspace = super::super::cycles::WorkCycleWorkspace {
            strategy: "snapshot-copy".into(),
            cwd: home.join("missing-ws").to_string_lossy().into_owned(),
            metadata: JsObject::new(),
        };
        let cycle = WorkCycleRecord {
            workspace: Some(workspace),
            ..workspaceless
        };
        let result = fire
            .run_apply_phase(ApplyPhaseInput {
                run_id: "run-2",
                cycle: &cycle,
                plans: &[],
                project_name: None,
                project_root: "/proj/a",
            })
            .await
            .expect("#2294 apply");
        negative_assert_eq!(
            serde_json::json!({
                "sessionKey": result.session_key,
                "error": as_error_json(result.error),
            }),
            serde_json::json!({
                "sessionKey": "always-on/apply:project=/proj/a:run=run-2",
                "error": {"code": "apply_failed", "message": "boom"},
            }),
            "#2294 the first error event wins, code falling back to apply_failed"
        );
        let submitted = gateway.submitted.lock().unwrap().clone();
        let input = &submitted[0];
        negative_assert_eq!(
            input.get("runId").and_then(JsValue::as_string).cloned(),
            Some(JsString::from("run-2.apply")),
            "#2294 the apply turn's run id"
        );
        let message = input
            .get("message")
            .and_then(JsValue::as_string)
            .expect("message")
            .to_utf8()
            .expect("utf-8");
        assert!(message.starts_with("Always-On apply for project \"undefined\"."));
        negative_assert_eq!(
            gateway.closed.lock().unwrap().clone(),
            vec!["always-on/apply:project=/proj/a:run=run-2".to_string()],
            "#2294 the apply session is closed"
        );
        let events = std::fs::read_to_string(home.join("events.jsonl")).expect("events");
        let phases: Vec<String> = events
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("json")["phase"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        negative_assert_eq!(
            phases,
            vec!["apply_started".to_string(), "run_failed".to_string()],
            "#2294 the phase events"
        );
        let persisted = std::fs::read_to_string(home.join("runs").join("run-2.apply.events.jsonl"))
            .expect("run events");
        negative_assert_eq!(
            persisted.lines().count(),
            3,
            "#2294 every turn event persisted"
        );
    }

    // `run` (DiscoveryFire.ts:644-664): a discovery turn that saves no
    // plan is `no_plan` — the state goes dormant and the history records it.
    #[tokio::test]
    async fn run_without_a_plan_goes_dormant() {
        let home = scratch("run-no-plan");
        let mut paths = paths(&home);
        paths.events_file = home.join("events.jsonl");
        paths.run_history_file = home.join("run-history.jsonl");
        let fire = fire(paths.clone());
        let outcome = fire
            .run(FireRunInput {
                run_id: "run-1",
                started_at_ms: 0.0,
            })
            .await
            .expect("#2294 run");
        negative_assert_eq!(outcome.outcome, "no_plan".to_string(), "#2294 outcome");
        let state: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&paths.state_file).expect("#2294 state"),
        )
        .expect("#2294 state json");
        negative_assert_eq!(state["lastFireOutcome"], "no_plan", "#2294 state outcome");
        assert!(state["dormant"].is_object());
        let history = std::fs::read_to_string(&paths.run_history_file).expect("#2294 history");
        negative_assert_eq!(
            serde_json::from_str::<serde_json::Value>(history.trim()).expect("json")["outcome"],
            "no_plan",
            "#2294 history outcome"
        );
    }
}
