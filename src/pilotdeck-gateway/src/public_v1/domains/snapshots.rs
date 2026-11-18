//! `snapshots.*` — 文件副作用快照与恢复（t018，S3）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `snapshots.*` 条目。TS 基
//! 线（cc7c4e6 `createLocalGateway.ts:1271-1314`）只有单段式的
//! `snapshotList/Get/Restore`，且 restore 恢复到 `.pilotdeck-restore-*` 临
//! 时目录、不改工作区；合同把 restore 升级为两段式
//! （`plan_restore` 的 before/after 方向 + ChangePlan）且 events 指明
//! `files.changed` —— 因此本实现**落回项目工作区**（合同验收与事件语
//! 义的直接推论，登记为设计决定）。
//!
//! ## 快照数据模型（本域定义，供捕获接缝与 restore 共用）
//!
//! 一条快照 = 一个 run/turn 边界上聚合的文件副作用集合：entry
//! `{path, beforeRevision?, afterRevision?, action: create|replace|delete,
//! artifactId?}`，`artifactId` 是**动作完成后**的路径内容产物
//! （create/replace；delete 无内容）。方向语义：
//!
//! - `after`：目标态 = 按时间序最后一个触碰该路径的选中条目的 after 内
//!   容（delete 条目 → 目标态缺失）。
//! - `before`：目标态 = 最早选中快照触碰该路径**之前**的状态——create →
//!   缺失；replace/delete → 该项目更早快照中最近的内容产物，找不到 →
//!   冲突 SNAPSHOT_UNAVAILABLE（快照内容被清理时不伪装可恢复）。
//!
//! ## 快照捕获缺口（集成者接线点）
//!
//! v1 当前无人写 `v1_file_snapshots`：合同没有 capture 操作，t013 的
//! `files.rs` 是已入库文件（本域不可改）。本域提供捕获 seam
//! [`commit_turn_snapshot`]：在 files.write/move/delete 之上的 run/turn
//! 边界聚合调用（由 run driver 或 files 域接线，方案见任务报告 §6）。
//! 接线落地前，本域对已有快照数据提供完整读面与恢复面。
//!
//! ## 协议
//!
//! - `plan_restore`（sync 只读）：派生目标态 + 当前态差异 → ChangePlan；
//!   内容产物缺失 → `plan.conflicts`（不展示为可恢复成功）。
//! - `restore`（async idempotentSubmit）：受理时复验（PLAN_EXPIRED /
//!   REVISION_CONFLICT：过期、planRevision、**计划生成后文件再改动**）
//!   并一次性消费 plan；执行 = 先捕获恢复快照（当前态产物化，恢复资料
//!   保留）→ 暂存全部目标 → 逐项原子 rename/删除 → `files.changed` +
//!   `snapshots.changed` 事件。失败尝试回滚并逐项报告；无法全恢复时
//!   PARTIAL_FAILURE 明示。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::artifacts::{copy_artifact_into, publish_artifact};
use crate::public_v1::domains::files::{
    ProjectResolveError, ProjectWorkspace, byte_path_json, decode_byte_path, file_revision_of,
    request_target, resolve_project,
};
use crate::public_v1::domains::memory::{
    CODE_PLAN_EXPIRED, CODE_SNAPSHOT_UNAVAILABLE, insert_plan, load_plan, memory_db, plan_json,
    set_optional_str, sql_err, submit_memory_operation,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 两段式计划过期窗口（与 memory 域一致）。
const PLAN_TTL_MINUTES: i64 = 10;
/// 列表分页上限。
const LIST_PAGE_LIMIT: usize = 500;
/// 恢复快照中当前内容产物的 origin。
const RECOVERY_ORIGIN: &str = "snapshot-recovery";
/// restore 的部分失败错误码（合同错误表）。
const CODE_PARTIAL_FAILURE: &str = "PARTIAL_FAILURE";

const DDL_SNAPSHOTS: &str = "CREATE TABLE IF NOT EXISTS v1_file_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    run_id TEXT,
    turn_id TEXT,
    created_at TEXT NOT NULL
)";
const DDL_SNAPSHOT_ENTRIES: &str = "CREATE TABLE IF NOT EXISTS v1_file_snapshot_entries (
    snapshot_id TEXT NOT NULL,
    path_bytes BLOB NOT NULL,
    before_revision TEXT,
    after_revision TEXT,
    action TEXT NOT NULL,
    artifact_id TEXT,
    PRIMARY KEY (snapshot_id, path_bytes)
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_file_snapshots", DDL_SNAPSHOTS);
    reg.schema("v1_file_snapshot_entries", DDL_SNAPSHOT_ENTRIES);

    reg.handler("snapshots.list", |rt, input, _key, _rev| {
        Box::pin(snapshots_list(rt, input))
    });
    reg.handler("snapshots.get", |rt, input, _key, _rev| {
        Box::pin(snapshots_get(rt, input))
    });
    reg.handler("snapshots.plan_restore", |rt, input, _key, _rev| {
        Box::pin(snapshots_plan_restore(rt, input))
    });
    reg.handler("snapshots.restore", |rt, input, key, _rev| {
        Box::pin(snapshots_restore(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn domain_not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("snapshots worker task failed: {err}"),
        data: None,
    }
}

fn io_unavailable(err: std::io::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("snapshot storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn revision_conflict(expected: &str, actual: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("file revision `{expected}` does not match current `{actual}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn rfc3339_lt(a: &str, b: &str) -> Option<bool> {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(x), Ok(y)) => Some(x < y),
        _ => None,
    }
}

fn expires_in_minutes(minutes: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::minutes(minutes)).to_rfc3339()
}

fn path_display(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// 当前文件 revision（内容哈希派生；缺失或非普通文件 → None）。
fn stat_current(
    workspace: &ProjectWorkspace,
    path: &[u8],
) -> Result<Option<String>, ContractError> {
    let target = request_target(workspace, path)?;
    let meta = match std::fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_unavailable(err)),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let mut file = std::fs::File::open(&target).map_err(io_unavailable)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    use std::io::Read;
    loop {
        let read = file.read(&mut buffer).map_err(io_unavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let sha = format!("{:x}", hasher.finalize());
    Ok(Some(file_revision_of(&sha, meta.len())))
}

// ---------------------------------------------------------------------------
// 行模型
// ---------------------------------------------------------------------------

struct SnapshotRow {
    snapshot_id: String,
    project_id: String,
    revision: String,
    run_id: Option<String>,
    turn_id: Option<String>,
    created_at: String,
}

struct SnapshotEntryRow {
    path_bytes: Vec<u8>,
    before_revision: Option<String>,
    after_revision: Option<String>,
    action: String,
    artifact_id: Option<String>,
}

const SNAPSHOT_COLUMNS: &str = "snapshot_id, project_id, revision, run_id, turn_id, created_at";

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        snapshot_id: row.get(0)?,
        project_id: row.get(1)?,
        revision: row.get(2)?,
        run_id: row.get(3)?,
        turn_id: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn load_snapshot_entries(
    conn: &Connection,
    snapshot_id: &str,
) -> Result<Vec<SnapshotEntryRow>, ContractError> {
    conn.prepare(
        "SELECT path_bytes, before_revision, after_revision, action, artifact_id
         FROM v1_file_snapshot_entries WHERE snapshot_id = ?1 ORDER BY path_bytes ASC",
    )
    .map_err(sql_err)?
    .query_map(params![snapshot_id], |r| {
        Ok(SnapshotEntryRow {
            path_bytes: r.get(0)?,
            before_revision: r.get(1)?,
            after_revision: r.get(2)?,
            action: r.get(3)?,
            artifact_id: r.get(4)?,
        })
    })
    .map_err(sql_err)?
    .collect::<Result<Vec<_>, _>>()
    .map_err(sql_err)
}

fn snapshot_to_json(row: &SnapshotRow, entries: &[SnapshotEntryRow]) -> Value {
    let mut snapshot = json!({
        "snapshotId": row.snapshot_id,
        "projectId": row.project_id,
        "revision": row.revision,
        "createdAt": row.created_at,
        "entries": entries
            .iter()
            .map(|e| entry_to_json(e, &row.project_id))
            .collect::<Vec<_>>(),
    });
    set_optional_str(&mut snapshot, "runId", &row.run_id);
    set_optional_str(&mut snapshot, "turnId", &row.turn_id);
    snapshot
}

fn entry_to_json(entry: &SnapshotEntryRow, project_id: &str) -> Value {
    let mut value = json!({
        "path": {
            "projectId": project_id,
            "relativePath": byte_path_json(&entry.path_bytes),
        },
        "action": entry.action,
    });
    set_optional_str(&mut value, "beforeRevision", &entry.before_revision);
    set_optional_str(&mut value, "afterRevision", &entry.after_revision);
    set_optional_str(&mut value, "artifactId", &entry.artifact_id);
    value
}

// ---------------------------------------------------------------------------
// 事件
// ---------------------------------------------------------------------------

/// 追加快照域事件（`snapshots.changed` / `files.changed` 各带自己的
/// schema；形态与 files.rs 的 append_file_event 同构——该函数是 files 域私
/// 有）。
fn append_domain_event(
    conn: &Connection,
    resource_id: &str,
    event_type: &str,
    schema_id: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    static SEQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let placeholder = format!("tmpsnap-{nanos}-{seq}");
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', ?3, ?4, ?5, ?5, ?6)",
        params![placeholder, resource_id, event_type, schema_id, payload_str, now_rfc3339()],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![crate::public_v1::store::format_cursor(seq), seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 捕获 seam（集成者接线点；恢复快照与测试共用）
// ---------------------------------------------------------------------------

/// 一个 run/turn 边界上聚合的单个文件副作用。`artifact_id` 是**动作完成
/// 后**的路径内容产物（create/replace；delete 无）。
pub(crate) struct CapturedFileChange {
    pub path: Vec<u8>,
    pub before_revision: Option<String>,
    pub after_revision: Option<String>,
    pub action: &'static str,
    pub artifact_id: Option<String>,
}

/// 提交一条 turn 快照：聚合的 `changes` 落为一条 `v1_file_snapshots` +
/// entries。快照 revision 为内容寻址（条目哈希）。返回 snapshotId。
pub(crate) fn commit_turn_snapshot(
    store: &V1Store,
    project_id: &str,
    run_id: Option<&str>,
    turn_id: Option<&str>,
    changes: &[CapturedFileChange],
) -> Result<String, ContractError> {
    if changes.is_empty() {
        return Err(invalid(
            "a turn snapshot needs at least one captured change",
        ));
    }
    for change in changes {
        if !matches!(change.action, "create" | "replace" | "delete") {
            return Err(invalid(format!(
                "unsupported capture action `{}`",
                change.action
            )));
        }
    }
    let mut hasher = Sha256::new();
    for change in changes {
        hasher.update(change.path.len().to_le_bytes());
        hasher.update(&change.path);
        hasher.update(change.before_revision.as_deref().unwrap_or(""));
        hasher.update(change.after_revision.as_deref().unwrap_or(""));
        hasher.update(change.action);
        hasher.update(change.artifact_id.as_deref().unwrap_or(""));
    }
    let revision = format!("fsnap-{}", &format!("{:x}", hasher.finalize())[..24]);

    let conn = store.conn.lock().expect("sqlite mutex");
    let snapshot_id = store.next_id();
    conn.execute(
        "INSERT INTO v1_file_snapshots (snapshot_id, project_id, revision, run_id, turn_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![snapshot_id, project_id, revision, run_id, turn_id, now_rfc3339()],
    )
    .map_err(sql_err)?;
    for change in changes {
        conn.execute(
            "INSERT INTO v1_file_snapshot_entries
                (snapshot_id, path_bytes, before_revision, after_revision, action, artifact_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                snapshot_id,
                change.path,
                change.before_revision,
                change.after_revision,
                change.action,
                change.artifact_id,
            ],
        )
        .map_err(sql_err)?;
    }
    append_domain_event(
        &conn,
        project_id,
        "snapshots.changed",
        "agentcpm-pd.snapshot.event/1",
        &json!({"projectId": project_id, "snapshotId": snapshot_id, "change": "captured"}),
    )?;
    Ok(snapshot_id)
}

// ---------------------------------------------------------------------------
// 读面：list / get
// ---------------------------------------------------------------------------

async fn snapshots_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || list_snapshots(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 列表游标：`snaplist-<count>-<offset>`（count = 当前快照总数；删除/新
/// 增使游标失效，从第一页重开——files.list 的 revision 钉扎同纪律）。
fn parse_list_cursor(cursor: &str) -> Result<(usize, usize), ContractError> {
    let bad = || invalid(format!("invalid snapshots.list cursor `{cursor}`"));
    let Some(rest) = cursor.strip_prefix("snaplist-") else {
        return Err(bad());
    };
    let (count, offset) = rest.rsplit_once('-').ok_or_else(bad)?;
    Ok((
        count.parse::<usize>().map_err(|_| bad())?,
        offset.parse::<usize>().map_err(|_| bad())?,
    ))
}

fn require_registered_project(conn: &Connection, project_id: &str) -> Result<(), ContractError> {
    let registered: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM projects WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if registered.is_none() {
        return Err(domain_not_found("project", project_id));
    }
    Ok(())
}

fn list_snapshots(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let run_id = input
        .get("runId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let limit = input
        .get("page")
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, LIST_PAGE_LIMIT as u64) as usize;
    let cursor_raw = input
        .get("page")
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let conn = store.conn.lock().expect("sqlite mutex");
    require_registered_project(&conn, &project_id)?;

    let mut sql = format!("SELECT {SNAPSHOT_COLUMNS} FROM v1_file_snapshots WHERE project_id = ?");
    if run_id.is_some() {
        sql.push_str(" AND run_id = ?");
    }
    sql.push_str(" ORDER BY created_at ASC, snapshot_id ASC");
    let mut all = vec![project_id.clone()];
    if let Some(run) = &run_id {
        all.push(run.clone());
    }
    let rows: Vec<SnapshotRow> = conn
        .prepare(&sql)
        .map_err(sql_err)?
        .query_map(
            all.iter()
                .map(|p| p as &dyn rusqlite::types::ToSql)
                .collect::<Vec<_>>()
                .as_slice(),
            row_to_snapshot,
        )
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;

    let mut offset = 0usize;
    if let Some(cursor) = cursor_raw.as_deref() {
        let (cursor_count, cursor_offset) = parse_list_cursor(cursor)?;
        if cursor_count != rows.len() {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "the snapshot list changed during pagination; restart from the first page",
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        offset = cursor_offset;
    }
    if offset > rows.len() {
        return Err(invalid("pagination cursor beyond end of the snapshot list"));
    }
    let end = (offset + limit).min(rows.len());
    let snapshots: Vec<Value> = rows[offset..end]
        .iter()
        .map(|row| {
            let entries = load_snapshot_entries(&conn, &row.snapshot_id)?;
            Ok(snapshot_to_json(row, &entries))
        })
        .collect::<Result<Vec<_>, ContractError>>()?;
    // nextCursor 是可选 string（非 nullable）：完成页省略字段。
    let mut result = json!({"snapshots": snapshots});
    if end < rows.len() {
        result["nextCursor"] = json!(format!("snaplist-{}-{end}", rows.len()));
    }
    Ok(result)
}

async fn snapshots_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let snapshot_id = input["snapshotId"].as_str().unwrap_or_default().to_owned();
    let snapshot = tokio::task::spawn_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let row: Option<SnapshotRow> = conn
            .query_row(
                &format!("SELECT {SNAPSHOT_COLUMNS} FROM v1_file_snapshots WHERE snapshot_id = ?1"),
                params![snapshot_id],
                row_to_snapshot,
            )
            .optional()
            .map_err(sql_err)?;
        let Some(row) = row else {
            return Err(domain_not_found("snapshot", &snapshot_id));
        };
        let entries = load_snapshot_entries(&conn, &row.snapshot_id)?;
        Ok(snapshot_to_json(&row, &entries))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, json!({"snapshot": snapshot})))
}

// ---------------------------------------------------------------------------
// plan_restore（sync 只读，两段式第一跳）
// ---------------------------------------------------------------------------

async fn snapshots_plan_restore(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || plan_restore(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 恢复目标态：路径 → 目标内容产物（None = 目标缺失）。
struct RestoreTarget {
    artifact_id: Option<String>,
    /// 目标态的文件 revision（缺失态为 None）。
    target_revision: Option<String>,
}

/// 项目快照全史中一条内容记录（按 (created_at, snapshot_id) 定序）。
struct HistoryRecord {
    order: (String, String),
    path: Vec<u8>,
    action: String,
    artifact_id: Option<String>,
    before_revision: Option<String>,
    after_revision: Option<String>,
}

fn plan_restore(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let direction = input
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if direction != "before" && direction != "after" {
        return Err(invalid(format!(
            "unsupported restore direction `{direction}`"
        )));
    }
    let snapshot_ids: Vec<String> = input
        .get("snapshotIds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned())
                .collect()
        })
        .unwrap_or_default();
    if snapshot_ids.is_empty() {
        return Err(invalid("snapshotIds must not be empty"));
    }
    let path_filter: Option<Vec<Vec<u8>>> = input
        .get("paths")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
        .map(|paths| {
            paths
                .iter()
                .filter_map(|p| decode_byte_path(p).ok())
                .collect()
        });

    // 工作区必须真实存在（restore 落工作区；虚拟/未注册项目 → NOT_FOUND）。
    let workspace = resolve_workspace(store, &project_id)?;

    // 选中快照（校验归属 + 时间序）、其条目（同序）与项目全史。
    let (selected_rows, selected, history) = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let mut selected: Vec<SnapshotRow> = Vec::with_capacity(snapshot_ids.len());
        for snapshot_id in &snapshot_ids {
            let row: Option<SnapshotRow> = conn
                .query_row(
                    &format!(
                        "SELECT {SNAPSHOT_COLUMNS} FROM v1_file_snapshots WHERE snapshot_id = ?1"
                    ),
                    params![snapshot_id],
                    row_to_snapshot,
                )
                .optional()
                .map_err(sql_err)?;
            let row = row.ok_or_else(|| domain_not_found("snapshot", snapshot_id))?;
            if row.project_id != project_id {
                return Err(ContractError::new(
                    CODE_NOT_FOUND,
                    format!("snapshot `{snapshot_id}` does not belong to project `{project_id}`"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            selected.push(row);
        }
        selected
            .sort_by(|a, b| (&a.created_at, &a.snapshot_id).cmp(&(&b.created_at, &b.snapshot_id)));
        let mut entries_by_snapshot = Vec::new();
        for row in &selected {
            entries_by_snapshot.push(load_snapshot_entries(&conn, &row.snapshot_id)?);
        }
        // 全史（含未选中快照）：before 方向的内容回溯来源。
        let all_rows: Vec<SnapshotRow> = conn
            .prepare(&format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM v1_file_snapshots
                 WHERE project_id = ?1 ORDER BY created_at ASC, snapshot_id ASC"
            ))
            .map_err(sql_err)?
            .query_map(params![project_id], row_to_snapshot)
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        let mut history = Vec::new();
        for row in &all_rows {
            let entries = load_snapshot_entries(&conn, &row.snapshot_id)?;
            for entry in entries {
                history.push(HistoryRecord {
                    order: (row.created_at.clone(), row.snapshot_id.clone()),
                    path: entry.path_bytes,
                    action: entry.action,
                    artifact_id: entry.artifact_id,
                    before_revision: entry.before_revision,
                    after_revision: entry.after_revision,
                });
            }
        }
        (selected, entries_by_snapshot, history)
    };

    let path_allowed = |path: &[u8]| -> bool {
        path_filter
            .as_ref()
            .is_none_or(|set| set.iter().any(|p| p == path))
    };

    // 选中条目（按时间序）。
    let mut selected_entries: Vec<(usize, SnapshotEntryRow)> = Vec::new();
    for (index, entries) in selected.iter().enumerate() {
        for entry in entries {
            if path_allowed(&entry.path_bytes) {
                selected_entries.push((
                    index,
                    SnapshotEntryRow {
                        path_bytes: entry.path_bytes.clone(),
                        before_revision: entry.before_revision.clone(),
                        after_revision: entry.after_revision.clone(),
                        action: entry.action.clone(),
                        artifact_id: entry.artifact_id.clone(),
                    },
                ));
            }
        }
    }

    let mut targets: HashMap<Vec<u8>, RestoreTarget> = HashMap::new();
    let mut conflicts: Vec<Value> = Vec::new();
    let mut conflict_paths: Vec<Vec<u8>> = Vec::new();

    // 目标态派生。
    if direction == "after" {
        for (_, entry) in &selected_entries {
            let target = if entry.action == "delete" {
                RestoreTarget {
                    artifact_id: None,
                    target_revision: None,
                }
            } else {
                RestoreTarget {
                    artifact_id: entry.artifact_id.clone(),
                    target_revision: entry.after_revision.clone(),
                }
            };
            targets.insert(entry.path_bytes.clone(), target);
        }
    } else {
        // before：每个路径取最早选中触碰；目标 = 该动作之前的状态。
        let mut earliest_done: Vec<Vec<u8>> = Vec::new();
        for (index, entry) in &selected_entries {
            if earliest_done.contains(&entry.path_bytes) {
                continue;
            }
            earliest_done.push(entry.path_bytes.clone());
            let row = &selected_rows[*index];
            if entry.action == "create" {
                targets.insert(
                    entry.path_bytes.clone(),
                    RestoreTarget {
                        artifact_id: None,
                        target_revision: None,
                    },
                );
            } else {
                // replace/delete：回溯全史中严格早于最早选中触碰的内容产物。
                let order = (row.created_at.clone(), row.snapshot_id.clone());
                let mut found: Option<&HistoryRecord> = None;
                for record in &history {
                    if record.path == entry.path_bytes
                        && record.action != "delete"
                        && record.order < order
                        && record.artifact_id.is_some()
                    {
                        found = Some(record); // 时间序遍历 → 末次命中即最近。
                    }
                }
                match found {
                    Some(record) => {
                        targets.insert(
                            entry.path_bytes.clone(),
                            RestoreTarget {
                                artifact_id: record.artifact_id.clone(),
                                target_revision: record.after_revision.clone(),
                            },
                        );
                    }
                    None => {
                        let mut conflict = json!({
                            "resourceId": path_display(&entry.path_bytes),
                            "code": CODE_SNAPSHOT_UNAVAILABLE,
                        });
                        set_optional_str(&mut conflict, "observedRevision", &entry.before_revision);
                        conflicts.push(conflict);
                        conflict_paths.push(entry.path_bytes.clone());
                    }
                }
            }
        }
    }

    // 产物存在性：非 delete 目标的内容产物必须存在（被清理状态明确）。
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        for (path, target) in &targets {
            if let Some(artifact) = &target.artifact_id {
                let exists: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM artifacts WHERE artifact_id = ?1",
                        params![artifact],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql_err)?;
                if exists.is_none() {
                    let mut conflict = json!({
                        "resourceId": path_display(path),
                        "code": CODE_SNAPSHOT_UNAVAILABLE,
                    });
                    set_optional_str(&mut conflict, "observedRevision", &target.target_revision);
                    conflicts.push(conflict);
                    conflict_paths.push(path.clone());
                }
            }
        }
    }
    for path in &conflict_paths {
        targets.remove(path);
    }

    // 当前态 → 计划条目。
    let mut plan_entries: Vec<Value> = Vec::new();
    let mut payload_targets: Vec<Value> = Vec::new();
    let mut base_revisions: Vec<Value> = Vec::new();
    let mut destructive = false;
    let mut sorted_paths: Vec<&Vec<u8>> = targets.keys().collect();
    sorted_paths.sort();
    for path in sorted_paths.iter().copied() {
        let target = &targets[path];
        let current = stat_current(&workspace, path)?;
        let base_marker = current.clone().unwrap_or_else(|| "absent".to_owned());
        base_revisions.push(json!({
            "resourceId": path_display(path),
            "revision": base_marker,
        }));
        payload_targets.push(json!({
            "path": byte_path_json(path),
            "artifactId": target.artifact_id,
            "targetRevision": target.target_revision,
            "currentRevision": current,
        }));
        let same = match (&current, &target.target_revision) {
            (Some(current), Some(target_revision)) => current == target_revision,
            (None, None) => true,
            _ => false,
        };
        if same {
            continue; // no-op：不进计划。
        }
        let action = match (&current, &target.artifact_id) {
            (None, Some(_)) => "create",
            (Some(_), Some(_)) => "replace",
            (Some(_), None) => "delete",
            (None, None) => continue, // absent → absent：no-op。
        };
        if current.is_some() {
            destructive = true;
        }
        let mut plan_entry = json!({
            "path": {
                "projectId": project_id,
                "relativePath": byte_path_json(path),
            },
            "action": action,
        });
        set_optional_str(&mut plan_entry, "beforeRevision", &current);
        set_optional_str(&mut plan_entry, "afterRevision", &target.target_revision);
        set_optional_str(&mut plan_entry, "artifactId", &target.artifact_id);
        plan_entries.push(plan_entry);
    }

    let conn = store.conn.lock().expect("sqlite mutex");
    let plan_id = store.next_id();
    let expires_at = expires_in_minutes(PLAN_TTL_MINUTES);
    let mut affected = vec![project_id.clone()];
    affected.extend(sorted_paths.iter().map(|p| path_display(p)));
    let payload = json!({
        "projectId": project_id,
        "direction": direction,
        "targets": payload_targets,
    });
    insert_plan(
        &conn,
        &plan_id,
        &plan_id,
        "restore",
        &expires_at,
        &json!({"kind": "project", "projectId": project_id}),
        &affected,
        destructive,
        &payload,
    )?;
    // restore 的 base revision 是逐路径文件 revision（非 memory 域计数），
    // 覆写 plan 行的 base_revisions_json。
    conn.execute(
        "UPDATE v1_memory_change_plans SET base_revisions_json = ?1 WHERE plan_id = ?2",
        params![
            serde_json::to_string(&base_revisions).unwrap_or_default(),
            plan_id
        ],
    )
    .map_err(sql_err)?;

    let plan = plan_json(
        &plan_id,
        &plan_id,
        &expires_at,
        &affected,
        &conflicts,
        destructive,
    );
    Ok(json!({"plan": plan, "entries": plan_entries}))
}

fn resolve_workspace(store: &V1Store, project_id: &str) -> Result<ProjectWorkspace, ContractError> {
    match resolve_project(store, project_id) {
        Ok(workspace) => Ok(workspace),
        Err(ProjectResolveError::Contract(error)) => Err(error),
        Err(other) => Err(other.into()),
    }
}

// ---------------------------------------------------------------------------
// restore（async idempotentSubmit，两段式第二跳）
// ---------------------------------------------------------------------------

async fn snapshots_restore(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = memory_db(&rt).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let expected_plan_revision = input["expectedPlanRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let store_for_validate = store.clone();
    let plan_id_for_validate = plan_id.clone();
    let accepted_revision = store.next_id();
    let (operation_id, receipt) = submit_memory_operation(
        &db,
        "snapshots.restore",
        &input,
        idempotency_key.as_deref(),
        accepted_revision,
        || store.next_id(),
        move || {
            validate_and_consume_restore_plan(
                &store_for_validate,
                &plan_id_for_validate,
                &expected_plan_revision,
            )?;
            Ok(())
        },
    )
    .map_err(DispatchError::Application)?;
    let db = db.clone();
    let store2 = store.clone();
    tokio::task::spawn_blocking(move || {
        let _ = execute_restore(&store2, &db, &operation_id, &plan_id);
    });
    Ok((ResponseOutcome::Accepted, receipt))
}

/// restore 第二跳复验：plan 存在/未消费/类型、过期、planRevision、
/// **计划生成后的文件再改动**（当前 revision ≠ plan 记录 →
/// REVISION_CONFLICT，不无条件覆盖）。通过即标记 consumed。
fn validate_and_consume_restore_plan(
    store: &V1Store,
    plan_id: &str,
    expected_plan_revision: &str,
) -> Result<(), ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let plan =
        load_plan(&conn, plan_id)?.ok_or_else(|| domain_not_found("change plan", plan_id))?;
    if plan.kind != "restore" {
        return Err(invalid(format!(
            "plan `{plan_id}` is a `{}` plan, not a restore plan",
            plan.kind
        )));
    }
    if plan.state != "open" {
        return Err(domain_not_found("change plan", plan_id));
    }
    if rfc3339_lt(&plan.expires_at, &now_rfc3339()) == Some(true) {
        return Err(ContractError::new(
            CODE_PLAN_EXPIRED,
            format!(
                "change plan expired at `{}`; re-plan and retry",
                plan.expires_at
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if plan.plan_revision != expected_plan_revision {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "plan revision `{expected_plan_revision}` does not match `{}`",
                plan.plan_revision
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    // 文件现状复验：计划生成后文件改动导致冲突。
    let project_id = plan.payload["projectId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let workspace = resolve_workspace(store, &project_id)?;
    if let Some(targets) = plan.payload.get("targets").and_then(Value::as_array) {
        for target in targets {
            let path = decode_byte_path(&target["path"])?;
            let current = stat_current(&workspace, &path)?;
            let recorded = target["currentRevision"].as_str().unwrap_or("absent");
            let observed = current.as_deref().unwrap_or("absent");
            if recorded != observed {
                return Err(revision_conflict(recorded, observed));
            }
        }
    }
    conn.execute(
        "UPDATE v1_memory_change_plans SET state = 'consumed' WHERE plan_id = ?1 AND state = 'open'",
        params![plan_id],
    )
    .map_err(sql_err)?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_restore(
    store: &Arc<V1Store>,
    db: &Arc<crate::public_v1::domains::schedules::DomainDb>,
    operation_id: &str,
    plan_id: &str,
) -> Result<Value, ContractError> {
    crate::public_v1::domains::schedules::mark_domain_operation_running(db, operation_id, "")?;
    let result = execute_restore_inner(store, plan_id);
    match result {
        Ok(completion) => {
            crate::public_v1::domains::schedules::finish_domain_operation(
                db,
                operation_id,
                "",
                "succeeded",
                &completion,
            )?;
            Ok(completion)
        }
        Err(error) => {
            crate::public_v1::domains::schedules::finish_domain_operation(
                db,
                operation_id,
                "",
                "failed",
                &json!({"errorCode": error.code, "message": error.message}),
            )?;
            Err(error)
        }
    }
}

/// 多文件恢复：先捕获恢复快照（当前态产物化，恢复资料保留）→ 全部暂存
/// → 逐项原子提交；失败尝试回滚并逐项报告（partial + PARTIAL_FAILURE）。
fn execute_restore_inner(store: &Arc<V1Store>, plan_id: &str) -> Result<Value, ContractError> {
    let payload = {
        let conn = store.conn.lock().expect("sqlite mutex");
        load_plan(&conn, plan_id)?
            .ok_or_else(|| domain_not_found("change plan", plan_id))?
            .payload
    };
    let project_id = payload["projectId"].as_str().unwrap_or_default().to_owned();
    let workspace = resolve_workspace(store, &project_id)?;

    // 目标列表（path 字节 + artifact + 当前态）。
    struct Target {
        path: Vec<u8>,
        artifact_id: Option<String>,
        current: Option<String>,
    }
    let mut targets: Vec<Target> = Vec::new();
    for target in payload
        .get("targets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let path = decode_byte_path(&target["path"])?;
        targets.push(Target {
            path,
            artifact_id: target["artifactId"].as_str().map(ToOwned::to_owned),
            current: target["currentRevision"].as_str().map(ToOwned::to_owned),
        });
    }

    // 1) 恢复快照：当前态产物化（缺失路径不记内容条目——恢复「缺失」可
    //    由原 plan 重放获得；存在路径的内容是恢复资料）。
    let mut recovery_changes: Vec<CapturedFileChange> = Vec::new();
    for target in &targets {
        let current = stat_current(&workspace, &target.path)?;
        if let Some(revision) = &current {
            let bytes =
                std::fs::read(request_target(&workspace, &target.path)?).map_err(io_unavailable)?;
            let artifact = publish_artifact(
                store,
                &project_id,
                "application/octet-stream",
                Some(&format!("snapshot-recovery:{}", path_display(&target.path))),
                &bytes,
                RECOVERY_ORIGIN,
            )?;
            let artifact_id = artifact["artifactId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            // 恢复快照条目：当前态作为 after 内容（对恢复快照做
            // direction=after 的恢复即可回到恢复前状态）。
            recovery_changes.push(CapturedFileChange {
                path: target.path.clone(),
                before_revision: target.current.clone(),
                after_revision: Some(revision.clone()),
                action: "replace",
                artifact_id: Some(artifact_id),
            });
        }
    }
    let recovery_snapshot_id = if recovery_changes.is_empty() {
        None
    } else {
        Some(commit_turn_snapshot(
            store,
            &project_id,
            None,
            None,
            &recovery_changes,
        )?)
    };

    // 2) 暂存：每个有内容的目标写同目录临时文件（fsync）。
    struct Staged {
        path: Vec<u8>,
        temp: PathBuf,
    }
    let mut staged: Vec<Staged> = Vec::new();
    let mut staged_failed: Vec<Vec<u8>> = Vec::new();
    for target in &targets {
        let Some(artifact_id) = &target.artifact_id else {
            continue; // 目标缺失（delete）：无需暂存。
        };
        let dest = request_target(&workspace, &target.path)?;
        let Some(parent) = dest.parent() else {
            staged_failed.push(target.path.clone());
            continue;
        };
        std::fs::create_dir_all(parent).map_err(io_unavailable)?;
        let temp = parent.join(format!(".agentcpm-pd-restore-{}", store.next_id()));
        let write = (|| -> Result<(), ContractError> {
            let mut file = std::fs::File::create(&temp).map_err(io_unavailable)?;
            copy_artifact_into(store, artifact_id, &mut file)?;
            file.sync_all().map_err(io_unavailable)?;
            Ok(())
        })();
        if let Err(error) = write {
            let _ = std::fs::remove_file(&temp);
            let _ = error;
            staged_failed.push(target.path.clone());
            continue;
        }
        staged.push(Staged {
            path: target.path.clone(),
            temp,
        });
    }

    // 3) 提交：rename（create/replace）+ delete；逐项报告失败。
    let mut restored: Vec<Vec<u8>> = Vec::new();
    let mut failed: Vec<Vec<u8>> = staged_failed;
    let mut committed: Vec<Staged> = Vec::new();
    for item in staged.drain(..) {
        let dest = request_target(&workspace, &item.path)?;
        if std::fs::rename(&item.temp, &dest).is_ok() {
            restored.push(item.path.clone());
            committed.push(item);
        } else {
            let _ = std::fs::remove_file(&item.temp);
            failed.push(item.path.clone());
        }
    }
    for target in &targets {
        if target.artifact_id.is_none() && target.current.is_some() {
            let dest = request_target(&workspace, &target.path)?;
            match std::fs::remove_file(&dest) {
                Ok(()) => restored.push(target.path.clone()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    restored.push(target.path.clone());
                }
                Err(_) => failed.push(target.path.clone()),
            }
        }
    }

    // 失败时回滚已提交项（从恢复快照内容还原）。
    let mut outcome = "complete";
    let mut rollback_failures: Vec<Vec<u8>> = Vec::new();
    if !failed.is_empty() {
        outcome = "partial";
        let mut rolled_back = 0usize;
        for item in &committed {
            match recovery_artifact_for(&recovery_changes, &item.path) {
                Some(artifact) => {
                    let dest = request_target(&workspace, &item.path)?;
                    if write_bytes_from_artifact(store, &artifact, &dest).is_ok() {
                        rolled_back += 1;
                    } else {
                        rollback_failures.push(item.path.clone());
                    }
                }
                None => rollback_failures.push(item.path.clone()),
            }
        }
        if !committed.is_empty() && rolled_back == committed.len() {
            outcome = "rolledBack";
            restored.clear();
        }
    }

    // 4) 事件：files.changed（逐变更路径）+ snapshots.changed（恢复快照）。
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        for path in &restored {
            append_domain_event(
                &conn,
                &project_id,
                "files.changed",
                "agentcpm-pd.file.event/1",
                &json!({
                    "projectId": project_id,
                    "path": {
                        "projectId": project_id,
                        "relativePath": byte_path_json(path),
                    },
                    "change": "restore",
                }),
            )?;
        }
        if let Some(recovery) = &recovery_snapshot_id {
            append_domain_event(
                &conn,
                &project_id,
                "snapshots.changed",
                "agentcpm-pd.snapshot.event/1",
                &json!({
                    "projectId": project_id,
                    "snapshotId": recovery,
                    "change": "recovery",
                }),
            )?;
        }
    }

    let mut completion = json!({
        "restoredPaths": restored.iter().map(|p| json!({
            "projectId": project_id,
            "relativePath": byte_path_json(p),
        })).collect::<Vec<_>>(),
        "outcome": outcome,
        "failedPaths": failed.iter().map(|p| json!({
            "projectId": project_id,
            "relativePath": byte_path_json(p),
        })).collect::<Vec<_>>(),
    });
    if let Some(recovery) = recovery_snapshot_id {
        completion["recoverySnapshotId"] = json!(recovery);
    }
    if !rollback_failures.is_empty() {
        // partial 且部分已提交项连回滚都失败：效果混合不可辨 → 显式
        // PARTIAL_FAILURE（恢复资料保留在恢复快照里）。
        return Err(ContractError::new(
            CODE_PARTIAL_FAILURE,
            format!(
                "restore applied partially and rollback incomplete for {} path(s); recovery material is preserved in the recovery snapshot",
                rollback_failures.len()
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::Applied,
        ));
    }
    Ok(completion)
}

fn recovery_artifact_for(recovery_changes: &[CapturedFileChange], path: &[u8]) -> Option<String> {
    recovery_changes
        .iter()
        .find(|change| change.path == path)
        .and_then(|change| change.artifact_id.clone())
}

fn write_bytes_from_artifact(
    store: &V1Store,
    artifact_id: &str,
    dest: &std::path::Path,
) -> Result<(), ContractError> {
    let mut file = std::fs::File::create(dest).map_err(io_unavailable)?;
    copy_artifact_into(store, artifact_id, &mut file)?;
    file.sync_all().map_err(io_unavailable)?;
    Ok(())
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

    fn open_runtime(root_dir: &std::path::Path) -> Arc<PublicV1Runtime> {
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
        PublicV1Runtime::new(store)
    }

    fn setup(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, PathBuf, String) {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        // 播种带真实授权根的项目（authorized_roots 直插：本域测试不需要
        // directories 声明面的 CLI 往返）。
        let workspace = tmp.0.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = rt.store();
        let project_id = format!("proj-{tag}");
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, source, created_at)
                 VALUES ('test-root', ?1, ?2, 'test', ?3)",
                params![
                    rt.store().meta().instance_id,
                    workspace.to_string_lossy(),
                    now_rfc3339()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
                 VALUES (?1, 'Snap Test', ?2, 'registered', 'local', 'prev-1', ?3)",
                params![
                    project_id,
                    json!({"rootId": "test-root", "relativePath": ""}).to_string(),
                    now_rfc3339()
                ],
            )
            .unwrap();
        }
        let canonical = std::fs::canonicalize(&workspace).unwrap();
        (tmp, rt, canonical, project_id)
    }

    async fn dispatch_ok(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> Value {
        let (_, result) = rt.dispatch(method, input, None, None).await.unwrap();
        OperationRegistry::global()
            .validate_result(method, &result)
            .unwrap_or_else(|e| panic!("`{method}` result failed schema: {e:?}\n{result}"));
        result
    }

    async fn dispatch_submit(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: &str,
    ) -> Value {
        let (_, result) = rt
            .dispatch(method, input, Some(key), None)
            .await
            .unwrap_or_else(|e| panic!("`{method}` submit failed: {e:?}"));
        OperationRegistry::global()
            .validate_result(method, &result)
            .unwrap_or_else(|e| panic!("`{method}` receipt failed schema: {e:?}\n{result}"));
        result
    }

    async fn dispatch_err(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: Option<&str>,
    ) -> ContractError {
        match rt.dispatch(method, input, key, None).await {
            Err(DispatchError::Application(e)) => e,
            other => panic!("`{method}` expected application error, got {other:?}"),
        }
    }

    /// 播种一条快照：给定的 (path, bytes) 视作一次 create/replace 副作用。
    fn seed_snapshot(
        rt: &Arc<PublicV1Runtime>,
        project_id: &str,
        tag: &str,
        writes: &[(&str, &[u8], &str)],
        deletes: &[(&str, &str)],
    ) -> String {
        let store = rt.store();
        let mut changes = Vec::new();
        for (path, bytes, action) in writes {
            let artifact = publish_artifact(
                store,
                project_id,
                "application/octet-stream",
                Some(&format!("seed:{path}")),
                bytes,
                "snapshot-seed",
            )
            .unwrap();
            let sha = {
                let mut hasher = Sha256::new();
                hasher.update(bytes);
                format!("{:x}", hasher.finalize())
            };
            let after = file_revision_of(&sha, bytes.len() as u64);
            changes.push(CapturedFileChange {
                path: path.as_bytes().to_vec(),
                before_revision: None,
                after_revision: Some(after),
                action,
                artifact_id: Some(
                    artifact["artifactId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ),
            });
        }
        for (path, before) in deletes {
            changes.push(CapturedFileChange {
                path: path.as_bytes().to_vec(),
                before_revision: Some(before.to_owned()),
                after_revision: None,
                action: "delete",
                artifact_id: None,
            });
        }
        let run_id = format!("run-{tag}");
        let turn_id = format!("turn-{tag}");
        commit_turn_snapshot(store, project_id, Some(&run_id), Some(&turn_id), &changes).unwrap()
    }

    fn read_workspace_file(workspace: &std::path::Path, path: &str) -> Option<Vec<u8>> {
        std::fs::read(workspace.join(path)).ok()
    }

    #[tokio::test]
    async fn list_get_and_after_restore_roundtrip() {
        let (_tmp, rt, workspace, project_id) = setup("snap-after");

        // 无快照：空列表（读面可用性）。
        let listed = dispatch_ok(&rt, "snapshots.list", json!({"projectId": project_id})).await;
        assert_eq!(listed["snapshots"].as_array().unwrap().len(), 0);

        // 播种真实文件 + 快照：a.txt 创建（v1），b.txt 创建（v2）。
        let a1 = b"alpha-one".to_vec();
        let b1 = b"beta-one".to_vec();
        let snap1 = seed_snapshot(&rt, &project_id, "one", &[("a.txt", &a1, "create")], &[]);
        std::fs::write(workspace.join("a.txt"), &a1).unwrap();
        let snap2 = seed_snapshot(
            &rt,
            &project_id,
            "two",
            &[("b.txt", &b1, "create"), ("a.txt", b"alpha-two", "replace")],
            &[],
        );
        std::fs::write(workspace.join("b.txt"), &b1).unwrap();
        std::fs::write(workspace.join("a.txt"), b"alpha-two").unwrap();

        // list / get。
        let listed = dispatch_ok(
            &rt,
            "snapshots.list",
            json!({"projectId": project_id, "page": {"limit": 1}}),
        )
        .await;
        assert_eq!(listed["snapshots"].as_array().unwrap().len(), 1);
        assert!(listed["nextCursor"].as_str().is_some());
        let got = dispatch_ok(
            &rt,
            "snapshots.get",
            json!({"snapshotId": listed["snapshots"][0]["snapshotId"]}),
        )
        .await;
        assert_eq!(
            got["snapshot"]["entries"].as_array().unwrap().len(),
            1,
            "first snapshot has one entry"
        );
        let err = dispatch_err(&rt, "snapshots.get", json!({"snapshotId": "missing"}), None).await;
        assert_eq!(err.code, "NOT_FOUND");

        // plan_restore direction=after 选中快照1：只触碰选中快照记录的路径
        // （合同：只选择指定快照与路径）——a.txt 回 v1；b.txt 不在计划里。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snap1],
                "direction": "after",
            }),
        )
        .await;
        assert_eq!(plan["plan"]["destructive"], json!(true));
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(
            entries.len(),
            1,
            "only selected snapshot paths: {entries:?}"
        );
        assert_eq!(entries[0]["action"], json!("replace"));
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        // 受理后文件再改动 → REVISION_CONFLICT（不无条件覆盖）。
        std::fs::write(workspace.join("a.txt"), b"alpha-three").unwrap();
        let err = dispatch_err(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            Some("restore-stale"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");
        // 改回计划生成时的状态后重新计划。
        std::fs::write(workspace.join("a.txt"), b"alpha-two").unwrap();
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snap1],
                "direction": "after",
            }),
        )
        .await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        // 错 planRevision → REVISION_CONFLICT。
        let err = dispatch_err(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": "wrong"}),
            Some("restore-wrong-rev"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");

        // 正确二跳：恢复 a.txt v1、删除 b.txt。
        let receipt = dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "restore-ok",
        )
        .await;
        assert_eq!(receipt["kind"], json!("operation"));
        // 等执行完成（恢复快照写入即已生效）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if read_workspace_file(&workspace, "a.txt").as_deref() == Some(&b"alpha-one"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "restore never applied"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            read_workspace_file(&workspace, "b.txt").is_some(),
            "b.txt untouched (not part of the selected snapshot)"
        );

        // 选整个运行（两快照）+ before = 逆副作用顺序回退：两文件都删。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snap1, snap2],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 2, "both created files rewind: {entries:?}");
        assert!(entries.iter().all(|e| e["action"] == json!("delete")));

        // events：files.changed + snapshots.changed。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let files: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'files.changed' AND payload_json LIKE '%\"change\":\"restore\"%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(files >= 1, "files.changed restore events: {files}");
            let snaps: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'snapshots.changed'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(snaps >= 2, "captured + recovery snapshot events: {snaps}");
        }

        // 恢复资料：restore 后 list 里应有恢复快照（recovery）。
        let listed = dispatch_ok(&rt, "snapshots.list", json!({"projectId": project_id})).await;
        assert!(listed["snapshots"].as_array().unwrap().len() >= 3);
        let _ = snap2;
    }

    #[tokio::test]
    async fn before_direction_and_unavailable_conflict() {
        let (_tmp, rt, workspace, project_id) = setup("snap-before");

        // v1 创建 a.txt，v2 替换 a.txt。
        let a1 = b"content-one".to_vec();
        let a2 = b"content-two".to_vec();
        let snap1 = seed_snapshot(&rt, &project_id, "v1", &[("a.txt", &a1, "create")], &[]);
        std::fs::write(workspace.join("a.txt"), &a1).unwrap();
        let _snap2 = seed_snapshot(&rt, &project_id, "v2", &[("a.txt", &a2, "replace")], &[]);
        std::fs::write(workspace.join("a.txt"), &a2).unwrap();

        // before 选中 v2：目标 = v2 之前的 a.txt 内容（v1 的产物）。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [_snap2],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["action"], json!("replace"));
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        let receipt = dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "restore-before",
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if read_workspace_file(&workspace, "a.txt").as_deref() == Some(&b"content-one"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "before-restore never applied"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let _ = receipt;

        // before 选中 v1（create）：目标 = 缺失 → delete。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snap1],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["action"], json!("delete"));
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        let _receipt = dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "restore-before-delete",
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if read_workspace_file(&workspace, "a.txt").is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "delete-restore never applied"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 无历史内容可回溯的 before（replace 但无更早快照）→ 冲突明确，
        // 不展示为可恢复成功。
        let snap3 = seed_snapshot(
            &rt,
            &project_id,
            "v3",
            &[("c.txt", b"orphan", "replace")],
            &[],
        );
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snap3],
                "direction": "before",
            }),
        )
        .await;
        let conflicts = plan["plan"]["conflicts"].as_array().unwrap().clone();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0]["code"], json!("SNAPSHOT_UNAVAILABLE"));
        assert_eq!(plan["entries"].as_array().unwrap().len(), 0);

        // 他人项目的快照 → NOT_FOUND。
        let err = dispatch_err(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": ["someone-else"],
                "direction": "after",
            }),
            None,
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");
    }
}
