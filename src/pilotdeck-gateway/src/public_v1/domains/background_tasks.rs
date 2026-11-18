//! `background_tasks.*` — the v1 view over the ported background-task
//! runtime (`crate::runtime::background`, the TS-baseline port).
//!
//! Background tasks are session-owned tool subprocesses (bash tools running
//! detached), not runs: a `taskId` is never a `runId` and nothing here mixes
//! the two (capability a035 contract). `list`/`get` map the runtime's session
//! access (`TaskAccess`) onto the v1 `sessionId` scope; `stop` terminates the
//! owning process group (SIGTERM → grace → SIGKILL, `background.rs`) and is
//! idempotent — a repeated stop of a finished task returns its terminal
//! state.
//!
//! The runtime instance is a per-dataRoot singleton in this domain. The
//! production embedding (native-process assembly) owns one instance wired
//! into the tool chain; [`attach_background_runtime`] lets the integrator
//! inject that instance so v1 observations and tool writes share it.
//! Without an attachment a domain-local instance is constructed (disk spill
//! under `<home>/v1/background-tasks`), which is what the offline-driver
//! world (no tool chain) observes: an empty set.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, ContractError, EffectStatus, RetrySpec,
};
use pilotdeck_protocol::{JsString, JsValue};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::runtime::background::{
    BackgroundTaskRuntime, BackgroundTaskRuntimeOptions, TaskListFilter,
};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("background_tasks.list", |rt, input, _key, _rev| {
        Box::pin(background_tasks_list(rt, input))
    });
    reg.handler("background_tasks.get", |rt, input, _key, _rev| {
        Box::pin(background_tasks_get(rt, input))
    });
    reg.handler("background_tasks.stop", |rt, input, key, _rev| {
        Box::pin(background_tasks_stop(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// The per-dataRoot runtime singleton
// ---------------------------------------------------------------------------

pub(crate) struct BackgroundDomain {
    rt: Arc<PublicV1Runtime>,
    runtime: Arc<BackgroundTaskRuntime>,
    db: DomainDb,
}

fn domains_registry() -> &'static Mutex<HashMap<PathBuf, Weak<BackgroundDomain>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<BackgroundDomain>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Integrator hook: points the v1 `background_tasks.*` view of one dataRoot
/// at the assembly-owned runtime the tool chain writes into. Must be called
/// before the first `background_tasks.*` dispatch; after that the instance is
/// fixed and a late attach is a wiring-order error.
pub fn attach_background_runtime(
    rt: &Arc<PublicV1Runtime>,
    runtime: Arc<BackgroundTaskRuntime>,
) -> Result<(), ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    if registry.get(&home).and_then(Weak::upgrade).is_some() {
        // The domain already exists: nothing to attach (the instance was
        // fixed at first dispatch). This is a wiring-order error, reported
        // rather than silently ignored.
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "background runtime already serving this dataRoot; attach before the first background_tasks dispatch",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let db = DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?;
    let domain = Arc::new(BackgroundDomain {
        rt: rt.clone(),
        runtime,
        db,
    });
    registry.insert(home, Arc::downgrade(&domain));
    Ok(())
}

fn domain_for(rt: &Arc<PublicV1Runtime>) -> Result<Arc<BackgroundDomain>, ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let options = BackgroundTaskRuntimeOptions {
        disk_spill_dir: Some(home.join("v1").join("background-tasks")),
        ..BackgroundTaskRuntimeOptions::default()
    };
    let domain = Arc::new(BackgroundDomain {
        rt: rt.clone(),
        runtime: Arc::new(BackgroundTaskRuntime::new(options)),
        db: DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?,
    });
    registry.insert(home, Arc::downgrade(&domain));
    Ok(domain)
}

// ---------------------------------------------------------------------------
// Projection helpers
// ---------------------------------------------------------------------------

fn str_field(input: &Value, name: &str) -> Result<String, ContractError> {
    input
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("`{name}` is required"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })
}

fn js_text(value: &JsValue) -> String {
    match value {
        JsValue::String(text) => String::from_utf16_lossy(text.units()),
        _ => String::new(),
    }
}

/// `Some(None)` is a signal exit; both `None` and `Some(None)` project to a
/// JSON null per the contract (`exitCode` integer or null).
fn exit_code_json(exit: Option<Option<i32>>) -> Value {
    match exit {
        Some(Some(code)) => json!(code),
        _ => Value::Null,
    }
}

fn task_json(task: &crate::runtime::background::BackgroundBashTask) -> Value {
    json!({
        "taskId": String::from_utf16_lossy(task.task_id.units()),
        "kind": js_text(&task.kind),
        "ownerSessionId": js_text(&task.session_id),
        // The ported runtime records the model-side `agentId`, not the
        // owning run; v1 reports null instead of guessing a runId
        // ("不得把任务ID当runId" — and not the reverse either).
        "ownerRunId": Value::Null,
        "state": task.status.as_str().to_owned(),
        "startedAt": String::from_utf16_lossy(crate::runtime::background::iso(task.started_at).units()),
        "finishedAt": task
            .ended_at
            .map(|ended| String::from_utf16_lossy(crate::runtime::background::iso(ended).units())),
        "exitCode": exit_code_json(task.exit_code),
    })
}

/// The old-line runtime has no revision counter; the snapshot token is a
/// deterministic function of the visible set (count, latest start, terminal
/// count), so equal sets share a token and any change moves it.
fn snapshot_token(tasks: &[crate::runtime::background::BackgroundBashTask]) -> String {
    let latest = tasks
        .iter()
        .map(|task| task.started_at as u64)
        .max()
        .unwrap_or(0);
    let terminal = tasks
        .iter()
        .filter(|task| task.status.is_terminal())
        .count();
    format!("bt-{}-{}-{}", tasks.len(), latest, terminal)
}

fn parse_offset_cursor(cursor: Option<&str>, prefix: &str) -> Result<usize, ContractError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    cursor
        .strip_prefix(prefix)
        .and_then(|rest| rest.parse::<usize>().ok())
        .ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("malformed cursor `{cursor}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn background_tasks_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let session_id = str_field(&input, "sessionId")?;
    let cursor = parse_offset_cursor(input.get("cursor").and_then(Value::as_str), "bo")?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;

    let session = JsValue::String(JsString::from(session_id.as_str()));
    let status = match input.get("states").and_then(Value::as_array) {
        Some(states) if !states.is_empty() => {
            let items: Vec<JsValue> = states
                .iter()
                .map(|state| JsValue::String(JsString::from(state.as_str().unwrap_or_default())))
                .collect();
            JsValue::Array(items)
        }
        _ => JsValue::Undefined,
    };
    let filter = TaskListFilter {
        agent_id: JsValue::Undefined,
        status,
        kind: JsValue::Undefined,
    };
    let tasks = domain.runtime.list(&filter, Some(&session));
    let snapshot_revision = snapshot_token(&tasks);
    let mut items = Vec::new();
    let mut next_cursor = None;
    for (index, task) in tasks.iter().enumerate().skip(cursor) {
        if items.len() >= limit {
            next_cursor = Some(format!("bo-{index}"));
            break;
        }
        items.push(task_json(task));
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({"items": items, "nextCursor": next_cursor, "snapshotRevision": snapshot_revision}),
    ))
}

async fn background_tasks_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let session_id = str_field(&input, "sessionId")?;
    let task_id = str_field(&input, "taskId")?;
    let max_bytes = input
        .get("maxBytes")
        .and_then(Value::as_u64)
        .unwrap_or(65_536)
        .max(1);
    let offset = match input.get("outputCursor").and_then(Value::as_str) {
        Some(cursor) => cursor
            .parse::<f64>()
            .map_err(|_| {
                ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!("malformed outputCursor `{cursor}`"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?
            .max(0.0),
        None => 0.0,
    };
    if !offset.is_finite() {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("malformed outputCursor `{offset}`"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    let session = JsValue::String(JsString::from(session_id.as_str()));
    let task_key = JsString::from(task_id.as_str());
    let Some(task) = domain.runtime.get(&task_key, Some(&session)) else {
        return Err(not_found_task(&task_id).into());
    };
    let slice = domain
        .runtime
        .get_output(
            &task_key,
            &JsValue::Number(offset),
            &JsValue::Number(max_bytes as f64),
            Some(&session),
        )
        .map_err(|_| not_found_task(&task_id))?;
    let next_output_cursor = if slice.next_offset < slice.total_bytes && slice.next_offset > offset
    {
        Some(slice.next_offset.to_string())
    } else {
        None
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "task": task_json(&task),
            "output": slice.content,
            "nextOutputCursor": next_output_cursor,
            "truncated": slice.truncated,
        }),
    ))
}

fn not_found_task(task_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("background task `{task_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// `background_tasks.stop` — async. Terminates the task's process group;
/// repeated stops return the (terminal) state and never resurrect a task.
async fn background_tasks_stop(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let session_id = str_field(&input, "sessionId")?;
    let task_id = str_field(&input, "taskId")?;

    let session = JsValue::String(JsString::from(session_id.as_str()));
    let task_key = JsString::from(task_id.as_str());
    let operation = accept_domain_operation(
        &domain.db,
        "background_tasks.stop",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || {
            if domain.runtime.get(&task_key, Some(&session)).is_none() {
                Err(not_found_task(&task_id))
            } else {
                Ok(())
            }
        },
    )?;
    let runner = domain.clone();
    let task_key = task_key.clone();
    let session = session.clone();
    let task_id = task_id.clone();
    let operation_id = operation.operation_id.clone();
    let session_for_events = session_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, &session_for_events);
        // SIGTERM → grace → SIGKILL escalation lives inside the runtime
        // (`stop`); a dropped caller future cannot cancel it.
        let _ = runner
            .runtime
            .stop(&task_key, &JsValue::Undefined, Some(&session))
            .await;
        let task = runner.runtime.get(&task_key, Some(&session));
        let completion = match task {
            Some(task) => json!({
                "taskId": task_id,
                "taskState": task.status.as_str().to_owned(),
                "exitCode": exit_code_json(task.exit_code),
            }),
            None => json!({
                "taskId": task_id,
                "taskState": "unknown",
                "exitCode": Value::Null,
            }),
        };
        let _ = finish_domain_operation(
            &runner.db,
            &operation_id,
            &session_for_events,
            "succeeded",
            &completion,
        );
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// Tests (drive the ported runtime directly: the offline-driver v1 world has
// no tool chain that starts background tasks, so the domain is exercised by
// attaching tasks through the same public `start` the tool layer uses)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;
    use crate::runtime::background::StartTaskSpec;
    use std::time::{Duration, Instant};

    struct Fixture {
        _tmp: TempDir,
        rt: Arc<PublicV1Runtime>,
        _instance_id: String,
    }

    async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let (file, _token) =
            crate::public_v1::bootstrap::load_local_instance_and_token(&home, None).unwrap();
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
        let store = V1Store::open_and_recover(&home, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        let instance_id = rt.store().meta().instance_id.clone();
        Fixture {
            _tmp: tmp,
            rt,
            _instance_id: instance_id,
        }
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> ContractError {
        match rt.dispatch(method, input, None, None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    async fn start_task(
        domain: &BackgroundDomain,
        session_id: &str,
        command: &str,
    ) -> crate::runtime::background::BackgroundBashTask {
        let spec = StartTaskSpec {
            command: JsValue::String(JsString::from(command)),
            cwd: JsString::from("/tmp"),
            env: None,
            session_id: JsValue::String(JsString::from(session_id)),
            agent_id: JsValue::Undefined,
            kind: JsValue::Undefined,
        };
        domain
            .runtime
            .start(spec)
            .await
            .expect("start background task")
    }

    #[tokio::test]
    async fn list_get_and_stop_follow_the_task_lifecycle() {
        let fx = fixture("bg-lifecycle").await;
        let domain = domain_for(&fx.rt).unwrap();

        let task = start_task(&domain, "sess-1", "echo hello-bg; sleep 60").await;
        let task_id = String::from_utf16_lossy(task.task_id.units());

        // List is scoped to the owning session.
        let (_, listed) = fx
            .rt
            .dispatch(
                "background_tasks.list",
                json!({"sessionId": "sess-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        let items = listed["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["taskId"], task_id);
        assert_eq!(items[0]["ownerSessionId"], "sess-1");
        assert_eq!(items[0]["state"], "running");
        assert_eq!(items[0]["ownerRunId"], Value::Null);
        assert!(listed["snapshotRevision"].as_str().is_some());

        let (_, foreign) = fx
            .rt
            .dispatch(
                "background_tasks.list",
                json!({"sessionId": "sess-other"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(foreign["items"].as_array().map(Vec::len), Some(0));

        // Get: the task row plus its bounded output (the echo lands shortly).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, got) = fx
                .rt
                .dispatch(
                    "background_tasks.get",
                    json!({"sessionId": "sess-1", "taskId": task_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["output"].as_str().unwrap_or("").contains("hello-bg") {
                assert_eq!(got["task"]["state"], "running");
                break;
            }
            assert!(Instant::now() < deadline, "echo output never arrived");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // A foreign session cannot observe or stop the task.
        let error = dispatch_err(
            &fx.rt,
            "background_tasks.get",
            json!({"sessionId": "sess-other", "taskId": task_id}),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // Stop (async): the process group terminates, the task reaches a
        // terminal state, and a second stop is idempotent.
        let (_, receipt) = fx
            .rt
            .dispatch(
                "background_tasks.stop",
                json!({"sessionId": "sess-1", "taskId": task_id}),
                Some("key-bg-stop-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], "operation");

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, got) = fx
                .rt
                .dispatch(
                    "background_tasks.get",
                    json!({"sessionId": "sess-1", "taskId": task_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            let state = got["task"]["state"].as_str().unwrap_or_default();
            if state == "cancelled" || state == "completed" || state == "failed" {
                break;
            }
            assert!(Instant::now() < deadline, "task never became terminal");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let (_, replay) = fx
            .rt
            .dispatch(
                "background_tasks.stop",
                json!({"sessionId": "sess-1", "taskId": task_id}),
                Some("key-bg-stop-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            replay["deduplicated"], true,
            "a repeated stop replays the same termination request"
        );

        let (_, second) = fx
            .rt
            .dispatch(
                "background_tasks.stop",
                json!({"sessionId": "sess-1", "taskId": task_id}),
                Some("key-bg-stop-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(second["kind"], "operation", "a fresh stop still answers");

        // Unknown tasks are NOT_FOUND (`stop` carries its required
        // idempotency key on the dispatch).
        let error = dispatch_err(
            &fx.rt,
            "background_tasks.get",
            json!({"sessionId": "sess-1", "taskId": "task-none"}),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
        let error = fx
            .rt
            .dispatch(
                "background_tasks.stop",
                json!({"sessionId": "sess-1", "taskId": "task-none"}),
                Some("key-bg-stop-none"),
                None,
            )
            .await;
        match error {
            Err(DispatchError::Application(error)) => assert_eq!(error.code, "NOT_FOUND"),
            other => panic!("expected NOT_FOUND, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_bounds_output_and_follows_the_cursor() {
        let fx = fixture("bg-output").await;
        let domain = domain_for(&fx.rt).unwrap();

        // ~400 bytes of output, then hold the task open. The first bounded
        // read keeps the ring intact, so `truncated` (the baseline's
        // dropped-from-ring marker) stays false and the cursor carries the
        // rest.
        let filler = "b".repeat(400);
        let task = start_task(
            &domain,
            "sess-1",
            &format!("printf %s '{filler}'; sleep 60"),
        )
        .await;
        let task_id = String::from_utf16_lossy(task.task_id.units());

        let deadline = Instant::now() + Duration::from_secs(10);
        let first;
        loop {
            let (_, got) = fx
                .rt
                .dispatch(
                    "background_tasks.get",
                    json!({"sessionId": "sess-1", "taskId": task_id, "maxBytes": 100}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["output"].as_str().unwrap_or("").len() >= 100 {
                first = got;
                break;
            }
            assert!(Instant::now() < deadline, "filler output never arrived");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(first["truncated"], false);
        let cursor = first["nextOutputCursor"].as_str().unwrap().to_owned();

        // Follow the cursor: the remainder arrives and the cursor ends.
        let deadline = Instant::now() + Duration::from_secs(10);
        let tail;
        loop {
            let (_, got) = fx
                .rt
                .dispatch(
                    "background_tasks.get",
                    json!({"sessionId": "sess-1", "taskId": task_id, "outputCursor": cursor}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["output"].as_str().unwrap_or("").len() >= 300 {
                tail = got;
                break;
            }
            assert!(Instant::now() < deadline, "remaining output never arrived");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            tail["nextOutputCursor"],
            Value::Null,
            "the cursor ends when the output is fully consumed"
        );

        // Cleanup: stop the task.
        let _ = fx
            .rt
            .dispatch(
                "background_tasks.stop",
                json!({"sessionId": "sess-1", "taskId": task_id}),
                Some("key-bg-cleanup"),
                None,
            )
            .await;
    }

    #[tokio::test]
    async fn completed_task_reports_exit_code() {
        let fx = fixture("bg-exit").await;
        let domain = domain_for(&fx.rt).unwrap();

        let task = start_task(&domain, "sess-1", "echo done; true").await;
        let task_id = String::from_utf16_lossy(task.task_id.units());

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (_, got) = fx
                .rt
                .dispatch(
                    "background_tasks.get",
                    json!({"sessionId": "sess-1", "taskId": task_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["task"]["state"] == "completed" {
                assert_eq!(got["task"]["exitCode"], json!(0));
                assert!(got["task"]["finishedAt"].as_str().is_some());
                break;
            }
            assert!(Instant::now() < deadline, "task never completed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
