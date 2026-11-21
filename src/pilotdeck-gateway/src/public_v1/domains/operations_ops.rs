//! `operations.*` + `resources.snapshot` + `system.resources` +
//! `system.diagnostics` — the collections domain (`cli.task.collections`).
//!
//! The `operations.*` family is the unified progress surface over the
//! instance's long-running async operations (`retry=idempotentSubmit` class:
//! upload assembly, git mutations, configuration reload, maintenance, …).
//! Every async domain keeps its durable operation records in the shared
//! `<home>/v1/state.sqlite`; this domain projects them into the contract's
//! `Operation` object and serves get/list/events/cancel/snapshot over the
//! union:
//!
//! * `operations` (this file) — the collections seam: the write API later
//!   async tasks hook onto ([`V1Store::begin_long_operation`] and friends);
//! * `async_operations` — uploads.complete / documents.preview receipts
//!   (uploads.rs documents this table as this domain's data surface);
//! * `git_operations` — git.rs's mutation records (its `git.changed` /
//!   `files.changed` events are keyed by operationId for `operations.events`);
//! * `history_archive_operations` — history export/restore records;
//! * `v1_config_operations` — configuration.reload records (scope_json is
//!   stored precisely so this projection needs no domain re-parsing);
//! * `v1_domain_operations` — schedules / background_tasks accept records.
//!
//! Projection sources are discovered per call through `sqlite_master`: a
//! source whose owning domain is not registered in this build simply does
//! not participate (a missing table is not a storage error). Cancelling is
//! only supported for seam-owned rows (`operations`): foreign records have
//! no cancellation path and answer `CANCELLATION_UNSUPPORTED` instead of
//! pretending.
//!
//! Events live in the shared `events` table (resource_id = operationId,
//! schema `agentcpm-pd.operation.event/1`), so `operations.events` reuses
//! the S2 cursor discipline: opaque `cur-<seq>` cursors, gap frames with
//! `recoveryMethod: "operations.snapshot"`, `terminal` only when the
//! operation is terminal AND the page is caught up.
//!
//! Pagination policy for the list-shaped operations (`operations.list`,
//! `system.resources`, `system.diagnostics`): the page cursor pins the
//! digest of the listed view; a page whose view no longer matches the
//! digest answers `SNAPSHOT_CHANGED` instead of silently continuing (the
//! task's acceptance anchor "跨页变更不静默假完整"). `complete: true` is
//! therefore only ever reported for a view the caller has seen unchanged.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentcpm_pd_protocol::contract::{EventEnvelope, GapFrame, GapRecoveryPolicy, ResponseOutcome};
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_CANCELLATION_UNSUPPORTED, CODE_CAPABILITY_UNAVAILABLE, CODE_CURSOR_EXPIRED,
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_LIMIT,
    CODE_SNAPSHOT_CHANGED, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::wire::{
    JSONRPC_VERSION, JsonRpcNotification, SubEventNotification, WIRE_SUB_EVENT,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::public_v1::PublicV1State;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::artifacts::publish_artifact;
use crate::public_v1::domains::workspace_dirs::relative_components;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, format_cursor, now_rfc3339, parse_cursor};
use crate::public_v1::ws::{StreamFetch, StreamFrame, StreamSource};

// ---------------------------------------------------------------------------
// Schema (executed by V1Store::init_schema through the domain registry)
// ---------------------------------------------------------------------------

/// The collections seam's own operation records: the full contract
/// `Operation` shape plus the driving columns the seam API maintains.
/// Other async domains keep their own tables (see the module doc); this one
/// backs the [`V1Store::begin_long_operation`] write seam and the
/// driver-driven maintenance operations.
const DDL_OPERATIONS: &str = "CREATE TABLE IF NOT EXISTS operations (
    operation_id TEXT PRIMARY KEY,
    operation_type TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    project_id TEXT,
    session_id TEXT,
    state TEXT NOT NULL,
    revision TEXT NOT NULL,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    cancellable INTEGER NOT NULL DEFAULT 1,
    effect_status TEXT NOT NULL DEFAULT 'known',
    progress_json TEXT NOT NULL DEFAULT '[]',
    result_json TEXT,
    result_schema_id TEXT,
    error_json TEXT,
    final_cursor TEXT,
    terminal INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

/// Immutable state sections referenced by `operations.snapshot` snapshots.
/// Content-addressed by sha256 (`INSERT OR IGNORE`): re-snapshotting the
/// same state reuses the same artifact id, and a later state writes a new
/// one — never mutating an existing section.
const DDL_STATE_ARTIFACTS: &str = "CREATE TABLE IF NOT EXISTS operation_state_artifacts (
    artifact_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    schema_id TEXT NOT NULL,
    content_json TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL
)";

const OPERATION_EVENT_SCHEMA: &str = "agentcpm-pd.operation.event/1";
const OPERATION_STATE_SECTION_SCHEMA: &str = "agentcpm-pd.operation.state/1";
/// Resource kinds whose snapshot revision this build can derive. Kinds
/// whose owning domain has not landed (or exposes no revision source yet)
/// answer `state: "unavailable"` — explicitly not "absent".
///
/// Also the per-call cap on the `resources` array: `resources.snapshot` and
/// the `resources.watch` stream (t023 E, which reuses the same resolver)
/// both refuse more than this many refs per call.
pub(super) const MAX_SNAPSHOT_RESOURCES: usize = 64;

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("operations", DDL_OPERATIONS);
    reg.schema("operation_state_artifacts", DDL_STATE_ARTIFACTS);

    reg.handler("operations.get", |rt, input, _key, _rev| {
        Box::pin(operations_get(rt, input))
    });
    reg.handler("operations.list", |rt, input, _key, _rev| {
        Box::pin(operations_list(rt, input))
    });
    reg.handler("operations.events", |rt, input, _key, _rev| {
        Box::pin(operations_events(rt, input))
    });
    reg.handler("operations.cancel", |rt, input, key, _rev| {
        Box::pin(operations_cancel(rt, input, key))
    });
    reg.handler("operations.snapshot", |rt, input, _key, _rev| {
        Box::pin(operations_snapshot(rt, input))
    });
    reg.handler("resources.snapshot", |rt, input, _key, _rev| {
        Box::pin(resources_snapshot(rt, input))
    });
    reg.handler("system.resources", |rt, input, _key, _rev| {
        Box::pin(system_resources(rt, input))
    });
    reg.handler("system.diagnostics", |rt, input, _key, _rev| {
        Box::pin(system_diagnostics(rt, input))
    });
    crate::public_v1::ws::register_stream_source("operations.events", operations_events_source);
}

async fn run_blocking<T, F>(task: F) -> Result<T, ContractError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ContractError> + Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|err| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("operations worker failed: {err}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn sql_err(err: rusqlite::Error) -> ContractError {
    storage_err(err.to_string())
}

