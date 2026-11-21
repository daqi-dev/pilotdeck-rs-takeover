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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE,
    ContractError, EffectStatus, RetrySpec,
};
use pilotdeck_protocol::{JsString, JsValue};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::FILE_CHUNK_MAX_BYTES;
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, finish_domain_operation, mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::runtime::background::{
    BackgroundTaskRuntime, BackgroundTaskRuntimeOptions, TaskListFilter, TaskStatus,
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

fn domains_registry() -> &'static Mutex<HashMap<PathBuf, Arc<BackgroundDomain>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<BackgroundDomain>>>> = OnceLock::new();
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
    {
        let registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
        if registry.contains_key(&home) {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "background runtime already serving this dataRoot; attach before the first background_tasks dispatch",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }
    let db = DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?;
    let candidate = Arc::new(BackgroundDomain {
        rt: rt.clone(),
        runtime,
        db,
    });
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    if registry.contains_key(&home) {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "background runtime already serving this dataRoot; attach before the first background_tasks dispatch",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    registry.insert(home, candidate);
    Ok(())
}

fn domain_for(rt: &Arc<PublicV1Runtime>) -> Result<Arc<BackgroundDomain>, ContractError> {
    let home = rt.store().home().to_path_buf();
    {
        let registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = registry.get(&home) {
            return Ok(existing.clone());
        }
    }
    let db = DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?;
    let options = BackgroundTaskRuntimeOptions {
        disk_spill_dir: Some(home.join("v1").join("background-tasks")),
        ..BackgroundTaskRuntimeOptions::default()
    };
    let candidate = Arc::new(BackgroundDomain {
        rt: rt.clone(),
        runtime: Arc::new(BackgroundTaskRuntime::new(options)),
        db,
    });
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    Ok(registry.entry(home).or_insert(candidate).clone())
}

/// Gracefully removes and disposes the background-task runtime for `home` on
/// gateway shutdown / test teardown.
pub(crate) async fn dispose_for_home(home: &Path) {
    let domain = domains_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(home);
    if let Some(domain) = domain {
        let _ = domain.runtime.dispose().await;
    }
}

/// Returns `true` if `home` has an initialized background runtime with any
/// `Pending` or `Running` tasks owned by any session in `session_ids`.
pub(crate) fn has_active_tasks_for_sessions(home: &Path, session_ids: &[String]) -> bool {
    if session_ids.is_empty() {
        return false;
    }
    let domain = domains_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(home)
        .cloned();
    let Some(domain) = domain else {
        return false;
    };
    let filter = TaskListFilter::default();
    for sid in session_ids {
        let session = JsValue::String(JsString::from(sid.as_str()));
        for task in domain.runtime.list(&filter, Some(&session)) {
            if matches!(task.status, TaskStatus::Pending | TaskStatus::Running) {
                return true;
            }
        }
    }
    false
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
        .and_then(|rest| rest.strip_prefix('-'))
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse::<usize>().ok())
        .ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("malformed cursor `{cursor}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })
}

fn parse_output_cursor(cursor: Option<&str>) -> Result<u64, ContractError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let parsed = if !cursor.is_empty() && cursor.bytes().all(|b| b.is_ascii_digit()) {
        cursor.parse::<u64>().ok()
    } else {
        None
    };
    parsed.ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("malformed outputCursor `{cursor}`"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })
}

