//! `session.update` / `session.close` / `session.reopen` / `session.delete`
//! (`t012`).
//!
//! All four are conditional (CAS on `sessions.revision`) writes. `close`
//! and `delete` refuse with `ACTIVE_RESOURCES` while the session still has
//! an active (non-terminal) or queued run — the contract is the only
//! authority for that interplay. `delete` marks the session row as a
//! tombstone (`session_state='deleted'`) and keeps runs and idempotency
//! records ("不清除run防重墓碑"); `reopen` refuses to resurrect a deleted
//! session with `SESSION_DELETED`. This file also owns the admission gate
//! `run.submit` consults so a closed session answers `SESSION_CLOSED`
//! instead of admitting a new run.

use std::path::Path;
use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError, EffectStatus,
    RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::background_tasks::has_active_tasks_for_sessions;
use crate::public_v1::domains::workspace_dirs::{
    CODE_ACTIVE_RESOURCES, append_domain_event_in_tx, bump_rev_in_tx, domain_not_found, sql_err,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// Error codes declared by the `session.*` / `run.submit` contracts that the
/// protocol crate has no constants for yet.
pub(crate) const CODE_SESSION_DELETED: &str = "SESSION_DELETED";
pub(crate) const CODE_SESSION_CLOSED: &str = "SESSION_CLOSED";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("session.update", |rt, input, _key, _rev| {
        Box::pin(session_update(rt, input))
    });
    reg.handler("session.close", |rt, input, _key, _rev| {
        Box::pin(session_close(rt, input))
    });
    reg.handler("session.reopen", |rt, input, _key, _rev| {
        Box::pin(session_reopen(rt, input))
    });
    reg.handler("session.delete", |rt, input, _key, _rev| {
        Box::pin(session_delete(rt, input))
    });
}

async fn run_blocking<T, F>(task: F) -> Result<T, ContractError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ContractError> + Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("session admin worker failed: {e}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?
}

