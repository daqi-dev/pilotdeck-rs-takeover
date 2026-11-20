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
//! ## 快照捕获接线（snap-wire 报告 §5 方案 A）
//!
//! 合同没有 capture 操作——快照是系统在 run/turn 边界**自动捕获**的文
//! 件副作用记录（`snapshots.list` acceptance「可按run查询」；TS 基线两
//! 套机制均为自动）。接线分两层：
//!
//! - **数据源层（A1）**：`files.rs` 的 write/move/delete 在落盘后把捕获
//!   条目登记进 `v1_file_capture_ledger`（before/after revision + after
//!   内容产物；move = src delete + dst 条目；delete 单条）。**登记在写门
//!   临界区内完成**（dsh r3 P1#1）：落盘→登记→事件→A6b 聚合一次做完，
//!   台账 seq 与文件系统变更时刻同序，并发同路径操作不会把时间序倒置进
//!   恢复面（write→delete 倒置会让 before 方向恢复删掉成功创建的文件）。
//!   产物化纪律：条目模型的 `artifactId` 是 **after** 内容——inline 字节发
//!   布 origin=`snapshot-capture` 的产物，artifact 输入直接复用输入
//!   artifactId（recovery artifact 不能充当）。move 覆盖已存在目标时条
//!   目是 replace（before=门内观测的 dest revision）并在 rename 前发布
//!   dest 原字节 recovery 产物（P2a）。登记失败随 files.rs 现行事件纪律
//!   上抛（落盘已生效，EffectStatus 语义不变；move 后的产物化失败统一
//!   RESULT_UNKNOWN——P2c）。
//! - **边界层（A2）**：run 终态后由 [`capture_run_boundary`] 聚合本 run
//!   的未消费条目为一条 run 归属快照（turn_id 定死 = run_id，与
//!   `drive_owned_turn` 以 runId 为 turn 身份一致）；聚合前按
//!   [`fold_ledger_rows`] 对同路径多行折叠（P1#4：同 run 同路径两行会撞
//!   entries 主键使整条快照事务回滚）。捕获失败**不翻转已终态的 run**：
//!   记 `run.snapshot_capture_failed` 进度事件 + 保留 ledger 行（对齐基
//!   线 post_agent 的 log-only 纪律）。
//! - **CLI 直写（A6b）**：无 run 归属的条目在每次 files.* 变更操作结束
//!   时立即聚合为一条 `run_id=NULL` 的快照（一写一快照；move 双条目一
//!   快照），`snapshots.list` 不传 runId 时全部可见、传 runId 时精确过
//!   滤（两个 acceptance 均满足）。
//!
//! 归属机制（dsh r3 P1#3 修订）：归属由**调用方显式传入**——driver 路径
//! （A5 的 `[pd-driver:write=...]`，及 D20 真实 turn 机落地后的桥接层）
//! 传 `Some(run_id)`，普通 CLI `files.*` 请求恒传 `None`。不再读进程内
//! project→run 单键 map：同项目并发两个 run（不同 session）时各归各的
//! 台账行，run 期间的 CLI 直写走 A6b 即时快照、不误归 run。
//!
//! 登记差异：不引入基线 `input_accepted` 后的 pre_user 全工作区快照
//! （fail-closed 那套）——v1 条目模型的 before 来自逐写 beforeRevision +
//! 更早快照 after 内容回溯（`plan_restore` 已实现），无需整树 pre 态。
//!
//! 开放项：快照保留/合并策略（写频高的脚本会产生大量单条目快照；基线
//! F13 是 100 条上限）；`run.snapshot_capture_failed` 事件类型命名；目
//! 录 move/delete 只登记顶层路径条目（不逐文件展开，恢复面对目录条目
//! 表现为不可恢复的明确冲突而非静默丢失）。
//!
//! ## 协议
//!
//! - `plan_restore`（sync 只读）：派生目标态 + 当前态差异 → ChangePlan；
//!   内容产物缺失 → `plan.conflicts`（不展示为可恢复成功）。
//! - `restore`（async idempotentSubmit）：受理时复验（PLAN_EXPIRED /
//!   REVISION_CONFLICT：过期、planRevision、**计划生成后文件再改动**）
//!   并一次性消费 plan；执行 = 全部目标路径的写门（字典序取齐，与
//!   files.rs move 同构）内：重验 target.current（受理复验与提交之间
//!   落下的并发改动 → 该路径零覆盖进 failedPaths，dsh r3 P1#2）→ 先捕获
//!   恢复快照（当前态产物化，恢复资料保留；流式——P2b）→ 暂存全部目标 →
//!   逐项原子 rename/删除 → `files.changed` + `snapshots.changed` 事件。
//!   失败尝试回滚并逐项报告；无法全恢复时 PARTIAL_FAILURE 明示。

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
use crate::public_v1::domains::artifacts::copy_artifact_into;
use crate::public_v1::domains::files::{
    ProjectResolveError, ProjectWorkspace, byte_path_json, decode_byte_path, file_revision_of,
    leaf_identity_path, open_file_canonical, request_target, require_within, resolve_project,
    with_write_gates,
};
use crate::public_v1::domains::memory::{
    CODE_PLAN_EXPIRED, CODE_SNAPSHOT_UNAVAILABLE, insert_plan, load_plan, memory_db, plan_json,
    set_optional_str, sql_err, submit_memory_operation,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{StoredRun, V1Store, now_rfc3339};

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
/// A1 捕获台账：files 域三写点登记、边界层消费。`run_id IS NULL` 的行
/// 由 A6b 直写路径在同事务内即时聚合消费；run 归属行由
/// [`capture_run_boundary`] 在 run 终态后消费。
const DDL_CAPTURE_LEDGER: &str = "CREATE TABLE IF NOT EXISTS v1_file_capture_ledger (
    seq INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL,
    run_id TEXT,
    path_bytes BLOB NOT NULL,
    before_revision TEXT,
    after_revision TEXT,
    action TEXT NOT NULL,
    artifact_id TEXT,
    occurred_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_file_snapshots", DDL_SNAPSHOTS);
    reg.schema("v1_file_snapshot_entries", DDL_SNAPSHOT_ENTRIES);
    reg.schema("v1_file_capture_ledger", DDL_CAPTURE_LEDGER);

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
// 捕获 seam（A1/A2/A6b 生产接线；恢复快照与测试共用）
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

/// 快照 revision：聚合条目的内容寻址哈希（含 action 校验）。
fn snapshot_revision_of(changes: &[CapturedFileChange]) -> Result<String, ContractError> {
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
    Ok(format!(
        "fsnap-{}",
        &format!("{:x}", hasher.finalize())[..24]
    ))
}

/// 在已有连接（可在打开的事务内）落一条快照 + entries +
/// `snapshots.changed` 事件。快照 revision 为内容寻址（条目哈希）。
fn insert_turn_snapshot(
    conn: &Connection,
    snapshot_id: &str,
    project_id: &str,
    run_id: Option<&str>,
    turn_id: Option<&str>,
    revision: &str,
    changes: &[CapturedFileChange],
) -> Result<(), ContractError> {
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
        conn,
        project_id,
        "snapshots.changed",
        "agentcpm-pd.snapshot.event/1",
        &json!({"projectId": project_id, "snapshotId": snapshot_id, "change": "captured"}),
    )?;
    Ok(())
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
    let revision = snapshot_revision_of(changes)?;
    let snapshot_id = store.next_id();
    let conn = store.conn.lock().expect("sqlite mutex");
    insert_turn_snapshot(
        &conn,
        &snapshot_id,
        project_id,
        run_id,
        turn_id,
        &revision,
        changes,
    )?;
    Ok(snapshot_id)
}

// ---------------------------------------------------------------------------
// 捕获台账（A1 登记 / A2 边界消费 / A6b 直写即时聚合）
// ---------------------------------------------------------------------------

/// 台账行（未消费的捕获条目，action 原样携带待转换）。
struct LedgerRow {
    seq: i64,
    path: Vec<u8>,
    before_revision: Option<String>,
    after_revision: Option<String>,
    action: String,
    artifact_id: Option<String>,
}

/// ledger 行的 action 列 → 合同枚举字面量（写入方只产这三种）。
fn ledger_action(action: &str) -> Result<&'static str, ContractError> {
    match action {
        "create" => Ok("create"),
        "replace" => Ok("replace"),
        "delete" => Ok("delete"),
        other => Err(invalid(format!("unsupported capture action `{other}`"))),
    }
}

