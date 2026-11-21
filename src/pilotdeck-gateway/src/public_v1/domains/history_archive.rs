//! `history.*` and `archive.*` (`t012`): session history reading, forking,
//! export/restore archives.
//!
//! Data model (design decision, registered in the task report):
//! - `history_entries` is the authoritative per-session conversation log
//!   (entryId/turnId/role/content parts/runId/toolCallId, ordered by a
//!   per-session `seq`). The run/turn projection that writes it lands with
//!   the D20 wiring tasks; this file owns the storage API
//!   ([`V1Store::append_history_entries`]) those projections will call.
//! - `history.export` is an async operation (`retry=idempotentSubmit`): the
//!   submit transaction durably creates the archive row (state `queued`),
//!   the operation row, and the idempotency receipt; a background task
//!   snapshots the entries, writes the archive payload under
//!   `<home>/v1/archives/<archiveId>/` and flips both rows to `succeeded`.
//! - `history.restore` validates the archive (state, blobs, hashes) before
//!   creating the target session, then copies the entries in one
//!   transaction — a bad archive fails before anything is published.
//! - `archive.get` / `archive.entries` are sync reads; an archive missing
//!   its blobs answers `ARCHIVE_INCOMPLETE` (`缺blob明确不完整`), and the
//!   artifacts they expose are the archive's own payload descriptors — no
//!   raw storage paths ever leave the gateway.

use std::path::PathBuf;
use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_RESOURCE_LIMIT, CODE_REVISION_CONFLICT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
#[cfg(unix)]
use crate::public_v1::domains::files;
use crate::public_v1::domains::workspace_dirs::{
    CODE_ACTIVE_RESOURCES, bump_rev_in_tx, domain_not_found, sql_err,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// Error codes declared by the `history.*` / `archive.*` contracts that the
/// protocol crate has no constants for yet.
pub(crate) const CODE_CORRUPT_HISTORY: &str = "CORRUPT_HISTORY";
pub(crate) const CODE_ARCHIVE_INCOMPLETE: &str = "ARCHIVE_INCOMPLETE";
pub(crate) const CODE_INVALID_HISTORY_BOUNDARY: &str = "INVALID_HISTORY_BOUNDARY";

const HISTORY_ROLES: [&str; 4] = ["system", "user", "assistant", "tool"];
/// 归档清单与载荷读取安全上限（防恶意超大文件 OOM）。
const MAX_ARCHIVE_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVE_BLOB_BYTES: u64 = 256 * 1024 * 1024;

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "history_entries",
        "CREATE TABLE IF NOT EXISTS history_entries (
            session_id TEXT NOT NULL,
            entry_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            turn_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content_json TEXT NOT NULL,
            run_id TEXT,
            tool_call_id TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (session_id, entry_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_history_entries_session_seq
            ON history_entries(session_id, seq);",
    );
    reg.schema(
        "history_archive_operations",
        "CREATE TABLE IF NOT EXISTS history_archive_operations (
            operation_id TEXT PRIMARY KEY,
            method_id TEXT NOT NULL,
            state TEXT NOT NULL,
            input_json TEXT NOT NULL,
            result_json TEXT,
            error_code TEXT,
            error_message TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    );
    reg.schema(
        "archives",
        "CREATE TABLE IF NOT EXISTS archives (
            archive_id TEXT PRIMARY KEY,
            operation_id TEXT NOT NULL,
            source_session_id TEXT NOT NULL,
            source_revision TEXT NOT NULL,
            format TEXT NOT NULL,
            state TEXT NOT NULL,
            entry_count INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    );
    reg.schema(
        "archive_entries",
        "CREATE TABLE IF NOT EXISTS archive_entries (
            archive_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            entry_id TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content_json TEXT NOT NULL,
            run_id TEXT,
            tool_call_id TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (archive_id, position)
        );",
    );
    reg.schema(
        "archive_export_snapshots",
        "CREATE TABLE IF NOT EXISTS archive_export_snapshots (
            archive_id TEXT PRIMARY KEY,
            source_session_id TEXT NOT NULL,
            source_revision TEXT NOT NULL,
            max_seq INTEGER NOT NULL
        );",
    );
    reg.handler("history.read", |rt, input, _key, _rev| {
        Box::pin(history_read(rt, input))
    });
    reg.handler("history.fork", |rt, input, _key, _rev| {
        Box::pin(history_fork(rt, input))
    });
    reg.handler("history.export", |rt, input, key, _rev| {
        Box::pin(history_export(rt, input, key))
    });
    reg.handler("history.restore", |rt, input, key, _rev| {
        Box::pin(history_restore(rt, input, key))
    });
    reg.handler("archive.get", |rt, input, _key, _rev| {
        Box::pin(archive_get(rt, input))
    });
    reg.handler("archive.entries", |rt, input, _key, _rev| {
        Box::pin(archive_entries(rt, input))
    });
}

// ---------------------------------------------------------------------------
// Entry validation and row projection
// ---------------------------------------------------------------------------