async fn session_update(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = run_blocking(move || store.update_session(&input)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn session_close(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = run_blocking(move || store.close_session(&input)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn session_reopen(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = run_blocking(move || store.reopen_session(&input)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn session_delete(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = run_blocking(move || store.delete_session(&input)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

// ---------------------------------------------------------------------------
// Store layer
// ---------------------------------------------------------------------------

/// The columns `session.*` results need from the sessions table.
struct SessionRow {
    project_id: String,
    name: String,
    tags_json: String,
    revision: String,
    active_run_id: Option<String>,
    session_state: String,
}

fn fetch_session_row(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<SessionRow>, ContractError> {
    conn.query_row(
        "SELECT project_id, name, tags_json, revision, active_run_id, session_state
         FROM sessions WHERE session_id = ?1",
        params![session_id],
        |r| {
            Ok(SessionRow {
                project_id: r.get(0)?,
                name: r.get(1)?,
                tags_json: r.get(2)?,
                revision: r.get(3)?,
                active_run_id: r.get(4)?,
                session_state: r.get(5)?,
            })
        },
    )
    .optional()
    .map_err(sql_err)
}

fn session_row_json(session_id: &str, row: &SessionRow) -> Value {
    json!({
        "sessionId": session_id,
        "projectId": row.project_id,
        "name": row.name,
        "tags": serde_json::from_str::<Value>(&row.tags_json).unwrap_or(json!([])),
        "revision": row.revision,
        "activeRunId": row.active_run_id,
        "sessionState": row.session_state,
    })
}

fn session_not_found_or_deleted(
    session_id: &str,
    row: Option<SessionRow>,
) -> Result<SessionRow, ContractError> {
    match row {
        None => Err(domain_not_found("session", session_id)),
        Some(row) if row.session_state == "deleted" => Err(domain_not_found("session", session_id)),
        Some(row) => Ok(row),
    }
}

fn revision_conflict(expected: &str, actual: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("session expectedRevision `{expected}` does not match `{actual}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

/// `ACTIVE_RESOURCES` when the session still has active runs, queued runs,
/// non-terminal operations, enabled schedules, or active background tasks.
fn active_resources(session_id: &str) -> ContractError {
    ContractError::new(
        CODE_ACTIVE_RESOURCES,
        format!(
            "session `{session_id}` still has active resources (runs, operations, schedules, or background tasks); \
             stop or wait for them first"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, ContractError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |r| r.get(0),
    )
    .map_err(sql_err)
}

fn has_active_session_resources(
    home: &Path,
    conn: &Connection,
    session_id: &str,
) -> Result<bool, ContractError> {
    let active: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id = ?1 AND terminal = 0)
             OR EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
            params![session_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if active != 0 {
        return Ok(true);
    }

    if table_exists(conn, "operations")? {
        let op_active: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE session_id = ?1 AND terminal = 0)",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if op_active != 0 {
            return Ok(true);
        }
    }

    if table_exists(conn, "v1_schedules")? {
        let sched_active: i64 = conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM v1_schedules
                    WHERE session_id = ?1 AND enabled != 0)",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if sched_active != 0 {
            return Ok(true);
        }
        if table_exists(conn, "v1_schedule_executions")? {
            let exec_active: i64 = conn
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM v1_schedule_executions e
                        JOIN v1_schedules s ON s.schedule_id = e.schedule_id
                        WHERE s.session_id = ?1 AND e.state IN ('pending', 'running'))",
                    params![session_id],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            if exec_active != 0 {
                return Ok(true);
            }
        }
    }

    if has_active_tasks_for_sessions(home, &[session_id.to_owned()]) {
        return Ok(true);
    }

    Ok(false)
}

impl V1Store {
    /// `session.update`: conditional rename / retag. History identity is
    /// untouched (only the session row's display fields and revision move).
    pub(crate) fn update_session(&self, input: &Value) -> Result<Value, ContractError> {
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let new_name = input.get("name").and_then(Value::as_str);
        let new_tags = input.get("tags").and_then(Value::as_array);
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let row = session_not_found_or_deleted(session_id, fetch_session_row(&tx, session_id)?)?;
        if row.revision != expected {
            return Err(revision_conflict(expected, &row.revision));
        }
        let name = new_name.unwrap_or(&row.name).to_owned();
        let tags_json = match new_tags {
            Some(tags) => serde_json::to_string(tags).unwrap_or_else(|_| "[]".into()),
            None => row.tags_json.clone(),
        };
        let rev = bump_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        tx.execute(
            "UPDATE sessions SET name = ?1, tags_json = ?2, revision = ?3 WHERE session_id = ?4",
            params![name, tags_json, rev, session_id],
        )
        .map_err(sql_err)?;
        append_domain_event_in_tx(
            &tx,
            &self.meta().instance_id,
            session_id,
            session_id,
            "session.changed",
            &json!({"sessionId": session_id, "name": name, "revision": rev}),
            &now,
        )?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();
        Ok(json!({"session": {
            "sessionId": session_id,
            "projectId": row.project_id,
            "name": name,
            "tags": serde_json::from_str::<Value>(&tags_json).unwrap_or(json!([])),
            "revision": rev,
            "activeRunId": row.active_run_id,
            "sessionState": row.session_state,
        }}))
    }

    /// `session.close`: refuses while active/queued runs or active operations/
    /// schedules/background tasks exist; after the close, submissions answer
    /// `SESSION_CLOSED` but history stays readable.
    pub(crate) fn close_session(&self, input: &Value) -> Result<Value, ContractError> {
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let row = session_not_found_or_deleted(session_id, fetch_session_row(&tx, session_id)?)?;
        if row.revision != expected {
            return Err(revision_conflict(expected, &row.revision));
        }
        if row.session_state != "open" {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("session `{session_id}` is already closed"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if has_active_session_resources(self.home(), &tx, session_id)? {
            return Err(active_resources(session_id));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET session_state = 'closed', revision = ?1 WHERE session_id = ?2",
            params![rev, session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.state_notifier().notify_waiters();
        let mut after = row;
        after.revision = rev;
        after.session_state = "closed".to_owned();
        Ok(json!({"session": session_row_json(session_id, &after)}))
    }

    /// `session.reopen`: refuses deleted sessions (`SESSION_DELETED`), removed
    /// projects (`NOT_FOUND`), and already-open sessions; never re-runs old runs.
    pub(crate) fn reopen_session(&self, input: &Value) -> Result<Value, ContractError> {
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let row = fetch_session_row(&tx, session_id)?;
        let Some(row) = row else {
            return Err(domain_not_found("session", session_id));
        };
        match row.session_state.as_str() {
            "deleted" => {
                return Err(ContractError::new(
                    CODE_SESSION_DELETED,
                    format!("session `{session_id}` is deleted and cannot be reopened"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            "open" => {
                return Err(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!("session `{session_id}` is not closed"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            _ => {}
        }
        if row.revision != expected {
            return Err(revision_conflict(expected, &row.revision));
        }
        let project_exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE project_id = ?1)",
                params![row.project_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if !project_exists {
            return Err(domain_not_found("project", &row.project_id));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET session_state = 'open', revision = ?1 WHERE session_id = ?2",
            params![rev, session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.state_notifier().notify_waiters();
        let mut after = row;
        after.revision = rev;
        after.session_state = "open".to_owned();
        Ok(json!({"session": session_row_json(session_id, &after)}))
    }

    /// `session.delete`: refuses while active/queued runs or active operations/
    /// schedules/background tasks exist; marks the session as a deleted
    /// tombstone, keeping runs (dedup tombstones) and history rows for retention.
    pub(crate) fn delete_session(&self, input: &Value) -> Result<Value, ContractError> {
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let row = fetch_session_row(&tx, session_id)?;
        let Some(row) = row else {
            return Err(domain_not_found("session", session_id));
        };
        if row.session_state == "deleted" {
            return Err(domain_not_found("session", session_id));
        }
        if row.revision != expected {
            return Err(revision_conflict(expected, &row.revision));
        }
        if has_active_session_resources(self.home(), &tx, session_id)? {
            return Err(active_resources(session_id));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET session_state = 'deleted', revision = ?1, active_run_id = NULL
             WHERE session_id = ?2",
            params![rev, session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.state_notifier().notify_waiters();
        Ok(json!({
            "deletedSessionId": session_id,
            "revision": rev,
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests (contract anchors: active/queued存在拒绝；关闭后提交SESSION_CLOSED但
// 历史可读；已删除会话不可复活；不清除run防重墓碑；至少一个修改字段)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    fn open_runtime(root_dir: &std::path::Path) -> (std::sync::Arc<PublicV1Runtime>, String) {
        let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
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
        let store = V1Store::open_and_recover(root_dir, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    fn contract_code(error: &DispatchError) -> String {
        match error {
            DispatchError::Application(e) => e.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    /// Registers a project + session pair and returns (project_id,
    /// session_id, session revision).
    async fn seeded(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        tag: &str,
    ) -> (String, String, String) {
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": format!("p-{tag}"),
                "name": "S",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"},
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let (_, created) = rt
            .dispatch(
                "session.create",
                json!({
                    "instanceId": iid,
                    "sessionId": format!("s-{tag}"),
                    "projectId": format!("p-{tag}"),
                    "name": "Original",
                    "tags": ["a"],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let rev = created["session"]["revision"].as_str().unwrap().to_owned();
        (format!("p-{tag}"), format!("s-{tag}"), rev)
    }

    #[tokio::test]
    async fn session_update_renames_and_retags_under_cas() {
        let tmp = TempDir::new("sess-update");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_pid, sid, rev) = seeded(&rt, &iid, "u").await;

        let (_, updated) = rt
            .dispatch(
                "session.update",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "expectedRevision": rev,
                    "name": "Renamed",
                    "tags": ["x", "y"],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("session.update", &updated)
            .unwrap();
        assert_eq!(updated["session"]["name"], "Renamed");
        assert_eq!(
            updated["session"]["tags"],
            json!(["x", "y"]),
            "the session identity and history stay intact"
        );

        let stale = rt
            .dispatch(
                "session.update",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "expectedRevision": rev,
                    "name": "Loser",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&stale), "REVISION_CONFLICT");

        let missing = rt
            .dispatch(
                "session.update",
                json!({
                    "instanceId": iid,
                    "sessionId": "ghost",
                    "expectedRevision": rev,
                    "name": "x",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&missing), "NOT_FOUND");
    }

    #[tokio::test]
    async fn session_close_refuses_active_runs_and_gates_submission() {
        let tmp = TempDir::new("sess-close");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_pid, sid, rev) = seeded(&rt, &iid, "c").await;

        // A queued/active run refuses the close.
        let (_, submitted) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=2000]"}],
                }),
                Some("close-run-key-1"),
                None,
            )
            .await
            .unwrap();
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        let error = rt
            .dispatch(
                "session.close",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ACTIVE_RESOURCES");

        let (_, done) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 15000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(done["run"]["state"], "succeeded");

        // After the run completes the close succeeds...
        let (_, closed) = rt
            .dispatch(
                "session.close",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("session.close", &closed)
            .unwrap();
        assert_eq!(closed["session"]["sessionState"], "closed");

        // ...submission is refused with SESSION_CLOSED, history is readable.
        let error = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "input": [{"kind": "text", "text": "after close"}],
                }),
                Some("close-run-key-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "SESSION_CLOSED");
        let (_, readable) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(readable["items"].as_array().map(Vec::len), Some(0));

        // Reopen requires the fresh revision and restores admission.
        let closed_rev = closed["session"]["revision"].as_str().unwrap().to_owned();
        // Closing an already-closed session is an invalid transition. This
        // must be probed while the session is still closed and holding the
        // closed revision — after the reopen below both facts have moved on,
        // and a stale revision answers REVISION_CONFLICT before the state
        // gate is ever consulted.
        let error = rt
            .dispatch(
                "session.close",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": closed_rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "INVALID_ARGUMENT");
        let error = rt
            .dispatch(
                "session.reopen",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "REVISION_CONFLICT");
        let (_, reopened) = rt
            .dispatch(
                "session.reopen",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": closed_rev}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("session.reopen", &reopened)
            .unwrap();
        assert_eq!(reopened["session"]["sessionState"], "open");
        rt.dispatch(
            "run.submit",
            json!({
                "instanceId": iid,
                "sessionId": sid,
                "input": [{"kind": "text", "text": "after reopen"}],
            }),
            Some("close-run-key-3"),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn session_delete_keeps_run_tombstones_and_cannot_be_reopened() {
        let tmp = TempDir::new("sess-delete");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_pid, sid, rev) = seeded(&rt, &iid, "d").await;

        // A finished run stays behind as a dedup tombstone.
        let (_, submitted) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=100]"}],
                }),
                Some("delete-run-key-1"),
                None,
            )
            .await
            .unwrap();
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        let (_, done) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 15000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(done["run"]["state"], "succeeded");

        let (_, deleted) = rt
            .dispatch(
                "session.delete",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("session.delete", &deleted)
            .unwrap();
        assert_eq!(deleted["deletedSessionId"], sid);

        // The run row survives as a tombstone...
        let (_, run) = rt
            .dispatch(
                "run.get",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(run["run"]["state"], "succeeded");
        // ...and the idempotency record still resolves the original run for
        // lost-response recovery (`requests.lookup`), even though a fresh
        // submission is refused on the deleted session.
        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({
                    "instanceId": iid,
                    "methodId": "run.submit",
                    "idempotencyKey": "delete-run-key-1",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup["requestState"], "accepted");
        assert_eq!(lookup["resource"]["runId"], run_id);

        // History of the deleted session is gone, the tombstone is visible,
        // and a reopen is refused with SESSION_DELETED.
        let error = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
        let (_, tombstone) = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(tombstone["session"]["sessionState"], "deleted");
        let tomb_rev = tombstone["session"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let error = rt
            .dispatch(
                "session.reopen",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": tomb_rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "SESSION_DELETED");

        // Deleting again is NOT_FOUND.
        let error = rt
            .dispatch(
                "session.delete",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": tomb_rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
    }

    #[tokio::test]
    async fn concurrent_run_submit_vs_close_or_delete_and_idempotent_replay() {
        let tmp = TempDir::new("sess-race-10");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);

        // Case A: concurrent run.submit vs session.close (never both succeeding
        // with an active run inside a closed session).
        let (_pid, sid_a, rev_a) = seeded(&rt, &iid, "racea").await;
        let submit_a = rt.dispatch(
            "run.submit",
            json!({
                "instanceId": iid,
                "sessionId": sid_a,
                "input": [{"kind": "text", "text": "[pd-driver:sleep=60000]"}],
            }),
            Some("race-submit-a"),
            None,
        );
        let close_a = rt.dispatch(
            "session.close",
            json!({"instanceId": iid, "sessionId": sid_a, "expectedRevision": rev_a}),
            None,
            None,
        );
        let (s_res, c_res) = tokio::join!(submit_a, close_a);
        match (s_res, c_res) {
            (Ok(_), Err(c_err)) => {
                assert_eq!(contract_code(&c_err), "ACTIVE_RESOURCES");
            }
            (Err(s_err), Ok(_)) => {
                assert_eq!(contract_code(&s_err), "SESSION_CLOSED");
            }
            other => panic!("must be mutually exclusive, got {other:?}"),
        }

        // Case B: idempotent replay of an accepted run.submit after session.close
        // and session.delete must return the original run receipt rather than
        // SESSION_CLOSED or NOT_FOUND.
        let (_pid, sid_b, rev_b) = seeded(&rt, &iid, "raceb").await;
        let (_, accepted) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid_b,
                    "input": [{"kind": "text", "text": "quick"}],
                }),
                Some("replay-key-b"),
                None,
            )
            .await
            .unwrap();
        let run_id = accepted["runId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "run.wait",
            json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
            None,
            None,
        )
        .await
        .unwrap();

        let (_, closed) = rt
            .dispatch(
                "session.close",
                json!({"instanceId": iid, "sessionId": sid_b, "expectedRevision": rev_b}),
                None,
                None,
            )
            .await
            .unwrap();
        let closed_rev = closed["session"]["revision"].as_str().unwrap().to_owned();

        let (_, replay_after_close) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid_b,
                    "input": [{"kind": "text", "text": "quick"}],
                }),
                Some("replay-key-b"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay_after_close["runId"], run_id);

        rt.dispatch(
            "session.delete",
            json!({"instanceId": iid, "sessionId": sid_b, "expectedRevision": closed_rev}),
            None,
            None,
        )
        .await
        .unwrap();
        let (_, replay_after_delete) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid_b,
                    "input": [{"kind": "text", "text": "quick"}],
                }),
                Some("replay-key-b"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay_after_delete["runId"], run_id);
    }

    #[tokio::test]
    async fn close_and_delete_block_on_active_ops_and_reopen_checks_project() {
        let tmp = TempDir::new("sess-res-16");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (pid, sid, rev) = seeded(&rt, &iid, "res16").await;

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO operations (operation_id, operation_type, scope_kind, project_id, session_id, state, revision, terminal, created_at, updated_at)
                 VALUES ('op-sess-1', 'search.start', 'session', ?1, ?2, 'running', 'rev-op', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                rusqlite::params![pid, sid],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "session.close",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "ACTIVE_RESOURCES");
        let err = rt
            .dispatch(
                "session.delete",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "ACTIVE_RESOURCES");

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE operations SET state = 'succeeded', terminal = 1 WHERE operation_id = 'op-sess-1'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO v1_schedules (schedule_id, task_id, project_id, session_id, kind, expression, timezone, enabled, message, channel_id, revision, created_at, updated_at)
                 VALUES ('sched-sess-1', 'task-s', ?1, ?2, 'cron', '* * * * *', 'UTC', 1, 'hi', NULL, 'srev-1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                rusqlite::params![pid, sid],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "session.close",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "ACTIVE_RESOURCES");

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE v1_schedules SET enabled = 0 WHERE schedule_id = 'sched-sess-1'",
                [],
            )
            .unwrap();
        }
        let (_, closed) = rt
            .dispatch(
                "session.close",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap();
        let closed_rev = closed["session"]["revision"].as_str().unwrap().to_owned();

        // Removing the parent project prevents session.reopen.
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "DELETE FROM projects WHERE project_id = ?1",
                rusqlite::params![pid],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "session.reopen",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": closed_rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }
}