impl LedgerRow {
    fn to_change(&self) -> Result<CapturedFileChange, ContractError> {
        Ok(CapturedFileChange {
            path: self.path.clone(),
            before_revision: self.before_revision.clone(),
            after_revision: self.after_revision.clone(),
            action: ledger_action(&self.action)?,
            artifact_id: self.artifact_id.clone(),
        })
    }
}

/// A1：登记捕获条目（files 域三写点调用；`conn` 是调用方事务内的连
/// 接）。`run_id` 来自 `PublicV1Runtime` 的活跃写归属。
pub(crate) fn insert_capture_ledger(
    conn: &Connection,
    project_id: &str,
    run_id: Option<&str>,
    changes: &[CapturedFileChange],
) -> Result<(), ContractError> {
    for change in changes {
        conn.execute(
            "INSERT INTO v1_file_capture_ledger
                (project_id, run_id, path_bytes, before_revision, after_revision, action, artifact_id, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                project_id,
                run_id,
                change.path,
                change.before_revision,
                change.after_revision,
                change.action,
                change.artifact_id,
                now_rfc3339(),
            ],
        )
        .map_err(sql_err)?;
    }
    Ok(())
}

/// 读取未消费台账行（`run_id = None` → 无归属行）。
fn load_capture_ledger(
    conn: &Connection,
    project_id: &str,
    run_id: Option<&str>,
) -> Result<Vec<LedgerRow>, ContractError> {
    let sql = if run_id.is_some() {
        "SELECT seq, path_bytes, before_revision, after_revision, action, artifact_id
         FROM v1_file_capture_ledger WHERE project_id = ?1 AND run_id = ?2 ORDER BY seq ASC"
    } else {
        "SELECT seq, path_bytes, before_revision, after_revision, action, artifact_id
         FROM v1_file_capture_ledger WHERE project_id = ?1 AND run_id IS NULL ORDER BY seq ASC"
    };
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<LedgerRow> {
        Ok(LedgerRow {
            seq: row.get(0)?,
            path: row.get(1)?,
            before_revision: row.get(2)?,
            after_revision: row.get(3)?,
            action: row.get(4)?,
            artifact_id: row.get(5)?,
        })
    };
    let mut stmt = conn.prepare(sql).map_err(sql_err)?;
    let rows = if let Some(run) = run_id {
        stmt.query_map(params![project_id, run], map_row)
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?
    } else {
        stmt.query_map(params![project_id], map_row)
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?
    };
    Ok(rows)
}

/// 删除已消费的台账行（按 seq 精确删；同事务内与快照提交一起落）。
fn delete_capture_ledger(conn: &Connection, seqs: &[i64]) -> Result<(), ContractError> {
    let mut stmt = conn
        .prepare("DELETE FROM v1_file_capture_ledger WHERE seq = ?1")
        .map_err(sql_err)?;
    for seq in seqs {
        stmt.execute(params![seq]).map_err(sql_err)?;
    }
    Ok(())
}

/// 按 `path_bytes` 折叠台账行为捕获条目（dsh r3 P1#4）：同一窗口内同一路
/// 径可留多行（run 内同文件两次变更，如 write a.txt → move a.txt b.txt 留
/// create+delete 两行），`v1_file_snapshot_entries` 的主键
/// `(snapshot_id, path_bytes)` 会整条事务回滚——快照丢失、台账行滞留。
/// 折叠规则（依据模块文档的方向语义定死）：
///
/// - `before_revision` := 首条的（窗口首次触碰**前**的状态——before 方向
///   的目标态）；
/// - `after_revision` / `artifact_id` := 末条的（after 方向「按时间序取
///   最后一条」的目标态；delete → None/None）；
/// - `action`：
///   - 首条 create 且末条 delete（先建后删的网络零窗）→ `"create"`（after
///     缺席）：before 方向按 create 语义回退到缺失，after 方向按无产物条
///     目标缺失——双向都是「目标缺失」，不伪装内容可恢复，也不回溯到更
///     早快照的陈旧内容；
///   - 末条 delete（首条 replace/delete）→ `"delete"`；
///   - 首条 create（末条 create/replace）→ `"create"`；
///   - 其余（先有后有，含 delete→create 的删后重建）→ `"replace"`（before
///     方向回溯更早内容产物）。
///
/// 行必须按 seq 升序传入（`load_capture_ledger` 保证）。
fn fold_ledger_rows(rows: &[LedgerRow]) -> Result<Vec<CapturedFileChange>, ContractError> {
    let mut folded: Vec<CapturedFileChange> = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(existing) = folded.iter_mut().find(|c| c.path == row.path) {
            // 同路径后续行：before 保持首条的，after/artifact 取末条的；
            // action 由「首条是否 create」与「末条是否 delete」合成——
            // 已折叠条目的 action == "create" 当且仅当首条是 create（上面
            // 的规则表恰好保持这一不变量）。
            let first_is_create = existing.action == "create";
            let last_action = ledger_action(&row.action)?;
            existing.after_revision = row.after_revision.clone();
            existing.artifact_id = row.artifact_id.clone();
            existing.action = if first_is_create && last_action == "delete" {
                "create"
            } else if last_action == "delete" {
                "delete"
            } else if first_is_create {
                "create"
            } else {
                "replace"
            };
        } else {
            folded.push(row.to_change()?);
        }
    }
    Ok(folded)
}

/// A6b（snap-wire §5）：把项目当前全部无 run 归属的台账行聚合为一条
/// `run_id=NULL` 的快照并删除已消费行。由 files 域在变更操作结束时于
/// 同一事务内调用（稳态下待聚合行恰为本操作的条目；崩溃残留的孤儿无归
/// 属行也会在此被收编；行先按 [`fold_ledger_rows`] 折叠——P1#4 的主键冲
/// 销对无归属聚合面同样成立）。返回 snapshotId（无行可聚合 → None）。
pub(crate) fn capture_unowned_entries(
    store: &V1Store,
    conn: &Connection,
    project_id: &str,
) -> Result<Option<String>, ContractError> {
    let rows = load_capture_ledger(conn, project_id, None)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let seqs: Vec<i64> = rows.iter().map(|row| row.seq).collect();
    let changes = fold_ledger_rows(&rows)?;
    let revision = snapshot_revision_of(&changes)?;
    let snapshot_id = store.next_id();
    insert_turn_snapshot(
        conn,
        &snapshot_id,
        project_id,
        None,
        None,
        &revision,
        &changes,
    )?;
    delete_capture_ledger(conn, &seqs)?;
    Ok(Some(snapshot_id))
}

/// A2（snap-wire §5）：run 终态聚合提交——取 ledger 中
/// `project_id = run.project_id AND run_id = run.run_id` 的未消费条目，
/// **先按 [`fold_ledger_rows`] 折叠**（dsh r3 P1#4：一个 run 在同一路径留
/// 两行——run 内同文件两次变更——会撞 `v1_file_snapshot_entries` 的主键
/// `(snapshot_id, path_bytes)`，整条边界事务回滚使 run 快照整条消失、台账
/// 行永久滞留），聚合为一条 run 归属快照（turn_id 定死传 run_id：v1 无独立
/// turn 概念，与 `drive_owned_turn` 以 runId 为 turn 身份一致）并删除已消
/// 费行（同事务）。空台账直接返回（`commit_turn_snapshot` 对空 changes 报
/// 错，调用侧先判空）。
pub(crate) fn capture_run_boundary(store: &V1Store, run: &StoredRun) -> Result<(), ContractError> {
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;
    let rows = load_capture_ledger(&tx, &run.project_id, Some(&run.run_id))?;
    if rows.is_empty() {
        return Ok(());
    }
    let seqs: Vec<i64> = rows.iter().map(|row| row.seq).collect();
    let changes = fold_ledger_rows(&rows)?;
    let revision = snapshot_revision_of(&changes)?;
    let snapshot_id = store.next_id();
    insert_turn_snapshot(
        &tx,
        &snapshot_id,
        &run.project_id,
        Some(&run.run_id),
        Some(&run.run_id),
        &revision,
        &changes,
    )?;
    delete_capture_ledger(&tx, &seqs)?;
    tx.commit().map_err(sql_err)?;
    Ok(())
}