fn invalid_argument(msg: String) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        msg,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn corrupt_history(msg: String) -> ContractError {
    ContractError::new(
        CODE_CORRUPT_HISTORY,
        msg,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// Validates one history entry's wire shape: role within the contract enum
/// and `content` an array of `text` / `attachment` parts.
fn validate_history_entry(entry: &Value) -> Result<(String, String, String), ContractError> {
    let turn_id = entry["turnId"].as_str().unwrap_or_default().to_owned();
    if turn_id.is_empty() {
        return Err(invalid_argument("history entry requires turnId".to_owned()));
    }
    let role = entry["role"].as_str().unwrap_or_default().to_owned();
    if !HISTORY_ROLES.contains(&role.as_str()) {
        return Err(invalid_argument(format!("invalid history role `{role}`")));
    }
    let Some(parts) = entry["content"].as_array() else {
        return Err(invalid_argument(
            "history entry content must be an array".to_owned(),
        ));
    };
    for part in parts {
        match part["kind"].as_str() {
            Some("text") => {
                if part.get("text").and_then(Value::as_str).is_none() {
                    return Err(invalid_argument("text part requires text".to_owned()));
                }
            }
            Some("attachment") => {
                if part["artifactId"].as_str().unwrap_or_default().is_empty() {
                    return Err(invalid_argument(
                        "attachment part requires artifactId".to_owned(),
                    ));
                }
            }
            _ => {
                return Err(invalid_argument(
                    "history part must be kind text or attachment".to_owned(),
                ));
            }
        }
    }
    Ok((
        turn_id,
        role,
        serde_json::to_string(&parts).unwrap_or_else(|_| "[]".into()),
    ))
}

/// Projects one stored row into the contract `History` item, refusing
/// corrupted rows explicitly (`坏记录明确报错`).
fn history_item(
    entry_id: &str,
    turn_id: &str,
    role: &str,
    content_json: &str,
    run_id: Option<&str>,
    tool_call_id: Option<&str>,
    created_at: &str,
) -> Result<Value, ContractError> {
    if !HISTORY_ROLES.contains(&role) {
        return Err(corrupt_history(format!(
            "history entry `{entry_id}` has invalid role `{role}`"
        )));
    }
    let content: Value = serde_json::from_str(content_json).map_err(|e| {
        corrupt_history(format!(
            "history entry `{entry_id}` has unreadable content: {e}"
        ))
    })?;
    let Some(parts) = content.as_array() else {
        return Err(corrupt_history(format!(
            "history entry `{entry_id}` content is not an array"
        )));
    };
    for part in parts {
        match part["kind"].as_str() {
            Some("text") if part.get("text").and_then(Value::as_str).is_some() => {}
            Some("attachment") if !part["artifactId"].as_str().unwrap_or_default().is_empty() => {}
            _ => {
                return Err(corrupt_history(format!(
                    "history entry `{entry_id}` has a malformed content part"
                )));
            }
        }
    }
    Ok(json!({
        "entryId": entry_id,
        "turnId": turn_id,
        "role": role,
        "content": content,
        "runId": run_id,
        "toolCallId": tool_call_id,
        "createdAt": created_at,
    }))
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("history/archive worker task failed: {err}"),
        data: None,
    }
}

/// 启动崩溃恢复（供 `store.rs::sync_meta_and_recover` 在启动恢复事务中调用）：
/// 将残留非终态 `history_archive_operations` 标为 `interrupted`（`GATEWAY_RESTART`），
/// 并将未完成 `archives` 标为 `failed`。
pub(crate) fn recover_orphaned_history_archive_operations(
    conn: &Connection,
    now: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE history_archive_operations
         SET state = 'interrupted',
             error_code = 'GATEWAY_RESTART',
             error_message = 'gateway restarted while operation was running',
             updated_at = ?1
         WHERE state IN ('queued', 'running')",
        params![now],
    )
    .map_err(sql_err)?;
    conn.execute(
        "UPDATE archives SET state = 'failed', updated_at = ?1
         WHERE state IN ('queued', 'running')",
        params![now],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// history.read
// ---------------------------------------------------------------------------

async fn history_read(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = tokio::task::spawn_blocking(move || store.read_history_page(&input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, result))
}

impl V1Store {
    /// Appends conversation entries to a session's history (the projection
    /// API the run/turn wiring will call; tests use it to seed fixtures).
    /// Entry ids and per-session sequence numbers are assigned here; the
    /// returned value echoes the stored items.
    pub fn append_history_entries(
        &self,
        session_id: &str,
        entries: &[Value],
    ) -> Result<Value, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT session_state FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match state.as_deref() {
            None => return Err(domain_not_found("session", session_id)),
            Some("deleted") => return Err(domain_not_found("session", session_id)),
            Some(_) => {}
        }
        let mut max_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM history_entries WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let now = now_rfc3339();
        let mut items = Vec::with_capacity(entries.len());
        for entry in entries {
            let (turn_id, role, content_json) = validate_history_entry(entry)?;
            let entry_id = self.next_id();
            max_seq += 1;
            let created_at = entry
                .get("createdAt")
                .and_then(Value::as_str)
                .unwrap_or(&now)
                .to_owned();
            tx.execute(
                "INSERT INTO history_entries
                 (session_id, entry_id, seq, turn_id, role, content_json, run_id, tool_call_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    entry_id,
                    max_seq,
                    turn_id,
                    role,
                    content_json,
                    entry.get("runId").and_then(Value::as_str),
                    entry.get("toolCallId").and_then(Value::as_str),
                    created_at,
                ],
            )
            .map_err(sql_err)?;
            items.push(history_item(
                &entry_id,
                &turn_id,
                &role,
                &content_json,
                entry.get("runId").and_then(Value::as_str),
                entry.get("toolCallId").and_then(Value::as_str),
                &created_at,
            )?);
        }
        tx.commit().map_err(sql_err)?;
        Ok(json!({"items": items}))
    }

    /// `history.read`: paged, optionally anchored at `fromEntryId`. The
    /// cursor is opaque (`h<seq>`, exclusive); cursors below the session's
    /// retained window answer `CURSOR_EXPIRED`. Open and closed sessions
    /// are readable; deleted sessions are not (`NOT_FOUND`).
    pub(crate) fn read_history_page(&self, input: &Value) -> Result<Value, ContractError> {
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let from_entry_id = input.get("fromEntryId").and_then(Value::as_str);
        let page = input.get("page");
        let obj = page.and_then(Value::as_object);
        let limit = obj
            .and_then(|p| p.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;

        let conn = self.conn.lock().expect("sqlite mutex");
        let state: Option<String> = conn
            .query_row(
                "SELECT session_state FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match state.as_deref() {
            None => return Err(domain_not_found("session", session_id)),
            Some("deleted") => return Err(domain_not_found("session", session_id)),
            Some(_) => {}
        }

        let after_seq: i64 = if let Some(cursor) =
            obj.and_then(|p| p.get("cursor")).and_then(Value::as_str)
        {
            let seq = cursor
                .strip_prefix('h')
                .and_then(|rest| rest.parse::<i64>().ok())
                .ok_or_else(|| invalid_argument(format!("invalid history cursor `{cursor}`")))?;
            let earliest: i64 = conn
                .query_row(
                    "SELECT COALESCE(MIN(seq) - 1, 0) FROM history_entries WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            if seq < earliest {
                return Err(ContractError::new(
                    agentcpm_pd_protocol::errors::CODE_CURSOR_EXPIRED,
                    format!("history cursor `{cursor}` points before the retained window"),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                ));
            }
            seq
        } else if let Some(anchor) = from_entry_id {
            // `fromEntryId` must belong to this session ("不属于会话的
            // fromEntryId拒绝") and reads inclusively from that entry.
            let seq: Option<i64> = conn
                .query_row(
                    "SELECT seq FROM history_entries WHERE session_id = ?1 AND entry_id = ?2",
                    params![session_id, anchor],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            match seq {
                Some(seq) => seq - 1,
                None => {
                    return Err(invalid_argument(format!(
                        "fromEntryId `{anchor}` does not belong to session `{session_id}`"
                    )));
                }
            }
        } else {
            0
        };

        let snapshot_revision = current_revision(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT seq, entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at
                 FROM history_entries WHERE session_id = ?1 AND seq > ?2
                 ORDER BY seq ASC LIMIT ?3",
            )
            .map_err(sql_err)?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            i64,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = stmt
            .query_map(params![session_id, after_seq, (limit + 1) as i64], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        let complete = rows.len() <= limit;
        let page_rows: Vec<_> = rows.into_iter().take(limit).collect();
        let mut items = Vec::with_capacity(page_rows.len());
        let mut last_seq = after_seq;
        for (seq, entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at) in
            page_rows
        {
            items.push(history_item(
                &entry_id,
                &turn_id,
                &role,
                &content_json,
                run_id.as_deref(),
                tool_call_id.as_deref(),
                &created_at,
            )?);
            last_seq = seq;
        }
        let next_cursor = if complete {
            Value::Null
        } else {
            json!(format!("h{last_seq}"))
        };
        Ok(json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": snapshot_revision,
                "nextCursor": next_cursor,
                "complete": complete,
            }
        }))
    }
}

fn current_revision(conn: &Connection) -> Result<String, ContractError> {
    let cur: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(format!(
        "rev-{:08}",
        cur.and_then(|v| v.parse::<i64>().ok()).unwrap_or(1)
    ))
}

// ---------------------------------------------------------------------------
// history.fork
// ---------------------------------------------------------------------------

async fn history_fork(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = tokio::task::spawn_blocking(move || store.fork_session_history(&input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, result))
}

impl V1Store {
    /// `history.fork`: copies the source session's entries through
    /// `throughEntryId` into a brand-new session in one transaction.
    /// Invalid boundaries (`INVALID_HISTORY_BOUNDARY`) and concurrent
    /// writes (`REVISION_CONFLICT` CAS on the source) leave zero rows
    /// behind; the two sessions evolve independently afterwards.
    pub(crate) fn fork_session_history(&self, input: &Value) -> Result<Value, ContractError> {
        let source_session_id = input["sessionId"].as_str().unwrap_or_default();
        let through_entry_id = input["throughEntryId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let new_session_id = input["newSessionId"].as_str().unwrap_or_default();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let source: Option<(String, String, String, String)> = tx
            .query_row(
                "SELECT project_id, name, tags_json, revision FROM sessions WHERE session_id = ?1",
                params![source_session_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((project_id, source_name, tags_json, revision)) = source else {
            return Err(domain_not_found("session", source_session_id));
        };
        let state: String = tx
            .query_row(
                "SELECT session_state FROM sessions WHERE session_id = ?1",
                params![source_session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if state == "deleted" {
            return Err(domain_not_found("session", source_session_id));
        }
        if revision != expected {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!("session expectedRevision `{expected}` does not match `{revision}`"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        // The boundary must be an entry of this session.
        let through_seq: Option<i64> = tx
            .query_row(
                "SELECT seq FROM history_entries WHERE session_id = ?1 AND entry_id = ?2",
                params![source_session_id, through_entry_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(through_seq) = through_seq else {
            return Err(ContractError::new(
                CODE_INVALID_HISTORY_BOUNDARY,
                format!(
                    "throughEntryId `{through_entry_id}` is not an entry of session `{source_session_id}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };

        let target_exists: Option<String> = tx
            .query_row(
                "SELECT session_id FROM sessions WHERE session_id = ?1",
                params![new_session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if target_exists.is_some() {
            return Err(ContractError::new(
                CODE_ACTIVE_RESOURCES,
                format!("target session `{new_session_id}` already exists"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        // One transaction: new session row + copied entries + consumed CAS
        // token on the source. Everything before this point read only.
        let rev = bump_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("⑂ {source_name}"));
        tx.execute(
            "INSERT INTO sessions
             (session_id, project_id, name, tags_json, revision, active_run_id, session_state,
              queue_paused, pause_reason, queue_revision, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'open', 0, NULL, ?6, ?7)",
            params![
                new_session_id,
                project_id,
                name,
                tags_json,
                rev,
                bump_rev_in_tx(&tx)?,
                now
            ],
        )
        .map_err(sql_err)?;
        tx.execute(
            "INSERT INTO history_entries
             (session_id, entry_id, seq, turn_id, role, content_json, run_id, tool_call_id, created_at)
             SELECT ?1, entry_id, seq, turn_id, role, content_json, run_id, tool_call_id, created_at
             FROM history_entries WHERE session_id = ?2 AND seq <= ?3",
            params![new_session_id, source_session_id, through_seq],
        )
        .map_err(sql_err)?;
        tx.execute(
            "UPDATE sessions SET revision = ?1 WHERE session_id = ?2",
            params![bump_rev_in_tx(&tx)?, source_session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;

        Ok(json!({
            "session": {
                "sessionId": new_session_id,
                "projectId": project_id,
                "name": name,
                "tags": serde_json::from_str::<Value>(&tags_json).unwrap_or(json!([])),
                "revision": rev,
                "activeRunId": Value::Null,
                "sessionState": "open",
            },
            "sourceSessionId": source_session_id,
            "throughEntryId": through_entry_id,
        }))
    }
}

// ---------------------------------------------------------------------------
// history.export / history.restore (async operations)
// ---------------------------------------------------------------------------

enum AsyncOpDisposition {
    New {
        receipt: Value,
        operation_id: String,
    },
    Deduplicated {
        receipt: Value,
    },
}

/// The common idempotent-submit prologue: replay-or-conflict for a known
/// key, then the caller's validation closure records rejections and mints
/// the durable rows.
fn submit_async_operation(
    store: &V1Store,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    validate: impl FnOnce(&Connection) -> Result<(), ContractError>,
) -> Result<AsyncOpDisposition, ContractError> {
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    let retained_until = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;

    if let Some(key) = idempotency_key {
        let existing: Option<(String, String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT input_digest, request_state, receipt_json, rejection_code
                 FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
                params![key, method_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((stored_digest, req_state, receipt_json, rejection_code)) = existing {
            if stored_digest != digest {
                return Err(ContractError::new(
                    CODE_IDEMPOTENCY_CONFLICT,
                    format!(
                        "idempotencyKey `{key}` already used for {method_id} with different input"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if req_state == "accepted"
                && let Some(raw) = receipt_json
                && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
            {
                if let Some(obj) = receipt.as_object_mut() {
                    obj.insert("deduplicated".into(), json!(true));
                }
                return Ok(AsyncOpDisposition::Deduplicated { receipt });
            }
            return Err(ContractError::new(
                rejection_code.unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
                "request previously rejected definitively",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }

    if let Err(error) = validate(&tx) {
        if let Some(key) = idempotency_key {
            let _ = tx.execute(
                "INSERT OR IGNORE INTO idempotency_records
                 (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                  receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6,
                         'correctRejectedInputWithNewKey')",
                params![key, method_id, digest, error.code, now, retained_until],
            );
            let _ = tx.commit();
        }
        return Err(error);
    }

    let operation_id = store.next_id();
    let rev = bump_rev_in_tx(&tx)?;
    tx.execute(
        "INSERT INTO history_archive_operations
         (operation_id, method_id, state, input_json, created_at, updated_at)
         VALUES (?1, ?2, 'queued', ?3, ?4, ?4)",
        params![
            operation_id,
            method_id,
            serde_json::to_string(input).unwrap_or_default(),
            now
        ],
    )
    .map_err(sql_err)?;
    if let Err(error) = create_operation_resource(&tx, store, method_id, &operation_id, &now) {
        let _ = tx.execute(
            "DELETE FROM history_archive_operations WHERE operation_id = ?1",
            params![operation_id],
        );
        if let Some(key) = idempotency_key {
            let _ = tx.execute(
                "INSERT OR IGNORE INTO idempotency_records
                 (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                  receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6,
                         'correctRejectedInputWithNewKey')",
                params![key, method_id, digest, error.code, now, retained_until],
            );
            let _ = tx.commit();
        }
        return Err(error);
    }

    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": rev,
        "deduplicated": false,
        "state": "queued",
    });
    if let Some(key) = idempotency_key {
        tx.execute(
            "INSERT INTO idempotency_records
             (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
              receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
             VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')",
            params![
                key,
                method_id,
                digest,
                serde_json::to_string(&json!({"kind": "operation", "operationId": operation_id}))
                    .unwrap_or_default(),
                serde_json::to_string(&receipt).unwrap_or_default(),
                now,
                retained_until,
            ],
        )
        .map_err(sql_err)?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(AsyncOpDisposition::New {
        receipt,
        operation_id,
    })
}

/// Method-specific durable resource creation inside the submit transaction
/// (the archive row for `history.export`; nothing extra for restore).
fn create_operation_resource(
    tx: &Connection,
    store: &V1Store,
    method_id: &str,
    operation_id: &str,
    now: &str,
) -> Result<(), ContractError> {
    if method_id != "history.export" {
        return Ok(());
    }
    let input: Value = tx
        .query_row(
            "SELECT input_json FROM history_archive_operations WHERE operation_id = ?1",
            params![operation_id],
            |r| {
                let raw: String = r.get(0)?;
                Ok(serde_json::from_str(&raw).unwrap_or(Value::Null))
            },
        )
        .map_err(sql_err)?;
    let session_id = input["sessionId"].as_str().unwrap_or_default();
    let format = input
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("structured");
    let archive_id = store.next_id();
    let revision: String = tx
        .query_row(
            "SELECT revision FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    let max_seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM history_entries WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    tx.execute(
        "INSERT INTO archives
         (archive_id, operation_id, source_session_id, source_revision, format, state, entry_count, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 0, ?6, ?6)",
        params![archive_id, operation_id, session_id, revision, format, now],
    )
    .map_err(sql_err)?;
    tx.execute(
        "INSERT INTO archive_export_snapshots
         (archive_id, source_session_id, source_revision, max_seq)
         VALUES (?1, ?2, ?3, ?4)",
        params![archive_id, session_id, revision, max_seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

async fn history_export(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let store_sub = store.clone();
    let key_sub = idempotency_key;
    let submission = tokio::task::spawn_blocking(move || {
        submit_history_export(&store_sub, &input, key_sub.as_deref())
    })
    .await
    .map_err(join_err)??;
    match submission {
        AsyncOpDisposition::Deduplicated { receipt } => Ok((ResponseOutcome::Accepted, receipt)),
        AsyncOpDisposition::New {
            receipt,
            operation_id,
        } => {
            let worker_store = store.clone();
            let worker_op = operation_id.clone();
            tokio::task::spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    execute_history_export(&worker_store, &worker_op)
                })
                .await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = fail_operation(&store, &operation_id, &err.code, &err.message);
                        })
                        .await;
                    }
                    Err(join_err) => {
                        let msg = format!("history.export worker panicked: {join_err}");
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = fail_operation(&store, &operation_id, CODE_UNAVAILABLE, &msg);
                        })
                        .await;
                    }
                }
            });
            Ok((ResponseOutcome::Accepted, receipt))
        }
    }
}

fn submit_history_export(
    store: &V1Store,
    input: &Value,
    idempotency_key: Option<&str>,
) -> Result<AsyncOpDisposition, ContractError> {
    let session_id = input["sessionId"].as_str().unwrap_or_default();
    let expected = input["expectedRevision"].as_str().unwrap_or_default();
    submit_async_operation(store, "history.export", input, idempotency_key, |tx| {
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT session_state, revision FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        match row {
            None => Err(domain_not_found("session", session_id)),
            Some((state, revision)) => {
                if state == "deleted" {
                    return Err(domain_not_found("session", session_id));
                }
                if revision != expected {
                    return Err(ContractError::new(
                        CODE_REVISION_CONFLICT,
                        format!(
                            "session expectedRevision `{expected}` does not match `{revision}`"
                        ),
                        RetrySpec::after_reconcile(),
                        EffectStatus::NotApplied,
                    ));
                }
                Ok(())
            }
        }
    })
}

/// Marks the operation (and, for exports, its archive) failed if still non-terminal.
/// Never called while holding the store mutex.
fn fail_operation(store: &V1Store, operation_id: &str, code: &str, message: &str) -> ContractError {
    let conn = store.conn.lock().expect("sqlite mutex");
    let now = now_rfc3339();
    let _ = conn.execute(
        "UPDATE history_archive_operations
         SET state = 'failed', error_code = ?1, error_message = ?2, updated_at = ?3
         WHERE operation_id = ?4 AND state IN ('queued', 'running')",
        params![code, message, now, operation_id],
    );
    let _ = conn.execute(
        "UPDATE archives SET state = 'failed', updated_at = ?1
         WHERE operation_id = ?2 AND state IN ('queued', 'running')",
        params![now, operation_id],
    );
    ContractError::new(
        code,
        message.to_owned(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// Claims a queued operation for execution; exactly-once per operation row.
fn claim_operation(store: &V1Store, operation_id: &str) -> Result<bool, ContractError> {
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let state: Option<String> = tx
        .query_row(
            "SELECT state FROM history_archive_operations WHERE operation_id = ?1",
            params![operation_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if state.as_deref() != Some("queued") {
        return Ok(false);
    }
    tx.execute(
        "UPDATE history_archive_operations SET state = 'running', updated_at = ?1
         WHERE operation_id = ?2",
        params![now_rfc3339(), operation_id],
    )
    .map_err(sql_err)?;
    tx.commit().map_err(sql_err)?;
    Ok(true)
}

/// One raw history row as read from `history_entries` / `archive_entries`.
type RawEntry = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);

const ENTRY_COLUMNS: &str =
    "entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at";

fn read_session_history_snapshot(
    store: &V1Store,
    session_id: &str,
    expected_revision: &str,
    max_seq: Option<i64>,
) -> Result<Vec<RawEntry>, ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let current: Option<(String, String)> = conn
        .query_row(
            "SELECT session_state, revision FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    match current {
        None => {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!("source session `{session_id}` no longer exists"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        Some((state, _)) if state == "deleted" => {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!("source session `{session_id}` was deleted before export"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        Some((_, revision)) if revision != expected_revision => {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "source session revision changed from `{expected_revision}` to `{revision}`"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        Some(_) => {}
    }
    match max_seq {
        Some(limit_seq) => {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM history_entries
                     WHERE session_id = ?1 AND seq <= ?2 ORDER BY seq ASC"
                ))
                .map_err(sql_err)?;
            stmt.query_map(params![session_id, limit_seq], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)
        }
        None => {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM history_entries
                     WHERE session_id = ?1 ORDER BY seq ASC"
                ))
                .map_err(sql_err)?;
            stmt.query_map(params![session_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)
        }
    }
}

/// Executes one accepted `history.export`: verify session revision/deletion and
/// fixed `max_seq` snapshot, write archive payload via pinned NOFOLLOW atomic write,
/// and publish `completionResult` `{archiveId, artifact, sourceRevision, entryCount}`.
fn execute_history_export(store: &V1Store, operation_id: &str) -> Result<(), ContractError> {
    if !claim_operation(store, operation_id)? {
        return Ok(());
    }
    #[allow(clippy::type_complexity)]
    let archive_row: Option<(String, String, String, String, Option<i64>)> = {
        let conn = store.conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT a.archive_id, a.source_session_id, a.source_revision, a.format, s.max_seq
             FROM archives a
             LEFT JOIN archive_export_snapshots s ON s.archive_id = a.archive_id
             WHERE a.operation_id = ?1",
            params![operation_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(sql_err)?
    };
    let Some((archive_id, session_id, source_revision, format, max_seq)) = archive_row else {
        return Err(fail_operation(
            store,
            operation_id,
            CODE_INVALID_ARGUMENT,
            "export operation has no archive row",
        ));
    };

    // 固定快照（受理时锁定的 max_seq）并复核 session revision/非删除；失配即失败。
    let raw = match read_session_history_snapshot(store, &session_id, &source_revision, max_seq) {
        Ok(rows) => rows,
        Err(error) => {
            return Err(fail_operation(
                store,
                operation_id,
                &error.code,
                &error.message,
            ));
        }
    };
    let mut rows: Vec<Value> = Vec::with_capacity(raw.len());
    for (entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at) in &raw {
        match history_item(
            entry_id,
            turn_id,
            role,
            content_json,
            run_id.as_deref(),
            tool_call_id.as_deref(),
            created_at,
        ) {
            Ok(item) => rows.push(item),
            Err(error) => {
                return Err(fail_operation(
                    store,
                    operation_id,
                    CODE_INVALID_ARGUMENT,
                    &format!("history is corrupt: {}", error.message),
                ));
            }
        }
    }

    let entries_payload = json!({
        "schemaVersion": 1,
        "kind": "agentcpm-pd.archive.entries/1",
        "archiveId": archive_id,
        "sourceSessionId": session_id,
        "entries": rows,
    });
    let entries_bytes = serde_json::to_vec_pretty(&entries_payload).map_err(|e| {
        fail_operation(
            store,
            operation_id,
            CODE_INVALID_ARGUMENT,
            &format!("serialize entries: {e}"),
        )
    })?;
    let entries_sha = sha256_hex(&entries_bytes);
    let entries_artifact = json!({
        "artifactId": format!("{archive_id}.entries"),
        "mediaType": "application/json",
        "bytes": entries_bytes.len(),
        "sha256": entries_sha,
    });
    let mut artifacts = vec![entries_artifact];
    if format == "markdown" {
        let markdown = render_markdown(&session_id, &rows);
        let md_bytes = markdown.into_bytes();
        let md_sha = sha256_hex(&md_bytes);
        let md_artifact = json!({
            "artifactId": format!("{archive_id}.transcript.md"),
            "mediaType": "text/markdown",
            "bytes": md_bytes.len(),
            "sha256": md_sha,
        });
        if let Err(error) =
            write_archive_file_nofollow(store, &archive_id, "transcript.md", &md_bytes)
        {
            return Err(fail_operation(
                store,
                operation_id,
                &error.code,
                &format!("write transcript.md failed: {}", error.message),
            ));
        }
        artifacts.push(md_artifact);
    }
    if let Err(error) =
        write_archive_file_nofollow(store, &archive_id, "entries.json", &entries_bytes)
    {
        return Err(fail_operation(
            store,
            operation_id,
            &error.code,
            &format!("write entries.json failed: {}", error.message),
        ));
    }

    let manifest = json!({
        "schemaVersion": 1,
        "kind": "agentcpm-pd.archive.manifest/1",
        "archiveId": archive_id,
        "sourceSessionId": session_id,
        "format": format,
        "entryCount": rows.len(),
        "artifacts": artifacts,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| fail_operation(store, operation_id, CODE_INVALID_ARGUMENT, &format!("{e}")))?;
    if let Err(error) =
        write_archive_file_nofollow(store, &archive_id, "manifest.json", &manifest_bytes)
    {
        return Err(fail_operation(
            store,
            operation_id,
            &error.code,
            &format!("write manifest.json failed: {}", error.message),
        ));
    }
    let manifest_artifact = json!({
        "artifactId": format!("{archive_id}.manifest"),
        "mediaType": "application/json",
        "bytes": manifest_bytes.len(),
        "sha256": sha256_hex(&manifest_bytes),
    });

    // Publish: archive rows in one Immediate transaction.
    {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        for (position, row) in raw.iter().enumerate() {
            tx.execute(
                "INSERT INTO archive_entries
                 (archive_id, position, entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    archive_id,
                    (position + 1) as i64,
                    row.0,
                    row.1,
                    row.2,
                    row.3,
                    row.4,
                    row.5,
                    row.6,
                ],
            )
            .map_err(sql_err)?;
        }
        let count = raw.len() as i64;
        let now = now_rfc3339();
        tx.execute(
            "UPDATE archives SET state = 'succeeded', entry_count = ?1, updated_at = ?2
             WHERE archive_id = ?3",
            params![count, now, archive_id],
        )
        .map_err(sql_err)?;
        // P1#14：history.export completionResult 对齐 cli-v1-operations.json schema：
        // required ["archiveId", "artifact", "sourceRevision", "entryCount"]，additionalProperties=false。
        let result = json!({
            "archiveId": archive_id,
            "artifact": manifest_artifact,
            "sourceRevision": source_revision,
            "entryCount": count,
        });
        tx.execute(
            "UPDATE history_archive_operations
             SET state = 'succeeded', result_json = ?1, updated_at = ?2
             WHERE operation_id = ?3 AND state = 'running'",
            params![
                serde_json::to_string(&result).unwrap_or_default(),
                now,
                operation_id
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
    }
    Ok(())
}

async fn history_restore(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let store_sub = store.clone();
    let key_sub = idempotency_key;
    let submission = tokio::task::spawn_blocking(move || {
        submit_history_restore(&store_sub, &input, key_sub.as_deref())
    })
    .await
    .map_err(join_err)??;
    match submission {
        AsyncOpDisposition::Deduplicated { receipt } => Ok((ResponseOutcome::Accepted, receipt)),
        AsyncOpDisposition::New {
            receipt,
            operation_id,
        } => {
            let worker_store = store.clone();
            let worker_op = operation_id.clone();
            tokio::task::spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    execute_history_restore(&worker_store, &worker_op)
                })
                .await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = fail_operation(&store, &operation_id, &err.code, &err.message);
                        })
                        .await;
                    }
                    Err(join_err) => {
                        let msg = format!("history.restore worker panicked: {join_err}");
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = fail_operation(&store, &operation_id, CODE_UNAVAILABLE, &msg);
                        })
                        .await;
                    }
                }
            });
            Ok((ResponseOutcome::Accepted, receipt))
        }
    }
}

fn submit_history_restore(
    store: &V1Store,
    input: &Value,
    idempotency_key: Option<&str>,
) -> Result<AsyncOpDisposition, ContractError> {
    let archive_id = input["archiveId"].as_str().unwrap_or_default();
    submit_async_operation(store, "history.restore", input, idempotency_key, |tx| {
        // The archive must exist; it may legitimately still be exporting
        // (state `queued`/`running`) and finish before this operation runs.
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM archives WHERE archive_id = ?1",
                params![archive_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match state {
            Some(_) => Ok(()),
            None => Err(ContractError::new(
                CODE_ARCHIVE_INCOMPLETE,
                format!("archive `{archive_id}` does not exist"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )),
        }
    })
}

/// Executes one accepted `history.restore`: verify the archive completely
/// (state, NOFOLLOW bounded reads, non-empty manifest/artifacts/64-hex hashes,
/// entries.json count + `archive_entries` validity) BEFORE creating session/entries.
fn execute_history_restore(store: &V1Store, operation_id: &str) -> Result<(), ContractError> {
    if !claim_operation(store, operation_id)? {
        return Ok(());
    }
    let input: Value = {
        let conn = store.conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT input_json FROM history_archive_operations WHERE operation_id = ?1",
            params![operation_id],
            |r| {
                let raw: String = r.get(0)?;
                Ok(serde_json::from_str(&raw).unwrap_or(Value::Null))
            },
        )
        .optional()
        .map_err(sql_err)?
        .unwrap_or(Value::Null)
    };
    let archive_id = input["archiveId"].as_str().unwrap_or_default();
    let project_id = input["projectId"].as_str().unwrap_or_default();
    let new_session_id = input["newSessionId"].as_str().unwrap_or_default();

    let archive_row = {
        let conn = store.conn.lock().expect("sqlite mutex");
        fetch_archive_row(&conn, archive_id)?
    };
    let Some(archive_meta) = archive_row else {
        return Err(fail_operation(
            store,
            operation_id,
            CODE_ARCHIVE_INCOMPLETE,
            &format!("archive `{archive_id}` does not exist"),
        ));
    };
    if archive_meta.state != "succeeded" {
        return Err(fail_operation(
            store,
            operation_id,
            CODE_ARCHIVE_INCOMPLETE,
            &format!("archive `{archive_id}` is not complete"),
        ));
    }

    // 在 DB 锁之外严格核查 manifest.json、全部声明 blob、SHA-256、entries.json 计数。
    if let Err(err) = verify_and_load_archive_artifacts(
        store,
        archive_id,
        &archive_meta.format,
        archive_meta.entry_count,
    ) {
        return Err(fail_operation(store, operation_id, &err.code, &err.message));
    }

    {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let project_exists: Option<String> = tx
            .query_row(
                "SELECT project_id FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if project_exists.is_none() {
            drop(tx);
            drop(conn);
            return Err(fail_operation(
                store,
                operation_id,
                CODE_INVALID_ARGUMENT,
                &format!("project `{project_id}` does not exist"),
            ));
        }
        let session_exists: Option<String> = tx
            .query_row(
                "SELECT session_id FROM sessions WHERE session_id = ?1",
                params![new_session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if session_exists.is_some() {
            drop(tx);
            drop(conn);
            return Err(fail_operation(
                store,
                operation_id,
                agentcpm_pd_protocol::errors::CODE_ALREADY_EXISTS,
                &format!("target session `{new_session_id}` already exists"),
            ));
        }

        // 校验全部 archive_entries 内容合法性后再创建目标会话与写入历史条目。
        let archived_rows: Vec<RawEntry> = {
            let mut stmt = tx
                .prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM archive_entries WHERE archive_id = ?1 ORDER BY position ASC"
                ))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![archive_id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            rows
        };
        if archived_rows.len() as i64 != archive_meta.entry_count {
            drop(tx);
            drop(conn);
            return Err(fail_operation(
                store,
                operation_id,
                CODE_ARCHIVE_INCOMPLETE,
                &format!("archive `{archive_id}` archive_entries count disagrees with archive row"),
            ));
        }
        let mut identity_mapping: Vec<Value> = Vec::with_capacity(archived_rows.len());
        for (entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at) in
            &archived_rows
        {
            if let Err(err) = history_item(
                entry_id,
                turn_id,
                role,
                content_json,
                run_id.as_deref(),
                tool_call_id.as_deref(),
                created_at,
            ) {
                drop(tx);
                drop(conn);
                return Err(fail_operation(
                    store,
                    operation_id,
                    &err.code,
                    &format!("archive entry `{entry_id}` is invalid: {}", err.message),
                ));
            }
            identity_mapping.push(json!({"key": entry_id, "value": entry_id}));
        }

        let rev = bump_rev_in_tx(&tx)?;
        let queue_rev = bump_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO sessions
             (session_id, project_id, name, tags_json, revision, active_run_id, session_state,
              queue_paused, pause_reason, queue_revision, created_at)
             VALUES (?1, ?2, ?1, '[]', ?3, NULL, 'open', 0, NULL, ?4, ?5)",
            params![new_session_id, project_id, rev, queue_rev, now],
        )
        .map_err(sql_err)?;
        tx.execute(
            "INSERT INTO history_entries
             (session_id, entry_id, seq, turn_id, role, content_json, run_id, tool_call_id, created_at)
             SELECT ?1, entry_id, position, turn_id, role, content_json, run_id, tool_call_id, created_at
             FROM archive_entries WHERE archive_id = ?2",
            params![new_session_id, archive_id],
        )
        .map_err(sql_err)?;
        // P1#14：history.restore completionResult 对齐 cli-v1-operations.json schema：
        // required ["session", "identityMapping"]，additionalProperties=false。
        let result = json!({
            "session": {
                "sessionId": new_session_id,
                "projectId": project_id,
                "name": new_session_id,
                "tags": [],
                "revision": rev,
                "activeRunId": Value::Null,
                "sessionState": "open",
            },
            "identityMapping": identity_mapping,
        });
        tx.execute(
            "UPDATE history_archive_operations
             SET state = 'succeeded', result_json = ?1, updated_at = ?2
             WHERE operation_id = ?3 AND state = 'running'",
            params![
                serde_json::to_string(&result).unwrap_or_default(),
                now,
                operation_id
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// archive.get / archive.entries
// ---------------------------------------------------------------------------

async fn archive_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = tokio::task::spawn_blocking(move || store.get_archive(&input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn archive_entries(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = tokio::task::spawn_blocking(move || store.read_archive_entries(&input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, result))
}

struct ArchiveRow {
    #[allow(dead_code)] // provenance for future audit views
    source_session_id: String,
    source_revision: String,
    format: String,
    state: String,
    entry_count: i64,
}

fn fetch_archive_row(
    conn: &Connection,
    archive_id: &str,
) -> Result<Option<ArchiveRow>, ContractError> {
    conn.query_row(
        "SELECT source_session_id, source_revision, format, state, entry_count
         FROM archives WHERE archive_id = ?1",
        params![archive_id],
        |r| {
            Ok(ArchiveRow {
                source_session_id: r.get(0)?,
                source_revision: r.get(1)?,
                format: r.get(2)?,
                state: r.get(3)?,
                entry_count: r.get(4)?,
            })
        },
    )
    .optional()
    .map_err(sql_err)
}

fn archive_incomplete(archive_id: &str, detail: &str) -> ContractError {
    ContractError::new(
        CODE_ARCHIVE_INCOMPLETE,
        format!("archive `{archive_id}` is incomplete: {detail}"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_archive_id(archive_id: &str) -> bool {
    !archive_id.is_empty()
        && archive_id != "."
        && archive_id != ".."
        && !archive_id.contains('/')
        && !archive_id.contains('\\')
        && !archive_id.contains('\0')
}

fn archive_dir(store: &V1Store, archive_id: &str) -> PathBuf {
    store.home().join("v1").join("archives").join(archive_id)
}

fn write_archive_file_nofollow(
    store: &V1Store,
    archive_id: &str,
    file: &str,
    bytes: &[u8],
) -> Result<(), ContractError> {
    if !valid_archive_id(archive_id) {
        return Err(invalid_argument(format!(
            "invalid archiveId `{archive_id}`"
        )));
    }
    let dir = archive_dir(store, archive_id);
    #[cfg(unix)]
    {
        crate::project_files::create_directories_beneath(store.home(), &dir).map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("{err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        let dir_fd =
            crate::project_files::open_directory_beneath(store.home(), &dir).map_err(|err| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("{err}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;
        files::atomic_write_at(&dir_fd, std::ffi::OsStr::new(file), bytes, 0o600)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir).map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("{err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        std::fs::write(dir.join(file), bytes).map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("{err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })
    }
}

fn read_archive_file_nofollow(
    store: &V1Store,
    archive_id: &str,
    file: &str,
    max_bytes: u64,
    missing_detail: &str,
) -> Result<Vec<u8>, ContractError> {
    if !valid_archive_id(archive_id) {
        return Err(domain_not_found("archive", archive_id));
    }
    let dir = archive_dir(store, archive_id);
    #[cfg(unix)]
    {
        let dir_fd = match crate::project_files::open_directory_beneath(store.home(), &dir) {
            Ok(fd) => fd,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(archive_incomplete(archive_id, missing_detail));
            }
            Err(err) => {
                return Err(archive_incomplete(
                    archive_id,
                    &format!("archive directory error: {err}"),
                ));
            }
        };
        files::read_file_bounded_at(&dir_fd, std::ffi::OsStr::new(file), max_bytes).map_err(|err| {
            match err.code.as_str() {
                agentcpm_pd_protocol::errors::CODE_NOT_FOUND => {
                    archive_incomplete(archive_id, missing_detail)
                }
                CODE_RESOURCE_LIMIT => err,
                _ => archive_incomplete(
                    archive_id,
                    &format!("invalid `{file}` blob: {}", err.message),
                ),
            }
        })
    }
    #[cfg(not(unix))]
    {
        use std::io::Read;
        let path = dir.join(file);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|_| archive_incomplete(archive_id, missing_detail))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(archive_incomplete(
                archive_id,
                &format!("`{file}` is not a regular file"),
            ));
        }
        if meta.len() > max_bytes {
            return Err(ContractError::new(
                CODE_RESOURCE_LIMIT,
                format!("archive `{file}` exceeds {max_bytes} bytes"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let mut f = std::fs::File::open(&path)
            .map_err(|_| archive_incomplete(archive_id, missing_detail))?;
        let mut buf = Vec::with_capacity(meta.len().min(max_bytes) as usize);
        f.by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|_| archive_incomplete(archive_id, missing_detail))?;
        if buf.len() as u64 > max_bytes {
            return Err(ContractError::new(
                CODE_RESOURCE_LIMIT,
                format!("archive `{file}` exceeds {max_bytes} bytes"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Ok(buf)
    }
}

/// 严格校验归档 manifest 与全部载荷 blob（NOFOLLOW + 字节上限 + 非空 artifacts +
/// 必含有效 64-hex sha256 + entries.json 计数一致 + 绝不吞错跳过）。
fn verify_and_load_archive_artifacts(
    store: &V1Store,
    archive_id: &str,
    expected_format: &str,
    expected_entry_count: i64,
) -> Result<(Vec<u8>, Vec<Value>), ContractError> {
    let manifest_bytes = read_archive_file_nofollow(
        store,
        archive_id,
        "manifest.json",
        MAX_ARCHIVE_MANIFEST_BYTES,
        "missing `manifest.json` blob",
    )?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| archive_incomplete(archive_id, "`manifest.json` blob is not valid JSON"))?;
    if manifest["archiveId"].as_str() != Some(archive_id) {
        return Err(archive_incomplete(
            archive_id,
            "`manifest.json` archiveId mismatch",
        ));
    }
    if manifest["entryCount"].as_i64() != Some(expected_entry_count) {
        return Err(archive_incomplete(
            archive_id,
            "`manifest.json` entryCount disagrees with archive row",
        ));
    }
    let declared = manifest["artifacts"]
        .as_array()
        .filter(|arr| !arr.is_empty())
        .ok_or_else(|| {
            archive_incomplete(
                archive_id,
                "`manifest.json` declares an empty or missing artifacts array",
            )
        })?;

    let expected_entries_id = format!("{archive_id}.entries");
    let expected_md_id = format!("{archive_id}.transcript.md");
    let mut saw_entries = false;
    let mut saw_md = false;
    let mut verified_artifacts = Vec::with_capacity(declared.len());

    for item in declared {
        let artifact_id = item["artifactId"].as_str().unwrap_or_default();
        let Some(suffix) = artifact_id.strip_prefix(&format!("{archive_id}.")) else {
            return Err(archive_incomplete(
                archive_id,
                &format!("manifest declares foreign artifact `{artifact_id}`"),
            ));
        };
        let (file, expected_media) = match suffix {
            "entries" => {
                saw_entries = true;
                ("entries.json", "application/json")
            }
            "transcript.md" => {
                saw_md = true;
                ("transcript.md", "text/markdown")
            }
            other => {
                return Err(archive_incomplete(
                    archive_id,
                    &format!("manifest declares unknown artifact kind `{other}`"),
                ));
            }
        };
        let expected_sha = item["sha256"].as_str().unwrap_or_default();
        if !is_valid_sha256_hex(expected_sha) {
            return Err(archive_incomplete(
                archive_id,
                &format!("manifest artifact `{artifact_id}` is missing a valid 64-hex sha256"),
            ));
        }
        let expected_bytes = item["bytes"].as_u64().ok_or_else(|| {
            archive_incomplete(
                archive_id,
                &format!("manifest artifact `{artifact_id}` is missing bytes length"),
            )
        })?;
        let blob_bytes = read_archive_file_nofollow(
            store,
            archive_id,
            file,
            MAX_ARCHIVE_BLOB_BYTES,
            &format!("missing `{file}` blob"),
        )?;
        if blob_bytes.len() as u64 != expected_bytes {
            return Err(archive_incomplete(
                archive_id,
                &format!("`{file}` byte length disagrees with manifest"),
            ));
        }
        let actual_sha = sha256_hex(&blob_bytes);
        if actual_sha != expected_sha {
            return Err(archive_incomplete(
                archive_id,
                &format!("`{file}` blob hash disagrees with manifest"),
            ));
        }
        if suffix == "entries" {
            let entries_doc: Value = serde_json::from_slice(&blob_bytes).map_err(|_| {
                archive_incomplete(archive_id, "`entries.json` blob is not valid JSON")
            })?;
            let entries_arr = entries_doc["entries"].as_array().ok_or_else(|| {
                archive_incomplete(archive_id, "`entries.json` has no `entries` array")
            })?;
            if entries_arr.len() as i64 != expected_entry_count {
                return Err(archive_incomplete(
                    archive_id,
                    "`entries.json` length disagrees with archive entryCount",
                ));
            }
        }
        let media_type = item["mediaType"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or(expected_media);
        verified_artifacts.push(json!({
            "artifactId": artifact_id,
            "mediaType": media_type,
            "bytes": blob_bytes.len(),
            "sha256": actual_sha,
        }));
    }

    if !saw_entries {
        return Err(archive_incomplete(
            archive_id,
            &format!("manifest does not declare required `{expected_entries_id}` artifact"),
        ));
    }
    if expected_format == "markdown" && !saw_md {
        return Err(archive_incomplete(
            archive_id,
            &format!("markdown manifest does not declare required `{expected_md_id}` artifact"),
        ));
    }
    Ok((manifest_bytes, verified_artifacts))
}

impl V1Store {
    /// `archive.get`: the manifest descriptor plus entry count and source
    /// revision. Incomplete exports, missing/corrupt blobs or tampered
    /// manifest answer `ARCHIVE_INCOMPLETE`; unknown ids answer `NOT_FOUND`.
    pub(crate) fn get_archive(&self, input: &Value) -> Result<Value, ContractError> {
        let archive_id = input["archiveId"].as_str().unwrap_or_default();
        if !valid_archive_id(archive_id) {
            return Err(domain_not_found("archive", archive_id));
        }
        let row = {
            let conn = self.conn.lock().expect("sqlite mutex");
            fetch_archive_row(&conn, archive_id)?
                .ok_or_else(|| domain_not_found("archive", archive_id))?
        };
        if row.state != "succeeded" {
            return Err(archive_incomplete(
                archive_id,
                &format!("export state is `{}`", row.state),
            ));
        }
        // 在 DB 锁外做磁盘读、NOFOLLOW 边界核查及 SHA-256 校验（P1#15/#16）。
        let (manifest_bytes, _) =
            verify_and_load_archive_artifacts(self, archive_id, &row.format, row.entry_count)?;
        Ok(json!({
            "manifest": {
                "artifactId": format!("{archive_id}.manifest"),
                "mediaType": "application/json",
                "bytes": manifest_bytes.len(),
                "sha256": sha256_hex(&manifest_bytes),
            },
            "entryCount": row.entry_count,
            "sourceRevision": row.source_revision,
        }))
    }

    /// `archive.entries`: pages the archived history items and lists the
    /// verified payload artifacts (rejects negative cursors and releases DB
    /// lock before reading/hashing archive files on disk).
    pub(crate) fn read_archive_entries(&self, input: &Value) -> Result<Value, ContractError> {
        let archive_id = input["archiveId"].as_str().unwrap_or_default();
        if !valid_archive_id(archive_id) {
            return Err(domain_not_found("archive", archive_id));
        }
        let obj = input.get("page").and_then(Value::as_object);
        let limit = obj
            .and_then(|p| p.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;
        let after_pos: i64 = match obj.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
            Some(cursor) => {
                let pos = cursor
                    .strip_prefix('a')
                    .and_then(|rest| rest.parse::<i64>().ok())
                    .ok_or_else(|| {
                        invalid_argument(format!("invalid archive cursor `{cursor}`"))
                    })?;
                if pos < 0 {
                    return Err(invalid_argument(format!(
                        "archive cursor `{cursor}` must not have negative position"
                    )));
                }
                pos
            }
            None => 0,
        };

        let (row, rows) = {
            let conn = self.conn.lock().expect("sqlite mutex");
            let row = fetch_archive_row(&conn, archive_id)?
                .ok_or_else(|| domain_not_found("archive", archive_id))?;
            if row.state != "succeeded" {
                return Err(archive_incomplete(
                    archive_id,
                    &format!("export state is `{}`", row.state),
                ));
            }

            let mut stmt = conn
                .prepare(
                    "SELECT position, entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at
                     FROM archive_entries WHERE archive_id = ?1 AND position > ?2
                     ORDER BY position ASC LIMIT ?3",
                )
                .map_err(sql_err)?;
            #[allow(clippy::type_complexity)]
            let rows: Vec<(
                i64,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                String,
            )> = stmt
                .query_map(params![archive_id, after_pos, (limit + 1) as i64], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            (row, rows)
        };

        // 在 DB 锁外做磁盘 NOFOLLOW 读取、SHA-256 校验与 manifest 核验（P1#15/#16）。
        let (_, artifacts) =
            verify_and_load_archive_artifacts(self, archive_id, &row.format, row.entry_count)?;

        let complete = rows.len() <= limit;
        let page_rows: Vec<_> = rows.into_iter().take(limit).collect();
        let mut items = Vec::with_capacity(page_rows.len());
        let mut last_pos = after_pos;
        for (position, entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at) in
            page_rows
        {
            items.push(history_item(
                &entry_id,
                &turn_id,
                &role,
                &content_json,
                run_id.as_deref(),
                tool_call_id.as_deref(),
                &created_at,
            )?);
            last_pos = position;
        }
        let next_cursor = if complete {
            Value::Null
        } else {
            json!(format!("a{last_pos}"))
        };

        Ok(json!({
            "items": items,
            "artifacts": artifacts,
            "pageInfo": {
                "snapshotRevision": row.source_revision,
                "nextCursor": next_cursor,
                "complete": complete,
            }
        }))
    }
}

/// Renders the archive's markdown transcript (format=`markdown`). The exact
/// layout is a t012 design decision: one heading per entry with role and
/// timestamp, text parts joined, attachments as bracketed references.
fn render_markdown(session_id: &str, entries: &[Value]) -> String {
    let mut out = format!("# Session transcript {session_id}\n\n");
    for entry in entries {
        let role = entry["role"].as_str().unwrap_or("user");
        let created_at = entry["createdAt"].as_str().unwrap_or_default();
        out.push_str(&format!("## {role} — {created_at}\n\n"));
        if let Some(parts) = entry["content"].as_array() {
            for part in parts {
                match part["kind"].as_str() {
                    Some("text") => {
                        out.push_str(part["text"].as_str().unwrap_or_default());
                        out.push('\n');
                    }
                    Some("attachment") => {
                        out.push_str(&format!(
                            "[attachment {} ({})]\n",
                            part["artifactId"].as_str().unwrap_or_default(),
                            part["mediaType"].as_str().unwrap_or("unknown")
                        ));
                    }
                    _ => {}
                }
            }
        }
        out.push('\n');
    }
    out
}

/// Exposed for the future `operations.*` domain: the durable record of this
/// family's async operations (state, result, error), keyed by operation id.
pub fn history_archive_operation_status(
    store: &V1Store,
    operation_id: &str,
) -> Result<Option<Value>, ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT method_id, state, result_json, error_code, error_message
             FROM history_archive_operations WHERE operation_id = ?1",
            params![operation_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((method_id, state, result_json, error_code, error_message)) = row else {
        return Ok(None);
    };
    Ok(Some(json!({
        "operationId": operation_id,
        "methodId": method_id,
        "state": state,
        "result": result_json.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
        "errorCode": error_code,
        "errorMessage": error_message,
    })))
}

// Keep the module's contract surface greppable: the read paths above answer
// deleted sessions with NOT_FOUND (via `domain_not_found`).

// ---------------------------------------------------------------------------
// Tests (contract anchors: 角色顺序tool关联不丢；不属于会话的fromEntryId拒绝；
// 坏记录明确报错；fork事务性；导出附清单/hash；缺blob明确不完整；已有目标拒绝)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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

    async fn seeded(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        tag: &str,
    ) -> (String, String) {
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": format!("p-{tag}"),
                "name": "H",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"},
            }),
            None,
            None,
        )
        .await
        .unwrap();
        rt.dispatch(
            "session.create",
            json!({
                "instanceId": iid,
                "sessionId": format!("s-{tag}"),
                "projectId": format!("p-{tag}"),
            }),
            None,
            None,
        )
        .await
        .unwrap();
        (format!("p-{tag}"), format!("s-{tag}"))
    }

    /// Seeds a 4-entry conversation with role ordering and tool association.
    fn conversation() -> Vec<Value> {
        vec![
            json!({
                "turnId": "t-1",
                "role": "user",
                "content": [{"kind": "text", "text": "hello"}],
                "runId": "r-1",
            }),
            json!({
                "turnId": "t-1",
                "role": "assistant",
                "content": [{"kind": "text", "text": "thinking"}],
                "runId": "r-1",
            }),
            json!({
                "turnId": "t-2",
                "role": "tool",
                "content": [
                    {"kind": "text", "text": "artifact attached"},
                    {"kind": "attachment", "artifactId": "art-1", "mediaType": "text/plain"},
                ],
                "runId": "r-1",
                "toolCallId": "call-1",
            }),
            json!({
                "turnId": "t-2",
                "role": "assistant",
                "content": [{"kind": "text", "text": "final answer"}],
                "runId": "r-1",
            }),
        ]
    }

    async fn session_revision(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        session_id: &str,
    ) -> String {
        let (_, got) = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": session_id}),
                None,
                None,
            )
            .await
            .unwrap();
        got["session"]["revision"].as_str().unwrap().to_owned()
    }

    async fn wait_operation(rt: &std::sync::Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        for _ in 0..600 {
            if let Some(status) =
                history_archive_operation_status(rt.store(), operation_id).unwrap()
            {
                let state = status["state"].as_str().unwrap_or_default().to_owned();
                if state == "succeeded" || state == "failed" {
                    return status;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("operation `{operation_id}` did not finish in time");
    }

    #[tokio::test]
    async fn history_read_pages_anchors_and_reports_corruption() {
        let tmp = TempDir::new("hist-read");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_pid, sid) = seeded(&rt, &iid, "r").await;
        let stored = rt
            .store()
            .append_history_entries(&sid, &conversation())
            .unwrap();
        let items = stored["items"].as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[2]["role"], "tool");
        assert_eq!(
            items[2]["toolCallId"], "call-1",
            "tool association survives"
        );

        // Full read.
        let (_, all) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("history.read", &all)
            .unwrap();
        assert_eq!(all["items"].as_array().map(Vec::len), Some(4));
        assert_eq!(all["pageInfo"]["complete"], true);

        // Pagination with the opaque cursor.
        let (_, page1) = rt
            .dispatch(
                "history.read",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "page": {"limit": 3},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page1["items"].as_array().map(Vec::len), Some(3));
        assert_eq!(page1["pageInfo"]["complete"], false);
        let cursor = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        let (_, page2) = rt
            .dispatch(
                "history.read",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "page": {"cursor": cursor},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page2["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(page2["pageInfo"]["complete"], true);

        // fromEntryId anchors inclusively; a foreign entry id is rejected.
        let third = items[2]["entryId"].as_str().unwrap().to_owned();
        let (_, from_anchor) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid, "fromEntryId": third}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(from_anchor["items"].as_array().map(Vec::len), Some(2));
        let error = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid, "fromEntryId": "foreign"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "INVALID_ARGUMENT");

        // A corrupted row is reported explicitly, never silently skipped.
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE history_entries SET content_json = 'not json' WHERE session_id = ?1
                 AND entry_id = ?2",
                params![sid, items[3]["entryId"].as_str().unwrap()],
            )
            .unwrap();
        }
        let error = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "CORRUPT_HISTORY");

        let error = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": "ghost"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
    }

    #[tokio::test]
    async fn history_fork_copies_through_the_boundary_atomically() {
        let tmp = TempDir::new("hist-fork");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_pid, sid) = seeded(&rt, &iid, "f").await;
        let stored = rt
            .store()
            .append_history_entries(&sid, &conversation())
            .unwrap();
        let items = stored["items"].as_array().cloned().unwrap_or_default();
        let through = items[2]["entryId"].as_str().unwrap().to_owned();
        let rev = session_revision(&rt, &iid, &sid).await;

        let (_, forked) = rt
            .dispatch(
                "history.fork",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "throughEntryId": through,
                    "expectedRevision": rev,
                    "newSessionId": "s-fork-1",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("history.fork", &forked)
            .unwrap();
        assert_eq!(forked["sourceSessionId"], sid);
        assert_eq!(forked["throughEntryId"], through);
        assert_eq!(forked["session"]["sessionState"], "open");

        let (_, new_history) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": "s-fork-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        let new_items = new_history["items"].as_array().cloned().unwrap_or_default();
        assert_eq!(new_items.len(), 3, "entries through the boundary are kept");
        for (idx, item) in new_items.iter().take(3).enumerate() {
            assert_eq!(item["entryId"], items[idx]["entryId"], "identity preserved");
        }

        // The sessions evolve independently afterwards.
        rt.store()
            .append_history_entries(
                &sid,
                &[json!({
                    "turnId": "t-3",
                    "role": "user",
                    "content": [{"kind": "text", "text": "only source"}],
                })],
            )
            .unwrap();
        let (_, reread) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": "s-fork-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(reread["items"].as_array().map(Vec::len), Some(3));

        // Invalid boundary: zero modification (no target session, same
        // source revision, same entry count).
        let rev_now = session_revision(&rt, &iid, &sid).await;
        let error = rt
            .dispatch(
                "history.fork",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "throughEntryId": "bogus",
                    "expectedRevision": rev_now,
                    "newSessionId": "s-fork-2",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "INVALID_HISTORY_BOUNDARY");
        let error = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": "s-fork-2"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
        assert_eq!(session_revision(&rt, &iid, &sid).await, rev_now);
        let (_, after) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(after["items"].as_array().map(Vec::len), Some(5));

        // Concurrent-write CAS: zero modification.
        let error = rt
            .dispatch(
                "history.fork",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "throughEntryId": through,
                    "expectedRevision": rev,
                    "newSessionId": "s-fork-3",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "REVISION_CONFLICT");
        let error = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": "s-fork-3"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");

        // An existing target session cannot be silently taken over.
        let rev3 = session_revision(&rt, &iid, &sid).await;
        let error = rt
            .dispatch(
                "history.fork",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "throughEntryId": through,
                    "expectedRevision": rev3,
                    "newSessionId": "s-fork-1",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ACTIVE_RESOURCES");
    }

    #[tokio::test]
    async fn history_export_and_archive_reads_roundtrip_with_restore() {
        let tmp = TempDir::new("hist-export");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_pid, sid) = seeded(&rt, &iid, "e").await;
        rt.store()
            .append_history_entries(&sid, &conversation()[..3])
            .unwrap();
        let rev = session_revision(&rt, &iid, &sid).await;

        // Submit the async export; the receipt is an operation receipt.
        let (_, receipt) = rt
            .dispatch(
                "history.export",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "expectedRevision": rev,
                }),
                Some("export-key-1"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("history.export", &receipt)
            .unwrap();
        assert_eq!(receipt["kind"], "operation");
        assert_eq!(receipt["deduplicated"], false);
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        // The background export finishes durably and matches completionResult schema.
        let status = wait_operation(&rt, &operation_id).await;
        assert_eq!(status["state"], "succeeded");
        let export_res = &status["result"];
        assert!(export_res.get("format").is_none());
        assert_eq!(export_res["sourceRevision"], json!(rev));
        assert_eq!(export_res["entryCount"], json!(3));
        let archive_id = export_res["archiveId"].as_str().unwrap().to_owned();
        assert_eq!(
            export_res["artifact"]["artifactId"],
            json!(format!("{archive_id}.manifest"))
        );
        assert!(
            export_res["artifact"]["sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64)
        );

        // archive.get: manifest artifact + entry count + source revision.
        let (_, manifest) = rt
            .dispatch(
                "archive.get",
                json!({"instanceId": iid, "archiveId": archive_id}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("archive.get", &manifest)
            .unwrap();
        assert_eq!(manifest["entryCount"], 3);
        assert_eq!(manifest["sourceRevision"], rev);
        assert!(
            manifest["manifest"]["sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64)
        );

        // archive.entries pages the items and lists payload artifacts
        // without ever exposing storage paths.
        let (_, entries) = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": archive_id}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("archive.entries", &entries)
            .unwrap();
        assert_eq!(entries["items"].as_array().map(Vec::len), Some(3));
        let artifacts = entries["artifacts"].as_array().cloned().unwrap_or_default();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0]["artifactId"], format!("{archive_id}.entries"));
        assert!(
            artifacts[0]["artifactId"]
                .as_str()
                .unwrap()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-')),
            "artifact ids carry no storage paths"
        );
        assert_eq!(entries["pageInfo"]["complete"], true);

        // Dedup: same key + same input replays the original receipt.
        let (_, replay) = rt
            .dispatch(
                "history.export",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "expectedRevision": rev,
                }),
                Some("export-key-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"], operation_id);

        // Same key + different input conflicts.
        let error = rt
            .dispatch(
                "history.export",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "expectedRevision": rev,
                    "format": "markdown",
                }),
                Some("export-key-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "IDEMPOTENCY_CONFLICT");

        // Restore into a new session (second project, registered for the
        // restore target).
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": "p-e2",
                "name": "RestoreTarget",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws2"},
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let (_, restore_receipt) = rt
            .dispatch(
                "history.restore",
                json!({
                    "instanceId": iid,
                    "archiveId": archive_id,
                    "projectId": "p-e2",
                    "newSessionId": "s-restored-1",
                }),
                Some("restore-key-1"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("history.restore", &restore_receipt)
            .unwrap();
        let restore_op = restore_receipt["operationId"].as_str().unwrap().to_owned();
        let restore_status = wait_operation(&rt, &restore_op).await;
        assert_eq!(restore_status["state"], "succeeded");
        let restore_res = &restore_status["result"];
        assert!(restore_res.get("sessionId").is_none());
        assert!(restore_res.get("archiveId").is_none());
        assert!(restore_res.get("entryCount").is_none());
        assert_eq!(restore_res["session"]["sessionId"], json!("s-restored-1"));
        assert_eq!(restore_res["session"]["projectId"], json!("p-e2"));
        assert_eq!(restore_res["session"]["sessionState"], json!("open"));
        assert_eq!(restore_res["session"]["activeRunId"], Value::Null);
        assert_eq!(
            restore_res["identityMapping"].as_array().map(Vec::len),
            Some(3)
        );

        // The restored session carries the archived entries (identity
        // preserved) and no active callbacks.
        let (_, restored) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": "s-restored-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(restored["items"].as_array().map(Vec::len), Some(3));
        let (_, restored_session) = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": "s-restored-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(restored_session["session"]["sessionState"], "open");
        assert_eq!(restored_session["session"]["activeRunId"], Value::Null);

        // An existing target session fails the restore before publishing.
        let (_, dup_receipt) = rt
            .dispatch(
                "history.restore",
                json!({
                    "instanceId": iid,
                    "archiveId": archive_id,
                    "projectId": "p-e2",
                    "newSessionId": "s-restored-1",
                }),
                Some("restore-key-2"),
                None,
            )
            .await
            .unwrap();
        let dup_status = wait_operation(&rt, dup_receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(dup_status["state"], "failed");
        assert_eq!(dup_status["errorCode"], "ALREADY_EXISTS");

        // An unknown archive is refused at submit time.
        let error = rt
            .dispatch(
                "history.restore",
                json!({
                    "instanceId": iid,
                    "archiveId": "no-such-archive",
                    "projectId": "p-e2",
                    "newSessionId": "s-restored-2",
                }),
                Some("restore-key-3"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ARCHIVE_INCOMPLETE");

        // Markdown exports carry the transcript artifact alongside the
        // structured payload.
        let (_, md_receipt) = rt
            .dispatch(
                "history.export",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "expectedRevision": rev,
                    "format": "markdown",
                }),
                Some("export-key-md"),
                None,
            )
            .await
            .unwrap();
        let md_status = wait_operation(&rt, md_receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(md_status["state"], "succeeded");
        let md_archive = md_status["result"]["archiveId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, md_entries) = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": md_archive}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(md_entries["artifacts"].as_array().map(Vec::len), Some(2));

        // A missing blob is explicitly incomplete.
        let archive_dir = archive_dir(rt.store(), &archive_id);
        std::fs::remove_file(archive_dir.join("entries.json")).unwrap();
        let error = rt
            .dispatch(
                "archive.get",
                json!({"instanceId": iid, "archiveId": archive_id}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ARCHIVE_INCOMPLETE");
        let error = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": archive_id}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ARCHIVE_INCOMPLETE");

        // Unknown archives are NOT_FOUND.
        let error = rt
            .dispatch(
                "archive.get",
                json!({"instanceId": iid, "archiveId": "ghost"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
    }

    #[tokio::test]
    async fn export_snapshot_revision_race_integrity_bypass_cursor_and_recovery() {
        let tmp = TempDir::new("hist-p1-12-16");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (pid, sid) = seeded(&rt, &iid, "hard").await;
        rt.store()
            .append_history_entries(&sid, &conversation()[..3])
            .unwrap();
        let rev = session_revision(&rt, &iid, &sid).await;

        // 1) P1#14 固定快照：submit 后追加 2 条历史，execute_history_export 仍只导出提交时 max_seq=3 条。
        let disp = submit_history_export(
            rt.store(),
            &json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
            Some("snap-fixed-key"),
        )
        .unwrap();
        let AsyncOpDisposition::New { operation_id, .. } = disp else {
            panic!("expected new export operation");
        };
        rt.store()
            .append_history_entries(&sid, &conversation()[3..4])
            .unwrap();
        execute_history_export(rt.store(), &operation_id).unwrap();
        let (_, op_got) = rt
            .dispatch(
                "operations.get",
                json!({"instanceId": iid, "operationId": operation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(op_got["operation"]["state"], "succeeded");
        let export_res = &op_got["operation"]["result"];
        assert_eq!(export_res["entryCount"], json!(3));
        assert_eq!(export_res["sourceRevision"], json!(rev));
        assert_eq!(
            export_res["artifact"]["artifactId"],
            json!(format!(
                "{}.manifest",
                export_res["archiveId"].as_str().unwrap()
            ))
        );
        let archive_id = export_res["archiveId"].as_str().unwrap().to_owned();

        // 2) P1#14 并发修改 source_revision 后执行 export 必须失败为 REVISION_CONFLICT。
        let disp_race = submit_history_export(
            rt.store(),
            &json!({"instanceId": iid, "sessionId": sid, "expectedRevision": rev}),
            Some("snap-race-key"),
        )
        .unwrap();
        let AsyncOpDisposition::New {
            operation_id: race_op_id,
            ..
        } = disp_race
        else {
            panic!("expected new export operation");
        };
        {
            let mut conn = rt.store().conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            let bumped = bump_rev_in_tx(&tx).unwrap();
            tx.execute(
                "UPDATE sessions SET revision = ?1 WHERE session_id = ?2",
                params![bumped, sid],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let race_err = execute_history_export(rt.store(), &race_op_id).unwrap_err();
        assert_eq!(race_err.code, CODE_REVISION_CONFLICT);

        // 3) P1#16 archive.entries 负数游标必须报 INVALID_ARGUMENT，且正数分页 cursor 基于真实 position。
        let neg_err = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": archive_id, "page": {"limit": 2, "cursor": "a-5"}}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&neg_err), "INVALID_ARGUMENT");

        let (_, page1) = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": archive_id, "page": {"limit": 2}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page1["items"].as_array().map(Vec::len), Some(2));
        assert_eq!(page1["pageInfo"]["nextCursor"], json!("a2"));
        assert_eq!(page1["pageInfo"]["complete"], json!(false));

        let (_, page2) = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": archive_id, "page": {"limit": 2, "cursor": "a2"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page2["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(page2["pageInfo"]["complete"], json!(true));

        // 4) P1#15 篡改清单（清空 artifacts / 清空 sha256 / 删 manifest.json / 替换 symlink 或 FIFO）
        //    一律报 ARCHIVE_INCOMPLETE，绝不绕过或挂死。
        let adir = archive_dir(rt.store(), &archive_id);
        let original_manifest = std::fs::read(adir.join("manifest.json")).unwrap();
        let mut tampered: Value = serde_json::from_slice(&original_manifest).unwrap();
        tampered["artifacts"] = json!([]);
        std::fs::write(
            adir.join("manifest.json"),
            serde_json::to_vec(&tampered).unwrap(),
        )
        .unwrap();
        for method in ["archive.get", "archive.entries"] {
            let err = rt
                .dispatch(
                    method,
                    json!({"instanceId": iid, "archiveId": archive_id}),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(
                contract_code(&err),
                CODE_ARCHIVE_INCOMPLETE,
                "{method} empty artifacts"
            );
        }
        let (_, r_empty) = rt
            .dispatch(
                "history.restore",
                json!({"instanceId": iid, "archiveId": archive_id, "projectId": pid, "newSessionId": "s-bad-empty"}),
                Some("restore-empty-art"),
                None,
            )
            .await
            .unwrap();
        let r_empty_term = wait_operation(&rt, r_empty["operationId"].as_str().unwrap()).await;
        assert_eq!(r_empty_term["state"], "failed");
        assert_eq!(r_empty_term["errorCode"], CODE_ARCHIVE_INCOMPLETE);

        // 空 sha256 同样禁止绕过。
        let mut no_sha: Value = serde_json::from_slice(&original_manifest).unwrap();
        no_sha["artifacts"][0]["sha256"] = json!("");
        std::fs::write(
            adir.join("manifest.json"),
            serde_json::to_vec(&no_sha).unwrap(),
        )
        .unwrap();
        let err = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": archive_id}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), CODE_ARCHIVE_INCOMPLETE);

        // 缺失 manifest.json 时 archive.entries 不再吞错放行。
        std::fs::remove_file(adir.join("manifest.json")).unwrap();
        let err = rt
            .dispatch(
                "archive.entries",
                json!({"instanceId": iid, "archiveId": archive_id}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), CODE_ARCHIVE_INCOMPLETE);

        // 符号链接 / FIFO 防护。
        #[cfg(unix)]
        {
            std::fs::write(adir.join("manifest.json"), &original_manifest).unwrap();
            let outside = tmp.0.join("outside-manifest.json");
            std::fs::write(&outside, &original_manifest).unwrap();
            std::fs::remove_file(adir.join("entries.json")).unwrap();
            let outside_entries = tmp.0.join("outside-entries.json");
            std::fs::write(&outside_entries, b"{}").unwrap();
            std::os::unix::fs::symlink(&outside_entries, adir.join("entries.json")).unwrap();
            let sym_err = rt
                .dispatch(
                    "archive.get",
                    json!({"instanceId": iid, "archiveId": archive_id}),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code(&sym_err), CODE_ARCHIVE_INCOMPLETE);

            std::fs::remove_file(adir.join("entries.json")).unwrap();
            let st = std::process::Command::new("mkfifo")
                .arg(adir.join("entries.json"))
                .status()
                .expect("mkfifo");
            assert!(st.success());
            let fifo_err = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                rt.dispatch(
                    "archive.get",
                    json!({"instanceId": iid, "archiveId": archive_id}),
                    None,
                    None,
                ),
            )
            .await
            .expect("FIFO archive read must not hang")
            .unwrap_err();
            assert_eq!(contract_code(&fifo_err), CODE_ARCHIVE_INCOMPLETE);
        }

        // 5) P1#12 recover_orphaned_history_archive_operations：重启恢复非终态操作与未完档案。
        {
            let conn = rt.store().conn.lock().unwrap();
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO history_archive_operations
                 (operation_id, method_id, state, input_json, created_at, updated_at)
                 VALUES ('op-hist-crash', 'history.export', 'running', '{}', ?1, ?1)",
                params![now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO archives
                 (archive_id, operation_id, source_session_id, source_revision, format, state, entry_count, created_at, updated_at)
                 VALUES ('arch-crash', 'op-hist-crash', ?1, ?2, 'structured', 'running', 0, ?3, ?3)",
                params![sid, rev, now],
            )
            .unwrap();
            recover_orphaned_history_archive_operations(&conn, &now).unwrap();
            let (op_state, op_code): (String, Option<String>) = conn
                .query_row(
                    "SELECT state, error_code FROM history_archive_operations WHERE operation_id = 'op-hist-crash'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(op_state, "interrupted");
            assert_eq!(op_code.as_deref(), Some("GATEWAY_RESTART"));
            let arch_state: String = conn
                .query_row(
                    "SELECT state FROM archives WHERE archive_id = 'arch-crash'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(arch_state, "failed");
        }
    }
}