fn storage_err(msg: String) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {msg}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn invalid(msg: String) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        msg,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn cancellation_unsupported(operation_id: &str, why: &str) -> ContractError {
    ContractError::new(
        CODE_CANCELLATION_UNSUPPORTED,
        format!("operation `{operation_id}` does not support cancellation: {why}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

/// A short deterministic revision over the given parts; used for view
/// digests and for projection sources that store no revision of their own.
fn digest_revision(prefix: &str, parts: &[String]) -> String {
    let refs: Vec<&[u8]> = parts.iter().map(String::as_bytes).collect();
    format!("{prefix}-{}", &sha256_hex(&refs)[..24])
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, ContractError> {
    let present: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(present.is_some())
}

fn parse_json_or<T: for<'de> serde::Deserialize<'de> + Default>(raw: Option<String>) -> T {
    raw.and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The contract's terminal operation states.
fn is_terminal_operation_state(state: &str) -> bool {
    matches!(
        state,
        "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
    )
}

/// Cursor parsing for the digest-pinned list pages:
/// `opview-<digest_revision>-<offset>` (the digest revision itself carries
/// its own `prefix-hex` shape, so only the final segment is the offset).
fn parse_view_cursor(raw: &str) -> Option<(String, usize)> {
    let rest = raw.strip_prefix("opview-")?;
    let (digest, offset) = rest.rsplit_once('-')?;
    if digest.is_empty() {
        return None;
    }
    offset
        .parse::<usize>()
        .ok()
        .map(|off| (digest.to_owned(), off))
}

fn view_cursor(digest: &str, offset: usize) -> String {
    format!("opview-{digest}-{offset}")
}

/// Extracts (limit) from the contract `page` object (offset comes from the
/// digest-pinned cursor, never from a bare number).
fn page_limit(page: Option<&Value>) -> Result<usize, ContractError> {
    let limit = page
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    Ok(limit)
}

// ---------------------------------------------------------------------------
// The unified operation projection
// ---------------------------------------------------------------------------

/// One projected long operation (contract `$defs.Operation` plus the
/// internals the handlers need).
struct UnifiedOperation {
    source: &'static str,
    operation_id: String,
    operation_type: String,
    /// Contract `Scope` JSON (`{kind: "instance"}` / project / session).
    scope: Value,
    state: String,
    revision: String,
    cancel_requested: bool,
    effect_status: String,
    progress: Vec<Value>,
    result: Option<Value>,
    result_schema_id: Option<String>,
    error: Option<Value>,
    final_cursor: Option<String>,
    terminal: bool,
    created_at: String,
}

impl UnifiedOperation {
    fn to_contract_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("operationId".into(), json!(self.operation_id));
        obj.insert("operationType".into(), json!(self.operation_type));
        obj.insert("scope".into(), self.scope.clone());
        obj.insert("state".into(), json!(self.state));
        obj.insert("revision".into(), json!(self.revision));
        obj.insert("cancelRequested".into(), json!(self.cancel_requested));
        obj.insert("effectStatus".into(), json!(self.effect_status));
        obj.insert("progress".into(), json!(self.progress));
        if let Some(result) = &self.result {
            obj.insert("result".into(), result.clone());
        }
        if let Some(schema_id) = &self.result_schema_id {
            obj.insert("resultSchemaId".into(), json!(schema_id));
        }
        if let Some(error) = &self.error {
            obj.insert("error".into(), error.clone());
        }
        if let Some(cursor) = &self.final_cursor {
            obj.insert("finalCursor".into(), json!(cursor));
        }
        Value::Object(obj)
    }

    /// The identity/revision pair the list-view digest folds over: any state
    /// or revision change of any listed operation invalidates an in-flight
    /// pagination.
    fn view_digest_part(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.source, self.operation_id, self.state, self.revision
        )
    }
}

fn scope_instance() -> Value {
    json!({"kind": "instance"})
}

fn scope_project(project_id: &str) -> Value {
    json!({"kind": "project", "projectId": project_id})
}

fn scope_session(project_id: &str, session_id: &str) -> Value {
    json!({"kind": "session", "projectId": project_id, "sessionId": session_id})
}

fn issue_json(code: &str, message: &str) -> Value {
    json!({"code": code, "message": message})
}

/// Reads one own-table (`operations`) row.
#[allow(clippy::too_many_arguments)]
fn own_row(
    operation_id: &str,
    operation_type: &str,
    scope_kind: &str,
    project_id: Option<String>,
    session_id: Option<String>,
    state: &str,
    revision: &str,
    cancel_requested: bool,
    effect_status: &str,
    progress: Vec<Value>,
    result: Option<Value>,
    result_schema_id: Option<String>,
    error: Option<Value>,
    final_cursor: Option<String>,
    terminal: bool,
    created_at: String,
) -> UnifiedOperation {
    let scope = match scope_kind {
        "project" => scope_project(project_id.as_deref().unwrap_or_default()),
        "session" => scope_session(
            project_id.as_deref().unwrap_or_default(),
            session_id.as_deref().unwrap_or_default(),
        ),
        _ => scope_instance(),
    };
    UnifiedOperation {
        source: "operations",
        operation_id: operation_id.to_owned(),
        operation_type: operation_type.to_owned(),
        scope,
        state: state.to_owned(),
        revision: revision.to_owned(),
        cancel_requested,
        effect_status: if effect_status == "unknown" {
            "unknown".to_owned()
        } else {
            "known".to_owned()
        },
        progress,
        result,
        result_schema_id,
        error,
        final_cursor,
        terminal,
        created_at,
    }
}

fn read_own_operations(conn: &Connection) -> Result<Vec<UnifiedOperation>, ContractError> {
    if !table_exists(conn, "operations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT operation_id, operation_type, scope_kind, project_id, session_id, state,
                    revision, cancel_requested, cancellable, effect_status, progress_json,
                    result_json, result_schema_id, error_json, final_cursor, terminal, created_at
             FROM operations",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<String>>(13)?,
                r.get::<_, Option<String>>(14)?,
                r.get::<_, i64>(15)?,
                r.get::<_, String>(16)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    Ok(rows
        .into_iter()
        .map(
            |(
                operation_id,
                operation_type,
                scope_kind,
                project_id,
                session_id,
                state,
                revision,
                cancel_requested,
                _cancellable,
                effect_status,
                progress_json,
                result_json,
                result_schema_id,
                error_json,
                final_cursor,
                terminal,
                created_at,
            )| {
                own_row(
                    &operation_id,
                    &operation_type,
                    &scope_kind,
                    project_id,
                    session_id,
                    &state,
                    &revision,
                    cancel_requested != 0,
                    &effect_status,
                    parse_json_or(Some(progress_json)),
                    result_json.and_then(|text| serde_json::from_str(&text).ok()),
                    result_schema_id,
                    error_json.and_then(|text| serde_json::from_str(&text).ok()),
                    final_cursor,
                    terminal != 0,
                    created_at,
                )
            },
        )
        .collect())
}

/// `async_operations` (uploads.complete / documents.preview): project scope
/// is recovered from the `uploads` row for upload completions (documents
/// previews record no scope — they project as instance-scoped).
fn read_async_operations(conn: &Connection) -> Result<Vec<UnifiedOperation>, ContractError> {
    if !table_exists(conn, "async_operations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT o.operation_id, o.method_id, o.resource_id, o.state, o.revision,
                    o.result_json, o.error_code, o.error_message, o.created_at
             FROM async_operations o",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, String>(8)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);

    let mut out = Vec::with_capacity(rows.len());
    for (
        operation_id,
        method_id,
        resource_id,
        state,
        revision,
        result_json,
        error_code,
        error_message,
        created_at,
    ) in rows
    {
        let scope = if method_id == "uploads.complete" {
            conn.query_row(
                "SELECT project_id FROM uploads WHERE upload_id = ?1",
                params![resource_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_err)?
            .map(|project_id| scope_project(&project_id))
            .unwrap_or_else(scope_instance)
        } else {
            scope_instance()
        };
        out.push(UnifiedOperation {
            source: "async_operations",
            operation_id,
            operation_type: method_id,
            scope,
            terminal: is_terminal_operation_state(&state),
            state,
            revision,
            cancel_requested: false,
            effect_status: "known".to_owned(),
            progress: Vec::new(),
            result: result_json.and_then(|text| serde_json::from_str(&text).ok()),
            result_schema_id: None,
            error: match (error_code, error_message) {
                (Some(code), message) => Some(issue_json(
                    &code,
                    message.as_deref().unwrap_or("operation failed"),
                )),
                (None, _) => None,
            },
            final_cursor: None,
            created_at,
        });
    }
    Ok(out)
}

fn read_git_operations(conn: &Connection) -> Result<Vec<UnifiedOperation>, ContractError> {
    if !table_exists(conn, "git_operations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT operation_id, method_id, project_id, state, revision, cancel_requested,
                    effect_status, progress_json, result_json, error_code, error_message, created_at
             FROM git_operations",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, String>(11)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    Ok(rows
        .into_iter()
        .map(
            |(
                operation_id,
                method_id,
                project_id,
                state,
                revision,
                cancel_requested,
                effect_status,
                progress_json,
                result_json,
                error_code,
                error_message,
                created_at,
            )| {
                UnifiedOperation {
                    source: "git_operations",
                    operation_id,
                    operation_type: method_id,
                    scope: scope_project(&project_id),
                    state: state.clone(),
                    revision,
                    cancel_requested: cancel_requested != 0,
                    effect_status: if effect_status == "unknown" {
                        "unknown".to_owned()
                    } else {
                        "known".to_owned()
                    },
                    progress: parse_json_or(Some(progress_json)),
                    result: result_json.and_then(|text| serde_json::from_str(&text).ok()),
                    result_schema_id: None,
                    error: match (error_code, error_message) {
                        (Some(code), message) => Some(issue_json(
                            &code,
                            message.as_deref().unwrap_or("git mutation failed"),
                        )),
                        (None, _) => None,
                    },
                    final_cursor: None,
                    terminal: is_terminal_operation_state(&state),
                    created_at,
                }
            },
        )
        .collect())
}

fn read_history_operations(conn: &Connection) -> Result<Vec<UnifiedOperation>, ContractError> {
    if !table_exists(conn, "history_archive_operations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT operation_id, method_id, state, input_json, result_json, error_code,
                    error_message, created_at, updated_at
             FROM history_archive_operations",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    let mut out = Vec::with_capacity(rows.len());
    for (
        operation_id,
        method_id,
        state,
        input_json,
        result_json,
        error_code,
        error_message,
        created_at,
        updated_at,
    ) in rows
    {
        let input: Value = serde_json::from_str(&input_json).unwrap_or(Value::Null);
        let project_id = input.get("projectId").and_then(Value::as_str);
        let session_id = input.get("sessionId").and_then(Value::as_str);
        let scope = match (project_id, session_id) {
            (Some(pid), Some(sid)) => scope_session(pid, sid),
            (Some(pid), None) => scope_project(pid),
            _ => scope_instance(),
        };
        let revision = digest_revision(
            "hstop",
            &[
                state.clone(),
                result_json.clone().unwrap_or_default(),
                updated_at.clone(),
            ],
        );
        out.push(UnifiedOperation {
            source: "history_archive_operations",
            operation_id,
            operation_type: method_id,
            scope,
            state: state.clone(),
            revision,
            cancel_requested: false,
            effect_status: "known".to_owned(),
            progress: Vec::new(),
            result: result_json.and_then(|text| serde_json::from_str(&text).ok()),
            result_schema_id: None,
            error: match (error_code, error_message) {
                (Some(code), message) => Some(issue_json(
                    &code,
                    message.as_deref().unwrap_or("history operation failed"),
                )),
                (None, _) => None,
            },
            final_cursor: None,
            terminal: is_terminal_operation_state(&state),
            created_at,
        });
    }
    Ok(out)
}

fn read_config_operations(conn: &Connection) -> Result<Vec<UnifiedOperation>, ContractError> {
    if !table_exists(conn, "v1_config_operations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT operation_id, operation_type, scope_json, state, revision, cancel_requested,
                    effect_status, progress_json, result_json, error_json, created_at
             FROM v1_config_operations",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, String>(10)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    Ok(rows
        .into_iter()
        .map(
            |(
                operation_id,
                operation_type,
                scope_json,
                state,
                revision,
                cancel_requested,
                effect_status,
                progress_json,
                result_json,
                error_json,
                created_at,
            )| {
                let scope = serde_json::from_str(&scope_json).unwrap_or_else(|_| scope_instance());
                UnifiedOperation {
                    source: "v1_config_operations",
                    operation_id,
                    operation_type,
                    scope,
                    state: state.clone(),
                    revision,
                    cancel_requested: cancel_requested != 0,
                    effect_status: if effect_status == "unknown" {
                        "unknown".to_owned()
                    } else {
                        "known".to_owned()
                    },
                    progress: parse_json_or(Some(progress_json)),
                    result: result_json.and_then(|text| serde_json::from_str(&text).ok()),
                    result_schema_id: None,
                    error: error_json.and_then(|text| serde_json::from_str(&text).ok()),
                    final_cursor: None,
                    terminal: is_terminal_operation_state(&state),
                    created_at,
                }
            },
        )
        .collect())
}

/// `v1_domain_operations` (schedules / background_tasks). Pre-accept
/// rejections (`state='rejected'`) never became operations and are skipped.
/// The table stores no revision; the digest keeps the projection honest.
fn read_domain_operations(conn: &Connection) -> Result<Vec<UnifiedOperation>, ContractError> {
    if !table_exists(conn, "v1_domain_operations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT operation_id, method_id, state, completion_json, created_at, updated_at
             FROM v1_domain_operations",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    let mut out = Vec::new();
    for (operation_id, method_id, state, completion_json, created_at, updated_at) in rows {
        if state == "rejected" {
            continue;
        }
        let revision = digest_revision(
            "dstop",
            &[
                state.clone(),
                completion_json.clone().unwrap_or_default(),
                updated_at,
            ],
        );
        let result_val = completion_json
            .as_deref()
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        let error = if state == "failed" {
            let code = result_val
                .as_ref()
                .and_then(|v| v.get("code"))
                .and_then(Value::as_str)
                .unwrap_or(CODE_UNAVAILABLE);
            let message = result_val
                .as_ref()
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("domain operation failed");
            Some(issue_json(code, message))
        } else {
            None
        };
        out.push(UnifiedOperation {
            source: "v1_domain_operations",
            operation_id,
            operation_type: method_id,
            // The table records no scope; schedules/background-task records
            // project as instance-scoped (documented limitation — they stay
            // visible in every authorized listing instead of vanishing).
            scope: scope_instance(),
            state: state.clone(),
            revision,
            cancel_requested: false,
            effect_status: "known".to_owned(),
            progress: Vec::new(),
            result: if state == "failed" { None } else { result_val },
            result_schema_id: None,
            error,
            final_cursor: None,
            terminal: is_terminal_operation_state(&state),
            created_at,
        });
    }
    Ok(out)
}

/// The unified view: every projection source, ordered by (created_at, id).
fn unified_operations(conn: &Connection) -> Result<Vec<UnifiedOperation>, ContractError> {
    let mut all = Vec::new();
    all.extend(read_own_operations(conn)?);
    all.extend(read_async_operations(conn)?);
    all.extend(read_git_operations(conn)?);
    all.extend(read_history_operations(conn)?);
    all.extend(read_config_operations(conn)?);
    all.extend(read_domain_operations(conn)?);
    all.sort_by(|a, b| (&a.created_at, &a.operation_id).cmp(&(&b.created_at, &b.operation_id)));
    Ok(all)
}

fn find_unified_operation(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<UnifiedOperation>, ContractError> {
    if operation_id.is_empty() {
        return Ok(None);
    }
    Ok(unified_operations(conn)?
        .into_iter()
        .find(|op| op.operation_id == operation_id))
}

fn scope_matches(op: &UnifiedOperation, scope: &Value) -> bool {
    let kind = scope.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "instance" => true,
        "project" => {
            scope.get("projectId").and_then(Value::as_str)
                == op.scope.get("projectId").and_then(Value::as_str)
                && op.scope.get("kind").and_then(Value::as_str) == Some("project")
        }
        "session" => {
            op.scope.get("kind").and_then(Value::as_str) == Some("session")
                && scope.get("projectId").and_then(Value::as_str)
                    == op.scope.get("projectId").and_then(Value::as_str)
                && scope.get("sessionId").and_then(Value::as_str)
                    == op.scope.get("sessionId").and_then(Value::as_str)
        }
        _ => false,
    }
}

/// Folds the listed view into the digest a pagination cursor pins.
fn view_digest(ops: &[UnifiedOperation]) -> String {
    let parts: Vec<String> = ops.iter().map(UnifiedOperation::view_digest_part).collect();
    digest_revision("oplist", &parts)
}

// ---------------------------------------------------------------------------
// The seam: long-operation write API (future async tasks hook on here)
// ---------------------------------------------------------------------------

/// Terminal outcomes for [`V1Store::finish_long_operation`].
pub enum LongOperationOutcome {
    Succeeded {
        result: Value,
        result_schema_id: Option<String>,
    },
    Failed {
        code: String,
        message: String,
    },
    Cancelled,
}

impl V1Store {
    /// Begins one durable long operation (state `queued`, event
    /// `operation.queued`). `scope` is the contract `Scope` JSON
    /// (instance / project / session). Returns (operation_id, revision).
    pub fn begin_long_operation(
        &self,
        operation_type: &str,
        scope: &Value,
        cancellable: bool,
    ) -> Result<(String, String), ContractError> {
        let operation_id = self.next_id();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let rev = next_op_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let (scope_kind, project_id, session_id) = split_scope(scope);
        tx.execute(
            "INSERT INTO operations (operation_id, operation_type, scope_kind, project_id, session_id,
                state, revision, cancel_requested, cancellable, effect_status, progress_json,
                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, 0, ?7, 'known', '[]', ?8, ?8)",
            params![
                operation_id,
                operation_type,
                scope_kind,
                project_id,
                session_id,
                rev,
                i64::from(cancellable),
                now
            ],
        )
        .map_err(sql_err)?;
        append_operation_event_in_tx(
            &tx,
            &self.meta().instance_id,
            &operation_id,
            session_id.as_deref().unwrap_or(""),
            "operation.queued",
            &json!({"operationId": operation_id, "operationType": operation_type}),
        )?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        Ok((operation_id, rev))
    }

    /// Moves a queued operation to `running` (event `operation.started`).
    /// Idempotent: an already-running (or terminal) operation is a no-op.
    pub fn mark_operation_running(&self, operation_id: &str) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT state, COALESCE(session_id, '') FROM operations WHERE operation_id = ?1",
                params![operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((state, session_id)) = row else {
            return Err(not_found("operation", operation_id));
        };
        if state != "queued" {
            tx.commit().map_err(sql_err)?;
            return Ok(());
        }
        let rev = next_op_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        tx.execute(
            "UPDATE operations SET state = 'running', revision = ?1, updated_at = ?2
             WHERE operation_id = ?3 AND state = 'queued'",
            params![rev, now, operation_id],
        )
        .map_err(sql_err)?;
        append_operation_event_in_tx(
            &tx,
            &self.meta().instance_id,
            operation_id,
            &session_id,
            "operation.started",
            &json!({"operationId": operation_id}),
        )?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        Ok(())
    }

    /// Appends one progress record (event `operation.progress`).
    pub fn record_operation_progress(
        &self,
        operation_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT progress_json, COALESCE(session_id, '') FROM operations WHERE operation_id = ?1",
                params![operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((existing, session_id)) = row else {
            return Err(not_found("operation", operation_id));
        };
        let mut progress: Vec<Value> = serde_json::from_str(&existing).unwrap_or_default();
        progress.push(json!({"key": key, "value": value}));
        let progress_json = serde_json::to_string(&progress).unwrap_or_else(|_| "[]".into());
        let rev = next_op_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        tx.execute(
            "UPDATE operations SET progress_json = ?1, revision = ?2, updated_at = ?3
             WHERE operation_id = ?4",
            params![progress_json, rev, now, operation_id],
        )
        .map_err(sql_err)?;
        append_operation_event_in_tx(
            &tx,
            &self.meta().instance_id,
            operation_id,
            &session_id,
            "operation.progress",
            &json!({"operationId": operation_id, "progress": {"key": key, "value": value}}),
        )?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        Ok(())
    }

    /// Terminalizes the operation (`operation.completed` / `.failed` /
    /// `.cancelled`), recording result / error / final cursor. Idempotent: a
    /// second finish on an already-terminal operation changes nothing.
    pub fn finish_long_operation(
        &self,
        operation_id: &str,
        outcome: LongOperationOutcome,
    ) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let row: Option<(bool, String)> = tx
            .query_row(
                "SELECT terminal, COALESCE(session_id, '') FROM operations WHERE operation_id = ?1",
                params![operation_id],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((already_terminal, session_id)) = row else {
            return Err(not_found("operation", operation_id));
        };
        if already_terminal {
            tx.commit().map_err(sql_err)?;
            return Ok(());
        }
        let (state, result_json, result_schema_id, error_json, event_type) = match outcome {
            LongOperationOutcome::Succeeded {
                result,
                result_schema_id,
            } => (
                "succeeded",
                serde_json::to_string(&result).ok(),
                result_schema_id,
                None,
                "operation.completed",
            ),
            LongOperationOutcome::Failed { code, message } => (
                "failed",
                None,
                None,
                Some(
                    serde_json::to_string(&issue_json(&code, &message))
                        .unwrap_or_else(|_| "{\"code\":\"FAILED\"}".into()),
                ),
                "operation.failed",
            ),
            LongOperationOutcome::Cancelled => {
                ("cancelled", None, None, None, "operation.cancelled")
            }
        };
        let rev = next_op_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let cursor = append_operation_event_in_tx(
            &tx,
            &self.meta().instance_id,
            operation_id,
            &session_id,
            event_type,
            &json!({"operationId": operation_id, "state": state}),
        )?;
        tx.execute(
            "UPDATE operations SET state = ?1, revision = ?2, result_json = ?3, result_schema_id = ?4,
                error_json = ?5, final_cursor = ?6, terminal = 1, updated_at = ?7
             WHERE operation_id = ?8",
            params![
                state,
                rev,
                result_json,
                result_schema_id,
                error_json,
                cursor,
                now,
                operation_id
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        Ok(())
    }

    /// Whether a cancel intent has been recorded for the operation — the
    /// polling seam driving loops watch (the offline driver's maintenance
    /// directive uses exactly this).
    pub fn operation_cancel_requested(&self, operation_id: &str) -> Result<bool, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let requested: Option<i64> = conn
            .query_row(
                "SELECT cancel_requested FROM operations WHERE operation_id = ?1",
                params![operation_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        Ok(requested == Some(1))
    }
}

fn next_op_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let next: i64 = conn
        .query_row(
            "SELECT COALESCE(CAST(value AS INTEGER), 0) + 1 FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("rev-{next:08}"))
}

fn split_scope(scope: &Value) -> (&'static str, Option<String>, Option<String>) {
    match scope.get("kind").and_then(Value::as_str) {
        Some("project") => (
            "project",
            scope
                .get("projectId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            None,
        ),
        Some("session") => (
            "session",
            scope
                .get("projectId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            scope
                .get("sessionId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        ),
        _ => ("instance", None, None),
    }
}

/// Appends an operation-keyed event into the shared `events` table with the
/// same two-step cursor assignment the S2 core uses.
fn append_operation_event_in_tx(
    conn: &Connection,
    _instance_id: &str,
    operation_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<String, ContractError> {
    let placeholder = format!("tmp-op-{}", now_rfc3339());
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
        params![
            placeholder,
            operation_id,
            session_id,
            event_type,
            OPERATION_EVENT_SCHEMA,
            payload_str,
            now_rfc3339()
        ],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(sql_err)?;
    Ok(cursor)
}

// ---------------------------------------------------------------------------
// operations.get / operations.list
// ---------------------------------------------------------------------------

async fn operations_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let operation_id = input["operationId"].as_str().unwrap_or_default().to_owned();
    let store = rt.store().clone();
    let result = run_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let op = find_unified_operation(&conn, &operation_id)?
            .ok_or_else(|| not_found("operation", &operation_id))?;
        Ok(json!({"operation": op.to_contract_json()}))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn operations_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = input
        .get("scope")
        .cloned()
        .ok_or_else(|| invalid("`scope` is required".to_owned()))?;
    let operation_type = input
        .get("operationType")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let state = input
        .get("state")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(state_val) = state.as_deref()
        && !is_valid_operation_state(state_val)
    {
        return Err(invalid(format!("unknown operation state `{state_val}`")).into());
    }
    let limit = page_limit(input.get("page"))?;
    let cursor = input
        .get("page")
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let store = rt.store().clone();
    let result = run_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let listed: Vec<UnifiedOperation> = unified_operations(&conn)?
            .into_iter()
            .filter(|op| scope_matches(op, &scope))
            .filter(|op| operation_type.as_deref().is_none_or(|t| op.operation_type == t))
            .filter(|op| state.as_deref().is_none_or(|s| op.state == s))
            .collect();
        let digest = view_digest(&listed);

        let offset = match cursor.as_deref() {
            None => 0usize,
            Some(raw) => match parse_view_cursor(raw) {
                Some((pinned, off)) => {
                    if pinned != digest {
                        return Err(ContractError::new(
                            CODE_SNAPSHOT_CHANGED,
                            "the operation listing changed while paging; restart the query from the first page",
                            RetrySpec::after_reconcile(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    off
                }
                None => {
                    return Err(ContractError::new(
                        CODE_CURSOR_EXPIRED,
                        format!("invalid pagination cursor `{raw}`"),
                        RetrySpec::after_reconcile(),
                        EffectStatus::NotApplied,
                    ));
                }
            },
        };
        if offset > listed.len() {
            return Err(ContractError::new(
                CODE_CURSOR_EXPIRED,
                "pagination cursor beyond the end of the listing",
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let end = (offset + limit).min(listed.len());
        let complete = end >= listed.len();
        let items: Vec<Value> = listed[offset..end]
            .iter()
            .map(UnifiedOperation::to_contract_json)
            .collect();
        let next_cursor = if complete {
            Value::Null
        } else {
            json!(view_cursor(&digest, end))
        };
        Ok(json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": digest,
                "nextCursor": next_cursor,
                "complete": complete,
            }
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

fn is_valid_operation_state(state: &str) -> bool {
    matches!(
        state,
        "queued"
            | "running"
            | "waiting"
            | "cancelling"
            | "succeeded"
            | "failed"
            | "cancelled"
            | "interrupted"
            | "result_unknown"
    )
}

// ---------------------------------------------------------------------------
// operations.events (unary paged read + WebSocket StreamSource for follow)
// ---------------------------------------------------------------------------

async fn operations_events(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    if input.get("follow").and_then(Value::as_bool) == Some(true) {
        return Err(invalid(
            "follow=true is only supported over WebSocket sub.open; open a stream subscription instead".to_owned(),
        )
        .into());
    }
    let operation_id = input["operationId"].as_str().unwrap_or_default().to_owned();
    let after = input
        .get("afterCursor")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let limit = input
        .get("page")
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;

    let store = rt.store().clone();
    let result = run_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let op = find_unified_operation(&conn, &operation_id)?
            .ok_or_else(|| not_found("operation", &operation_id))?;
        let earliest_seq = earliest_retained_seq(&conn)?;
        let earliest_cursor = format_cursor(earliest_seq);

        let after_seq = match after.as_deref() {
            None => earliest_seq.saturating_sub(1),
            Some(c) => match parse_cursor(c) {
                Some(seq) if seq >= earliest_seq.saturating_sub(1) => seq,
                Some(_) => {
                    return Ok(json!({"frame": {
                        "kind": "gap",
                        "reason": "cursor_expired",
                        "resourceId": operation_id,
                        "recoveryMethod": "operations.snapshot",
                        "requestedCursor": c,
                        "earliestCursor": earliest_cursor,
                        "snapshotRevision": op.revision,
                        "recovery": "fetch_snapshot_then_resume",
                    }}));
                }
                None => {
                    return Ok(json!({"frame": {
                        "kind": "gap",
                        "reason": "cursor_invalid",
                        "resourceId": operation_id,
                        "recoveryMethod": "operations.snapshot",
                        "requestedCursor": c,
                        "earliestCursor": earliest_cursor,
                        "snapshotRevision": op.revision,
                        "recovery": "restart_query",
                    }}));
                }
            },
        };

        let events = fetch_operation_events_after(&conn, &operation_id, after_seq, limit)?;
        let high_watermark: Option<String> = conn
            .query_row(
                "SELECT cursor FROM events WHERE resource_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![operation_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let next_cursor = events.last().map(|e| e.1.clone());
        let instance_id = store.meta().instance_id.clone();
        let event_objs: Vec<Value> = events
            .iter()
            .map(
                |(_seq, cursor, event_type, schema_id, payload_json, occurred_at, data_json)| {
                    json!({
                        "resourceId": operation_id,
                        "cursor": cursor,
                        "type": event_type,
                        "schemaId": schema_id,
                        "payload": serde_json::from_str::<Value>(payload_json).unwrap_or(Value::Null),
                        "occurredAt": occurred_at,
                        "version": "agentcpm-pd.event/1",
                        "instanceId": instance_id,
                        "data": serde_json::from_str::<Value>(data_json).unwrap_or(json!({})),
                    })
                },
            )
            .collect();
        let caught_up = match (&next_cursor, &high_watermark) {
            (Some(nc), Some(hw)) => nc == hw,
            _ => true,
        };
        Ok(json!({"frame": {
            "events": event_objs,
            "nextCursor": next_cursor,
            "highWatermark": high_watermark,
            "caughtUp": caught_up,
            "terminal": op.terminal && caught_up,
        }}))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

fn op_cursor_expired_error(
    requested: &str,
    earliest: &str,
    revision: &str,
    recovery: GapRecoveryPolicy,
    message: String,
) -> ContractError {
    let mut details = serde_json::Map::new();
    details.insert(
        "gap".to_owned(),
        serde_json::to_value(GapFrame {
            kind: "gap".to_owned(),
            requested_cursor: Some(requested.to_owned()),
            earliest_cursor: Some(earliest.to_owned()),
            snapshot_revision: revision.to_owned(),
            recovery,
        })
        .unwrap_or(Value::Null),
    );
    let mut err = ContractError::new(
        CODE_CURSOR_EXPIRED,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    );
    err.details = Some(details);
    err
}

struct OperationsEventsSource {
    store: Arc<V1Store>,
    instance_id: String,
    operation_id: String,
    sub_id: String,
    last_fetched_seq: i64,
}

fn operations_events_source(
    state: &Arc<PublicV1State>,
    input: &Value,
    sub_id: &str,
) -> Result<Box<dyn StreamSource>, ContractError> {
    let operation_id = input
        .get("operationId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("`operationId` is required".to_owned()))?
        .to_owned();
    let after_cursor = input
        .get("afterCursor")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let store = state.runtime.store().clone();
    let instance_id = store.meta().instance_id.clone();

    let start_seq = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let op = find_unified_operation(&conn, &operation_id)?
            .ok_or_else(|| not_found("operation", &operation_id))?;
        let earliest_seq = earliest_retained_seq(&conn)?;
        let earliest_cursor = format_cursor(earliest_seq);
        match after_cursor.as_deref() {
            None => earliest_seq.saturating_sub(1),
            Some(c) => match parse_cursor(c) {
                Some(seq) if seq >= earliest_seq.saturating_sub(1) => seq,
                Some(_) => {
                    return Err(op_cursor_expired_error(
                        c,
                        &earliest_cursor,
                        &op.revision,
                        GapRecoveryPolicy::FetchSnapshotThenResume,
                        format!(
                            "afterCursor `{c}` is older than the earliest retained event; recover via operations.snapshot -> checkpoint.cursor -> sub.open"
                        ),
                    ));
                }
                None => {
                    return Err(op_cursor_expired_error(
                        c,
                        &earliest_cursor,
                        &op.revision,
                        GapRecoveryPolicy::RestartQuery,
                        format!("invalid cursor `{c}`"),
                    ));
                }
            },
        }
    };

    Ok(Box::new(OperationsEventsSource {
        store,
        instance_id,
        operation_id,
        sub_id: sub_id.to_owned(),
        last_fetched_seq: start_seq,
    }))
}

impl StreamSource for OperationsEventsSource {
    fn fetch_next(&mut self, batch: usize) -> Result<StreamFetch, ContractError> {
        let conn = self.store.conn.lock().expect("sqlite mutex");
        let op = find_unified_operation(&conn, &self.operation_id)?
            .ok_or_else(|| not_found("operation", &self.operation_id))?;
        let earliest_seq = earliest_retained_seq(&conn)?;
        if self.last_fetched_seq < earliest_seq.saturating_sub(1) {
            return Err(op_cursor_expired_error(
                &format_cursor(self.last_fetched_seq),
                &format_cursor(earliest_seq),
                &op.revision,
                GapRecoveryPolicy::FetchSnapshotThenResume,
                "event cursor fell behind retained history; recover via operations.snapshot"
                    .to_owned(),
            ));
        }

        let events =
            fetch_operation_events_after(&conn, &self.operation_id, self.last_fetched_seq, batch)?;
        let mut frames = Vec::with_capacity(events.len());
        for (seq, cursor, event_type, _schema_id, _payload_json, _occurred_at, data_json) in events
        {
            self.last_fetched_seq = seq;
            let data_val: Value = serde_json::from_str(&data_json).unwrap_or_else(|_| json!({}));
            let data = data_val.as_object().cloned().unwrap_or_default();
            let notif = JsonRpcNotification {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                method: WIRE_SUB_EVENT.to_owned(),
                params: serde_json::to_value(SubEventNotification {
                    subscription_id: self.sub_id.clone(),
                    event: EventEnvelope {
                        version: "agentcpm-pd.event/1".to_owned(),
                        instance_id: self.instance_id.clone(),
                        run_id: None,
                        operation_id: Some(self.operation_id.clone()),
                        subscription_id: Some(self.sub_id.clone()),
                        cursor: cursor.clone(),
                        event_type,
                        data,
                    },
                })
                .unwrap_or(Value::Null),
            };
            let text = serde_json::to_string(&notif).unwrap_or_default();
            frames.push(StreamFrame {
                estimated_bytes: text.len() + 128,
                text,
                cursor,
            });
        }
        Ok(StreamFetch {
            frames,
            stream_ended: op.terminal,
        })
    }

    fn wake(&self) -> Arc<Notify> {
        self.store.event_notifier()
    }

    fn oversized_recovery_hint(&self) -> &'static str {
        "resume from operations.snapshot"
    }
}

fn earliest_retained_seq(conn: &Connection) -> Result<i64, ContractError> {
    let seq: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'earliest_cursor_seq'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(seq.unwrap_or(1))
}

#[allow(clippy::type_complexity)]
fn fetch_operation_events_after(
    conn: &Connection,
    operation_id: &str,
    after_seq: i64,
    limit: usize,
) -> Result<Vec<(i64, String, String, String, String, String, String)>, ContractError> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, cursor, event_type, schema_id, payload_json, occurred_at, data_json
             FROM events WHERE resource_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![operation_id, after_seq, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// operations.cancel
// ---------------------------------------------------------------------------

async fn operations_cancel(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let operation_id = input["operationId"].as_str().unwrap_or_default().to_owned();
    let store = rt.store().clone();
    let result = run_blocking(move || {
        let instance_id = store.meta().instance_id.clone();
        let lookup_input = json!({"instanceId": instance_id, "operationId": operation_id});
        let digest = compute_input_digest("operations.cancel", &lookup_input);
        let retained_until = {
            use chrono::Duration;
            (chrono::Utc::now() + Duration::days(7)).to_rfc3339()
        };

        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let now = now_rfc3339();

        // Idempotency (mirror of run.cancel's machinery): same key + same input
        // replays the stored receipt; a different input conflicts.
        if let Some(key) = key.as_deref() {
            let existing: Option<(String, String, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, request_state, receipt_json
                     FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = 'operations.cancel'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(sql_err)?;
            if let Some((stored_digest, req_state, receipt_json)) = existing {
                if stored_digest != digest {
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for operations.cancel with a different operationId"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if req_state == "accepted"
                    && let Some(raw) = receipt_json
                    && let Ok(receipt) = serde_json::from_str::<Value>(&raw)
                {
                    return Ok(json!({"receipt": receipt}));
                }
            }
        }

        let row: Option<(String, String, i64, i64, String, bool)> = tx
            .query_row(
                "SELECT state, revision, cancel_requested, cancellable, COALESCE(session_id, ''), terminal
                 FROM operations WHERE operation_id = ?1",
                params![operation_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)? != 0,
                    ))
                },
            )
            .optional()
            .map_err(sql_err)?;

        let Some((state, revision, cancel_requested, cancellable, session_id, terminal)) = row else {
            // Not a seam-owned row: either it exists in a foreign projection
            // (no cancellation path there) or it does not exist at all.
            let foreign = find_unified_operation(&tx, &operation_id)?;
            let error = match foreign {
                Some(_) => cancellation_unsupported(
                    &operation_id,
                    "this operation type has no cancellation path",
                ),
                None => not_found("operation", &operation_id),
            };
            if let Some(key) = key.as_deref() {
                tx.execute(
                    "INSERT OR REPLACE INTO idempotency_records
                        (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                         receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                     VALUES (?1, 'operations.cancel', ?2, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?3, ?4, ?5, 'correctRejectedInputWithNewKey')",
                    params![key, digest, error.code, now, retained_until],
                )
                .map_err(sql_err)?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(error);
        };

        let receipt = if terminal {
            json!({
                "disposition": "alreadyTerminal",
                "revision": revision,
                "state": state,
            })
        } else if cancellable == 0 {
            // Seam-owned but non-cancellable (e.g. a migration that cannot stop).
            let error =
                cancellation_unsupported(&operation_id, "this operation type cannot be cancelled");
            if let Some(key) = key.as_deref() {
                tx.execute(
                    "INSERT OR REPLACE INTO idempotency_records
                        (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                         receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                     VALUES (?1, 'operations.cancel', ?2, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?3, ?4, ?5, 'correctRejectedInputWithNewKey')",
                    params![key, digest, error.code, now, retained_until],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            return Err(error);
        } else if cancel_requested != 0 {
            // Repeated cancel: exactly one durable intent — replay the current
            // state without writing a second request.
            json!({
                "disposition": "requested",
                "revision": revision,
                "state": state,
            })
        } else {
            let new_rev = next_op_rev_in_tx(&tx)?;
            append_operation_event_in_tx(
                &tx,
                &instance_id,
                &operation_id,
                &session_id,
                "operation.cancel_requested",
                &json!({"operationId": operation_id}),
            )?;
            tx.execute(
                "UPDATE operations SET cancel_requested = 1, state = 'cancelling', revision = ?1, updated_at = ?2
                 WHERE operation_id = ?3",
                params![new_rev, now_rfc3339(), operation_id],
            )
            .map_err(sql_err)?;
            json!({
                "disposition": "requested",
                "revision": new_rev,
                "state": "cancelling",
            })
        };
        if let Some(key) = key.as_deref() {
            tx.execute(
                "INSERT OR REPLACE INTO idempotency_records
                    (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                     receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'operations.cancel', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&json!({"kind": "operation", "operationId": operation_id}))
                        .unwrap_or_default(),
                    serde_json::to_string(&receipt).unwrap_or_default(),
                    now,
                    retained_until
                ],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        drop(conn);
        store.event_notifier().notify_waiters();
        store.state_notifier().notify_waiters();
        Ok(json!({"receipt": receipt}))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

// ---------------------------------------------------------------------------
// operations.snapshot
// ---------------------------------------------------------------------------

async fn operations_snapshot(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let operation_id = input["operationId"].as_str().unwrap_or_default().to_owned();
    let store = rt.store().clone();
    let result = run_blocking(move || {
        let (op, op_json, serialized, sha256, checkpoint_cursor, project_id, reused_artifact_id) = {
            let conn = store.conn.lock().expect("sqlite mutex");
            let op = find_unified_operation(&conn, &operation_id)?
                .ok_or_else(|| not_found("operation", &operation_id))?;

            // Same-lock commit watermark: when an operation has emitted no events
            // yet, anchor at `max(global_max_seq, earliest_seq - 1)` instead of
            // `cur-0` so subsequent `operations.events` resume avoids an expired
            // cursor loop after event retention advances (#12).
            let op_last_seq: Option<i64> = conn
                .query_row(
                    "SELECT seq FROM events WHERE resource_id = ?1 ORDER BY seq DESC LIMIT 1",
                    params![operation_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            let global_max_seq: i64 = conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
                .map_err(sql_err)?;
            let earliest_seq = earliest_retained_seq(&conn)?;
            let effective_seq = op_last_seq
                .unwrap_or(global_max_seq)
                .max(earliest_seq.saturating_sub(1));
            let checkpoint_cursor = format_cursor(effective_seq);

            let op_json = op.to_contract_json();
            let serialized = serde_json::to_vec(&op_json).unwrap_or_default();
            let sha256 = format!("{:x}", Sha256::digest(&serialized));
            let project_id = op
                .scope
                .get("projectId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("instance")
                .to_owned();

            let reused = if table_exists(&conn, "artifacts")? {
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT artifact_id FROM artifacts
                         WHERE project_id = ?1 AND sha256 = ?2 AND origin = 'operation_snapshot'
                         ORDER BY created_at ASC LIMIT 1",
                        params![project_id, sha256],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql_err)?;
                existing.filter(|id| store.home().join("v1").join("artifacts").join(id).is_file())
            } else {
                None
            };
            (
                op,
                op_json,
                serialized,
                sha256,
                checkpoint_cursor,
                project_id,
                reused,
            )
        };

        // Publish through the standard artifact store (after dropping `store.conn`
        // lock!) so `artifacts.get` / `artifacts.read` resolve `artifactId` (#12).
        let artifact_id = match reused_artifact_id {
            Some(id) => id,
            None => {
                let artifact = publish_artifact(
                    &store,
                    &project_id,
                    "application/json",
                    Some(&format!("operation-{operation_id}-state.json")),
                    &serialized,
                    "operation_snapshot",
                )?;
                artifact["artifactId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            }
        };

        Ok(json!({
            "operation": op_json,
            "snapshot": {
                "checkpoint": {
                    "instanceId": store.meta().instance_id,
                    "resourceKind": "operation",
                    "resourceId": operation_id,
                    "cursor": checkpoint_cursor,
                    "revision": op.revision,
                },
                "sections": [{
                    "name": "state",
                    "schemaId": OPERATION_STATE_SECTION_SCHEMA,
                    "artifactId": artifact_id,
                    "sha256": sha256,
                }],
            }
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

// ---------------------------------------------------------------------------
// resources.snapshot
// ---------------------------------------------------------------------------

/// One resolved resource version (contract `$defs.ResourceVersion`; the
/// optional `snapshotArtifactId` is not emitted in this slice — no public
/// artifact read path exists for resource snapshots yet, so the revision
/// carries the state identity).
///
/// `pub(super)`: the `resources.watch` domain (`domains/resources.rs`, t023
/// E) reuses the resolver and this row shape for its frames — same module
/// tree, no logic change.
pub(super) struct ResourceVersionOut {
    pub(super) resource: Value,
    pub(super) revision: String,
    pub(super) state: &'static str,
    pub(super) snapshot_schema_id: Option<&'static str>,
}

async fn resources_snapshot(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let resources = input
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if resources.is_empty() {
        return Err(invalid("`resources` must list at least one resource".to_owned()).into());
    }
    if resources.len() > MAX_SNAPSHOT_RESOURCES {
        return Err(ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!(
                "one snapshot may reference at most {MAX_SNAPSHOT_RESOURCES} resources (got {})",
                resources.len()
            ),
            RetrySpec::backoff_read(Some(100)),
            EffectStatus::NotApplied,
        )
        .into());
    }
    let known: Vec<(Value, String)> = input
        .get("knownVersions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let revision = entry.get("revision")?.as_str()?.to_owned();
            let resource = entry.get("resource")?.clone();
            Some((resource, revision))
        })
        .collect();

    let store = rt.store().clone();
    let result = run_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let observed_at = now_rfc3339();

        let mut snapshots = Vec::with_capacity(resources.len());
        let mut unchanged = Vec::new();
        for resource in &resources {
            let out = resolve_resource_version(&store, &conn, resource)?;
            if let Some((known_ref, known_rev)) = known.iter().find(|(r, _)| r == resource)
                && known_rev == &out.revision
            {
                unchanged.push(known_ref.clone());
                continue;
            }
            snapshots.push(resource_version_json(&out));
        }

        Ok(json!({
            "instanceId": store.meta().instance_id,
            "snapshots": snapshots,
            "unchanged": unchanged,
            "observedAt": observed_at,
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

/// The `ResourceVersion` JSON object; shared with the `resources.watch`
/// frames (t023 E) so a watch snapshot and a `resources.snapshot` entry
/// serialize identically.
pub(super) fn resource_version_json(out: &ResourceVersionOut) -> Value {
    let mut obj = Map::new();
    obj.insert("resource".into(), out.resource.clone());
    obj.insert("revision".into(), json!(out.revision));
    obj.insert("state".into(), json!(out.state));
    if let Some(schema_id) = out.snapshot_schema_id {
        obj.insert("snapshotSchemaId".into(), json!(schema_id));
    }
    Value::Object(obj)
}

/// Resolves the current version of one `ResourceRef`. Sources are the
/// instance's authoritative tables; kinds whose owning subsystem is absent
/// (or exposes no revision source yet) return `unavailable` — never a fake
/// `present`.
///
/// `pub(super)`: the `resources.watch` stream's conditional polls (t023 E)
/// resolve through this same function — the authoritative result always
/// comes from the resource snapshot tables, never from a side cache.
pub(super) fn resolve_resource_version(
    store: &V1Store,
    conn: &Connection,
    resource: &Value,
) -> Result<ResourceVersionOut, ContractError> {
    let kind = resource.get("kind").and_then(Value::as_str).unwrap_or("");
    let resource_id = resource.get("resourceId").and_then(Value::as_str);
    let project_id = resource.get("projectId").and_then(Value::as_str);
    match kind {
        "projects" => {
            let mut stmt = conn
                .prepare(
                    "SELECT project_id, name, registration_state, environment_id, revision, created_at
                     FROM projects ORDER BY created_at ASC, project_id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            drop(stmt);
            let parts: Vec<String> = rows
                .iter()
                .flat_map(|(id, name, state, env, rev, created)| {
                    vec![
                        id.clone(),
                        name.clone(),
                        state.clone(),
                        env.clone(),
                        rev.clone(),
                        created.clone(),
                    ]
                })
                .collect();
            Ok(ResourceVersionOut {
                resource: resource.clone(),
                revision: digest_revision("rsrc-projects", &parts),
                state: "present",
                snapshot_schema_id: Some("resources.projects.v1"),
            })
        }
        "project" => {
            let target = resource_id.or(project_id).unwrap_or_default();
            let row: Option<(String, String, String, String, String)> = conn
                .query_row(
                    "SELECT name, registration_state, environment_id, revision, created_at
                     FROM projects WHERE project_id = ?1",
                    params![target],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()
                .map_err(sql_err)?;
            match row {
                Some((name, state, env, revision, created)) => Ok(ResourceVersionOut {
                    resource: resource.clone(),
                    revision: digest_revision(
                        "rsrc-project",
                        &[
                            target.to_owned(),
                            name.to_owned(),
                            state.to_owned(),
                            env.to_owned(),
                            revision.to_owned(),
                            created.to_owned(),
                        ],
                    ),
                    state: "present",
                    snapshot_schema_id: Some("resources.project.v1"),
                }),
                None => Ok(ResourceVersionOut {
                    resource: resource.clone(),
                    revision: "absent".to_owned(),
                    state: "absent",
                    snapshot_schema_id: None,
                }),
            }
        }
        "sessions" => {
            let filter = project_id.unwrap_or("");
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, project_id, revision, active_run_id, session_state, queue_paused, queue_revision, created_at
                     FROM sessions WHERE ?1 = '' OR project_id = ?1
                     ORDER BY created_at ASC, session_id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![filter], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            drop(stmt);
            // Session activity points at the authoritative activeRunId: it
            // participates in the revision, so a changed activity bumps it.
            let parts: Vec<String> = rows
                .iter()
                .flat_map(|(id, pid, rev, active, state, paused, qrev, created)| {
                    vec![
                        id.clone(),
                        pid.clone(),
                        rev.clone(),
                        active.clone().unwrap_or_default(),
                        state.clone(),
                        paused.to_string(),
                        qrev.clone(),
                        created.clone(),
                    ]
                })
                .collect();
            Ok(ResourceVersionOut {
                resource: resource.clone(),
                revision: digest_revision("rsrc-sessions", &parts),
                state: "present",
                snapshot_schema_id: Some("resources.sessions.v1"),
            })
        }
        "session" => {
            let target = resource_id
                .or_else(|| resource.get("sessionId").and_then(Value::as_str))
                .unwrap_or_default();
            let row: Option<(
                String,
                Option<String>,
                String,
                i64,
                String,
                String,
            )> = conn
                .query_row(
                    "SELECT revision, active_run_id, session_state, queue_paused, queue_revision, project_id
                     FROM sessions WHERE session_id = ?1 AND session_state != 'deleted'",
                    params![target],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(sql_err)?;
            match row {
                Some((revision, active, state, paused, qrev, _project)) => {
                    let parts = vec![
                        target.to_owned(),
                        revision.clone(),
                        active.unwrap_or_default().to_string(),
                        state.to_string(),
                        paused.to_string(),
                        qrev.clone(),
                    ];
                    Ok(ResourceVersionOut {
                        resource: resource.clone(),
                        revision: digest_revision("rsrc-session", &parts),
                        state: "present",
                        snapshot_schema_id: Some("resources.session.v1"),
                    })
                }
                None => Ok(ResourceVersionOut {
                    resource: resource.clone(),
                    revision: "absent".to_owned(),
                    state: "absent",
                    snapshot_schema_id: None,
                }),
            }
        }
        "config" => {
            if !table_exists(conn, "v1_config_effective")? {
                return Ok(unavailable(resource));
            }
            // Distinguishes validity, the loaded generation, and the running
            // state — all three fold into the revision.
            let generation: String = conn
                .query_row(
                    "SELECT generation FROM v1_config_effective WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?
                .unwrap_or_else(|| "base".to_owned());
            let reload_state: String = conn
                .query_row(
                    "SELECT reload_state FROM v1_config_effective WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?
                .unwrap_or_else(|| "current".to_owned());
            let mut parts = vec![generation, reload_state];
            if table_exists(conn, "v1_config_layer_state")? {
                let mut stmt = conn
                    .prepare(
                        "SELECT layer, stored_revision FROM v1_config_layer_state ORDER BY layer",
                    )
                    .map_err(sql_err)?;
                let layers = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(sql_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_err)?;
                drop(stmt);
                for (layer, rev) in layers {
                    parts.push(layer);
                    parts.push(rev);
                }
            }
            Ok(ResourceVersionOut {
                resource: resource.clone(),
                revision: digest_revision("rsrc-config", &parts),
                state: "present",
                snapshot_schema_id: Some("resources.config.v1"),
            })
        }
        "uploads" => {
            if !table_exists(conn, "uploads")? {
                return Ok(unavailable(resource));
            }
            let filter = project_id.unwrap_or("");
            let mut stmt = conn
                .prepare(
                    "SELECT upload_id, state, revision, updated_at FROM uploads
                     WHERE (?1 = '' OR project_id = ?1)
                     ORDER BY created_at ASC, upload_id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![filter], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            drop(stmt);
            let parts: Vec<String> = rows
                .into_iter()
                .flat_map(|(id, state, rev, updated)| vec![id, state, rev, updated])
                .collect();
            Ok(ResourceVersionOut {
                resource: resource.clone(),
                revision: digest_revision("rsrc-uploads", &parts),
                state: "present",
                snapshot_schema_id: Some("resources.uploads.v1"),
            })
        }
        "files" => {
            if let Some(pid) = project_id.or(resource_id).filter(|s| !s.is_empty()) {
                match resolve_project_workspace_dir(store, conn, pid)? {
                    ProjectWorkspace::Absent => Ok(absent_resource(resource)),
                    ProjectWorkspace::Unavailable => Ok(unavailable(resource)),
                    ProjectWorkspace::Dir(ws_dir) => {
                        let Ok(parts) = fingerprint_workspace_files(&ws_dir) else {
                            return Ok(unavailable(resource));
                        };
                        Ok(ResourceVersionOut {
                            resource: resource.clone(),
                            revision: digest_revision("rsrc-files", &parts),
                            state: "present",
                            snapshot_schema_id: Some("resources.files.v1"),
                        })
                    }
                }
            } else {
                let mut stmt = conn
                    .prepare(
                        "SELECT project_id FROM projects ORDER BY created_at ASC, project_id ASC",
                    )
                    .map_err(sql_err)?;
                let pids: Vec<String> = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(sql_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_err)?;
                drop(stmt);
                if pids.is_empty() {
                    return Ok(ResourceVersionOut {
                        resource: resource.clone(),
                        revision: digest_revision("rsrc-files", &[]),
                        state: "present",
                        snapshot_schema_id: Some("resources.files.v1"),
                    });
                }
                let mut parts = Vec::new();
                let mut any_resolved = false;
                for pid in pids {
                    if let ProjectWorkspace::Dir(ws_dir) =
                        resolve_project_workspace_dir(store, conn, &pid)?
                        && let Ok(fp) = fingerprint_workspace_files(&ws_dir)
                    {
                        any_resolved = true;
                        parts.push(pid);
                        parts.extend(fp);
                    }
                }
                if !any_resolved {
                    return Ok(unavailable(resource));
                }
                Ok(ResourceVersionOut {
                    resource: resource.clone(),
                    revision: digest_revision("rsrc-files", &parts),
                    state: "present",
                    snapshot_schema_id: Some("resources.files.v1"),
                })
            }
        }
        "git" => {
            if let Some(pid) = project_id.or(resource_id).filter(|s| !s.is_empty()) {
                match resolve_project_workspace_dir(store, conn, pid)? {
                    ProjectWorkspace::Absent => Ok(absent_resource(resource)),
                    ProjectWorkspace::Unavailable => Ok(unavailable(resource)),
                    ProjectWorkspace::Dir(ws_dir) => {
                        let Some(parts) = git_project_parts(conn, pid, &ws_dir)? else {
                            return Ok(unavailable(resource));
                        };
                        Ok(ResourceVersionOut {
                            resource: resource.clone(),
                            revision: digest_revision("rsrc-git", &parts),
                            state: "present",
                            snapshot_schema_id: Some("resources.git.v1"),
                        })
                    }
                }
            } else {
                let mut stmt = conn
                    .prepare(
                        "SELECT project_id FROM projects ORDER BY created_at ASC, project_id ASC",
                    )
                    .map_err(sql_err)?;
                let pids: Vec<String> = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(sql_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_err)?;
                drop(stmt);
                let mut parts = Vec::new();
                let mut any_git = false;
                for pid in pids {
                    if let ProjectWorkspace::Dir(ws_dir) =
                        resolve_project_workspace_dir(store, conn, &pid)?
                        && let Some(gparts) = git_project_parts(conn, &pid, &ws_dir)?
                    {
                        any_git = true;
                        parts.extend(gparts);
                    }
                }
                if !any_git {
                    return Ok(unavailable(resource));
                }
                Ok(ResourceVersionOut {
                    resource: resource.clone(),
                    revision: digest_revision("rsrc-git", &parts),
                    state: "present",
                    snapshot_schema_id: Some("resources.git.v1"),
                })
            }
        }
        "memory" => {
            if !table_exists(conn, "v1_memory_entries")? {
                return Ok(unavailable(resource));
            }
            if let Some(mid) = resource_id.filter(|s| !s.is_empty()) {
                type MemoryEntryRow = (
                    Option<String>,
                    String,
                    String,
                    String,
                    String,
                    i64,
                    i64,
                    String,
                );
                let row: Option<MemoryEntryRow> = conn
                    .query_row(
                        "SELECT project_id, scope_kind, kind, name, revision, deprecated, read_only, updated_at
                         FROM v1_memory_entries WHERE memory_id = ?1",
                        params![mid],
                        |r| {
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
                        },
                    )
                    .optional()
                    .map_err(sql_err)?;
                let Some((pid_opt, scope_kind, mkind, name, rev, dep, ro, updated)) = row else {
                    return Ok(absent_resource(resource));
                };
                if let Some(req_pid) = project_id.filter(|s| !s.is_empty())
                    && (pid_opt.as_deref() != Some(req_pid) || !project_row_exists(conn, req_pid)?)
                {
                    return Ok(absent_resource(resource));
                }
                let parts = vec![
                    mid.to_owned(),
                    pid_opt.unwrap_or_default(),
                    scope_kind,
                    mkind,
                    name,
                    rev,
                    dep.to_string(),
                    ro.to_string(),
                    updated,
                ];
                return Ok(ResourceVersionOut {
                    resource: resource.clone(),
                    revision: digest_revision("rsrc-memory", &parts),
                    state: "present",
                    snapshot_schema_id: Some("resources.memory.v1"),
                });
            }

            let filter = project_id.unwrap_or("");
            if !filter.is_empty() && !project_row_exists(conn, filter)? {
                return Ok(absent_resource(resource));
            }
            let mut stmt = conn
                .prepare(
                    "SELECT memory_id, COALESCE(project_id, ''), revision, deprecated, read_only, updated_at
                     FROM v1_memory_entries
                     WHERE (?1 = '' OR project_id = ?1)
                     ORDER BY created_at ASC, memory_id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![filter], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            drop(stmt);
            let mut parts: Vec<String> = rows
                .into_iter()
                .flat_map(|(id, pid, rev, dep, ro, updated)| {
                    vec![id, pid, rev, dep.to_string(), ro.to_string(), updated]
                })
                .collect();
            if table_exists(conn, "v1_memory_project_meta")? {
                let mut meta_stmt = conn
                    .prepare(
                        "SELECT project_id, status, revision, read_only, updated_at
                         FROM v1_memory_project_meta
                         WHERE (?1 = '' OR project_id = ?1)
                         ORDER BY project_id ASC",
                    )
                    .map_err(sql_err)?;
                let meta_rows = meta_stmt
                    .query_map(params![filter], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })
                    .map_err(sql_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_err)?;
                drop(meta_stmt);
                for (pid, st, rev, ro, updated) in meta_rows {
                    parts.extend([pid, st, rev, ro.to_string(), updated]);
                }
            }
            if filter.is_empty() && table_exists(conn, "v1_memory_settings")? {
                let setting: Option<(String, i64, i64, String, String)> = conn
                    .query_row(
                        "SELECT reasoning_mode, auto_index_interval_minutes, auto_dream_interval_minutes, revision, updated_at
                         FROM v1_memory_settings WHERE id = 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()
                    .map_err(sql_err)?;
                if let Some((mode, idx_m, drm_m, rev, updated)) = setting {
                    parts.extend([mode, idx_m.to_string(), drm_m.to_string(), rev, updated]);
                }
            }
            Ok(ResourceVersionOut {
                resource: resource.clone(),
                revision: digest_revision("rsrc-memory", &parts),
                state: "present",
                snapshot_schema_id: Some("resources.memory.v1"),
            })
        }
        "terminal" => {
            let tid_opt = resource_id.filter(|s| !s.is_empty());
            let pid_opt = project_id.filter(|s| !s.is_empty());
            if tid_opt.is_none() && pid_opt.is_none() {
                return Ok(unavailable(resource));
            }
            if !table_exists(conn, "terminals")? {
                return Ok(unavailable(resource));
            }
            let current_epoch = &store.meta().generation;
            if let Some(tid) = tid_opt {
                #[allow(clippy::type_complexity)]
                let row: Option<(
                    String,
                    String,
                    String,
                    i64,
                    Option<String>,
                    i64,
                    Option<i64>,
                    Option<String>,
                    i64,
                    i64,
                    String,
                )> = conn
                    .query_row(
                        "SELECT project_id, server_epoch, state, control_epoch, controller_id,
                                latest_output_offset, exit_code, exit_signal, term_rows, term_columns, updated_at
                         FROM terminals WHERE terminal_id = ?1",
                        params![tid],
                        |r| {
                            Ok((
                                r.get(0)?,
                                r.get(1)?,
                                r.get(2)?,
                                r.get(3)?,
                                r.get(4)?,
                                r.get(5)?,
                                r.get(6)?,
                                r.get(7)?,
                                r.get(8)?,
                                r.get(9)?,
                                r.get(10)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(sql_err)?;
                let Some((
                    pid,
                    srv_epoch,
                    term_state,
                    ctrl_epoch,
                    ctrl_id,
                    out_off,
                    exit_code,
                    exit_signal,
                    term_rows,
                    term_columns,
                    updated_at,
                )) = row
                else {
                    return Ok(absent_resource(resource));
                };
                if pid_opt.is_some_and(|p| p != pid) {
                    return Ok(absent_resource(resource));
                }
                let eff_state = if !matches!(term_state.as_str(), "exited" | "lost")
                    && &srv_epoch != current_epoch
                {
                    "lost".to_owned()
                } else {
                    term_state
                };
                let parts = vec![
                    tid.to_owned(),
                    pid,
                    srv_epoch,
                    eff_state,
                    ctrl_epoch.to_string(),
                    ctrl_id.unwrap_or_default(),
                    out_off.to_string(),
                    exit_code.map(|c| c.to_string()).unwrap_or_default(),
                    exit_signal.unwrap_or_default(),
                    term_rows.to_string(),
                    term_columns.to_string(),
                    updated_at,
                ];
                return Ok(ResourceVersionOut {
                    resource: resource.clone(),
                    revision: digest_revision("rsrc-terminal", &parts),
                    state: "present",
                    snapshot_schema_id: Some("resources.terminal.v1"),
                });
            }

            let pid = pid_opt.expect("checked above");
            if !project_row_exists(conn, pid)? {
                return Ok(absent_resource(resource));
            }
            let mut stmt = conn
                .prepare(
                    "SELECT terminal_id, server_epoch, state, control_epoch, controller_id,
                            latest_output_offset, exit_code, exit_signal, term_rows, term_columns, updated_at
                     FROM terminals WHERE project_id = ?1
                     ORDER BY created_at ASC, terminal_id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![pid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, i64>(8)?,
                        r.get::<_, i64>(9)?,
                        r.get::<_, String>(10)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            drop(stmt);
            let mut parts = vec![pid.to_owned()];
            for (
                tid,
                srv_epoch,
                term_state,
                ctrl_epoch,
                ctrl_id,
                out_off,
                exit_code,
                exit_signal,
                term_rows,
                term_columns,
                updated_at,
            ) in rows
            {
                let eff_state = if !matches!(term_state.as_str(), "exited" | "lost")
                    && &srv_epoch != current_epoch
                {
                    "lost".to_owned()
                } else {
                    term_state
                };
                parts.extend([
                    tid,
                    srv_epoch,
                    eff_state,
                    ctrl_epoch.to_string(),
                    ctrl_id.unwrap_or_default(),
                    out_off.to_string(),
                    exit_code.map(|c| c.to_string()).unwrap_or_default(),
                    exit_signal.unwrap_or_default(),
                    term_rows.to_string(),
                    term_columns.to_string(),
                    updated_at,
                ]);
            }
            Ok(ResourceVersionOut {
                resource: resource.clone(),
                revision: digest_revision("rsrc-terminal", &parts),
                state: "present",
                snapshot_schema_id: Some("resources.terminal.v1"),
            })
        }
        _ => Ok(unavailable(resource)),
    }
}

fn unavailable(resource: &Value) -> ResourceVersionOut {
    ResourceVersionOut {
        resource: resource.clone(),
        revision: "unavailable".to_owned(),
        state: "unavailable",
        snapshot_schema_id: None,
    }
}

fn absent_resource(resource: &Value) -> ResourceVersionOut {
    ResourceVersionOut {
        resource: resource.clone(),
        revision: "absent".to_owned(),
        state: "absent",
        snapshot_schema_id: None,
    }
}

fn project_row_exists(conn: &Connection, project_id: &str) -> Result<bool, ContractError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE project_id = ?1)",
        params![project_id],
        |r| r.get(0),
    )
    .map_err(sql_err)
}

enum ProjectWorkspace {
    Absent,
    Unavailable,
    Dir(PathBuf),
}

fn resolve_project_workspace_dir(
    store: &V1Store,
    conn: &Connection,
    project_id: &str,
) -> Result<ProjectWorkspace, ContractError> {
    let root_json: Option<String> = conn
        .query_row(
            "SELECT root_json FROM projects WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let Some(raw) = root_json else {
        return Ok(ProjectWorkspace::Absent);
    };
    let Ok(root_val) = serde_json::from_str::<Value>(&raw) else {
        return Ok(ProjectWorkspace::Unavailable);
    };
    let Some(root_id) = root_val
        .get("rootId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(ProjectWorkspace::Unavailable);
    };
    let Ok(auth_root) = crate::public_v1::domains::workspace_dirs::resolve_authorized_root_conn(
        conn,
        store.home(),
        root_id,
    ) else {
        return Ok(ProjectWorkspace::Unavailable);
    };
    let Ok(canonical_root) = std::fs::canonicalize(&auth_root) else {
        return Ok(ProjectWorkspace::Unavailable);
    };
    let rel = root_val
        .get("relativePath")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Ok(components) = relative_components(Path::new(rel)) else {
        return Ok(ProjectWorkspace::Unavailable);
    };
    let mut cur = canonical_root.clone();
    for comp in components {
        cur.push(comp);
        let Ok(verified) = std::fs::canonicalize(&cur) else {
            return Ok(ProjectWorkspace::Unavailable);
        };
        if verified.strip_prefix(&canonical_root).is_err() {
            return Ok(ProjectWorkspace::Unavailable);
        }
        cur = verified;
    }
    if !cur.is_dir() {
        return Ok(ProjectWorkspace::Unavailable);
    }
    Ok(ProjectWorkspace::Dir(cur))
}

fn fingerprint_workspace_files(ws_dir: &Path) -> std::io::Result<Vec<String>> {
    use std::io::Read as _;

    const MAX_ENTRIES: usize = 5_000;
    const MAX_FILE_HASH_BYTES: u64 = 64 * 1024;
    const MAX_TOTAL_HASH_BYTES: usize = 4 * 1024 * 1024;

    let mut parts = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(ws_dir.to_path_buf(), String::new())];
    let mut seen_entries = 0usize;
    let mut hashed_bytes = 0usize;

    while let Some((dir, prefix)) = stack.pop() {
        let mut children: Vec<(String, PathBuf)> = Vec::new();
        for entry_res in std::fs::read_dir(&dir)? {
            let entry = entry_res?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            children.push((name, entry.path()));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));

        // Process in ascending lexicographical order by pushing in reverse.
        let mut subdirs = Vec::new();
        for (name, path) in children {
            if seen_entries >= MAX_ENTRIES {
                break;
            }
            seen_entries += 1;
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let meta = std::fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                let target = std::fs::read_link(&path)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                parts.push(format!("l:{rel}:{target}"));
            } else if ft.is_dir() {
                parts.push(format!("d:{rel}"));
                subdirs.push((path, rel));
            } else {
                let mtime_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let len = meta.len();
                let mut content_digest = String::new();
                if len <= MAX_FILE_HASH_BYTES && hashed_bytes < MAX_TOTAL_HASH_BYTES {
                    let mut buf = Vec::new();
                    if let Ok(mut f) = std::fs::File::open(&path)
                        && f.read_to_end(&mut buf).is_ok()
                    {
                        hashed_bytes = hashed_bytes.saturating_add(buf.len());
                        content_digest = format!("{:x}", Sha256::digest(&buf));
                    }
                }
                parts.push(format!("f:{rel}:{len}:{mtime_ns}:{content_digest}"));
            }
        }
        for sub in subdirs.into_iter().rev() {
            stack.push(sub);
        }
        if seen_entries >= MAX_ENTRIES {
            parts.push("truncated:1".to_owned());
            break;
        }
    }
    Ok(parts)
}

fn run_git(ws: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(ws)
        .arg("--no-optional-locks")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

fn git_project_parts(
    conn: &Connection,
    project_id: &str,
    ws_dir: &Path,
) -> Result<Option<Vec<String>>, ContractError> {
    let is_tree = run_git(ws_dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false);
    if !is_tree {
        return Ok(None);
    }
    let head_oid = run_git(ws_dir, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
    let head_ref = run_git(ws_dir, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
    let refs = run_git(ws_dir, &["show-ref"]).unwrap_or_default();
    let status = run_git(ws_dir, &["status", "--porcelain=v1", "-z", "-uall"]).unwrap_or_default();
    let mut parts = vec![project_id.to_owned(), head_oid, head_ref, refs, status];
    if table_exists(conn, "git_operations")? {
        let mut stmt = conn
            .prepare(
                "SELECT operation_id, state, revision, updated_at
                 FROM git_operations WHERE project_id = ?1
                 ORDER BY created_at ASC, operation_id ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![project_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        drop(stmt);
        for (op_id, state, rev, updated) in rows {
            parts.extend([op_id, state, rev, updated]);
        }
    }
    Ok(Some(parts))
}

// ---------------------------------------------------------------------------
// system.resources / system.diagnostics
// ---------------------------------------------------------------------------

async fn system_resources(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let kind = input.get("kind").and_then(Value::as_str).unwrap_or("all");
    if !matches!(kind, "session" | "browser" | "all") {
        return Err(invalid(format!("unknown resource kind `{kind}`")).into());
    }
    if kind == "browser" {
        // The headless gateway wires no browser manager; the operation
        // never touches the user's ordinary browser and says so explicitly
        // instead of returning an empty list (TS `manager_browsers`
        // without a provider raises CAPABILITY_UNAVAILABLE).
        return Err(ContractError::new(
            CODE_CAPABILITY_UNAVAILABLE,
            "no managed browser resources: this gateway has no browser manager",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    let limit = page_limit(input.get("page"))?;
    let cursor = input
        .get("page")
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let store = rt.store().clone();
    let result = run_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let mut stmt = conn
            .prepare(
                "SELECT session_id, active_run_id, revision, session_state, queue_revision
                 FROM sessions WHERE session_state != 'deleted'
                 ORDER BY created_at ASC, session_id ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        drop(stmt);

        let items: Vec<Value> = rows
            .iter()
            .map(|(session_id, active, revision, state, queue_revision)| {
                json!({
                    "resourceId": session_id,
                    "kind": "session",
                    "ownerRunId": active,
                    "revision": digest_revision("sysres", &[revision.clone(), queue_revision.clone()]),
                    "resourceStatus": state,
                })
            })
            .collect();
        let digest = digest_revision(
            "sysres",
            &rows
                .iter()
                .flat_map(|(id, active, revision, state, qrev)| {
                    vec![
                        id.clone(),
                        active.clone().unwrap_or_default(),
                        revision.clone(),
                        state.clone(),
                        qrev.clone(),
                    ]
                })
                .collect::<Vec<_>>(),
        );

        let offset = match cursor.as_deref() {
            None => 0usize,
            Some(raw) => match parse_view_cursor(raw) {
                Some((pinned, off)) => {
                    if pinned != digest {
                        return Err(ContractError::new(
                            CODE_SNAPSHOT_CHANGED,
                            "the managed resource listing changed while paging; restart the query",
                            RetrySpec::after_reconcile(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    off
                }
                None => {
                    return Err(ContractError::new(
                        CODE_CURSOR_EXPIRED,
                        format!("invalid pagination cursor `{raw}`"),
                        RetrySpec::after_reconcile(),
                        EffectStatus::NotApplied,
                    ));
                }
            },
        };
        if offset > items.len() {
            return Err(ContractError::new(
                CODE_CURSOR_EXPIRED,
                "pagination cursor beyond the end of the listing",
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let end = (offset + limit).min(items.len());
        let complete = end >= items.len();
        Ok(json!({
            "items": items[offset..end],
            "pageInfo": {
                "snapshotRevision": digest,
                "nextCursor": if complete { Value::Null } else { json!(view_cursor(&digest, end)) },
                "complete": complete,
            }
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

/// One redacted diagnostic (contract `$defs.Diagnostic`).
struct DiagnosticItem {
    code: String,
    severity: &'static str,
    message: String,
    resource_id: Option<String>,
    /// RFC3339 timestamp of the underlying state change; `None` marks
    /// current-condition diagnostics that carry no point-in-time origin.
    occurred_at: Option<String>,
}

async fn system_diagnostics(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let since = input.get("since").and_then(Value::as_str);
    let since_ts = match since {
        Some(raw) => Some(
            chrono::DateTime::parse_from_rfc3339(raw)
                .map_err(|e| invalid(format!("`since` is not a valid date-time: {e}")))?
                .to_rfc3339(),
        ),
        None => None,
    };
    let limit = page_limit(input.get("page"))?;
    let cursor = input
        .get("page")
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let store = rt.store().clone();
    let result = run_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");

        // Crash-recovered runs: interrupted (warning) / result_unknown (error).
        let mut stmt = conn
            .prepare(
                "SELECT run_id, state, updated_at FROM runs
                 WHERE state IN ('interrupted', 'result_unknown')
                 ORDER BY updated_at ASC, run_id ASC",
            )
            .map_err(sql_err)?;
        let run_rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        drop(stmt);

        // Paused session queues (info).
        let mut stmt = conn
            .prepare(
                "SELECT session_id, pause_reason FROM sessions
                 WHERE queue_paused = 1 AND session_state != 'deleted'
                 ORDER BY session_id ASC",
            )
            .map_err(sql_err)?;
        let paused_rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        drop(stmt);

        // Store-reconciliation diagnostics (legacy registry warnings).
        let stored = store.diagnostics();

        let mut items: Vec<DiagnosticItem> = Vec::new();
        for (run_id, state, updated_at) in run_rows {
            items.push(DiagnosticItem {
                code: if state == "result_unknown" {
                    "RUN_RESULT_UNKNOWN".to_owned()
                } else {
                    "RUN_INTERRUPTED".to_owned()
                },
                severity: if state == "result_unknown" {
                    "error"
                } else {
                    "warning"
                },
                // No run input / result bodies ever enter diagnostics — ids and
                // state names only (the "错误token与正文不出现在诊断" anchor).
                message: format!("run `{run_id}` is `{state}` after gateway recovery"),
                resource_id: Some(run_id),
                occurred_at: Some(updated_at),
            });
        }
        for (session_id, pause_reason) in paused_rows {
            items.push(DiagnosticItem {
                code: "SESSION_QUEUE_PAUSED".to_owned(),
                severity: "info",
                message: format!(
                    "session `{session_id}` queue is paused (`{}`)",
                    pause_reason.as_deref().unwrap_or("unspecified")
                ),
                resource_id: Some(session_id),
                occurred_at: None,
            });
        }
        for diag in stored {
            let severity = match diag.severity.as_str() {
                "info" => "info",
                "error" => "error",
                _ => "warning",
            };
            items.push(DiagnosticItem {
                code: diag.code,
                severity,
                message: diag.message,
                resource_id: diag.resource_id,
                occurred_at: None,
            });
        }
        // Deterministic order before the `since` filter and pagination.
        items.sort_by(|a, b| (&a.code, &a.resource_id).cmp(&(&b.code, &b.resource_id)));
        if let Some(since_ts) = since_ts {
            items.retain(|item| {
                // Point-in-time diagnostics filter by their state-change
                // timestamp; current-condition diagnostics (no per-item origin)
                // always report — an ongoing condition is true after any
                // `since` that has already passed.
                item.occurred_at
                    .as_deref()
                    .is_none_or(|at| at >= since_ts.as_str())
            });
        }

        let digest = digest_revision(
            "sysdiag",
            &items
                .iter()
                .map(|item| {
                    format!(
                        "{}|{}|{}|{}",
                        item.code,
                        item.severity,
                        item.resource_id.clone().unwrap_or_default(),
                        item.message
                    )
                })
                .collect::<Vec<_>>(),
        );
        let offset = match cursor.as_deref() {
            None => 0usize,
            Some(raw) => match parse_view_cursor(raw) {
                Some((pinned, off)) => {
                    if pinned != digest {
                        return Err(ContractError::new(
                            CODE_SNAPSHOT_CHANGED,
                            "the diagnostics listing changed while paging; restart the query",
                            RetrySpec::after_reconcile(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    off
                }
                None => {
                    return Err(ContractError::new(
                        CODE_CURSOR_EXPIRED,
                        format!("invalid pagination cursor `{raw}`"),
                        RetrySpec::after_reconcile(),
                        EffectStatus::NotApplied,
                    ));
                }
            },
        };
        if offset > items.len() {
            return Err(ContractError::new(
                CODE_CURSOR_EXPIRED,
                "pagination cursor beyond the end of the listing",
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let end = (offset + limit).min(items.len());
        let complete = end >= items.len();
        let page_items: Vec<Value> = items[offset..end]
            .iter()
            .map(|item| {
                json!({
                    "code": item.code,
                    "severity": item.severity,
                    "message": item.message,
                    "resourceId": item.resource_id,
                })
            })
            .collect();
        Ok(json!({
            "items": page_items,
            "pageInfo": {
                "snapshotRevision": digest,
                "nextCursor": if complete { Value::Null } else { json!(view_cursor(&digest, end)) },
                "complete": complete,
            }
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;

    fn open_runtime(root_dir: &std::path::Path) -> (Arc<PublicV1Runtime>, String) {
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
        let store = crate::public_v1::store::V1Store::open_and_recover(root_dir, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    /// A fresh runtime plus the temp home keeping it alive (the returned
    /// `TempDir` must stay bound for the store's files to survive).
    fn fresh_runtime(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, String) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        (tmp, rt, iid)
    }

    #[tokio::test]
    async fn long_operation_lifecycle_is_queryable_cancellable_and_snapshottable() {
        let (_home, rt, iid) = fresh_runtime("op-lifecycle");
        // A driven maintenance operation: queued -> running -> progress ->
        // cancel requested -> cancelled, each step observable through the
        // contract surface.
        let store = rt.store();
        let (op_id, _rev) = store
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();
        store.mark_operation_running(&op_id).unwrap();
        store
            .record_operation_progress(&op_id, "steps", &json!(1))
            .unwrap();

        let (_, got) = rt
            .dispatch(
                "operations.get",
                json!({"instanceId": iid, "operationId": op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["operation"]["operationId"], op_id.as_str());
        assert_eq!(got["operation"]["operationType"], "instance.maintenance");
        assert_eq!(got["operation"]["state"], "running");
        assert_eq!(got["operation"]["cancelRequested"], false);
        assert_eq!(
            got["operation"]["progress"][0],
            json!({"key": "steps", "value": 1})
        );
        OperationRegistry::global()
            .validate_result("operations.get", &got)
            .unwrap();

        // Events: queued / started / progress, in cursor order.
        let (_, page) = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let events = page["frame"]["events"].as_array().unwrap().clone();
        let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "operation.queued",
                "operation.started",
                "operation.progress"
            ]
        );
        assert_eq!(page["frame"]["terminal"], false);
        OperationRegistry::global()
            .validate_result("operations.events", &page)
            .unwrap();

        // Cancel: requested (not cancelled), repeatable as one intent.
        let (_, receipt) = rt
            .dispatch(
                "operations.cancel",
                json!({"instanceId": iid, "operationId": op_id}),
                Some("key-cancel-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["receipt"]["disposition"], "requested");
        assert_eq!(receipt["receipt"]["state"], "cancelling");
        assert_ne!(receipt["receipt"]["revision"], "");
        OperationRegistry::global()
            .validate_result("operations.cancel", &receipt)
            .unwrap();
        assert!(store.operation_cancel_requested(&op_id).unwrap());

        // Same key replays the stored receipt.
        let (_, replay) = rt
            .dispatch(
                "operations.cancel",
                json!({"instanceId": iid, "operationId": op_id}),
                Some("key-cancel-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["receipt"], receipt["receipt"]);

        // The driver observes the intent and terminalizes.
        store
            .finish_long_operation(&op_id, LongOperationOutcome::Cancelled)
            .unwrap();

        let (_, terminal) = rt
            .dispatch(
                "operations.get",
                json!({"instanceId": iid, "operationId": op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(terminal["operation"]["state"], "cancelled");

        let (_, cancel_again) = rt
            .dispatch(
                "operations.cancel",
                json!({"instanceId": iid, "operationId": op_id}),
                Some("key-cancel-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancel_again["receipt"]["disposition"], "alreadyTerminal");
        assert_eq!(cancel_again["receipt"]["state"], "cancelled");

        // Snapshot: checkpoint + exactly one artifact-referenced state section.
        let (_, snap) = rt
            .dispatch(
                "operations.snapshot",
                json!({"instanceId": iid, "operationId": op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(snap["operation"]["state"], "cancelled");
        assert_eq!(snap["snapshot"]["checkpoint"]["resourceKind"], "operation");
        assert_eq!(snap["snapshot"]["checkpoint"]["resourceId"], op_id.as_str());
        let sections = snap["snapshot"]["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0]["name"], "state");
        assert!(
            sections[0]["artifactId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(
            sections[0]["sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64)
        );
        OperationRegistry::global()
            .validate_result("operations.snapshot", &snap)
            .unwrap();

        // Events after terminal: terminal=true only once caught up.
        let (_, final_page) = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let last_type = final_page["frame"]["events"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["type"]
            .clone();
        assert_eq!(last_type, "operation.cancelled");
        assert_eq!(final_page["frame"]["terminal"], true);
    }

    #[tokio::test]
    async fn operations_list_pages_are_digest_pinned_and_report_drift() {
        let (_home, rt, iid) = fresh_runtime("op-list");
        let store = rt.store();
        for i in 0..5 {
            let (op_id, _) = store
                .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
                .unwrap();
            store
                .finish_long_operation(
                    &op_id,
                    LongOperationOutcome::Succeeded {
                        result: json!({"index": i}),
                        result_schema_id: None,
                    },
                )
                .unwrap();
        }

        let scope = json!({"kind": "instance"});
        let (_, page1) = rt
            .dispatch(
                "operations.list",
                json!({"instanceId": iid, "scope": scope, "page": {"limit": 2}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page1["items"].as_array().unwrap().len(), 2);
        assert_eq!(page1["pageInfo"]["complete"], false);
        let next = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        OperationRegistry::global()
            .validate_result("operations.list", &page1)
            .unwrap();

        // Drift between pages: a new operation invalidates the pinned view.
        let (extra, _) = store
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();
        let drifted = rt
            .dispatch(
                "operations.list",
                json!({"instanceId": iid, "scope": scope, "page": {"limit": 2, "cursor": next}}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match drifted {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_SNAPSHOT_CHANGED),
            other => panic!("expected SNAPSHOT_CHANGED, got {other:?}"),
        }

        // Restarting the query observes the new operation (no silent loss).
        store
            .finish_long_operation(
                &extra,
                LongOperationOutcome::Succeeded {
                    result: json!({"index": 5}),
                    result_schema_id: None,
                },
            )
            .unwrap();
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..8 {
            let mut input = json!({"instanceId": iid, "scope": scope, "page": {"limit": 2}});
            if let Some(cursor) = &cursor {
                input["page"]["cursor"] = json!(cursor);
            }
            let (_, page) = rt
                .dispatch("operations.list", input, None, None)
                .await
                .unwrap();
            for item in page["items"].as_array().unwrap() {
                seen.push(item["operationId"].as_str().unwrap().to_owned());
            }
            if page["pageInfo"]["complete"] == true {
                break;
            }
            cursor = page["pageInfo"]["nextCursor"]
                .as_str()
                .map(ToOwned::to_owned);
        }
        assert_eq!(seen.len(), 6, "all six operations survive pagination");
        assert!(seen.contains(&extra));

        // Filter coverage: by state and operationType.
        let (_, queued_only) = rt
            .dispatch(
                "operations.list",
                json!({"instanceId": iid, "scope": scope, "state": "queued"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(queued_only["items"].as_array().unwrap().len(), 0);
        let (_, typed) = rt
            .dispatch(
                "operations.list",
                json!({"instanceId": iid, "scope": scope, "operationType": "instance.maintenance"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(typed["items"].as_array().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn operations_cancel_rejects_unknown_and_wrong_shapes() {
        let (_home, rt, iid) = fresh_runtime("op-cancel-neg");
        // runId never substitutes for operationId.
        let store = rt.store();
        store
            .register_project(&json!({
                "instanceId": iid,
                "projectId": "proj-c",
                "name": "Cancel",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"}
            }))
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-c", "projectId": "proj-c"}),
            )
            .unwrap();
        let run_id = match store
            .submit_run(
                &json!({"instanceId": iid, "sessionId": "sess-c", "input": [{"kind": "text", "text": "hi"}]}),
                Some("key-cancel-run-1"),
            )
            .unwrap()
        {
            crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("expected a new run, got {other:?}"),
        };
        let err = rt
            .dispatch(
                "operations.cancel",
                json!({"instanceId": iid, "operationId": run_id}),
                Some("key-cancel-neg-1"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_NOT_FOUND),
            other => panic!("expected NOT_FOUND for a runId, got {other:?}"),
        }

        let err = rt
            .dispatch(
                "operations.get",
                json!({"instanceId": iid, "operationId": "op-missing"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_NOT_FOUND),
            other => panic!("expected NOT_FOUND, got {other:?}"),
        }

        // Same key, different operationId: IDEMPOTENCY_CONFLICT.
        let (op1, _) = store
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();
        let (op2, _) = store
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();
        rt.dispatch(
            "operations.cancel",
            json!({"instanceId": iid, "operationId": op1}),
            Some("key-cancel-dup-1"),
            None,
        )
        .await
        .unwrap();
        let err = rt
            .dispatch(
                "operations.cancel",
                json!({"instanceId": iid, "operationId": op2}),
                Some("key-cancel-dup-1"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_IDEMPOTENCY_CONFLICT),
            other => panic!("expected IDEMPOTENCY_CONFLICT, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn operations_events_reports_expired_cursors_as_gaps() {
        let (_home, rt, iid) = fresh_runtime("op-events-gap");
        let store = rt.store();
        let (op_id, _) = store
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();
        store.mark_operation_running(&op_id).unwrap();
        store
            .finish_long_operation(
                &op_id,
                LongOperationOutcome::Succeeded {
                    result: json!({"done": true}),
                    result_schema_id: None,
                },
            )
            .unwrap();

        // An unparseable cursor is an explicit gap frame with the
        // restart-query recovery, never a silently empty page.
        let (_, gap) = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": op_id, "afterCursor": "not-a-cursor"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(gap["frame"]["kind"], "gap");
        assert_eq!(gap["frame"]["reason"], "cursor_invalid");
        assert_eq!(gap["frame"]["recoveryMethod"], "operations.snapshot");
        assert_eq!(gap["frame"]["recovery"], "restart_query");
        OperationRegistry::global()
            .validate_result("operations.events", &gap)
            .unwrap();

        // A cursor older than the earliest retained event is expired.
        store.set_earliest_cursor_seq_for_test(1_000_000).unwrap();
        let (_, expired) = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": op_id, "afterCursor": "cur-000000000001"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(expired["frame"]["kind"], "gap");
        assert_eq!(expired["frame"]["reason"], "cursor_expired");
        assert_eq!(expired["frame"]["recovery"], "fetch_snapshot_then_resume");

        // Unknown operation: NOT_FOUND, not an empty page.
        let err = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": "op-none"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_NOT_FOUND),
            other => panic!("expected NOT_FOUND, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resources_snapshot_reports_revisions_unchanged_and_unavailable() {
        let (_home, rt, iid) = fresh_runtime("rsrc-snap");
        let store = rt.store();
        store
            .register_project(&json!({
                "instanceId": iid,
                "projectId": "proj-r",
                "name": "Res",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"}
            }))
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-r", "projectId": "proj-r"}),
            )
            .unwrap();

        // resources.snapshot 的合同 input 只有 resources/knownVersions
        // （additionalProperties: false，无 instanceId）——实例身份来自会话
        // 绑定，结果里的 instanceId 由服务端补充。
        let input = json!({
            "resources": [
                {"kind": "projects"},
                {"kind": "project", "projectId": "proj-r"},
                {"kind": "session", "resourceId": "sess-r"},
                {"kind": "terminal"},
            ],
        });
        let (_, snap) = rt
            .dispatch("resources.snapshot", input.clone(), None, None)
            .await
            .unwrap();
        assert_eq!(snap["instanceId"], iid);
        let snapshots = snap["snapshots"].as_array().unwrap();
        assert_eq!(snapshots.len(), 4);
        assert_eq!(snapshots[0]["state"], "present");
        assert_eq!(snapshots[0]["snapshotSchemaId"], "resources.projects.v1");
        assert_eq!(snapshots[1]["state"], "present");
        assert_eq!(snapshots[1]["snapshotSchemaId"], "resources.project.v1");
        assert_eq!(snapshots[2]["state"], "present");
        // kinds with no revision source this build report "unavailable",
        // never a fake "present".
        assert_eq!(snapshots[3]["state"], "unavailable");
        OperationRegistry::global()
            .validate_result("resources.snapshot", &snap)
            .unwrap();

        // knownVersions matching current revisions -> unchanged, not re-sent.
        let current = rt
            .dispatch("resources.snapshot", input.clone(), None, None)
            .await
            .unwrap()
            .1;
        let known: Vec<Value> = current["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                json!({
                    "resource": entry["resource"],
                    "revision": entry["revision"],
                })
            })
            .collect();
        let (_, again) = rt
            .dispatch(
                "resources.snapshot",
                json!({"resources": input["resources"].clone(), "knownVersions": known}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            again["unchanged"].as_array().unwrap().len(),
            input["resources"].as_array().unwrap().len(),
            "every known resource must fold into unchanged"
        );
        assert_eq!(again["snapshots"].as_array().unwrap().len(), 0);

        // A changed session (active run) bumps the session revision.
        let sess_before: Value = rt
            .dispatch("resources.snapshot", input.clone(), None, None)
            .await
            .unwrap()
            .1["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["resource"]["kind"] == "session")
            .cloned()
            .unwrap();
        store
            .submit_run(
                &json!({"instanceId": iid, "sessionId": "sess-r", "input": [{"kind": "text", "text": "x"}]}),
                Some("key-rsrc-1"),
            )
            .unwrap();
        let sess_after: Value = rt
            .dispatch("resources.snapshot", input.clone(), None, None)
            .await
            .unwrap()
            .1["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["resource"]["kind"] == "session")
            .cloned()
            .unwrap();
        assert_ne!(
            sess_before["revision"], sess_after["revision"],
            "session activity (activeRunId) must bump the revision"
        );

        // Absent project.
        let (_, absent) = rt
            .dispatch(
                "resources.snapshot",
                json!({"resources": [{"kind": "project", "projectId": "proj-gone"}]}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(absent["snapshots"][0]["state"], "absent");

        // Over the resource cap: RESOURCE_LIMIT.
        let too_many: Vec<Value> = (0..65)
            .map(|i| json!({"kind": "project", "projectId": format!("p-{i}")}))
            .collect();
        let err = rt
            .dispatch(
                "resources.snapshot",
                json!({"resources": too_many}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_RESOURCE_LIMIT),
            other => panic!("expected RESOURCE_LIMIT, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn system_resources_lists_sessions_and_refuses_browsers() {
        let (_home, rt, iid) = fresh_runtime("sys-res");
        let store = rt.store();
        store
            .register_project(&json!({
                "instanceId": iid,
                "projectId": "proj-s",
                "name": "Sys",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"}
            }))
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-s1", "projectId": "proj-s"}),
            )
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-s2", "projectId": "proj-s"}),
            )
            .unwrap();

        let (_, all) = rt
            .dispatch(
                "system.resources",
                json!({"instanceId": iid, "kind": "all"}),
                None,
                None,
            )
            .await
            .unwrap();
        let items = all["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["kind"], "session");
        assert!(items.iter().all(|item| {
            item["resourceStatus"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        }));
        OperationRegistry::global()
            .validate_result("system.resources", &all)
            .unwrap();

        // kind=browser without a manager: explicit CAPABILITY_UNAVAILABLE.
        let err = rt
            .dispatch(
                "system.resources",
                json!({"instanceId": iid, "kind": "browser"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_CAPABILITY_UNAVAILABLE),
            other => panic!("expected CAPABILITY_UNAVAILABLE, got {other:?}"),
        }

        // Cross-page drift is not silently completed.
        let (_, page1) = rt
            .dispatch(
                "system.resources",
                json!({"instanceId": iid, "kind": "session", "page": {"limit": 1}}),
                None,
                None,
            )
            .await
            .unwrap();
        let next = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-s3", "projectId": "proj-s"}),
            )
            .unwrap();
        let drifted = rt
            .dispatch(
                "system.resources",
                json!({"instanceId": iid, "kind": "session", "page": {"limit": 1, "cursor": next}}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match drifted {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_SNAPSHOT_CHANGED),
            other => panic!("expected SNAPSHOT_CHANGED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn system_diagnostics_reports_recovery_and_pause_without_bodies() {
        let (_home, rt, iid) = fresh_runtime("sys-diag");
        let store = rt.store();
        store
            .register_project(&json!({
                "instanceId": iid,
                "projectId": "proj-d",
                "name": "Diag",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"}
            }))
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-d", "projectId": "proj-d"}),
            )
            .unwrap();

        // A failing run pauses the session queue -> both diagnostics appear.
        // Submit through dispatch so the runtime's worker (the offline
        // driver) actually drives the run to its terminal state.
        let (_, submitted) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-d", "input": [{"kind": "text", "text": "[pd-driver:fail]"}]}),
                Some("key-diag-1"),
                None,
            )
            .await
            .unwrap();
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        // run.wait polls the store's state notifier asynchronously (the
        // current-thread test runtime yields to the driver's task here).
        let (_, waited) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 20000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            waited["conditionMet"], true,
            "run never terminated: {waited}"
        );

        let (_, diags) = rt
            .dispatch("system.diagnostics", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        let items = diags["items"].as_array().unwrap().clone();
        let codes: Vec<&str> = items.iter().map(|d| d["code"].as_str().unwrap()).collect();
        assert!(codes.contains(&"SESSION_QUEUE_PAUSED"), "codes: {codes:?}");
        for item in &items {
            // No run input/error bodies, no tokens: ids and state names only.
            let message = item["message"].as_str().unwrap();
            assert!(
                !message.contains("[pd-driver"),
                "diagnostic leaked a body: {message}"
            );
            assert!(
                item["severity"]
                    .as_str()
                    .is_some_and(|s| ["info", "warning", "error"].contains(&s))
            );
        }
        OperationRegistry::global()
            .validate_result("system.diagnostics", &diags)
            .unwrap();

        // `since` filters point-in-time items (none predate the epoch here
        // after the far-future filter) and validates its input.
        let err = rt
            .dispatch(
                "system.diagnostics",
                json!({"instanceId": iid, "since": "yesterday"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_INVALID_ARGUMENT),
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }
        let (_, filtered) = rt
            .dispatch(
                "system.diagnostics",
                json!({"instanceId": iid, "since": "2000-01-01T00:00:00+00:00"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            filtered["items"].as_array().unwrap().len(),
            items.len(),
            "an old `since` keeps every diagnostic"
        );
    }

    #[tokio::test]
    async fn results_validate_against_the_embedded_contract_schemas() {
        // Every handler's result shape is asserted against the embedded
        // registry schemas in the scenario tests above; this one pins the
        // two remaining shapes (list pages and a foreign-projection get)
        // once more through validate_result to satisfy the briefing's
        // "结果过 schema" requirement end to end.
        let (_home, rt, iid) = fresh_runtime("op-schema");
        let store = rt.store();
        let (op_id, _) = store
            .begin_long_operation(
                "instance.maintenance",
                &json!({"kind": "project", "projectId": "proj-x"}),
                false,
            )
            .unwrap();
        let (_, listed) = rt
            .dispatch(
                "operations.list",
                json!({"instanceId": iid, "scope": {"kind": "project", "projectId": "proj-x"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        OperationRegistry::global()
            .validate_result("operations.list", &listed)
            .unwrap();

        // Non-cancellable seam operation: CANCELLATION_UNSUPPORTED.
        let err = rt
            .dispatch(
                "operations.cancel",
                json!({"instanceId": iid, "operationId": op_id}),
                Some("key-nc-1"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_CANCELLATION_UNSUPPORTED),
            other => panic!("expected CANCELLATION_UNSUPPORTED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn operations_events_stream_source_and_follow_rejection() {
        let (_home, rt, iid) = fresh_runtime("op-stream");
        let store = rt.store();
        let (op_id, _) = store
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();
        store.mark_operation_running(&op_id).unwrap();

        // Unary dispatch with follow=true must reject with INVALID_ARGUMENT.
        let err = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": op_id, "follow": true}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, CODE_INVALID_ARGUMENT),
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }

        // StreamSource (`sub.open` backend) streams initial frames cleanly.
        let state = Arc::new(PublicV1State::new(rt.clone(), "tok-op-stream".to_owned()));
        let mut src = operations_events_source(&state, &json!({"operationId": op_id}), "sub-op-1")
            .expect("operations_events_source open");
        let batch1 = src.fetch_next(16).expect("first stream fetch");
        assert!(!batch1.stream_ended);
        assert_eq!(batch1.frames.len(), 2);
        let frame0: Value = serde_json::from_str(&batch1.frames[0].text).unwrap();
        assert_eq!(frame0["params"]["event"]["operationId"], op_id);
        assert_eq!(frame0["params"]["event"]["eventType"], "operation.queued");

        // Completing the operation delivers the final frame + stream_ended=true.
        store
            .finish_long_operation(
                &op_id,
                LongOperationOutcome::Succeeded {
                    result: json!({"ok": true}),
                    result_schema_id: None,
                },
            )
            .unwrap();
        let batch2 = src.fetch_next(16).expect("second stream fetch");
        assert!(batch2.stream_ended);
        assert_eq!(batch2.frames.len(), 1);
        let frame_end: Value = serde_json::from_str(&batch2.frames[0].text).unwrap();
        assert_eq!(
            frame_end["params"]["event"]["eventType"],
            "operation.completed"
        );

        // Failed v1_domain_operations row projects `error` accurately.
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO v1_domain_operations
                    (operation_id, method_id, idempotency_key, input_digest, state, receipt_json, completion_json, rejection_code, created_at, updated_at)
                 VALUES ('op-dom-fail', 'background_tasks.stop', NULL, 'd', 'failed', '{}', '{\"code\":\"UNAVAILABLE\",\"message\":\"stop failed\"}', NULL, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }
        let (_, dom_got) = rt
            .dispatch(
                "operations.get",
                json!({"instanceId": iid, "operationId": "op-dom-fail"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(dom_got["operation"]["state"], "failed");
        assert_eq!(dom_got["operation"]["error"]["code"], "UNAVAILABLE");
        assert_eq!(dom_got["operation"]["error"]["message"], "stop failed");
    }

    #[tokio::test]
    async fn operations_snapshot_publishes_real_artifact_and_safe_checkpoint_cursor() {
        use base64::Engine as _;

        let (_home, rt, iid) = fresh_runtime("op-snap-art");
        let store = rt.store();
        let (op_id, _) = store
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();

        let (_, snap) = rt
            .dispatch(
                "operations.snapshot",
                json!({"instanceId": iid, "operationId": op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("operations.snapshot", &snap)
            .unwrap();
        let artifact_id = snap["snapshot"]["sections"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        let sha = snap["snapshot"]["sections"][0]["sha256"]
            .as_str()
            .unwrap()
            .to_owned();

        let (_, art_meta) = rt
            .dispatch(
                "artifacts.get",
                json!({"artifactId": artifact_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(art_meta["artifact"]["sha256"], sha);

        let (_, art_read) = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": artifact_id, "offset": 0, "maxBytes": 65536}),
                None,
                None,
            )
            .await
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(art_read["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["operationId"], op_id);

        // Repeat snapshot of identical state reuses the same artifactId.
        let (_, snap2) = rt
            .dispatch(
                "operations.snapshot",
                json!({"instanceId": iid, "operationId": op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(snap2["snapshot"]["sections"][0]["artifactId"], artifact_id);

        // When retention advances (`earliest_cursor_seq > 1`) and an operation has
        // zero events yet (e.g. a domain op), snapshot checkpoint cursor does NOT
        // trap `operations.events(afterCursor: ...)` in a `cursor_expired` loop.
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO v1_domain_operations
                    (operation_id, method_id, idempotency_key, input_digest, state, receipt_json, completion_json, rejection_code, created_at, updated_at)
                 VALUES ('op-no-ev', 'background_tasks.stop', NULL, 'd2', 'queued', '{}', NULL, NULL, '2026-01-01T00:00:01Z', '2026-01-01T00:00:01Z')",
                [],
            )
            .unwrap();
        }
        store.set_earliest_cursor_seq_for_test(50).unwrap();
        let (_, snap_gap) = rt
            .dispatch(
                "operations.snapshot",
                json!({"instanceId": iid, "operationId": "op-no-ev"}),
                None,
                None,
            )
            .await
            .unwrap();
        let ck_cursor = snap_gap["snapshot"]["checkpoint"]["cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, ev_page) = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": "op-no-ev", "afterCursor": ck_cursor}),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(
            ev_page["frame"].get("events").is_some(),
            "checkpoint cursor must resume cleanly without cursor_expired loop: {ev_page}"
        );
    }

    #[tokio::test]
    async fn resources_snapshot_resolves_uploads_files_git_memory_and_targeted_terminal() {
        let (tmp, rt, iid) = fresh_runtime("rsrc-kinds-13");
        let root_dir = tmp.0.join("pd-home");
        let store = rt.store();

        // Create a real workspace-backed project (`rootId: "default"`).
        let (_, created_proj) = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "proj-real",
                    "parent": {"instanceId": iid, "rootId": "default", "relativePath": ""},
                    "directoryName": "repo-ws",
                    "name": "Real",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let _proj_rev = created_proj["project"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let ws = root_dir.join("workspaces").join("repo-ws");
        std::fs::write(ws.join("hello.txt"), b"v1").unwrap();

        // Initialize a git repository inside ws and create an initial commit.
        let run_git_cmd = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&ws)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Tester")
                .env("GIT_AUTHOR_EMAIL", "tester@example.com")
                .env("GIT_COMMITTER_NAME", "Tester")
                .env("GIT_COMMITTER_EMAIL", "tester@example.com")
                .status()
                .expect("git command");
            assert!(status.success(), "git {args:?} failed");
        };
        run_git_cmd(&["init"]);
        run_git_cmd(&["add", "hello.txt"]);
        run_git_cmd(&["commit", "-m", "init"]);

        // Seed uploads, memory, and terminals rows.
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO uploads (upload_id, project_id, state, byte_length, part_size, total_sha256, media_type, display_name, artifact_id, revision, expires_at, created_at, updated_at)
                 VALUES ('up-1', 'proj-real', 'open', 10, 10, 'abc', 'text/plain', 'a.txt', NULL, 'rev-up-1', '2099-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO v1_memory_entries (memory_id, scope_kind, project_id, session_id, kind, name, description, fields_json, revision, deprecated, read_only, content_artifact_id, created_at, updated_at)
                 VALUES ('mem-1', 'project', 'proj-real', NULL, 'note', 'k1', 'desc', '{}', 'mrev-1', 0, 0, NULL, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO terminals (terminal_id, project_id, working_directory, shell_profile_id, server_epoch, state, control_epoch, controller_id, lease_token_hash, lease_expires_ms, last_accepted_input_seq, last_input_digest, output_epoch, earliest_output_offset, latest_output_offset, exit_code, exit_signal, term_rows, term_columns, created_at, updated_at)
                 VALUES ('term-1', 'proj-real', NULL, NULL, 'gen-test-1', 'running', 1, 'ctrl-1', NULL, NULL, 0, NULL, 'oe-1', 0, 10, NULL, NULL, 24, 80, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }

        let input = json!({
            "resources": [
                {"kind": "uploads"},
                {"kind": "files", "projectId": "proj-real"},
                {"kind": "git", "projectId": "proj-real"},
                {"kind": "memory", "projectId": "proj-real"},
                {"kind": "terminal", "resourceId": "term-1"},
                {"kind": "terminal"},
            ]
        });
        let (_, snap1) = rt
            .dispatch("resources.snapshot", input.clone(), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("resources.snapshot", &snap1)
            .unwrap();
        let s1 = snap1["snapshots"].as_array().unwrap();
        assert_eq!(s1[0]["state"], "present");
        assert_eq!(s1[1]["state"], "present");
        assert_eq!(s1[1]["snapshotSchemaId"], "resources.files.v1");
        assert_eq!(s1[2]["state"], "present");
        assert_eq!(s1[2]["snapshotSchemaId"], "resources.git.v1");
        assert_eq!(s1[3]["state"], "present");
        assert_eq!(s1[3]["snapshotSchemaId"], "resources.memory.v1");
        assert_eq!(s1[4]["state"], "present");
        assert_eq!(s1[4]["snapshotSchemaId"], "resources.terminal.v1");
        assert_eq!(s1[5]["state"], "unavailable");

        // Mutate workspace file, git working tree, memory row, upload row, and
        // terminal row: every corresponding revision must move!
        let rev_uploads_1 = s1[0]["revision"].as_str().unwrap().to_owned();
        let rev_files_1 = s1[1]["revision"].as_str().unwrap().to_owned();
        let rev_git_1 = s1[2]["revision"].as_str().unwrap().to_owned();
        let rev_mem_1 = s1[3]["revision"].as_str().unwrap().to_owned();
        let rev_term_1 = s1[4]["revision"].as_str().unwrap().to_owned();

        // Check omitted `projectId` uploads (`up-1` actually included vs empty).
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO uploads (upload_id, project_id, state, byte_length, part_size, total_sha256, media_type, display_name, artifact_id, revision, expires_at, created_at, updated_at)
                 VALUES ('up-2', 'proj-real', 'open', 5, 5, 'def', 'text/plain', 'b.txt', NULL, 'rev-up-2', '2099-01-01T00:00:00Z', '2026-01-01T00:00:01Z', '2026-01-01T00:00:01Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE v1_memory_entries SET revision = 'mrev-2', updated_at = '2026-01-01T00:00:02Z' WHERE memory_id = 'mem-1'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE terminals SET latest_output_offset = 42, updated_at = '2026-01-01T00:00:03Z' WHERE terminal_id = 'term-1'",
                [],
            )
            .unwrap();
        }
        std::fs::write(ws.join("hello.txt"), b"v2-modified").unwrap();

        let (_, snap2) = rt
            .dispatch("resources.snapshot", input, None, None)
            .await
            .unwrap();
        let s2 = snap2["snapshots"].as_array().unwrap();
        assert_ne!(s2[0]["revision"], rev_uploads_1);
        assert_ne!(s2[1]["revision"], rev_files_1);
        assert_ne!(s2[2]["revision"], rev_git_1);
        assert_ne!(s2[3]["revision"], rev_mem_1);
        assert_ne!(s2[4]["revision"], rev_term_1);
    }
}