/// A2 失败语义（关键）：捕获失败**不得翻转已终态的 run**（终态吸收；基
/// 线 post_agent 失败仅 log）。做法：`run.snapshot_capture_failed` 进度
/// 事件（run.events 的 Event.type 是自由字符串）+ 保留台账行。run 终
/// 态、CLI 退出码、恢复面均不受影响。
pub(crate) fn capture_run_boundary_logged(store: &V1Store, run: &StoredRun) {
    if let Err(error) = capture_run_boundary(store, run) {
        let _ = store.append_run_progress_event(
            &run.run_id,
            &run.session_id,
            "run.snapshot_capture_failed",
            &json!({
                "runId": run.run_id,
                "error": error.message,
            }),
        );
    }
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
                .map(str::to_owned)
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
    // 重放闸门（memory.rs:2257 同款）：deduplicated 回执直接返回，不再启动
    // 执行体——重放不得再执行副作用，也不得覆写原 operation 的完成结果。
    if receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, receipt));
    }
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
///
/// conn 锁分两段持有：`resolve_workspace` 内部会再取 store.conn（sqlite
/// mutex 非重入），持锁跨调用曾把成功 restore 卡死在第二段；消费仍由
/// `state = 'open'` 守卫原子化，竞态下的输家与「已消费」同样收 NOT_FOUND。
fn validate_and_consume_restore_plan(
    store: &V1Store,
    plan_id: &str,
    expected_plan_revision: &str,
) -> Result<(), ContractError> {
    let (project_id, targets) = {
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
        let project_id = plan.payload["projectId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let targets = plan
            .payload
            .get("targets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        (project_id, targets)
    };
    // 文件现状复验：计划生成后文件改动导致冲突。
    let workspace = resolve_workspace(store, &project_id)?;
    for target in &targets {
        let path = decode_byte_path(&target["path"])?;
        let current = stat_current(&workspace, &path)?;
        let recorded = target["currentRevision"].as_str().unwrap_or("absent");
        let observed = current.as_deref().unwrap_or("absent");
        if recorded != observed {
            return Err(revision_conflict(recorded, observed));
        }
    }
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        let consumed = conn
            .execute(
                "UPDATE v1_memory_change_plans SET state = 'consumed' WHERE plan_id = ?1 AND state = 'open'",
                params![plan_id],
            )
            .map_err(sql_err)?;
        if consumed == 0 {
            return Err(domain_not_found("change plan", plan_id));
        }
    }
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
///
/// dsh r3 P1#2：提交段（门内重验 + 恢复快照捕获 + 暂存 + rename/删除 +
/// 回滚 + 事件）整体在全部目标路径的**写门**内执行——门按字典序一次性
/// 取齐（与 files.rs move 同构，不持一门等另一门）。受理复验（第二跳）
/// 与提交段之间的暂存窗口（可含大 artifact 复制）里并发 files.write 落下
/// 的改动会在门内重验时被识别（该路径零覆盖），不再被静默覆盖且缺席恢
/// 复快照。恢复快照捕获的也是门内当前态，恢复资料与实际被覆盖的字节一
/// 致。
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

    let gate_keys: Vec<PathBuf> = targets
        .iter()
        .map(|target| restore_gate_key(&workspace, &target.path))
        .collect::<Result<_, _>>()?;
    with_write_gates(&gate_keys, || {
        // 0) 门内重验（P1#2）：计划受理后文件再改动（并发 files.write 落在
        //    受理复验与提交段之间）→ 该路径零覆盖——不触碰、进 failedPaths，
        //    现行 partial/rolledBack 语义收口。非冲突路径照常恢复。
        let mut conflicted: Vec<Vec<u8>> = Vec::new();
        for target in &targets {
            let current = stat_current(&workspace, &target.path)?;
            if current != target.current {
                conflicted.push(target.path.clone());
            }
        }
        let is_live = |path: &[u8]| !conflicted.iter().any(|p| p == path);

        // 1) 恢复快照：当前态产物化（缺失路径不记内容条目——恢复「缺失」可
        //    由原 plan 重放获得；存在路径的内容是恢复资料）。P2b（dsh r3）：
        //    pinned NOFOLLOW 句柄 + 流式发布（与 files.rs 的 recovery 备份同
        //    款），不再 std::fs::read 整读进内存。
        let mut recovery_changes: Vec<CapturedFileChange> = Vec::new();
        for target in targets.iter().filter(|t| is_live(&t.path)) {
            let current = stat_current(&workspace, &target.path)?;
            if let Some(revision) = &current {
                let canonical =
                    leaf_identity_path(&workspace, &request_target(&workspace, &target.path)?)?;
                let mut original = open_file_canonical(&workspace, &canonical)?;
                let artifact = crate::public_v1::domains::artifacts::publish_artifact_from_reader(
                    store,
                    &project_id,
                    "application/octet-stream",
                    Some(&format!("snapshot-recovery:{}", path_display(&target.path))),
                    &mut original,
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
        for target in targets.iter().filter(|t| is_live(&t.path)) {
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
            if let Err(_error) = write {
                let _ = std::fs::remove_file(&temp);
                staged_failed.push(target.path.clone());
                continue;
            }
            staged.push(Staged {
                path: target.path.clone(),
                temp,
            });
        }

        // 3) 提交：rename（create/replace）+ delete；逐项报告失败。冲突路
        //    径零触碰（P1#2 的零覆盖承诺）。
        let mut restored: Vec<Vec<u8>> = Vec::new();
        let mut failed: Vec<Vec<u8>> = staged_failed;
        failed.extend(conflicted.iter().cloned());
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
            if target.artifact_id.is_none() && target.current.is_some() && is_live(&target.path) {
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
    })
}

/// restore 提交段的写门 key（P1#2）：目标叶路径的 canonical 身份。父目录
/// 存在时与 files.rs write/move/delete 的门 key 同源（canonical 父 + 叶，
/// [`leaf_identity_path`]）；父目录缺失（整棵父树被递归删除后的深路径重
/// 建）时自叶向上取最深存在祖先、canonical 化后拼回余下组件——该形态下
/// 同叶的 files 域写操作本就到不了门获取（父缺失即 NOT_FOUND），互斥面由
/// 门内重验兜底。
fn restore_gate_key(workspace: &ProjectWorkspace, path: &[u8]) -> Result<PathBuf, ContractError> {
    let target = request_target(workspace, path)?;
    match leaf_identity_path(workspace, &target) {
        Ok(key) => Ok(key),
        Err(err) if err.code == CODE_NOT_FOUND => {
            let mut suffix: Vec<std::ffi::OsString> = Vec::new();
            let mut probe = target;
            loop {
                let Some(parent) = probe.parent() else {
                    return Ok(probe);
                };
                let parent = parent.to_path_buf();
                match std::fs::canonicalize(&parent) {
                    Ok(canon) => {
                        require_within(&workspace.workspace, &canon)?;
                        // 自深向浅收集的组件按反序拼回（ws/a/b/c.txt 在 b 缺
                        // 失时：suffix=[c.txt]，key=canon(ws/a)+b+c.txt）。
                        let mut key = canon;
                        if let Some(leaf) = probe.file_name() {
                            key = key.join(leaf);
                        }
                        for component in suffix.iter().rev() {
                            key = key.join(component);
                        }
                        return Ok(key);
                    }
                    Err(_) => {
                        if let Some(leaf) = probe.file_name() {
                            suffix.push(leaf.to_owned());
                        }
                        probe = parent;
                    }
                }
            }
        }
        Err(err) => Err(err),
    }
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
    use crate::public_v1::domains::artifacts::publish_artifact;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::contract::RunState;
    use agentcpm_pd_protocol::registry::OperationRegistry;

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn b64_decode(text: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .expect("base64")
    }

    /// 直插一条终态 StoredRun（A2 边界聚合测试用；字段齐备即可，聚合只
    /// 消费 project_id/run_id/session_id）。
    fn stored_run(project_id: &str, run_id: &str) -> StoredRun {
        StoredRun {
            run_id: run_id.to_owned(),
            session_id: format!("sess-{run_id}"),
            project_id: project_id.to_owned(),
            state: RunState::Succeeded,
            revision: "runrev-1".to_owned(),
            cancel_requested: false,
            waiting_for: Vec::new(),
            effect_status: "applied".to_owned(),
            config_revision: "cfg-1".to_owned(),
            effective_generation: "gen-1".to_owned(),
            final_cursor: None,
            continued_from: None,
            outcome_code: Some("SUCCEEDED".to_owned()),
            result: None,
            input: json!({}),
            instance_id: String::new(),
            terminal: true,
            queue_paused: false,
        }
    }

    fn write_input(project_id: &str, relative: &str, bytes: &[u8], precondition: Value) -> Value {
        json!({
            "path": {
                "projectId": project_id,
                "relativePath": {"encoding": "utf8", "value": relative},
            },
            "content": {"bytesBase64": b64(bytes)},
            "precondition": precondition,
        })
    }

    fn ledger_count(rt: &Arc<PublicV1Runtime>, run_id: Option<&str>) -> i64 {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        match run_id {
            Some(run) => conn
                .query_row(
                    "SELECT COUNT(*) FROM v1_file_capture_ledger WHERE run_id = ?1",
                    params![run],
                    |r| r.get(0),
                )
                .unwrap(),
            None => conn
                .query_row(
                    "SELECT COUNT(*) FROM v1_file_capture_ledger WHERE run_id IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
        }
    }

    async fn listed_snapshots(rt: &Arc<PublicV1Runtime>, project_id: &str) -> Vec<Value> {
        dispatch_ok(rt, "snapshots.list", json!({"projectId": project_id})).await["snapshots"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

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
            // 元组携带的是借用的 &str；映射为静态字面量以入 CapturedFileChange。
            let action: &'static str = match *action {
                "create" => "create",
                "replace" => "replace",
                "delete" => "delete",
                other => panic!("unsupported seed action `{other}`"),
            };
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
                before_revision: Some((*before).to_owned()),
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

    // -----------------------------------------------------------------------
    // A1/A6b：files 域三写点登记 + CLI 直写即时快照（snap-wire §5）
    // -----------------------------------------------------------------------

    /// A6b：无 run 归属的写一写一快照（runId/turnId 缺席）、runId 过滤精确
    /// 为空、after 内容产物化（inline 与 artifact 两来源）、台账行同事务消
    /// 费无残留。
    #[tokio::test]
    async fn direct_writes_leave_immediate_run_less_snapshots() {
        let (_tmp, rt, _workspace, project_id) = setup("snap-a6b");

        // inline create：a.txt → 一条 run_id=NULL 快照。
        let written = dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "a.txt",
                b"alpha-one",
                json!({"state": "absent"}),
            ),
        )
        .await;
        let a_rev1 = written["entry"]["revision"].as_str().unwrap().to_owned();
        assert!(!a_rev1.is_empty());

        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(
            snapshots.len(),
            1,
            "one direct write = one run-less snapshot"
        );
        assert!(snapshots[0]["runId"].is_null());
        assert!(snapshots[0]["turnId"].is_null());
        let entries = snapshots[0]["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["action"], json!("create"));
        assert!(
            entries[0]["beforeRevision"].is_null(),
            "create has no before"
        );
        assert_eq!(entries[0]["afterRevision"], json!(a_rev1));
        let a_artifact = entries[0]["artifactId"].as_str().unwrap().to_owned();
        assert!(!a_artifact.is_empty(), "after content must be productized");

        // 条目 artifactId 语义 = 动作完成后的内容（可读回）。
        let read = dispatch_ok(
            &rt,
            "artifacts.read",
            json!({"artifactId": a_artifact, "maxBytes": 1024}),
        )
        .await;
        assert_eq!(
            b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
            b"alpha-one"
        );

        // runId 过滤 → 精确为空（合同 acceptance 双向满足）。
        let filtered = dispatch_ok(
            &rt,
            "snapshots.list",
            json!({"projectId": project_id, "runId": "run-never-existed"}),
        )
        .await;
        assert_eq!(filtered["snapshots"].as_array().unwrap().len(), 0);

        // artifact 输入的 write：条目 artifactId 复用输入 artifactId（不重复
        // 发布）。
        let artifact = publish_artifact(
            rt.store(),
            &project_id,
            "text/plain",
            Some("src.txt"),
            b"from-artifact",
            "upload",
        )
        .unwrap();
        let src_artifact = artifact["artifactId"].as_str().unwrap().to_owned();
        dispatch_ok(
            &rt,
            "files.write",
            json!({
                "path": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "src.txt"},
                },
                "content": {"artifactId": src_artifact},
                "precondition": {"state": "absent"},
            }),
        )
        .await;
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 2);
        let src_snapshot = snapshots
            .iter()
            .find(|s| {
                let entries = s["entries"].as_array().unwrap();
                entries.len() == 1
                    && entries[0]["path"]["relativePath"]["value"] == json!("src.txt")
            })
            .expect("the artifact-input write is captured");
        let entry = &src_snapshot["entries"][0];
        assert_eq!(entry["artifactId"], json!(src_artifact));

        // 台账行同事务消费，无残留。
        assert_eq!(ledger_count(&rt, None), 0, "unowned rows are consumed");
        assert_eq!(ledger_count(&rt, Some("run-never-existed")), 0);
    }

    /// A1 move：src delete + dst create 双条目落进**同一条**快照；src 的
    /// before_revision 是移动前的源 revision，dst 的 after 内容产物可读回。
    #[tokio::test]
    async fn move_captures_two_entries_in_one_snapshot() {
        let (_tmp, rt, _workspace, project_id) = setup("snap-move");

        dispatch_ok(
            &rt,
            "files.write",
            write_input(&project_id, "b.txt", b"move-me", json!({"state": "absent"})),
        )
        .await;
        let stat = dispatch_ok(
            &rt,
            "files.stat",
            json!({
                "path": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "b.txt"},
                }
            }),
        )
        .await;
        let b_rev = stat["entry"]["revision"].as_str().unwrap().to_owned();

        dispatch_ok(
            &rt,
            "files.move",
            json!({
                "source": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "b.txt"},
                },
                "destination": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "renamed.txt"},
                },
                "expectedSourceRevision": b_rev,
                "destinationPrecondition": {"state": "absent"},
            }),
        )
        .await;

        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 2, "create + move = two snapshots");
        let moved = &snapshots[1];
        assert!(moved["runId"].is_null());
        let entries = moved["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 2, "move captures src+dst in one snapshot");
        let by_action = |action: &str| {
            entries
                .iter()
                .find(|e| e["action"] == json!(action))
                .unwrap_or_else(|| panic!("missing `{action}` entry: {entries:?}"))
        };
        let src = by_action("delete");
        assert_eq!(src["path"]["relativePath"]["value"], json!("b.txt"));
        assert_eq!(src["beforeRevision"], json!(b_rev));
        assert!(src["afterRevision"].is_null());
        let dst = by_action("create");
        assert_eq!(dst["path"]["relativePath"]["value"], json!("renamed.txt"));
        assert_eq!(dst["afterRevision"], json!(b_rev), "content is unchanged");
        assert!(
            dst["artifactId"].as_str().is_some_and(|id| !id.is_empty()),
            "the moved content must be productized"
        );

        // moved 内容产物读回 = 原字节。
        let read = dispatch_ok(
            &rt,
            "artifacts.read",
            json!({"artifactId": dst["artifactId"], "maxBytes": 1024}),
        )
        .await;
        assert_eq!(
            b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
            b"move-me"
        );

        assert_eq!(ledger_count(&rt, None), 0);
    }

    /// A6b + §A4 一致性自检：t1 create(a,A1) → t2 replace(a,A2) → t3
    /// delete(a)。t2 的 before_revision 指向 t1 的 after（回溯链）；t3 选
    /// before → 回溯最近非 delete = t2 的 after → 恢复 A2；选 [t1,t2] 最
    /// 早触碰 create → 目标缺失 = delete。
    #[tokio::test]
    async fn replace_before_chain_and_delete_restore_roundtrip() {
        let (_tmp, rt, workspace, project_id) = setup("snap-chain");

        // t1：create a.txt = A1。
        let t1 = dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "a.txt",
                b"alpha-one",
                json!({"state": "absent"}),
            ),
        )
        .await;
        let a_rev1 = t1["entry"]["revision"].as_str().unwrap().to_owned();

        // t2：replace a.txt = A2（precondition revision → before_revision 链）。
        let t2 = dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "a.txt",
                b"alpha-two",
                json!({"state": "revision", "revision": a_rev1}),
            ),
        )
        .await;
        let a_rev2 = t2["entry"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(a_rev1, a_rev2);

        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 2);
        let snap1 = snapshots[0]["snapshotId"].as_str().unwrap().to_owned();
        let snap2 = snapshots[1]["snapshotId"].as_str().unwrap().to_owned();
        let entry1 = &snapshots[0]["entries"][0];
        let entry2 = &snapshots[1]["entries"][0];
        assert_eq!(entry1["action"], json!("create"));
        assert_eq!(entry2["action"], json!("replace"));
        assert_eq!(
            entry2["beforeRevision"], entry1["afterRevision"],
            "replace's before points at the create's after revision"
        );
        let artifact1 = entry1["artifactId"].as_str().unwrap().to_owned();
        let artifact2 = entry2["artifactId"].as_str().unwrap().to_owned();
        assert_ne!(artifact1, artifact2, "A1 and A2 are distinct products");

        // t3：delete a.txt。
        dispatch_ok(
            &rt,
            "files.delete",
            json!({
                "path": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "a.txt"},
                },
                "expectedRevision": a_rev2,
            }),
        )
        .await;
        assert!(
            std::fs::read(workspace.join("a.txt")).is_err(),
            "t3 removed the file"
        );
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 3);
        let snap3 = snapshots[2]["snapshotId"].as_str().unwrap().to_owned();
        let entry3 = &snapshots[2]["entries"][0];
        assert_eq!(entry3["action"], json!("delete"));
        assert_eq!(entry3["beforeRevision"], json!(a_rev2));
        assert!(
            entry3["artifactId"].is_null(),
            "delete has no after content"
        );

        // 选 t3 做 before：回溯最近非 delete = t2 的 after（A2）。
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
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["action"], json!("create"), "file is absent now");
        assert_eq!(entries[0]["artifactId"], json!(artifact2));
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "chain-before-restore",
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::fs::read(workspace.join("a.txt")).ok().as_deref() == Some(&b"alpha-two"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "before-restore of the delete never applied"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 选 [t1, t2] 做 before：最早触碰是 create → 目标缺失 = delete。
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
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["action"], json!("delete"));
    }

    // -----------------------------------------------------------------------
    // A2：run 归属登记 + 边界聚合（turn_id=run_id 钉扎）
    // -----------------------------------------------------------------------

    /// 归属由调用方显式传入（dsh r3 P1#3）：`driver_write_file` 带 run_id
    /// 的写落 run 归属台账行（不即时成快照）；run 驱动期间的普通 CLI 直写
    /// 恒 None、走 A6b 即时快照——同项目并发 run / CLI 混写各归各的。run
    /// 终态后 [`capture_run_boundary`] 聚合为一条 run 快照（turnId 钉扎 =
    /// runId）、消费台账行；空台账的重复调用是 no-op；他 run 的边界不动别
    /// 人的行。
    #[tokio::test]
    async fn run_attribution_boundary_capture_pins_turn_id_to_run_id() {
        let (_tmp, rt, _workspace, project_id) = setup("snap-a2");
        let run_id = "run-boundary-1";

        // driver 归属写：行带 run_id，不即时成快照（A6b 不触碰 run 归属行）。
        crate::public_v1::domains::files::driver_write_file(
            rt.store(),
            run_id,
            &project_id,
            b"r1.txt",
            b"owned-one",
        )
        .unwrap();
        assert_eq!(ledger_count(&rt, Some(run_id)), 1);
        assert_eq!(
            listed_snapshots(&rt, &project_id).await.len(),
            0,
            "run-owned rows wait for the boundary"
        );
        // run 期间的普通 CLI 直写恒 None（P1#3：不再读进程内归属 map）——
        // 走 A6b 即时快照，不误归 run。
        dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "r2.txt",
                b"unowned",
                json!({"state": "absent"}),
            ),
        )
        .await;
        assert_eq!(listed_snapshots(&rt, &project_id).await.len(), 1);
        assert_eq!(ledger_count(&rt, Some(run_id)), 1, "the owned row stays");

        // 他 run 的边界聚合：空台账（该 run 无行）→ no-op，不动别人的行。
        capture_run_boundary(rt.store(), &stored_run(&project_id, "run-other")).unwrap();
        assert_eq!(ledger_count(&rt, Some(run_id)), 1);

        // 本 run 的边界聚合：一条快照，turnId 钉扎 = runId。
        let run = stored_run(&project_id, run_id);
        capture_run_boundary(rt.store(), &run).unwrap();
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 2);
        let owned = snapshots
            .iter()
            .find(|s| s["runId"] == json!(run_id))
            .expect("the run-owned snapshot is visible");
        assert_eq!(
            owned["turnId"],
            json!(run_id),
            "turn identity is pinned to the run id (v1 has no separate turn)"
        );
        let entries = owned["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["path"]["relativePath"]["value"], json!("r1.txt"));
        assert_eq!(entries[0]["action"], json!("create"));

        // 台账消费 + runId 过滤可见 + 重复调用 no-op。
        assert_eq!(ledger_count(&rt, Some(run_id)), 0);
        let filtered = dispatch_ok(
            &rt,
            "snapshots.list",
            json!({"projectId": project_id, "runId": run_id}),
        )
        .await;
        assert_eq!(filtered["snapshots"].as_array().unwrap().len(), 1);
        capture_run_boundary(rt.store(), &run).unwrap();
        assert_eq!(
            listed_snapshots(&rt, &project_id).await.len(),
            2,
            "an empty ledger re-capture is a no-op"
        );
    }

    /// A2 失败语义：聚合失败不翻转终态——`run.snapshot_capture_failed` 进
    /// 度事件 + 台账行保留（修好后可再消费），run 无终止态迁移事件。
    #[tokio::test]
    async fn capture_failure_is_absorbed_into_progress_event() {
        let (_tmp, rt, _workspace, project_id) = setup("snap-a2-fail");
        let run_id = "run-cap-fail";
        let run = stored_run(&project_id, run_id);

        // 注入坏 action 的台账行（模拟登记面损坏 → 聚合校验失败）。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO v1_file_capture_ledger
                    (project_id, run_id, path_bytes, before_revision, after_revision, action, artifact_id, occurred_at)
                 VALUES (?1, ?2, ?3, NULL, 'r-weird', 'weird', NULL, ?4)",
                params![project_id, run_id, b"bad.txt".to_vec(), now_rfc3339()],
            )
            .unwrap();
        }

        capture_run_boundary_logged(rt.store(), &run);

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            // 诊断事件落地，且 run 没有任何终止态迁移事件（不翻转终态）。
            let failed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'run.snapshot_capture_failed' AND resource_id = ?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(failed, 1);
            let terminal: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE resource_id = ?1 AND event_type IN ('run.completed', 'run.failed', 'run.cancelled')",
                    params![run_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(terminal, 0, "capture failure must not flip the run");
            // 台账行保留（不吞错：下次可再消费）。
            let retained: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM v1_file_capture_ledger WHERE run_id = ?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(retained, 1);
            // 无快照落库。
            let snapshots: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM v1_file_snapshots WHERE project_id = ?1",
                    params![project_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(snapshots, 0);
        }

        // 修好行后同一 run 可再消费（保留行的意义）。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE v1_file_capture_ledger SET action = 'delete' WHERE run_id = ?1",
                params![run_id],
            )
            .unwrap();
        }
        capture_run_boundary(rt.store(), &run).unwrap();
        assert_eq!(ledger_count(&rt, Some(run_id)), 0);
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0]["runId"], json!(run_id));
    }

    // -----------------------------------------------------------------------
    // dsh r3 P1：登记时序（写门内登记）/ 按路径折叠 / restore 提交段写门
    // -----------------------------------------------------------------------

    fn sha256_of(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn file_revision_of_bytes(bytes: &[u8]) -> String {
        file_revision_of(&sha256_of(bytes), bytes.len() as u64)
    }

    async fn stat_revision(rt: &Arc<PublicV1Runtime>, project_id: &str, path: &str) -> String {
        let value = dispatch_ok(
            rt,
            "files.stat",
            json!({
                "path": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": path},
                }
            }),
        )
        .await;
        value["entry"]["revision"].as_str().unwrap().to_owned()
    }

    /// P1#1 结构性断言：捕获登记（含 A6b 即时聚合）在写门临界区内——门被
    /// 测试持有时，写操作进不了门（零落盘零登记，登记不可能发生在门外的
    /// 更早点）；门放行、写完成后（dispatch 返回 = 门临界区整体结束），
    /// 台账已聚合成即时快照必然可见（登记不可能晚于门释放）。两个方向一
    /// 起钉住「登记 ∈ 门临界区」。
    #[tokio::test]
    async fn ledger_registration_lands_inside_the_write_gate() {
        let (_tmp, rt, workspace, project_id) = setup("snap-gate-inside");

        // 种子文件直接落盘（本测试只观察后续 replace 的登记时点）。
        std::fs::write(workspace.join("g.txt"), b"gate-seed").unwrap();
        let rev_seed = file_revision_of_bytes(b"gate-seed");

        // 测试持有 g.txt 的写门：派驻的写将卡在门获取上。
        let gate_path = workspace.join("g.txt");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = tokio::task::spawn_blocking(move || {
            with_write_gates(&[gate_path], || {
                let _ = entered_tx.send(());
                // 持门直到测试放行（阻塞线程里用 std channel 等待）。
                let _ = release_rx.recv();
                Ok(())
            })
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the test acquired the write gate");

        // files.write g.txt（replace seed→new）：在门上排队。
        let rt_for_write = rt.clone();
        let project_for_write = project_id.clone();
        let rev_seed_for_write = rev_seed.clone();
        let write_task = tokio::spawn(async move {
            rt_for_write
                .dispatch(
                    "files.write",
                    write_input(
                        &project_for_write,
                        "g.txt",
                        b"gate-new",
                        json!({"state": "revision", "revision": rev_seed_for_write}),
                    ),
                    None,
                    None,
                )
                .await
        });
        // 给写一点时间到达门获取（它必然卡住——门被测试持有）。
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            std::fs::read(workspace.join("g.txt")).unwrap(),
            b"gate-seed".to_vec(),
            "the write cannot pass the held gate"
        );
        assert!(
            listed_snapshots(&rt, &project_id).await.is_empty(),
            "no registration can happen outside the gate"
        );
        assert_eq!(ledger_count(&rt, None), 0);

        // 放行：写在门临界区内完成（落盘 + 登记 + A6b 聚合一次做完）。
        let _ = release_tx.send(());
        let (_, written) = write_task
            .await
            .expect("write task joined")
            .expect("files.write succeeded");
        let rev_new = written["entry"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev_seed, rev_new);

        // dispatch 返回（门已释放）时登记已可见：A6b 即时快照带着 replace
        // 条目（before=seed，after=new）。
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 1, "one immediate snapshot: {snapshots:?}");
        let entry = &snapshots[0]["entries"][0];
        assert_eq!(entry["action"], json!("replace"));
        assert_eq!(entry["beforeRevision"], json!(rev_seed));
        assert_eq!(entry["afterRevision"], json!(rev_new));
        assert_eq!(ledger_count(&rt, None), 0, "the ledger row is consumed");
        holder.await.expect("gate holder joined").unwrap();
    }

    /// P1#1 时序断言：同路径 replace→delete 因果链上，两个操作都成功时台账
    /// /快照序 == 门序（文件系统变更序）。delete 的门内前提（rev2）只能在
    /// replace 的整个门临界区（rename **与登记**）完成后满足，因此 replace
    /// 的即时快照必然先于 delete 的快照落库（修复前登记在门释放之后，顺
    /// 序可整体倒置——恢复面会把先发生的改动当成后发生的）。
    #[tokio::test]
    async fn same_path_replace_then_delete_keep_ledger_order() {
        let (_tmp, rt, workspace, project_id) = setup("snap-order");

        // 种子（直接落盘）：a.txt = one。
        std::fs::write(workspace.join("a.txt"), b"one").unwrap();
        let rev_one = file_revision_of_bytes(b"one");
        let rev_two = file_revision_of_bytes(b"two");

        // replace one→two（spawn 后不 await）……
        let rt_for_write = rt.clone();
        let project_for_write = project_id.clone();
        let rev_one_for_write = rev_one.clone();
        let replace_task = tokio::spawn(async move {
            rt_for_write
                .dispatch(
                    "files.write",
                    write_input(
                        &project_for_write,
                        "a.txt",
                        b"two",
                        json!({"state": "revision", "revision": rev_one_for_write}),
                    ),
                    None,
                    None,
                )
                .await
        });
        // ……等落盘可见（rename 已生效；delete 的门内前提从此可满足）……
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::fs::read(workspace.join("a.txt")).ok().as_deref() == Some(&b"two"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the replace never landed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // ……再发 delete（expectedRevision=rev_two：只能在 replace 的门临
        // 界区整体完成后通过门内复验）。
        dispatch_ok(
            &rt,
            "files.delete",
            json!({
                "path": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "a.txt"},
                },
                "expectedRevision": rev_two,
            }),
        )
        .await;
        let (_, _replaced) = replace_task
            .await
            .expect("replace task joined")
            .expect("files.write succeeded");

        // 两个操作的即时快照都在，且 replace 先于 delete（列表 created_at
        // ASC, snapshot_id ASC——门序即落库序）。
        assert!(
            std::fs::read(workspace.join("a.txt")).is_err(),
            "the delete removed the file"
        );
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(
            snapshots.len(),
            2,
            "one immediate snapshot per operation: {snapshots:?}"
        );
        let replace_entry = &snapshots[0]["entries"][0];
        assert_eq!(replace_entry["action"], json!("replace"));
        assert_eq!(replace_entry["beforeRevision"], json!(rev_one));
        assert_eq!(replace_entry["afterRevision"], json!(rev_two));
        let delete_entry = &snapshots[1]["entries"][0];
        assert_eq!(delete_entry["action"], json!("delete"));
        assert_eq!(
            delete_entry["beforeRevision"],
            json!(rev_two),
            "the delete saw the replace's after revision"
        );
        assert_eq!(ledger_count(&rt, None), 0);
    }

    /// P1#4：同 run 同路径多行折叠——write→write（create 后 replace）合成
    /// create（before=首条 None、after/产物=末条）；write→delete（create 后
    /// delete 的网络零窗）合成 create 且 after 缺席。折叠后 entries 主键
    /// `(snapshot_id, path_bytes)` 不再相撞，边界事务不回滚；恢复面两方向
    /// 对合成条目的目标都是「缺失」（不伪装内容可恢复，也不回溯陈旧内容）。
    #[tokio::test]
    async fn boundary_folds_same_path_rows_per_path() {
        let (_tmp, rt, _workspace, project_id) = setup("snap-fold");
        let run_id = "run-fold-1";

        // 真实产物 + 修订（after 方向断言要读回内容）。
        let a1 = b"fold-a-one".to_vec();
        let a2 = b"fold-a-two".to_vec();
        let b1 = b"fold-b-one".to_vec();
        let publish = |tag: &str, bytes: &[u8]| -> String {
            let artifact = publish_artifact(
                rt.store(),
                &project_id,
                "application/octet-stream",
                Some(&format!("fold:{tag}")),
                bytes,
                "snapshot-seed",
            )
            .unwrap();
            artifact["artifactId"].as_str().unwrap().to_owned()
        };
        let art_a1 = publish("a1", &a1);
        let art_a2 = publish("a2", &a2);
        let art_b1 = publish("b1", &b1);
        let rev_a1 = file_revision_of_bytes(&a1);
        let rev_a2 = file_revision_of_bytes(&a2);
        let rev_b1 = file_revision_of_bytes(&b1);

        // 直插 run 归属台账行（模拟 D20 真实 turn 机在 run 内的同路径多次
        // 变更——离线 driver 的 absent 前提产不出同路径第二行）：
        // a.txt：create → replace；b.txt：create → delete。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            // (path, before, after, action, artifact)
            type LedgerSeed<'a> = (
                &'a str,
                Option<&'a str>,
                Option<&'a str>,
                &'a str,
                Option<&'a str>,
            );
            let rows: [LedgerSeed; 4] = [
                ("a.txt", None, Some(&rev_a1), "create", Some(&art_a1)),
                (
                    "a.txt",
                    Some(&rev_a1),
                    Some(&rev_a2),
                    "replace",
                    Some(&art_a2),
                ),
                ("b.txt", None, Some(&rev_b1), "create", Some(&art_b1)),
                ("b.txt", Some(&rev_b1), None, "delete", None),
            ];
            for (path, before, after, action, artifact) in rows {
                conn.execute(
                    "INSERT INTO v1_file_capture_ledger
                        (project_id, run_id, path_bytes, before_revision, after_revision, action, artifact_id, occurred_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        project_id,
                        run_id,
                        path.as_bytes().to_vec(),
                        before,
                        after,
                        action,
                        artifact,
                        now_rfc3339(),
                    ],
                )
                .unwrap();
            }
        }

        // 折叠 + 聚合（修复前：PK 相撞 → 事务回滚 → 无快照、行滞留）。
        capture_run_boundary(rt.store(), &stored_run(&project_id, run_id)).unwrap();
        assert_eq!(ledger_count(&rt, Some(run_id)), 0, "rows are consumed");

        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 1, "one folded run snapshot: {snapshots:?}");
        let owned = &snapshots[0];
        assert_eq!(owned["runId"], json!(run_id));
        let entries = owned["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 2, "one entry per path: {entries:?}");
        let entry_of = |path: &str| {
            entries
                .iter()
                .find(|e| e["path"]["relativePath"]["value"] == json!(path))
                .unwrap_or_else(|| panic!("missing `{path}` entry: {entries:?}"))
                .clone()
        };
        // write→write：合成 create——before=首条（None），after/产物=末条。
        let a = entry_of("a.txt");
        assert_eq!(a["action"], json!("create"));
        assert!(a["beforeRevision"].is_null());
        assert_eq!(a["afterRevision"], json!(rev_a2));
        assert_eq!(a["artifactId"], json!(art_a2));
        // write→delete（网络零窗）：合成 create、after 缺席。
        let b = entry_of("b.txt");
        assert_eq!(b["action"], json!("create"));
        assert!(b["beforeRevision"].is_null());
        assert!(b["afterRevision"].is_null());
        assert!(b["artifactId"].is_null());

        // 恢复面语义抽查：after 方向只有 a.txt 可恢复（b.txt 目标缺失 =
        // 当前缺失 → no-op）；before 方向两条都是 create 语义（目标缺失），
        // 无 SNAPSHOT_UNAVAILABLE 冲突（若折叠成 delete/replace 会回溯失
        // 败报冲突——正是要排除的形态）。
        let snapshot_id = owned["snapshotId"].as_str().unwrap().to_owned();
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snapshot_id],
                "direction": "after",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1, "only a.txt is recoverable: {entries:?}");
        assert_eq!(entries[0]["path"]["relativePath"]["value"], json!("a.txt"));
        assert_eq!(entries[0]["action"], json!("create"));
        assert_eq!(entries[0]["artifactId"], json!(art_a2));
        let read = dispatch_ok(
            &rt,
            "artifacts.read",
            json!({"artifactId": art_a2, "maxBytes": 1024}),
        )
        .await;
        assert_eq!(
            b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
            a2
        );
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snapshot_id],
                "direction": "before",
            }),
        )
        .await;
        assert!(
            plan["entries"].as_array().unwrap().is_empty(),
            "both folded entries rewind to absent: {plan:?}"
        );
        assert!(
            plan["plan"]["conflicts"].as_array().unwrap().is_empty(),
            "folded create entries must not surface SNAPSHOT_UNAVAILABLE: {plan:?}"
        );
    }

    /// P2a：move 覆盖已存在的 dest → 捕获条目 action=replace 且
    /// before=旧 revision；dest 原字节在 rename 前发布为 recovery 产物
    /// （内部安全网；`files.move` 合同结果只有 entry+previousPath，不暴露
    /// recoveryArtifactId——write/delete 才有该字段）。
    #[tokio::test]
    async fn move_over_existing_dest_captures_replace_entry() {
        let (_tmp, rt, _workspace, project_id) = setup("snap-move-replace");

        // victim.txt（将被覆盖的旧内容）+ mover.txt（移过去）。
        dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "victim.txt",
                b"victim-bytes",
                json!({"state": "absent"}),
            ),
        )
        .await;
        dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "mover.txt",
                b"mover-bytes",
                json!({"state": "absent"}),
            ),
        )
        .await;
        // 等两个即时快照落库后再取修订（stat 与登记无关，但让列表序确定）。
        let rev_victim = stat_revision(&rt, &project_id, "victim.txt").await;
        let rev_mover = stat_revision(&rt, &project_id, "mover.txt").await;
        assert_eq!(listed_snapshots(&rt, &project_id).await.len(), 2);

        // move mover.txt → victim.txt（destPrecondition revision=rev_victim）。
        let moved = dispatch_ok(
            &rt,
            "files.move",
            json!({
                "source": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "mover.txt"},
                },
                "destination": {
                    "projectId": project_id,
                    "relativePath": {"encoding": "utf8", "value": "victim.txt"},
                },
                "expectedSourceRevision": rev_mover,
                "destinationPrecondition": {"state": "revision", "revision": rev_victim},
            }),
        )
        .await;

        // 合同结果不带 recoveryArtifactId（schema additionalProperties:
        // false——该字段属 write/delete 的结果合同，move 没有）。
        assert!(
            moved.get("recoveryArtifactId").is_none(),
            "files.move result schema is entry+previousPath: {moved}"
        );

        // 捕获快照：src delete（before=rev_mover）+ dst **replace**
        // （before=rev_victim，after=rev_mover），after 产物 = 移动后的字节。
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 3, "two creates + one move");
        let entries = snapshots[2]["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 2, "src + dst entries: {entries:?}");
        let src = entries
            .iter()
            .find(|e| e["action"] == json!("delete"))
            .expect("the src delete entry");
        assert_eq!(src["path"]["relativePath"]["value"], json!("mover.txt"));
        assert_eq!(src["beforeRevision"], json!(rev_mover));
        let dst = entries
            .iter()
            .find(|e| e["action"] == json!("replace"))
            .unwrap_or_else(|| panic!("the dst entry is replace (not create): {entries:?}"));
        assert_eq!(dst["path"]["relativePath"]["value"], json!("victim.txt"));
        assert_eq!(
            dst["beforeRevision"],
            json!(rev_victim),
            "replace carries the dest's pre-move revision"
        );
        assert_eq!(dst["afterRevision"], json!(rev_mover));
        let after_artifact = dst["artifactId"].as_str().unwrap().to_owned();
        let read = dispatch_ok(
            &rt,
            "artifacts.read",
            json!({"artifactId": after_artifact, "maxBytes": 1024}),
        )
        .await;
        assert_eq!(
            b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
            b"mover-bytes"
        );
    }

    /// P1#2：restore 提交段在写门内重验——受理复验（第二跳）与提交段之间
    /// 落下的文件改动被识别为该路径零覆盖（failed、不覆盖、无恢复快照）。
    /// 确定性构造：测试持有目标路径的写门，restore 的提交段卡在门获取；
    /// 期间直接改文件（模拟窗口内的并发写）；放行后门内重验发现失配。
    #[tokio::test]
    async fn restore_commit_window_conflict_is_zero_coverage() {
        let (_tmp, rt, workspace, project_id) = setup("snap-restore-gate");

        // 当前态 v1；播种快照记录目标内容 v2 → after 方向计划是 replace。
        std::fs::write(workspace.join("a.txt"), b"current-v1").unwrap();
        let snap = seed_snapshot(
            &rt,
            &project_id,
            "restore-win",
            &[("a.txt", b"target-v2", "create")],
            &[],
        );
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [snap],
                "direction": "after",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["action"], json!("replace"));
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        // 测试持有 a.txt 的写门：restore 的提交段将卡在门获取上。
        let gate_path = workspace.join("a.txt");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = tokio::task::spawn_blocking(move || {
            with_write_gates(&[gate_path], || {
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
                Ok(())
            })
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the test acquired the write gate");

        // 提交 restore：受理复验（第二跳）在门外的受理线程完成（此刻文件
        // 仍是 v1，复验通过、plan 被消费）；执行体卡在门获取上。
        let receipt = dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "restore-gate-window",
        )
        .await;
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        // 窗口内（受理复验后、提交段前）：文件被改动（模拟并发写——直写
        // 绕过写门，正如窗口内真正的 files.write 拿得到门一样）。
        std::fs::write(workspace.join("a.txt"), b"raced-r2").unwrap();

        // 放行：提交段进门 → 门内重验发现 current=r2 ≠ plan 记录 v1 →
        // 该路径零覆盖。
        let _ = release_tx.send(());
        holder.await.expect("gate holder joined").unwrap();

        // 等 domain operation 终态（域操作表在 <home>/v1/state.sqlite——与核
        // 心库分离的共享库，WAL 允许并发读者）。
        let state_db = Connection::open(rt.store().home().join("v1/state.sqlite")).unwrap();
        state_db
            .execute_batch("PRAGMA busy_timeout = 2000;")
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let completion = loop {
            let (state, completion) = state_db
                .query_row(
                    "SELECT state, completion_json FROM v1_domain_operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap();
            if state == "succeeded" || state == "failed" {
                break serde_json::from_str::<Value>(&completion.expect("terminal completion"))
                    .unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the restore operation never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        // 零覆盖：文件仍是窗口内的改动；该路径进 failedPaths；无恢复快照
        // （没有任何路径被触碰——恢复资料不缺页也不多页）。
        assert_eq!(
            std::fs::read(workspace.join("a.txt")).unwrap(),
            b"raced-r2".to_vec(),
            "the raced write must not be silently overwritten"
        );
        assert_eq!(completion["outcome"], json!("partial"));
        let failed_paths: Vec<String> = completion["failedPaths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                p["relativePath"]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(failed_paths, vec!["a.txt".to_owned()]);
        assert_eq!(
            completion["restoredPaths"].as_array().unwrap().len(),
            0,
            "zero coverage for the conflicted path: {completion:?}"
        );
        assert!(
            completion["recoverySnapshotId"].is_null(),
            "no path was touched, so no recovery snapshot: {completion:?}"
        );
        assert_eq!(
            listed_snapshots(&rt, &project_id).await.len(),
            1,
            "only the seeded snapshot exists"
        );
    }

    // -----------------------------------------------------------------------
    // A5（snap-wire §5）：driver 写指令 —— run 写副作用 → run 归属快照
    // -----------------------------------------------------------------------

    /// 提交一条 run 并等终态（`run.submit` 的 start_immediately 自带
    /// worker：driver（写指令显式带 run_id 归属）→ 终态 → 边界聚合全链
    /// 路），返回 (run_id, 最终 run JSON)。
    async fn drive_offline_run(
        rt: &Arc<PublicV1Runtime>,
        session_id: &str,
        text: &str,
        key: &str,
    ) -> (String, Value) {
        let iid = rt.store().meta().instance_id.clone();
        let (_, submitted) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": text}],
                }),
                Some(key),
                None,
            )
            .await
            .expect("run.submit");
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
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
            if got["run"]["terminal"] == true {
                return (run_id, got["run"].clone());
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run `{run_id}` never became terminal"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// A5：driver 写指令的生产链路——`[pd-driver:write=<relpath>@<字面量>]`
    /// 经 files 域同一写路径落盘（worker 驱动期已把写归属设为本 run），
    /// run 终态后边界聚合为一条 run 归属快照（runId=turnId=run id）并消
    /// 费台账行；条目 action=create、after 产物字节=写的字面量。
    #[tokio::test]
    async fn driver_write_directive_lands_run_owned_snapshot() {
        let (_tmp, rt, workspace, project_id) = setup("snap-a5-driver");
        // driver 写不隐式建父目录（与 files.write 同语义）：先建 notes/。
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        dispatch_ok(
            &rt,
            "session.create",
            json!({"instanceId": rt.store().meta().instance_id, "sessionId": "sess-a5", "projectId": project_id}),
        )
        .await;

        let text = concat!(
            "write the run-owned tree\n",
            "[pd-driver:write=notes/a.txt@alpha-literal]\n",
            "[pd-driver:write=top.txt@beta-literal]"
        );
        let (run_id, run) = drive_offline_run(&rt, "sess-a5", text, "a5-driver-run").await;
        assert_eq!(run["state"], json!("succeeded"), "the write run: {run}");

        // 落盘：字面量原样（内容到 `]` 前原样保留，不做 trim）。
        assert_eq!(
            std::fs::read(workspace.join("notes/a.txt")).unwrap(),
            b"alpha-literal".to_vec()
        );
        assert_eq!(
            std::fs::read(workspace.join("top.txt")).unwrap(),
            b"beta-literal".to_vec()
        );

        // 边界聚合在终态迁移之后落库：轮询 run 归属快照出现。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let snapshots = loop {
            let snaps = listed_snapshots(&rt, &project_id).await;
            let owned: Vec<Value> = snaps
                .iter()
                .filter(|s| s["runId"] == json!(run_id))
                .cloned()
                .collect();
            if !owned.is_empty() {
                assert_eq!(
                    owned.len(),
                    1,
                    "exactly one run-owned snapshot for the two writes: {snaps:?}"
                );
                break snaps;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the run-owned snapshot never appeared"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };
        let snapshot = snapshots
            .iter()
            .find(|s| s["runId"] == json!(run_id))
            .unwrap();
        assert_eq!(
            snapshot["turnId"],
            json!(run_id),
            "turn identity is the run id (v1 has no separate turn)"
        );

        // 台账已被聚合消费（快照提交与行删除同事务）。
        assert_eq!(ledger_count(&rt, Some(&run_id)), 0);
        assert_eq!(ledger_count(&rt, None), 0);

        // 条目：两条 create，after 产物字节=写的字面量。
        let entries = snapshot["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 2);
        for (path, bytes) in [
            ("notes/a.txt", b"alpha-literal".as_slice()),
            ("top.txt", b"beta-literal".as_slice()),
        ] {
            let entry = entries
                .iter()
                .find(|e| e["path"]["relativePath"]["value"] == json!(path))
                .unwrap_or_else(|| panic!("missing entry for `{path}`: {entries:?}"));
            assert_eq!(entry["action"], json!("create"));
            assert!(entry["beforeRevision"].is_null());
            let artifact_id = entry["artifactId"].as_str().unwrap_or_default().to_owned();
            assert!(
                !artifact_id.is_empty(),
                "the after content must be productized: {entry}"
            );
            let read = dispatch_ok(
                &rt,
                "artifacts.read",
                json!({"artifactId": artifact_id, "maxBytes": 1024}),
            )
            .await;
            assert_eq!(
                b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
                bytes,
                "`{path}` after-artifact bytes must equal the written literal"
            );
        }
    }

    /// A5 失败语义：写失败（父目录缺失 → files 域 NOT_FOUND）与畸形指令
    /// （无 `@` 分隔）都把 run 失败收场（`FILE_WRITE_FAILED`）——不静默
    /// 回退成成功兜底，也不留 run 归属快照。
    #[tokio::test]
    async fn driver_write_directive_failures_fail_the_run() {
        let (_tmp, rt, workspace, project_id) = setup("snap-a5-fail");
        // 失败 run 会暂停所在会话的队列（run_failed），两个失败面各用独立会话。
        for (index, (session, text)) in [
            (
                "sess-a5-f1",
                "[pd-driver:write=missing-parent/a.txt@content]",
            ),
            ("sess-a5-f2", "[pd-driver:write=no-at-separator]"),
        ]
        .into_iter()
        .enumerate()
        {
            dispatch_ok(
                &rt,
                "session.create",
                json!({"instanceId": rt.store().meta().instance_id, "sessionId": session, "projectId": project_id}),
            )
            .await;
            let (run_id, run) =
                drive_offline_run(&rt, session, text, &format!("a5-fail-{index}")).await;
            assert_eq!(
                run["state"],
                json!("failed"),
                "the failed/malformed write run must fail loudly: {run}"
            );
            assert_eq!(run["outcomeCode"], json!("FILE_WRITE_FAILED"));
            // 零落盘 + 无 run 归属快照（写失败先于台账登记）。
            assert!(!workspace.join("missing-parent").exists());
            let snapshots = listed_snapshots(&rt, &project_id).await;
            assert!(
                snapshots.iter().all(|s| s["runId"] != json!(run_id)),
                "a failed write leaves no run-owned snapshot: {snapshots:?}"
            );
        }
    }
}