fn bg_sql_err(error: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite error: {error}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

async fn run_blocking<T, F>(task: F) -> Result<T, ContractError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ContractError> + Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|err| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("background task worker failed: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn background_tasks_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = str_field(&input, "sessionId")?;
    let cursor = parse_offset_cursor(input.get("cursor").and_then(Value::as_str), "bo")?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let states: Vec<String> = input
        .get("states")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|s| s.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();

    let payload = run_blocking(move || {
        let domain = domain_for(&rt)?;
        let session = JsValue::String(JsString::from(session_id.as_str()));
        let status = if !states.is_empty() {
            let items: Vec<JsValue> = states
                .iter()
                .map(|state| JsValue::String(JsString::from(state.as_str())))
                .collect();
            JsValue::Array(items)
        } else {
            JsValue::Undefined
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
        Ok(json!({
            "items": items,
            "nextCursor": next_cursor,
            "snapshotRevision": snapshot_revision,
        }))
    })
    .await?;

    Ok((ResponseOutcome::Succeeded, payload))
}

async fn background_tasks_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = str_field(&input, "sessionId")?;
    let task_id = str_field(&input, "taskId")?;
    let max_bytes = input
        .get("maxBytes")
        .and_then(Value::as_u64)
        .unwrap_or(65_536)
        .clamp(1, FILE_CHUNK_MAX_BYTES as u64);
    let offset = parse_output_cursor(input.get("outputCursor").and_then(Value::as_str))?;

    let payload = run_blocking(move || {
        let domain = domain_for(&rt)?;
        let session = JsValue::String(JsString::from(session_id.as_str()));
        let task_key = JsString::from(task_id.as_str());
        let Some(task) = domain.runtime.get(&task_key, Some(&session)) else {
            return Err(not_found_task(&task_id));
        };
        let offset_f64 = offset as f64;
        let slice = domain
            .runtime
            .get_output(
                &task_key,
                &JsValue::Number(offset_f64),
                &JsValue::Number(max_bytes as f64),
                Some(&session),
            )
            .map_err(|_| not_found_task(&task_id))?;
        let next_output_cursor =
            if slice.next_offset < slice.total_bytes && slice.next_offset > offset_f64 {
                Some(slice.next_offset.to_string())
            } else {
                None
            };
        Ok(json!({
            "task": task_json(&task),
            "output": slice.content,
            "nextOutputCursor": next_output_cursor,
            "truncated": slice.truncated,
        }))
    })
    .await?;

    Ok((ResponseOutcome::Succeeded, payload))
}

fn not_found_task(task_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("background task `{task_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn idempotency_conflict_err() -> ContractError {
    ContractError::new(
        CODE_IDEMPOTENCY_CONFLICT,
        "idempotencyKey already used for background_tasks.stop with different input",
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn rejected_replay_err(code: String) -> ContractError {
    ContractError::new(
        code,
        "background_tasks.stop request previously rejected definitively",
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn retained_until_from(now: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(now) {
        Ok(parsed) => (parsed + chrono::Duration::hours(24)).to_rfc3339(),
        Err(_) => now.to_owned(),
    }
}

/// Atomically looks up/validates/accepts `background_tasks.stop` under an
/// `Immediate` store transaction, keeping `v1_domain_operations` and
/// `idempotency_records` consistent so replays neither race nor re-spawn.
fn accept_stop_operation(
    rt: &Arc<PublicV1Runtime>,
    input: &Value,
    key: Option<&str>,
    session_id: &str,
    task_id: &str,
) -> Result<(Arc<BackgroundDomain>, String, Value, bool), ContractError> {
    let domain = domain_for(rt)?;
    let session = JsValue::String(JsString::from(session_id));
    let task_key = JsString::from(task_id);
    let digest = compute_input_digest("background_tasks.stop", input);
    let now = crate::public_v1::store::now_rfc3339();
    let retained_until = retained_until_from(&now);

    let mut conn = domain.rt.store().conn.lock().expect("sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(bg_sql_err)?;

    if let Some(key_str) = key {
        let existing_dom: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT input_digest, receipt_json, rejection_code FROM v1_domain_operations
                 WHERE idempotency_key = ?1 AND method_id = 'background_tasks.stop'",
                params![key_str],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(bg_sql_err)?;
        if let Some((stored_digest, receipt_json, rejection)) = existing_dom {
            if stored_digest != digest {
                return Err(idempotency_conflict_err());
            }
            if let Some(code) = rejection {
                return Err(rejected_replay_err(code));
            }
            let mut receipt: Value =
                serde_json::from_str(&receipt_json).unwrap_or_else(|_| json!({}));
            let op_id = receipt["operationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let resource = json!({"kind": "operation", "operationId": op_id});
            tx.execute(
                "INSERT OR IGNORE INTO idempotency_records (
                    idempotency_key, method_id, input_digest, request_state, reason,
                    resource_json, receipt_json, rejection_code, observed_at,
                    retained_until, retry_disposition
                 ) VALUES (?1, 'background_tasks.stop', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                params![
                    key_str,
                    digest,
                    serde_json::to_string(&resource).unwrap_or_else(|_| "null".into()),
                    receipt_json,
                    now,
                    retained_until,
                ],
            )
            .map_err(bg_sql_err)?;
            tx.commit().map_err(bg_sql_err)?;
            if let Some(obj) = receipt.as_object_mut() {
                obj.insert("deduplicated".into(), json!(true));
            }
            return Ok((domain.clone(), op_id, receipt, true));
        }

        let existing_idem: Option<(String, String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT input_digest, request_state, receipt_json, rejection_code
                 FROM idempotency_records
                 WHERE idempotency_key = ?1 AND method_id = 'background_tasks.stop'",
                params![key_str],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(bg_sql_err)?;
        if let Some((stored_digest, req_state, receipt_json, rejection)) = existing_idem {
            if stored_digest != digest {
                return Err(idempotency_conflict_err());
            }
            if req_state != "accepted" {
                return Err(rejected_replay_err(
                    rejection.unwrap_or_else(|| CODE_NOT_FOUND.to_owned()),
                ));
            }
            let mut receipt: Value = receipt_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            if let Some(obj) = receipt.as_object_mut() {
                obj.insert("deduplicated".into(), json!(true));
            }
            let op_id = receipt["operationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            tx.commit().map_err(bg_sql_err)?;
            return Ok((domain.clone(), op_id, receipt, true));
        }
    }

    if domain.runtime.get(&task_key, Some(&session)).is_none() {
        let err = not_found_task(task_id);
        if let Some(key_str) = key {
            tx.execute(
                "INSERT OR REPLACE INTO v1_domain_operations
                    (operation_id, method_id, idempotency_key, input_digest, state, receipt_json,
                     completion_json, rejection_code, created_at, updated_at)
                 VALUES (?1, 'background_tasks.stop', ?2, ?3, 'rejected', '{}', NULL, ?4, ?5, ?5)",
                params![
                    format!("rejected-background_tasks.stop-{key_str}"),
                    key_str,
                    digest,
                    err.code,
                    now,
                ],
            )
            .map_err(bg_sql_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO idempotency_records (
                    idempotency_key, method_id, input_digest, request_state, reason,
                    resource_json, receipt_json, rejection_code, observed_at,
                    retained_until, retry_disposition
                 ) VALUES (?1, 'background_tasks.stop', ?2, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?3, ?4, ?5, 'correctRejectedInputWithNewKey')",
                params![key_str, digest, err.code, now, retained_until],
            )
            .map_err(bg_sql_err)?;
            tx.commit().map_err(bg_sql_err)?;
        }
        return Err(err);
    }

    let cur_rev: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(bg_sql_err)?;
    let rev_num = cur_rev.and_then(|v| v.parse::<i64>().ok()).unwrap_or(1);
    let accepted_rev = format!("rev-{rev_num:08}");

    let operation_id = format!("op-{}", domain.rt.store().next_id());
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": accepted_rev,
        "deduplicated": false,
        "state": "queued",
    });
    let receipt_str = serde_json::to_string(&receipt).unwrap_or_else(|_| "{}".into());

    tx.execute(
        "INSERT INTO v1_domain_operations
            (operation_id, method_id, idempotency_key, input_digest, state, receipt_json,
             completion_json, rejection_code, created_at, updated_at)
         VALUES (?1, 'background_tasks.stop', ?2, ?3, 'queued', ?4, NULL, NULL, ?5, ?5)",
        params![operation_id, key, digest, receipt_str, now],
    )
    .map_err(bg_sql_err)?;

    if let Some(key_str) = key {
        let resource = json!({"kind": "operation", "operationId": operation_id});
        tx.execute(
            "INSERT INTO idempotency_records (
                idempotency_key, method_id, input_digest, request_state, reason,
                resource_json, receipt_json, rejection_code, observed_at,
                retained_until, retry_disposition
             ) VALUES (?1, 'background_tasks.stop', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
            params![
                key_str,
                digest,
                serde_json::to_string(&resource).unwrap_or_else(|_| "null".into()),
                receipt_str,
                now,
                retained_until,
            ],
        )
        .map_err(bg_sql_err)?;
    }

    tx.commit().map_err(bg_sql_err)?;
    drop(conn);
    domain.rt.store().state_notifier().notify_waiters();
    Ok((domain, operation_id, receipt, false))
}

/// `background_tasks.stop` — async. Terminates the task's process group;
/// repeated stops return the (terminal) state and never resurrect a task.
async fn background_tasks_stop(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = str_field(&input, "sessionId")?;
    let task_id = str_field(&input, "taskId")?;

    let input_for_accept = input.clone();
    let session_for_accept = session_id.clone();
    let task_for_accept = task_id.clone();
    let (domain, operation_id, receipt, deduplicated) = run_blocking(move || {
        accept_stop_operation(
            &rt,
            &input_for_accept,
            key.as_deref(),
            &session_for_accept,
            &task_for_accept,
        )
    })
    .await?;

    if deduplicated {
        return Ok((ResponseOutcome::Accepted, receipt));
    }

    let runner = domain.clone();
    let task_key = JsString::from(task_id.as_str());
    let session = JsValue::String(JsString::from(session_id.as_str()));
    tokio::spawn(async move {
        let r_mark = runner.clone();
        let op_mark = operation_id.clone();
        let sess_mark = session_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            mark_domain_operation_running(&r_mark.db, &op_mark, &sess_mark)
        })
        .await;
        runner.rt.store().event_notifier().notify_waiters();
        runner.rt.store().state_notifier().notify_waiters();

        // SIGTERM → grace → SIGKILL escalation lives inside the runtime
        // (`stop`); a dropped caller future cannot cancel it.
        let stop_res = runner
            .runtime
            .stop(&task_key, &JsValue::Undefined, Some(&session))
            .await;
        let task = runner.runtime.get(&task_key, Some(&session));
        let (state, completion) = match (stop_res, task) {
            (Ok(_), Some(task)) => (
                "succeeded",
                json!({
                    "taskId": task_id,
                    "taskState": task.status.as_str().to_owned(),
                    "exitCode": exit_code_json(task.exit_code),
                }),
            ),
            (Err(err), task) => {
                let err_msg = String::from_utf16_lossy(err.message.units());
                let task_state = task
                    .as_ref()
                    .map(|t| t.status.as_str().to_owned())
                    .unwrap_or_else(|| "unknown".to_owned());
                let exit_code = task
                    .as_ref()
                    .map(|t| exit_code_json(t.exit_code))
                    .unwrap_or(Value::Null);
                (
                    "failed",
                    json!({
                        "code": CODE_UNAVAILABLE,
                        "message": err_msg,
                        "taskId": task_id,
                        "taskState": task_state,
                        "exitCode": exit_code,
                    }),
                )
            }
            (Ok(_), None) => (
                "failed",
                json!({
                    "code": CODE_NOT_FOUND,
                    "message": format!("background task `{task_id}` vanished before stop completion"),
                    "taskId": task_id,
                    "taskState": "unknown",
                    "exitCode": Value::Null,
                }),
            ),
        };
        let r_fin = runner.clone();
        let op_fin = operation_id;
        let sess_fin = session_id;
        let state_owned = state.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            finish_domain_operation(&r_fin.db, &op_fin, &sess_fin, &state_owned, &completion)
        })
        .await;
        runner.rt.store().event_notifier().notify_waiters();
        runner.rt.store().state_notifier().notify_waiters();
    });
    Ok((ResponseOutcome::Accepted, receipt))
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
        dispose_for_home(fx.rt.store().home()).await;
    }

    #[tokio::test]
    async fn registry_holds_strong_arc_tracks_active_and_disposes() {
        let fx = fixture("bg-strong").await;
        let attached = Arc::new(BackgroundTaskRuntime::new(BackgroundTaskRuntimeOptions {
            disk_spill_dir: Some(fx.rt.store().home().join("v1").join("background-tasks")),
            ..BackgroundTaskRuntimeOptions::default()
        }));
        attach_background_runtime(&fx.rt, Arc::clone(&attached)).unwrap();

        let task = attached
            .start(StartTaskSpec {
                command: JsValue::String(JsString::from("sleep 60")),
                cwd: JsString::from("/tmp"),
                env: None,
                session_id: JsValue::String(JsString::from("sess-strong")),
                agent_id: JsValue::Undefined,
                kind: JsValue::Undefined,
            })
            .await
            .expect("start attached task");
        let task_id = String::from_utf16_lossy(task.task_id.units());
        drop(attached);

        let home = fx.rt.store().home().to_path_buf();
        assert!(has_active_tasks_for_sessions(
            &home,
            &["sess-strong".to_owned()]
        ));
        assert!(!has_active_tasks_for_sessions(
            &home,
            &["sess-other".to_owned()]
        ));

        let (_, listed) = fx
            .rt
            .dispatch(
                "background_tasks.list",
                json!({"sessionId": "sess-strong"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["items"][0]["taskId"], task_id);

        dispose_for_home(&home).await;
        assert!(!has_active_tasks_for_sessions(
            &home,
            &["sess-strong".to_owned()]
        ));
    }

    #[tokio::test]
    async fn list_pagination_accepts_bo_cursor_from_next_cursor() {
        let fx = fixture("bg-page").await;
        let domain = domain_for(&fx.rt).unwrap();

        let t1 = start_task(&domain, "sess-page", "echo one; sleep 60").await;
        let t2 = start_task(&domain, "sess-page", "echo two; sleep 60").await;
        let id1 = String::from_utf16_lossy(t1.task_id.units());
        let id2 = String::from_utf16_lossy(t2.task_id.units());

        let (_, page1) = fx
            .rt
            .dispatch(
                "background_tasks.list",
                json!({"sessionId": "sess-page", "limit": 1}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page1["items"].as_array().map(Vec::len), Some(1));
        let cursor = page1["nextCursor"]
            .as_str()
            .expect("expected nextCursor")
            .to_owned();
        assert!(cursor.starts_with("bo-"));

        let (_, page2) = fx
            .rt
            .dispatch(
                "background_tasks.list",
                json!({"sessionId": "sess-page", "limit": 1, "cursor": cursor}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page2["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(page2["nextCursor"], Value::Null);
        let seen = [
            page1["items"][0]["taskId"].as_str().unwrap().to_owned(),
            page2["items"][0]["taskId"].as_str().unwrap().to_owned(),
        ];
        assert!(seen.contains(&id1) && seen.contains(&id2));
        dispose_for_home(fx.rt.store().home()).await;
    }

    #[tokio::test]
    async fn stop_is_atomic_registers_idempotency_and_avoids_respawn_on_replay() {
        let fx = fixture("bg-stop-idem").await;
        let domain = domain_for(&fx.rt).unwrap();
        let iid = fx._instance_id.clone();

        let task = start_task(&domain, "sess-stop", "sleep 60").await;
        let task_id = String::from_utf16_lossy(task.task_id.units());

        // Concurrent dispatches with the same idempotencyKey produce a single
        // operationId and at most one fresh acceptance.
        let req_a = fx.rt.dispatch(
            "background_tasks.stop",
            json!({"sessionId": "sess-stop", "taskId": task_id}),
            Some("key-race-1"),
            None,
        );
        let req_b = fx.rt.dispatch(
            "background_tasks.stop",
            json!({"sessionId": "sess-stop", "taskId": task_id}),
            Some("key-race-1"),
            None,
        );
        let (res_a, res_b) = tokio::join!(req_a, req_b);
        let (_, rec_a) = res_a.expect("first concurrent stop succeeds");
        let (_, rec_b) = res_b.expect("second concurrent stop succeeds");
        assert_eq!(rec_a["operationId"], rec_b["operationId"]);
        assert_ne!(
            rec_a["deduplicated"], rec_b["deduplicated"],
            "exactly one caller is fresh and the other is deduplicated"
        );
        let op_id = rec_a["operationId"].as_str().unwrap().to_owned();

        // Wait for termination.
        let deadline = Instant::now() + Duration::from_secs(20);
        let first_updated_at;
        loop {
            let (_, op_got) = fx
                .rt
                .dispatch("operations.get", json!({"operationId": op_id}), None, None)
                .await
                .unwrap();
            if matches!(
                op_got["operation"]["state"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
                first_updated_at = conn
                    .query_row(
                        "SELECT updated_at FROM v1_domain_operations WHERE operation_id = ?1",
                        params![op_id],
                        |r| r.get::<_, String>(0),
                    )
                    .unwrap();
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stop operation never terminalized"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Replay does NOT re-spawn or touch the settled operation record.
        let (_, replay) = fx
            .rt
            .dispatch(
                "background_tasks.stop",
                json!({"sessionId": "sess-stop", "taskId": task_id}),
                Some("key-race-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"], op_id);
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            let updated_after: String = conn
                .query_row(
                    "SELECT updated_at FROM v1_domain_operations WHERE operation_id = ?1",
                    params![op_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(updated_after, first_updated_at);
        }

        // `requests.lookup` finds the stop acceptance.
        let (_, lookup) = fx
            .rt
            .dispatch(
                "requests.lookup",
                json!({
                    "instanceId": iid,
                    "methodId": "background_tasks.stop",
                    "idempotencyKey": "key-race-1",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup["requestState"], "accepted");
        assert_eq!(lookup["resource"]["operationId"], op_id);

        // Definitive rejection is also recorded for replay and `requests.lookup`.
        let err = fx
            .rt
            .dispatch(
                "background_tasks.stop",
                json!({"sessionId": "sess-stop", "taskId": "missing-task"}),
                Some("key-stop-missing"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, "NOT_FOUND"),
            other => panic!("expected NOT_FOUND, got {other:?}"),
        }
        let (_, lookup_rej) = fx
            .rt
            .dispatch(
                "requests.lookup",
                json!({
                    "instanceId": iid,
                    "methodId": "background_tasks.stop",
                    "idempotencyKey": "key-stop-missing",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup_rej["requestState"], "notAccepted");

        dispose_for_home(fx.rt.store().home()).await;
    }

    #[tokio::test]
    async fn get_validates_output_cursor_and_clamps_max_bytes() {
        let fx = fixture("bg-get-bounds").await;
        let domain = domain_for(&fx.rt).unwrap();

        let task = start_task(&domain, "sess-bounds", "echo bounded; sleep 60").await;
        let task_id = String::from_utf16_lossy(task.task_id.units());

        for bad in ["NaN", "-Infinity", "Infinity", "-10", "+10", "1.5", ""] {
            let err = dispatch_err(
                &fx.rt,
                "background_tasks.get",
                json!({
                    "sessionId": "sess-bounds",
                    "taskId": task_id,
                    "outputCursor": bad,
                }),
            )
            .await;
            assert_eq!(
                err.code, "INVALID_ARGUMENT",
                "outputCursor `{bad}` must fail with INVALID_ARGUMENT"
            );
        }

        // Huge maxBytes is clamped to FILE_CHUNK_MAX_BYTES without panicking.
        let (_, got) = fx
            .rt
            .dispatch(
                "background_tasks.get",
                json!({
                    "sessionId": "sess-bounds",
                    "taskId": task_id,
                    "maxBytes": 100_000_000,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(got["output"].is_string());

        dispose_for_home(fx.rt.store().home()).await;
    }
}
