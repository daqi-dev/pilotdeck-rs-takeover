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
//!   冲突 SNAPSHOT_UNAVAILABLE（快照内容被清理时不伪装可恢复）。找到候
//!   选还要过**回溯链校验**（dsh r6 P1#2）：候选的 after_revision 必须
//!   与该 entry 的 before_revision 吻合（entry 的 before 链指向紧前变更
//!   的 after），不吻合 = 两次触碰之间有未被捕获的变更（历史缺页），同
//!   样收口 SNAPSHOT_UNAVAILABLE——不把缺页前的陈旧内容静默当「之前」恢
//!   复。before_revision 缺席（create；或迁移/播种数据未记 before）时无
//!   链可校，维持既有回溯。
//!
//! ## 定序锚（dsh r4 P1 → r5 P1 entry 级化）
//!
//! 历史回溯、选中排序与 after/before 目标态推导按 **entry 级锚** 全史定
//! 序（dsh r5 P1）：`v1_file_snapshot_entries` 每条携带
//! `first_occurred_at` / `last_occurred_at` = 折叠窗口内该路径首/末行台账
//! 行的 `occurred_at`（RFC3339，词法可序）。分工：
//!
//! - `first_occurred_at`（触碰时刻）：before 方向「最早选中触碰」排序与回
//!   溯边界用它——条目 `before_revision` 是窗口首次触碰**前**的状态；
//! - `last_occurred_at`（内容位置）：回溯候选与 after 方向「最后触碰」排
//!   序用它——折叠条目的 after 内容是末次变更**后**的状态。
//!
//! 快照级 `order_key`（窗口行的 max(occurred_at)）只承担
//! `snapshots.list` 显示序。r4 把它同时用作回溯锚，在**多路径窗口**下会
//! 把窗口末时刻套到每条 entry 上：run 写 f@t0、g@t5，窗口内 CLI 直写
//! f@t2——S_run 的 order_key=t5 使 f 的候选压过 S_cli（t2），before 回溯
//! 选出陈旧的 A 而非 B（per-path 序倒置）。`created_at`（聚合提交时刻）
//! 更不能用：run 期间同路径被 CLI 直写覆盖时它晚于 CLI 快照，但 run 快照
//! 的 after 内容（窗口内 driver 写）早于 CLI 直写，按它定序会让 before 回
//! 溯把 run 之后的 CLI 内容当「之前」静默恢复（r4 修复的盲点）。
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
//!   逐项原子 rename/删除 → 恢复落盘捕获登记（dsh r6 P1#1，与 files 域
//!   三写点同构：before=恢复前 current、after=计划目标 revision、
//!   artifact 复用计划目标产物，`insert_capture_ledger` + A6b 即时聚合）
//!   与 `files.changed` / `snapshots.changed` 事件同事务落库。没有这一
//!   步，恢复快照的 after 停在**恢复前**内容，entry 级锚历史里「本次恢
//!   复之后该路径的内容」没有任何记录——之后再写该路径时 before 回溯停
//!   在恢复快照的陈旧 after 上（链校验则报历史缺页冲突）。
//!   失败尝试回滚并逐项报告；无法全恢复时 PARTIAL_FAILURE 明示。回滚成
//!   功的路径净效果 no-op、不登记；门内重验失败的 failedPaths 未触碰、
//!   不登记。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetryMode, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::artifacts::{copy_artifact_into, publish_artifact_from_reader};
use crate::public_v1::domains::files::{
    CODE_PATH_ESCAPE, ProjectResolveError, ProjectWorkspace, byte_path_json, decode_byte_path,
    file_revision_of, leaf_identity_path, open_file_canonical, request_target, require_within,
    resolve_project, with_write_gates,
};
use crate::public_v1::domains::memory::{
    CODE_PLAN_EXPIRED, CODE_SNAPSHOT_UNAVAILABLE, DomainOpLookup, claim_operation_execution,
    finish_operation_in_store, insert_plan, load_plan, lookup_domain_operation_in_conn,
    operation_execution_already_claimed, plan_json, record_domain_operation_accept_in_conn,
    record_domain_operation_rejection_in_conn, set_optional_str, sql_err,
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

/// `order_key`：快照级排序锚 = 快照代表的**变更时刻**（dsh r4 P1；模块文
/// 档「定序锚」）。run 边界/A6b 快照取窗口台账行的 max(occurred_at)，恢复
/// 快照与测试播种取捕获时刻；`created_at` 恒为聚合提交时刻，两者在 run 期
/// 间同路径被 CLI 直写时倒置。**职责边界（dsh r5 P1）**：回溯与目标态推导
/// 已下沉到 entry 级锚（见 [`DDL_SNAPSHOT_ENTRIES`]），本列只承担
/// `snapshots.list` 显示序。`DEFAULT ''` 仅占位（插入方恒显式传值）。
const DDL_SNAPSHOTS: &str = "CREATE TABLE IF NOT EXISTS v1_file_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    run_id TEXT,
    turn_id TEXT,
    created_at TEXT NOT NULL,
    order_key TEXT NOT NULL DEFAULT ''
)";
/// entry 级排序锚（dsh r5 P1；模块文档「定序锚」）：
/// `first_occurred_at` = 折叠窗口内该路径首行的 occurred_at（「本快照触碰
/// 该路径」的时刻——before 回溯边界与「最早选中触碰」排序），`last_occurred_at`
/// = 末行的（条目 after 内容的变更时刻——回溯候选序与 after 方向「最后触
/// 碰」排序）。`DEFAULT ''` 仅占位（折叠方恒显式传值；旧库升级面由
/// [`ensure_domain_columns`] 回填）。
const DDL_SNAPSHOT_ENTRIES: &str = "CREATE TABLE IF NOT EXISTS v1_file_snapshot_entries (
    snapshot_id TEXT NOT NULL,
    path_bytes BLOB NOT NULL,
    before_revision TEXT,
    after_revision TEXT,
    action TEXT NOT NULL,
    artifact_id TEXT,
    first_occurred_at TEXT NOT NULL DEFAULT '',
    last_occurred_at TEXT NOT NULL DEFAULT '',
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

/// dsh r5-snap-wire P1-1：列存在性守卫迁移（由 `V1Store::init_schema` 在
/// 域 DDL 之后调用；落点理由见 store.rs 调用处注释——域 schema 钩子只收
/// 静态 DDL 串，表达不了「PRAGMA table_info 条件 ALTER」）。
///
/// `v1_file_snapshots` / `v1_file_snapshot_entries` 是 dsh r3 起就存在的
/// 表，S3 底座「existing tables are never migrated —— new domains only
/// add tables」约束的是跨域**新增表**；排序锚列（r4 的
/// `v1_file_snapshots.order_key`、r5 的
/// `v1_file_snapshot_entries.first_occurred_at`/`last_occurred_at`）是在既
/// 有表上追加的列，`CREATE TABLE IF NOT EXISTS` 对旧库已存在的表是
/// no-op——旧库启新二进制后第一次快照 `INSERT ... order_key` 报 no such
/// column，整事务回滚（同事务的台账行也丢）→ files.* 写「落盘却报
/// UNAVAILABLE」，快照域整域不可用。守卫幂等：列在则 no-op。
pub(crate) fn ensure_domain_columns(conn: &Connection) -> Result<(), ContractError> {
    ensure_column(
        conn,
        "v1_file_snapshots",
        "order_key",
        "ALTER TABLE v1_file_snapshots ADD COLUMN order_key TEXT NOT NULL DEFAULT ''",
    )?;
    let added_first = ensure_column(
        conn,
        "v1_file_snapshot_entries",
        "first_occurred_at",
        "ALTER TABLE v1_file_snapshot_entries ADD COLUMN first_occurred_at TEXT NOT NULL DEFAULT ''",
    )?;
    let added_last = ensure_column(
        conn,
        "v1_file_snapshot_entries",
        "last_occurred_at",
        "ALTER TABLE v1_file_snapshot_entries ADD COLUMN last_occurred_at TEXT NOT NULL DEFAULT ''",
    )?;
    // r4→r5 升级回填：旧 entries 的 per-path 时刻没有落库、不可考，取父快
    // 照的 `order_key`（r4 的快照级锚）——升级前数据保持 r4 定序语义，而
    // 非以 `''` 锚排到所有真实时刻之前（那会让旧条目在回溯里恒「最早」，
    // 选为选中触碰时又恒「无更早内容」→ 无据冲突）。pre-r4 库的
    // order_key 本身刚以 `''` 补列，回填结果同为 `''`（诚实值：不可考）。
    if added_first {
        backfill_entry_anchor(conn, "first_occurred_at")?;
    }
    if added_last {
        backfill_entry_anchor(conn, "last_occurred_at")?;
    }
    Ok(())
}

/// 单列存在性守卫：缺列则 `ALTER TABLE ... ADD COLUMN ... NOT NULL DEFAULT ''`
/// （SQLite 的 ADD COLUMN 不支持 IF NOT EXISTS），返回是否刚补的列。
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    alter: &str,
) -> Result<bool, ContractError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(sql_err)?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    if names.iter().any(|name| name == column) {
        return Ok(false);
    }
    conn.execute_batch(alter).map_err(sql_err)?;
    Ok(true)
}

/// 升级回填：刚补的 entry 锚列从父快照 `order_key` 取值（见
/// [`ensure_domain_columns`] 的注释）。
fn backfill_entry_anchor(conn: &Connection, column: &str) -> Result<(), ContractError> {
    conn.execute(
        &format!(
            "UPDATE v1_file_snapshot_entries
             SET {column} = COALESCE((
                 SELECT s.order_key FROM v1_file_snapshots s
                 WHERE s.snapshot_id = v1_file_snapshot_entries.snapshot_id
             ), '')"
        ),
        [],
    )
    .map_err(sql_err)?;
    Ok(())
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

fn io_escape(message: &str) -> ContractError {
    ContractError::new(
        CODE_PATH_ESCAPE,
        message.to_owned(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn io_not_found_err(err: std::io::Error) -> ContractError {
    if crate::project_files::nofollow_loop(&err) {
        return io_escape(&format!("path refused (symlink loop or NOFOLLOW): {err}"));
    }
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::ELOOP) {
        return io_escape(&format!("path refused (symlink loop): {err}"));
    }
    match err.kind() {
        std::io::ErrorKind::NotFound => ContractError::new(
            CODE_NOT_FOUND,
            format!("path not found: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        std::io::ErrorKind::PermissionDenied => {
            io_escape(&format!("path refused (link or permission): {err}"))
        }
        _ => io_unavailable(err),
    }
}

fn checked_relative_target(
    workspace: &ProjectWorkspace,
    path: &[u8],
) -> Result<PathBuf, ContractError> {
    if path.is_empty() {
        return Err(invalid("target relativePath must not be empty"));
    }
    let target = request_target(workspace, path)?;
    if target == workspace.workspace {
        return Err(invalid("refusing to restore the project workspace root"));
    }
    Ok(target)
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
/// 先经 `leaf_identity_path` 校验父目录落在工作区内（缺失父目录时再走
/// `restore_gate_key` 排除悬空/循环/越界链接），再经 `open_file_canonical`
/// （pinned dirfd + `O_NOFOLLOW`）流式读取哈希，防止符号链接越界读取。
fn stat_current(
    workspace: &ProjectWorkspace,
    path: &[u8],
) -> Result<Option<String>, ContractError> {
    let target = checked_relative_target(workspace, path)?;
    let leaf = match leaf_identity_path(workspace, &target) {
        Ok(leaf) => leaf,
        Err(err) if err.code == CODE_NOT_FOUND => {
            let _ = restore_gate_key(workspace, path)?;
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let meta = match std::fs::symlink_metadata(&leaf) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        #[cfg(unix)]
        Err(err) if err.raw_os_error() == Some(libc::ENOTDIR) => return Ok(None),
        Err(err) => return Err(io_not_found_err(err)),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let mut file = open_file_canonical(workspace, &leaf)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut len: u64 = 0;
    use std::io::Read;
    loop {
        let read = file.read(&mut buffer).map_err(io_unavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        len += read as u64;
    }
    let sha = format!("{:x}", hasher.finalize());
    Ok(Some(file_revision_of(&sha, len)))
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
    // 快照级排序锚 `order_key`（dsh r4 P1）不在行模型里：**职责边界（dsh
    // r5 P1）**是且只是 `snapshots.list` 的 SQL `ORDER BY` 显示序，回溯
    // 与目标态推导用 entry 级锚（[`SnapshotEntryRow`] 的
    // first/last_occurred_at），Rust 侧不消费它。
}

struct SnapshotEntryRow {
    path_bytes: Vec<u8>,
    before_revision: Option<String>,
    after_revision: Option<String>,
    action: String,
    artifact_id: Option<String>,
    /// entry 级排序锚（dsh r5 P1；模块文档「定序锚」）：本快照窗口内该
    /// 路径首/末行台账行的 occurred_at——`first_occurred_at` 是「本快照触
    /// 碰该路径」的时刻（before 回溯边界与「最早选中触碰」排序），
    /// `last_occurred_at` 是条目 after 内容的变更时刻（回溯候选序与 after
    /// 方向「最后触碰」排序）。
    first_occurred_at: String,
    last_occurred_at: String,
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
        "SELECT path_bytes, before_revision, after_revision, action, artifact_id,
                first_occurred_at, last_occurred_at
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
            first_occurred_at: r.get(5)?,
            last_occurred_at: r.get(6)?,
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

/// 带排序锚的捕获条目（dsh r5 P1）：`first_occurred_at` / `last_occurred_at`
/// = 折叠窗口内该路径首/末行台账行的 occurred_at（模块文档「定序锚」的
/// 分工见 [`SnapshotEntryRow`] 字段注释）。files 域的 A1 登记不感知锚
/// （台账行自带 occurred_at），锚只在聚合落快照时由折叠（恢复快照 = 单次
/// 触碰直构）产生。
struct AnchoredChange {
    change: CapturedFileChange,
    first_occurred_at: String,
    last_occurred_at: String,
}

/// 快照 revision：聚合条目的内容寻址哈希（含 action 校验）。
fn snapshot_revision_of(changes: &[AnchoredChange]) -> Result<String, ContractError> {
    if changes.is_empty() {
        return Err(invalid(
            "a turn snapshot needs at least one captured change",
        ));
    }
    for anchored in changes {
        if !matches!(anchored.change.action, "create" | "replace" | "delete") {
            return Err(invalid(format!(
                "unsupported capture action `{}`",
                anchored.change.action
            )));
        }
    }
    let mut hasher = Sha256::new();
    for anchored in changes {
        let change = &anchored.change;
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
/// `order_key` = 快照级锚（显示序；dsh r5 P1 起回溯用 entry 级锚）：ledger
/// 聚合路径由调用方取窗口行的 max(occurred_at)，恢复快照/测试播种取捕获
/// 时刻——`created_at` 恒为聚合提交时刻，两者不可混用。
fn insert_turn_snapshot(
    conn: &Connection,
    snapshot_id: &str,
    project_id: &str,
    run: Option<(&str, &str)>,
    revision: &str,
    order_key: &str,
    changes: &[AnchoredChange],
) -> Result<(), ContractError> {
    conn.execute(
        "INSERT INTO v1_file_snapshots (snapshot_id, project_id, revision, run_id, turn_id, created_at, order_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot_id,
            project_id,
            revision,
            run.map(|(run_id, _)| run_id),
            run.map(|(_, turn_id)| turn_id),
            now_rfc3339(),
            order_key
        ],
    )
    .map_err(sql_err)?;
    for anchored in changes {
        let change = &anchored.change;
        conn.execute(
            "INSERT INTO v1_file_snapshot_entries
                (snapshot_id, path_bytes, before_revision, after_revision, action, artifact_id,
                 first_occurred_at, last_occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                snapshot_id,
                change.path,
                change.before_revision,
                change.after_revision,
                change.action,
                change.artifact_id,
                anchored.first_occurred_at,
                anchored.last_occurred_at,
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
/// entries。快照 revision 为内容寻址（条目哈希）。`order_key` = 快照级锚
/// （显示序）；entries 各自携带 first/last_occurred_at（回溯锚，dsh r5
/// P1）。返回 snapshotId。
fn commit_turn_snapshot(
    store: &V1Store,
    project_id: &str,
    run: Option<(&str, &str)>,
    order_key: &str,
    changes: &[AnchoredChange],
) -> Result<String, ContractError> {
    let revision = snapshot_revision_of(changes)?;
    let snapshot_id = store.next_id();
    let conn = store.conn.lock().expect("sqlite mutex");
    insert_turn_snapshot(
        &conn,
        &snapshot_id,
        project_id,
        run,
        &revision,
        order_key,
        changes,
    )?;
    Ok(snapshot_id)
}

// ---------------------------------------------------------------------------
// 捕获台账（A1 登记 / A2 边界消费 / A6b 直写即时聚合）
// ---------------------------------------------------------------------------

/// 台账行（未消费的捕获条目，action 原样携带待转换）。`occurred_at` 是
/// 门内登记的**变更时刻**——entry 级锚与快照级 order_key 的取值来源
/// （dsh r4/r5 P1）。
struct LedgerRow {
    seq: i64,
    path: Vec<u8>,
    before_revision: Option<String>,
    after_revision: Option<String>,
    action: String,
    artifact_id: Option<String>,
    occurred_at: String,
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
        "SELECT seq, path_bytes, before_revision, after_revision, action, artifact_id, occurred_at
         FROM v1_file_capture_ledger WHERE project_id = ?1 AND run_id = ?2 ORDER BY seq ASC"
    } else {
        "SELECT seq, path_bytes, before_revision, after_revision, action, artifact_id, occurred_at
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
            occurred_at: row.get(6)?,
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

/// 聚合快照的快照级锚（dsh r4 P1；**dsh r5 P1 起只承担 `snapshots.list`
/// 显示序**，回溯定序用 entry 级锚）：窗口内（折叠前）各台账行的
/// max(occurred_at)——快照代表的**变更时刻**。RFC3339 全 UTC（登记方
/// `insert_capture_ledger` 恒 `now_rfc3339()`），词法 max 即时间 max。
/// 行集非空（两个聚合调用侧都先判空再取锚）。
fn ledger_order_key(rows: &[LedgerRow]) -> String {
    rows.iter()
        .map(|row| row.occurred_at.as_str())
        .max()
        .unwrap_or_default()
        .to_owned()
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
///     方向回溯更早内容产物）；
/// - entry 级锚（dsh r5 P1）：`first_occurred_at` / `last_occurred_at` :=
///   该路径各行的 min/max(occurred_at)——时间序下的首/末行真实时刻，对乱
///   序登记稳健（保证 first ≤ last，回溯的「自排除」不变量依赖它：同快照
///   同路径的候选锚 (last, id) 恒不严格小于触碰锚 (first, id)）。
///
/// 行必须按 seq 升序传入（`load_capture_ledger` 保证）。
fn fold_ledger_rows(rows: &[LedgerRow]) -> Result<Vec<AnchoredChange>, ContractError> {
    let mut folded: Vec<AnchoredChange> = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(existing) = folded.iter_mut().find(|c| c.change.path == row.path) {
            // 同路径后续行：before 保持首条的，after/artifact 取末条的；
            // action 由「首条是否 create」与「末条是否 delete」合成——
            // 已折叠条目的 action == "create" 当且仅当首条是 create（上面
            // 的规则表恰好保持这一不变量）。
            let first_is_create = existing.change.action == "create";
            let last_action = ledger_action(&row.action)?;
            existing.change.after_revision = row.after_revision.clone();
            existing.change.artifact_id = row.artifact_id.clone();
            existing.change.action = if first_is_create && last_action == "delete" {
                "create"
            } else if last_action == "delete" {
                "delete"
            } else if first_is_create {
                "create"
            } else {
                "replace"
            };
            if row.occurred_at < existing.first_occurred_at {
                existing.first_occurred_at = row.occurred_at.clone();
            }
            if row.occurred_at > existing.last_occurred_at {
                existing.last_occurred_at = row.occurred_at.clone();
            }
        } else {
            folded.push(AnchoredChange {
                change: row.to_change()?,
                first_occurred_at: row.occurred_at.clone(),
                last_occurred_at: row.occurred_at.clone(),
            });
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
    // 快照级锚 = 本批行的变更时刻（dsh r4 P1，r5 起承担显示序；回溯定序
    // 由 entries 携带的 per-path 锚负责）：A6b 即时快照在写门内与登记同
    // 事务落库，锚即该次变更的时刻——不会被聚合时刻（晚于门内变更）拉偏。
    let order_key = ledger_order_key(&rows);
    let revision = snapshot_revision_of(&changes)?;
    let snapshot_id = store.next_id();
    insert_turn_snapshot(
        conn,
        &snapshot_id,
        project_id,
        None,
        &revision,
        &order_key,
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
    // 快照级锚 = 本窗口（折叠前）各行的变更时刻 max（dsh r4 P1；r5 起承
    // 担显示序——回溯定序由 entries 携带的 per-path 首末行锚负责，多路径
    // 窗口下快照级 max 会把窗口末时刻套到每条 entry 上、per-path 序倒置）。
    // created_at 是终态后的聚合时刻，与窗口内变更时刻可能隔着整个 run
    // （期间 CLI 直写同路径），显示序也不按它排。
    let order_key = ledger_order_key(&rows);
    let revision = snapshot_revision_of(&changes)?;
    let snapshot_id = store.next_id();
    insert_turn_snapshot(
        &tx,
        &snapshot_id,
        &run.project_id,
        Some((&run.run_id, &run.run_id)),
        &revision,
        &order_key,
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
    // 显示序 = 变更序（order_key，dsh r4 P1）：语义即「快照代表的改动
    // 先后」。dsh r5 P1 起这是 order_key 的**唯一**职责（回溯定序已下沉到
    // entry 级锚）；created_at（聚合提交时刻）在 run 边界快照上会与变更
    // 序倒置。
    sql.push_str(" ORDER BY order_key ASC, snapshot_id ASC");
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

/// 项目快照全史中一条内容记录，`order` = 内容锚 (last_occurred_at,
/// snapshot_id)——条目 after 内容是折叠窗口末行变更后的状态（dsh r5 P1
/// entry 级锚；r4 用快照级 order_key，多路径窗口下 per-path 倒置）。
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
    let path_filter: Option<Vec<Vec<u8>>> = match input.get("paths").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => {
            let mut decoded = Vec::with_capacity(arr.len());
            for p in arr {
                decoded.push(decode_byte_path(p)?);
            }
            Some(decoded)
        }
        _ => None,
    };

    // 工作区必须真实存在（restore 落工作区；虚拟/未注册项目 → NOT_FOUND）。
    let workspace = resolve_workspace(store, &project_id)?;

    // 选中快照（校验归属）、其条目与项目全史。选中条目不再按快照级序排
    // 序——「最早/最后选中触碰」按 entry 级锚逐路径归约（dsh r5 P1：多路
    // 径窗口下快照级 order_key 会把窗口末时刻套到每条 entry 上）。
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
        let mut entries_by_snapshot = Vec::new();
        for row in &selected {
            entries_by_snapshot.push(load_snapshot_entries(&conn, &row.snapshot_id)?);
        }
        // 全史（含未选中快照）：before 方向的内容回溯来源。SQL 的
        // order_key 序只是加载序——回溯候选的定序在下方按 entry 级锚
        // (last_occurred_at, snapshot_id) 显式重排（dsh r5 P1）。
        let all_rows: Vec<SnapshotRow> = conn
            .prepare(&format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM v1_file_snapshots
                 WHERE project_id = ?1 ORDER BY order_key ASC, snapshot_id ASC"
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
                    order: (entry.last_occurred_at.clone(), row.snapshot_id.clone()),
                    path: entry.path_bytes,
                    action: entry.action,
                    artifact_id: entry.artifact_id,
                    after_revision: entry.after_revision,
                });
            }
        }
        // 回溯候选序 = entry 级锚（dsh r5 P1）：条目 after 内容是末行变更后
        // 的状态，内容位置即 last_occurred_at。
        history.sort_by(|a, b| a.order.cmp(&b.order));
        (selected, entries_by_snapshot, history)
    };

    let path_allowed = |path: &[u8]| -> bool {
        path_filter
            .as_ref()
            .is_none_or(|set| set.iter().any(|p| p == path))
    };

    // 选中条目（含所在快照 index；路径过滤后）。
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
                        first_occurred_at: entry.first_occurred_at.clone(),
                        last_occurred_at: entry.last_occurred_at.clone(),
                    },
                ));
            }
        }
    }

    // 选中条目按路径分组。「最早选中触碰」（before 的回溯边界）与「最后
    // 触碰」（after 的目标态）都按 entry 级锚归约（dsh r5 P1）：
    // - 触碰锚 = (first_occurred_at, snapshot_id)：「本快照触碰该路径」的
    //   时刻是首行时刻（条目 before_revision 是窗口首次触碰前的状态）；
    // - 内容锚 = (last_occurred_at, snapshot_id)：条目 after 内容是末行变
    //   更后的状态，内容位置即末行时刻。
    // 快照级 order_key（窗口 max(occurred_at)）不再参与——多路径窗口下它
    // 把窗口末时刻套到每条 entry 上（run 写 f@t0、g@t5，窗口内 CLI 直写
    // f@t2：f 的候选按 t5 排会压过 CLI 的 t2，回溯选出陈旧内容）。
    let mut selected_by_path: HashMap<&Vec<u8>, Vec<(usize, &SnapshotEntryRow)>> = HashMap::new();
    for item in &selected_entries {
        selected_by_path
            .entry(&item.1.path_bytes)
            .or_default()
            .push((item.0, &item.1));
    }
    let mut touched_paths: Vec<&Vec<u8>> = selected_by_path.keys().copied().collect();
    touched_paths.sort();

    let mut targets: HashMap<Vec<u8>, RestoreTarget> = HashMap::new();
    let mut conflicts: Vec<Value> = Vec::new();
    let mut conflict_paths: Vec<Vec<u8>> = Vec::new();

    // 目标态派生。
    if direction == "after" {
        for path in touched_paths.iter().copied() {
            // 最后触碰：内容锚 max（同锚时按 snapshot_id 定序，与 id 的时间
            // 单调性一致）。
            let (_, entry) = *selected_by_path[path]
                .iter()
                .max_by(|a, b| {
                    (&a.1.last_occurred_at, &selected_rows[a.0].snapshot_id)
                        .cmp(&(&b.1.last_occurred_at, &selected_rows[b.0].snapshot_id))
                })
                .expect("per-path group is non-empty");
            let is_explicit_absent = entry.action == "delete"
                || (entry.action == "create"
                    && entry.after_revision.is_none()
                    && entry.artifact_id.is_none());
            if is_explicit_absent {
                targets.insert(
                    path.clone(),
                    RestoreTarget {
                        artifact_id: None,
                        target_revision: None,
                    },
                );
            } else if entry.artifact_id.is_some() {
                targets.insert(
                    path.clone(),
                    RestoreTarget {
                        artifact_id: entry.artifact_id.clone(),
                        target_revision: entry.after_revision.clone(),
                    },
                );
            } else {
                // 目录/符号链接等无内容产物的非缺失条目：绝不进入 targets
                // 被误判为 delete 或静默跳过，明确报 SNAPSHOT_UNAVAILABLE。
                let mut conflict = json!({
                    "resourceId": path_display(path),
                    "code": CODE_SNAPSHOT_UNAVAILABLE,
                });
                set_optional_str(&mut conflict, "observedRevision", &entry.after_revision);
                conflicts.push(conflict);
            }
        }
    } else {
        // before：每个路径取最早选中触碰（触碰锚 min）；目标 = 该动作之
        // 前的状态。
        for path in touched_paths.iter().copied() {
            let (index, entry) = *selected_by_path[path]
                .iter()
                .min_by(|a, b| {
                    (&a.1.first_occurred_at, &selected_rows[a.0].snapshot_id)
                        .cmp(&(&b.1.first_occurred_at, &selected_rows[b.0].snapshot_id))
                })
                .expect("per-path group is non-empty");
            if entry.action == "create" {
                targets.insert(
                    path.clone(),
                    RestoreTarget {
                        artifact_id: None,
                        target_revision: None,
                    },
                );
            } else {
                // replace/delete：回溯全史中严格早于「本快照首次触碰」的内
                // 容产物。边界 = 触碰锚 (first_occurred_at, snapshot_id)；候
                // 选 = 该路径各快照 entry，按内容锚 (last_occurred_at,
                // snapshot_id) 取序（dsh r5 P1 entry 级锚；r4 的快照级
                // order_key 定序在多路径窗口下 per-path 倒置，r3 的
                // created_at 定序会把 run 之后的 CLI 内容当「之前」）。
                let boundary = (
                    entry.first_occurred_at.clone(),
                    selected_rows[index].snapshot_id.clone(),
                );
                let mut found: Option<&HistoryRecord> = None;
                for record in &history {
                    if record.path == path.as_slice()
                        && record.action != "delete"
                        && record.order < boundary
                        && record.artifact_id.is_some()
                    {
                        found = Some(record); // 内容锚序遍历 → 末次命中即最近。
                    }
                }
                match found {
                    Some(record) => {
                        // 回溯链校验（dsh r6 P1#2）：候选的 after_revision
                        // 必须与该 entry 的 before_revision 吻合——entry 的
                        // before 链指向紧前变更的 after，候选不是紧前变更
                        // ＝ 两次触碰之间有未被捕获的变更（历史缺页：绕过
                        // 网关的直写、登记失败残留等）。静默采用候选会把
                        // 缺页前的陈旧内容当「之前」恢复；收口为明确冲突。
                        // before_revision 缺席（create 语义本不走此分支；
                        // 迁移/播种数据未记 before）时无链可校，维持既有
                        // 回溯。
                        let chain_broken = entry
                            .before_revision
                            .as_deref()
                            .is_some_and(|before| record.after_revision.as_deref() != Some(before));
                        if chain_broken {
                            let mut conflict = json!({
                                "resourceId": path_display(path),
                                "code": CODE_SNAPSHOT_UNAVAILABLE,
                            });
                            set_optional_str(
                                &mut conflict,
                                "observedRevision",
                                &entry.before_revision,
                            );
                            conflicts.push(conflict);
                            conflict_paths.push(path.clone());
                        } else {
                            targets.insert(
                                path.clone(),
                                RestoreTarget {
                                    artifact_id: record.artifact_id.clone(),
                                    target_revision: record.after_revision.clone(),
                                },
                            );
                        }
                    }
                    None => {
                        let mut conflict = json!({
                            "resourceId": path_display(path),
                            "code": CODE_SNAPSHOT_UNAVAILABLE,
                        });
                        set_optional_str(&mut conflict, "observedRevision", &entry.before_revision);
                        conflicts.push(conflict);
                        conflict_paths.push(path.clone());
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
    let store = rt.store().clone();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let store_for_accept = store.clone();
    let (operation_id, receipt) = tokio::task::spawn_blocking(move || {
        accept_restore(&store_for_accept, &input, idempotency_key.as_deref())
    })
    .await
    .map_err(join_err)?
    .map_err(DispatchError::Application)?;
    // 重放闸门（memory.rs 同款）：deduplicated 回执直接返回，不再启动
    // 执行体——重放不得再执行副作用，也不得覆写原 operation 的完成结果。
    if receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, receipt));
    }
    let store2 = store.clone();
    tokio::task::spawn_blocking(move || {
        let _ = execute_restore(&store2, &operation_id, &plan_id);
    });
    Ok((ResponseOutcome::Accepted, receipt))
}

fn validate_restore_plan_record(
    conn: &Connection,
    plan_id: &str,
    expected_plan_revision: &str,
) -> Result<(String, Vec<Value>), ContractError> {
    let plan = load_plan(conn, plan_id)?.ok_or_else(|| domain_not_found("change plan", plan_id))?;
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
    Ok((project_id, targets))
}

fn lookup_result_or_fresh(
    conn: &Connection,
    input: &Value,
    idempotency_key: Option<&str>,
) -> Result<Option<(String, Value)>, ContractError> {
    match lookup_domain_operation_in_conn(conn, "snapshots.restore", input, idempotency_key)? {
        DomainOpLookup::Replay(receipt) => {
            let operation_id = receipt["operationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Ok(Some((operation_id, receipt)))
        }
        DomainOpLookup::Conflict => Err(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_IDEMPOTENCY_CONFLICT,
            "idempotencyKey already used for snapshots.restore with different input",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        DomainOpLookup::Rejected(code) => Err(ContractError::new(
            code,
            "snapshots.restore request previously rejected definitively",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        DomainOpLookup::Fresh => Ok(None),
    }
}

/// restore 第二跳原子受理：
/// 1) 事务内幂等查重 + 计划基础校验（避免无意义的磁盘哈希）；
/// 2) 锁外执行工作区解析与全目标流式 SHA-256 现状复验；
/// 3) `BEGIN IMMEDIATE` 事务内再次幂等查重 + savepoint 计划复验并消费 +
///    受理入库原子提交（并发同键竞态绝不把胜者覆写为 rejection）。
#[allow(clippy::too_many_lines)]
fn accept_restore(
    store: &V1Store,
    input: &Value,
    idempotency_key: Option<&str>,
) -> Result<(String, Value), ContractError> {
    let plan_id = input["planId"].as_str().unwrap_or_default();
    let expected_plan_revision = input["expectedPlanRevision"].as_str().unwrap_or_default();

    let (project_id, targets) = {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        if let Some(hit) = lookup_result_or_fresh(&tx, input, idempotency_key)? {
            return Ok(hit);
        }
        match validate_restore_plan_record(&tx, plan_id, expected_plan_revision) {
            Ok(data) => data,
            Err(error) => {
                if error.retry.mode == RetryMode::Never {
                    record_domain_operation_rejection_in_conn(
                        &tx,
                        "snapshots.restore",
                        input,
                        idempotency_key,
                        &error.code,
                    )?;
                    tx.commit().map_err(sql_err)?;
                }
                return Err(error);
            }
        }
    };

    // 文件现状复验（锁外，避免持有 sqlite mutex 做磁盘哈希；resolve_workspace
    // 自身会短暂获取 store.conn）。
    let preflight = (|| -> Result<(), ContractError> {
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
        Ok(())
    })();
    if let Err(error) = preflight {
        if error.retry.mode == RetryMode::Never && idempotency_key.is_some() {
            let mut conn = store.conn.lock().expect("sqlite mutex");
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(sql_err)?;
            if let Some(hit) = lookup_result_or_fresh(&tx, input, idempotency_key)? {
                return Ok(hit);
            }
            record_domain_operation_rejection_in_conn(
                &tx,
                "snapshots.restore",
                input,
                idempotency_key,
                &error.code,
            )?;
            tx.commit().map_err(sql_err)?;
        }
        return Err(error);
    }

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let mut tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    if let Some(hit) = lookup_result_or_fresh(&tx, input, idempotency_key)? {
        return Ok(hit);
    }
    let sp = tx.savepoint().map_err(sql_err)?;
    let consume_result = (|| -> Result<(), ContractError> {
        validate_restore_plan_record(&sp, plan_id, expected_plan_revision)?;
        let consumed = sp
            .execute(
                "UPDATE v1_memory_change_plans SET state = 'consumed' WHERE plan_id = ?1 AND state = 'open'",
                params![plan_id],
            )
            .map_err(sql_err)?;
        if consumed == 0 {
            return Err(domain_not_found("change plan", plan_id));
        }
        Ok(())
    })();
    if let Err(error) = consume_result {
        drop(sp);
        if error.retry.mode == RetryMode::Never {
            record_domain_operation_rejection_in_conn(
                &tx,
                "snapshots.restore",
                input,
                idempotency_key,
                &error.code,
            )?;
            tx.commit().map_err(sql_err)?;
        }
        return Err(error);
    }
    sp.commit().map_err(sql_err)?;

    let operation_id = store.next_id();
    let accepted_revision = store.next_id();
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": accepted_revision,
        "deduplicated": false,
        "state": "queued",
    });
    record_domain_operation_accept_in_conn(
        &tx,
        "snapshots.restore",
        input,
        idempotency_key,
        &operation_id,
        &receipt,
    )?;
    tx.commit().map_err(sql_err)?;
    Ok((operation_id, receipt))
}

#[allow(clippy::too_many_lines)]
fn execute_restore(
    store: &Arc<V1Store>,
    operation_id: &str,
    plan_id: &str,
) -> Result<Value, ContractError> {
    if !claim_operation_execution(store, operation_id)? {
        return Err(operation_execution_already_claimed(operation_id));
    }
    let result = execute_restore_inner(store, plan_id);
    finish_operation_in_store(store, operation_id, result)
}

#[cfg(test)]
fn injected_rollback_failure_registry() -> &'static std::sync::Mutex<Vec<(String, Vec<u8>)>> {
    static FAIL_TARGETS: std::sync::OnceLock<std::sync::Mutex<Vec<(String, Vec<u8>)>>> =
        std::sync::OnceLock::new();
    FAIL_TARGETS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(test)]
fn should_inject_rollback_failure(project_id: &str, path: &[u8]) -> bool {
    let guard = injected_rollback_failure_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard
        .iter()
        .any(|(pid, target)| pid == project_id && target.as_slice() == path)
}

enum CommittedOp {
    Created {
        path: Vec<u8>,
        rel: PathBuf,
    },
    Replaced {
        path: Vec<u8>,
        rel: PathBuf,
        recovery_artifact: String,
    },
    Deleted {
        path: Vec<u8>,
        rel: PathBuf,
        recovery_artifact: String,
    },
}

impl CommittedOp {
    fn path(&self) -> &[u8] {
        match self {
            Self::Created { path, .. }
            | Self::Replaced { path, .. }
            | Self::Deleted { path, .. } => path,
        }
    }
}

struct StagedTempEntry {
    #[cfg(unix)]
    parent_dir: std::fs::File,
    #[cfg(unix)]
    temp_name: String,
    #[cfg(not(unix))]
    temp_path: PathBuf,
    armed: bool,
}

#[derive(Default)]
struct StagedTempGuard {
    entries: Vec<StagedTempEntry>,
}

impl StagedTempGuard {
    #[cfg(unix)]
    fn register(&mut self, parent_dir: &std::fs::File, temp_name: &str) -> std::io::Result<usize> {
        let cloned = parent_dir.try_clone()?;
        let index = self.entries.len();
        self.entries.push(StagedTempEntry {
            parent_dir: cloned,
            temp_name: temp_name.to_owned(),
            armed: true,
        });
        Ok(index)
    }

    #[cfg(not(unix))]
    fn register(&mut self, temp_path: PathBuf) -> usize {
        let index = self.entries.len();
        self.entries.push(StagedTempEntry {
            temp_path,
            armed: true,
        });
        index
    }

    fn disarm(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.armed = false;
        }
    }

    fn cleanup_index(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index)
            && entry.armed
        {
            entry.armed = false;
            #[cfg(unix)]
            {
                let _ = rustix::fs::unlinkat(
                    &entry.parent_dir,
                    &entry.temp_name,
                    rustix::fs::AtFlags::empty(),
                );
            }
            #[cfg(not(unix))]
            {
                let _ = std::fs::remove_file(&entry.temp_path);
            }
        }
    }
}

impl Drop for StagedTempGuard {
    fn drop(&mut self) {
        for index in 0..self.entries.len() {
            self.cleanup_index(index);
        }
    }
}

#[cfg(unix)]
fn directory_open_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    #[cfg(target_os = "linux")]
    let access = OFlags::PATH;
    #[cfg(target_vendor = "apple")]
    let access = OFlags::from_bits_retain(libc::O_SEARCH as u32);
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    let access = OFlags::RDONLY;
    access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

#[cfg(unix)]
fn open_workspace_dir(workspace: &ProjectWorkspace) -> Result<std::fs::File, ContractError> {
    crate::project_files::open_directory_beneath(&workspace.root, &workspace.workspace)
        .map_err(io_not_found_err)
}

#[cfg(unix)]
fn open_dir_nofollow(root: &std::fs::File, relative: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{AtFlags, FileType, Mode, openat, statat};
    let flags = directory_open_flags();
    let mut dir = root.try_clone()?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::from_raw_os_error(libc::EACCES));
        };
        let opened = openat(&dir, name, flags, Mode::empty()).map_err(|err| {
            let io_err = std::io::Error::from(err);
            if matches!(err, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
                && let Ok(stat) = statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)
                && FileType::from_raw_mode(stat.st_mode) == FileType::Symlink
            {
                return std::io::Error::from_raw_os_error(libc::ELOOP);
            }
            io_err
        })?;
        dir = std::fs::File::from(opened);
    }
    Ok(dir)
}

#[cfg(unix)]
fn open_or_create_parent_dir(
    workspace_dir: &std::fs::File,
    parent_rel: &Path,
) -> Result<std::fs::File, ContractError> {
    if !parent_rel.as_os_str().is_empty() {
        crate::project_files::create_directories_at(
            workspace_dir,
            parent_rel,
            rustix::fs::Mode::from_raw_mode(0o755),
        )
        .map_err(io_not_found_err)?;
    }
    open_dir_nofollow(workspace_dir, parent_rel).map_err(io_not_found_err)
}

#[cfg(unix)]
fn create_new_temp_at(
    parent_dir: &std::fs::File,
    temp_name: &str,
) -> Result<std::fs::File, ContractError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
    let file = std::fs::File::from(
        openat(
            parent_dir,
            temp_name,
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)
        .map_err(io_not_found_err)?,
    );
    let stat = fstat(&file)
        .map_err(std::io::Error::from)
        .map_err(io_unavailable)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(io_escape("refusing non-regular or hardlinked staging file"));
    }
    Ok(file)
}

#[cfg(unix)]
fn stage_temp_file_at(
    store: &V1Store,
    parent_dir: &std::fs::File,
    temp_name: &str,
    artifact_id: &str,
) -> Result<(), ContractError> {
    let mut file = create_new_temp_at(parent_dir, temp_name)?;
    copy_artifact_into(store, artifact_id, &mut file)?;
    file.sync_all().map_err(io_unavailable)?;
    Ok(())
}

#[cfg(unix)]
fn commit_temp_at(
    parent_dir: &std::fs::File,
    temp_name: &str,
    leaf_name: &std::ffi::OsStr,
) -> Result<(), ContractError> {
    use rustix::fs::{Mode, OFlags, openat, renameat};
    renameat(parent_dir, temp_name, parent_dir, leaf_name)
        .map_err(std::io::Error::from)
        .map_err(io_not_found_err)?;
    if let Ok(dir) = openat(
        parent_dir,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        let _ = std::fs::File::from(dir).sync_all();
    }
    Ok(())
}

#[cfg(unix)]
fn unlink_leaf_at(workspace_dir: &std::fs::File, rel: &Path) -> Result<bool, ContractError> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};
    let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
    let leaf_name = rel
        .file_name()
        .ok_or_else(|| invalid("target path has no file name"))?;
    let parent_dir = match open_dir_nofollow(workspace_dir, parent_rel) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(io_not_found_err(err)),
    };
    match unlinkat(&parent_dir, leaf_name, AtFlags::empty()).map_err(std::io::Error::from) {
        Ok(()) => {
            if let Ok(dir) = openat(
                &parent_dir,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                let _ = std::fs::File::from(dir).sync_all();
            }
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(io_not_found_err(err)),
    }
}

#[cfg(not(unix))]
fn stage_temp_file_nonunix(
    store: &V1Store,
    workspace: &ProjectWorkspace,
    rel: &Path,
    temp_name: &str,
    artifact_id: &str,
    temp_guard: &mut StagedTempGuard,
) -> Result<(PathBuf, usize), ContractError> {
    let dest = workspace.workspace.join(rel);
    require_within(&workspace.workspace, &dest)?;
    let parent = dest
        .parent()
        .ok_or_else(|| invalid("target path has no parent"))?;
    std::fs::create_dir_all(parent).map_err(io_unavailable)?;
    let parent_canon = std::fs::canonicalize(parent).map_err(io_not_found_err)?;
    require_within(&workspace.workspace, &parent_canon)?;
    let temp_path = parent_canon.join(temp_name);
    let guard_idx = temp_guard.register(temp_path.clone());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(io_unavailable)?;
    copy_artifact_into(store, artifact_id, &mut file)?;
    file.sync_all().map_err(io_unavailable)?;
    Ok((temp_path, guard_idx))
}

fn project_paths_json(project_id: &str, paths: &[Vec<u8>]) -> Vec<Value> {
    paths
        .iter()
        .map(|p| {
            json!({
                "projectId": project_id,
                "relativePath": byte_path_json(p),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn rollback_committed_op(
    store: &V1Store,
    project_id: &str,
    workspace: &ProjectWorkspace,
    #[cfg(unix)] workspace_dir: &std::fs::File,
    op: &CommittedOp,
) -> Result<(), ContractError> {
    #[cfg(test)]
    if should_inject_rollback_failure(project_id, op.path()) {
        return Err(io_unavailable(std::io::Error::other(
            "injected rollback failure for test",
        )));
    }
    let _ = (project_id, workspace);
    match op {
        CommittedOp::Created { rel, .. } => {
            #[cfg(unix)]
            {
                let _ = unlink_leaf_at(workspace_dir, rel)?;
                Ok(())
            }
            #[cfg(not(unix))]
            {
                let dest = workspace.workspace.join(rel);
                require_within(&workspace.workspace, &dest)?;
                match std::fs::remove_file(&dest) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(io_unavailable(e)),
                }
            }
        }
        CommittedOp::Replaced {
            rel,
            recovery_artifact,
            ..
        }
        | CommittedOp::Deleted {
            rel,
            recovery_artifact,
            ..
        } => {
            let temp_name = format!(".agentcpm-pd-restore-rb-{}", store.next_id());
            #[cfg(unix)]
            {
                let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
                let leaf_name = rel
                    .file_name()
                    .ok_or_else(|| invalid("target path has no file name"))?;
                let parent_dir = open_or_create_parent_dir(workspace_dir, parent_rel)?;
                let mut rb_guard = StagedTempGuard::default();
                let guard_idx = rb_guard
                    .register(&parent_dir, &temp_name)
                    .map_err(io_unavailable)?;
                stage_temp_file_at(store, &parent_dir, &temp_name, recovery_artifact)?;
                commit_temp_at(&parent_dir, &temp_name, leaf_name)?;
                rb_guard.disarm(guard_idx);
                Ok(())
            }
            #[cfg(not(unix))]
            {
                let mut rb_guard = StagedTempGuard::default();
                let (temp_path, guard_idx) = stage_temp_file_nonunix(
                    store,
                    workspace,
                    rel,
                    &temp_name,
                    recovery_artifact,
                    &mut rb_guard,
                )?;
                let dest = workspace.workspace.join(rel);
                require_within(&workspace.workspace, &dest)?;
                std::fs::rename(&temp_path, &dest).map_err(io_unavailable)?;
                rb_guard.disarm(guard_idx);
                Ok(())
            }
        }
    }
}

/// 多文件恢复：先全部门内校验 + 暂存（pinned 工作区句柄 + `O_NOFOLLOW`）→
/// 捕获恢复快照 → 逐项原子提交 → 恢复落盘捕获登记；任一步提交前失败则
/// 零修改并自动清理所有临时文件；提交期失败回滚所有已提交项（Create 删除，
/// Replace/Delete 从恢复快照原子写回）并逐项报告。
#[allow(clippy::too_many_lines)]
fn execute_restore_inner(store: &Arc<V1Store>, plan_id: &str) -> Result<Value, ContractError> {
    let payload = {
        let conn = store.conn.lock().expect("sqlite mutex");
        load_plan(&conn, plan_id)?
            .ok_or_else(|| domain_not_found("change plan", plan_id))?
            .payload
    };
    let project_id = payload["projectId"].as_str().unwrap_or_default().to_owned();
    let workspace = resolve_workspace(store, &project_id)?;

    struct Target {
        path: Vec<u8>,
        artifact_id: Option<String>,
        current: Option<String>,
        target_revision: Option<String>,
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
            target_revision: target["targetRevision"].as_str().map(ToOwned::to_owned),
        });
    }

    let gate_keys: Vec<PathBuf> = targets
        .iter()
        .map(|target| restore_gate_key(&workspace, &target.path))
        .collect::<Result<_, _>>()?;
    with_write_gates(&gate_keys, || {
        struct ResolvedTarget {
            target: Target,
            gate_key: PathBuf,
            rel: PathBuf,
        }
        let mut resolved: Vec<ResolvedTarget> = Vec::with_capacity(targets.len());
        for target in targets {
            let gate_key = restore_gate_key(&workspace, &target.path)?;
            let rel = gate_key
                .strip_prefix(&workspace.workspace)
                .map_err(|_| io_escape("resolved restore target escapes workspace"))?
                .to_path_buf();
            resolved.push(ResolvedTarget {
                target,
                gate_key,
                rel,
            });
        }

        // 0) 门内重验（P1#2）：任何路径冲突则在提交前全量中止（零修改）。
        let mut conflicted: Vec<Vec<u8>> = Vec::new();
        for item in &resolved {
            let current = stat_current(&workspace, &item.target.path)?;
            if current != item.target.current {
                conflicted.push(item.target.path.clone());
            }
        }
        if !conflicted.is_empty() {
            return Ok(json!({
                "restoredPaths": [],
                "outcome": "partial",
                "failedPaths": project_paths_json(&project_id, &conflicted),
            }));
        }

        // 1) 暂存阶段（先暂存校验再提交）：使用 pinned 工作区句柄与 O_NOFOLLOW
        //    创建父目录及 `.agentcpm-pd-restore-*` 独占临时文件，由 RAII guard
        //    确保任意提前返回时全部清理。
        #[cfg(unix)]
        let workspace_dir = open_workspace_dir(&workspace)?;

        struct Staged {
            path: Vec<u8>,
            rel: PathBuf,
            #[cfg(unix)]
            parent_dir: std::fs::File,
            #[cfg(unix)]
            leaf_name: std::ffi::OsString,
            #[cfg(unix)]
            temp_name: String,
            #[cfg(not(unix))]
            temp_path: PathBuf,
            is_replace: bool,
            guard_index: usize,
        }
        let mut temp_guard = StagedTempGuard::default();
        let mut staged: Vec<Staged> = Vec::new();
        let mut staged_failed: Vec<Vec<u8>> = Vec::new();

        for item in &resolved {
            let Some(artifact_id) = &item.target.artifact_id else {
                continue; // 目标缺失（delete）：无需暂存。
            };
            let temp_name = format!(".agentcpm-pd-restore-{}", store.next_id());
            #[cfg(unix)]
            {
                let parent_rel = item.rel.parent().unwrap_or_else(|| Path::new(""));
                let leaf_name = item
                    .rel
                    .file_name()
                    .ok_or_else(|| invalid("target path has no file name"))?
                    .to_os_string();
                let parent_dir = match open_or_create_parent_dir(&workspace_dir, parent_rel) {
                    Ok(dir) => dir,
                    Err(err) if err.code == CODE_PATH_ESCAPE => return Err(err),
                    Err(_) => {
                        staged_failed.push(item.target.path.clone());
                        continue;
                    }
                };
                let guard_index = temp_guard
                    .register(&parent_dir, &temp_name)
                    .map_err(io_unavailable)?;
                match stage_temp_file_at(store, &parent_dir, &temp_name, artifact_id) {
                    Ok(()) => staged.push(Staged {
                        path: item.target.path.clone(),
                        rel: item.rel.clone(),
                        parent_dir,
                        leaf_name,
                        temp_name,
                        is_replace: item.target.current.is_some(),
                        guard_index,
                    }),
                    Err(err) if err.code == CODE_PATH_ESCAPE => return Err(err),
                    Err(_) => {
                        temp_guard.cleanup_index(guard_index);
                        staged_failed.push(item.target.path.clone());
                    }
                }
            }
            #[cfg(not(unix))]
            {
                match stage_temp_file_nonunix(
                    store,
                    &workspace,
                    &item.rel,
                    &temp_name,
                    artifact_id,
                    &mut temp_guard,
                ) {
                    Ok((temp_path, guard_index)) => staged.push(Staged {
                        path: item.target.path.clone(),
                        rel: item.rel.clone(),
                        temp_path,
                        is_replace: item.target.current.is_some(),
                        guard_index,
                    }),
                    Err(err) if err.code == CODE_PATH_ESCAPE => return Err(err),
                    Err(_) => {
                        staged_failed.push(item.target.path.clone());
                    }
                }
            }
        }
        if !staged_failed.is_empty() {
            drop(temp_guard);
            return Ok(json!({
                "restoredPaths": [],
                "outcome": "partial",
                "failedPaths": project_paths_json(&project_id, &staged_failed),
            }));
        }

        // 2) 恢复快照：在所有校验和暂存通过之后、实际修改工作区之前，流式
        //    备份全部将被替换或删除的既有文件（缺失目标无需内容备份）。
        let recovery_anchor = now_rfc3339();
        let mut recovery_changes: Vec<AnchoredChange> = Vec::new();
        for item in &resolved {
            if let Some(revision) = &item.target.current {
                let mut original = open_file_canonical(&workspace, &item.gate_key)?;
                let artifact = publish_artifact_from_reader(
                    store,
                    &project_id,
                    "application/octet-stream",
                    Some(&format!(
                        "snapshot-recovery:{}",
                        path_display(&item.target.path)
                    )),
                    &mut original,
                    RECOVERY_ORIGIN,
                )?;
                let artifact_id = artifact["artifactId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                recovery_changes.push(AnchoredChange {
                    change: CapturedFileChange {
                        path: item.target.path.clone(),
                        before_revision: item.target.current.clone(),
                        after_revision: Some(revision.clone()),
                        action: "replace",
                        artifact_id: Some(artifact_id),
                    },
                    first_occurred_at: recovery_anchor.clone(),
                    last_occurred_at: recovery_anchor.clone(),
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
                &recovery_anchor,
                &recovery_changes,
            )?)
        };

        // 3) 提交：renameat（create/replace）+ unlinkat（delete）；统一记录
        //    CommittedOp::{Created, Replaced, Deleted} 以支持全量回滚。
        let mut restored: Vec<Vec<u8>> = Vec::new();
        let mut failed: Vec<Vec<u8>> = Vec::new();
        let mut committed_ops: Vec<CommittedOp> = Vec::new();

        for item in staged {
            #[cfg(unix)]
            let commit_res = commit_temp_at(&item.parent_dir, &item.temp_name, &item.leaf_name);
            #[cfg(not(unix))]
            let commit_res = (|| -> Result<(), ContractError> {
                let dest = workspace.workspace.join(&item.rel);
                require_within(&workspace.workspace, &dest)?;
                std::fs::rename(&item.temp_path, &dest).map_err(io_unavailable)
            })();
            match commit_res {
                Ok(()) => {
                    temp_guard.disarm(item.guard_index);
                    restored.push(item.path.clone());
                    if item.is_replace {
                        let recovery_artifact = recovery_artifact_for(
                            &recovery_changes,
                            &item.path,
                        )
                        .ok_or_else(|| {
                            io_unavailable(std::io::Error::other("missing recovery artifact"))
                        })?;
                        committed_ops.push(CommittedOp::Replaced {
                            path: item.path,
                            rel: item.rel,
                            recovery_artifact,
                        });
                    } else {
                        committed_ops.push(CommittedOp::Created {
                            path: item.path,
                            rel: item.rel,
                        });
                    }
                }
                Err(_) => {
                    temp_guard.cleanup_index(item.guard_index);
                    failed.push(item.path);
                }
            }
        }
        for item in &resolved {
            if item.target.artifact_id.is_none() && item.target.current.is_some() {
                #[cfg(unix)]
                let unlink_res = unlink_leaf_at(&workspace_dir, &item.rel);
                #[cfg(not(unix))]
                let unlink_res = (|| -> Result<bool, ContractError> {
                    let dest = workspace.workspace.join(&item.rel);
                    require_within(&workspace.workspace, &dest)?;
                    match std::fs::remove_file(&dest) {
                        Ok(()) => Ok(true),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                        Err(e) => Err(io_unavailable(e)),
                    }
                })();
                match unlink_res {
                    Ok(true) => {
                        restored.push(item.target.path.clone());
                        if let Some(recovery_artifact) =
                            recovery_artifact_for(&recovery_changes, &item.target.path)
                        {
                            committed_ops.push(CommittedOp::Deleted {
                                path: item.target.path.clone(),
                                rel: item.rel.clone(),
                                recovery_artifact,
                            });
                        }
                    }
                    Ok(false) => {
                        restored.push(item.target.path.clone());
                    }
                    Err(_) => {
                        failed.push(item.target.path.clone());
                    }
                }
            }
        }
        drop(temp_guard);

        // 4) 失败回滚：遍历全部 CommittedOp（含 Created/Replaced/Deleted）。
        //    只有全部 CommittedOp 均回滚成功时才报 `rolledBack`。
        let mut outcome = "complete";
        let mut rollback_failures: Vec<Vec<u8>> = Vec::new();
        let mut rolled_back_paths: Vec<Vec<u8>> = Vec::new();
        if !failed.is_empty() {
            outcome = "partial";
            for op in committed_ops.iter().rev() {
                let res = rollback_committed_op(
                    store,
                    &project_id,
                    &workspace,
                    #[cfg(unix)]
                    &workspace_dir,
                    op,
                );
                if res.is_ok() {
                    rolled_back_paths.push(op.path().to_vec());
                } else {
                    rollback_failures.push(op.path().to_vec());
                }
            }
            restored.retain(|p| !rolled_back_paths.contains(p));
            if !committed_ops.is_empty() && rollback_failures.is_empty() {
                outcome = "rolledBack";
            }
        }

        // 5) 恢复落盘捕获（dsh r6 P1#1）：只对最终保留落盘变更的 restored
        //    路径登记捕获与发射 files.changed 事件（过滤 rolled_back_paths）。
        let mut captures: Vec<CapturedFileChange> = Vec::new();
        for item in &resolved {
            if !restored.contains(&item.target.path) {
                continue;
            }
            let action = match (&item.target.current, &item.target.artifact_id) {
                (None, Some(_)) => "create",
                (Some(_), Some(_)) => "replace",
                (Some(_), None) => "delete",
                (None, None) => continue,
            };
            captures.push(CapturedFileChange {
                path: item.target.path.clone(),
                before_revision: item.target.current.clone(),
                after_revision: item.target.target_revision.clone(),
                action,
                artifact_id: item.target.artifact_id.clone(),
            });
        }

        {
            let append_restore_events = |conn: &Connection| -> Result<(), ContractError> {
                for path in &restored {
                    append_domain_event(
                        conn,
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
                        conn,
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
                Ok(())
            };
            let mut conn = store.conn.lock().expect("sqlite mutex");
            if captures.is_empty() {
                append_restore_events(&conn)?;
            } else {
                let tx = conn.transaction().map_err(sql_err)?;
                insert_capture_ledger(&tx, &project_id, None, &captures)?;
                append_restore_events(&tx)?;
                capture_unowned_entries(store, &tx, &project_id)?;
                tx.commit().map_err(sql_err)?;
            }
        }

        let restored_json = project_paths_json(&project_id, &restored);
        let failed_json = project_paths_json(&project_id, &failed);
        let mut completion = json!({
            "restoredPaths": restored_json,
            "outcome": outcome,
            "failedPaths": failed_json,
        });
        if let Some(recovery) = &recovery_snapshot_id {
            completion["recoverySnapshotId"] = json!(recovery);
        }
        if !rollback_failures.is_empty() {
            let mut details = Map::new();
            details.insert("outcome".to_owned(), json!("partial"));
            details.insert(
                "restoredPaths".to_owned(),
                completion["restoredPaths"].clone(),
            );
            details.insert("failedPaths".to_owned(), completion["failedPaths"].clone());
            details.insert(
                "rollbackFailedPaths".to_owned(),
                json!(project_paths_json(&project_id, &rollback_failures)),
            );
            if let Some(recovery) = recovery_snapshot_id {
                details.insert("recoverySnapshotId".to_owned(), json!(recovery));
            }
            return Err(ContractError::new(
                CODE_PARTIAL_FAILURE,
                format!(
                    "restore applied partially and rollback incomplete for {} path(s); recovery material is preserved in the recovery snapshot",
                    rollback_failures.len()
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::Applied,
            )
            .with_details(details));
        }
        Ok(completion)
    })
}

/// restore 提交段的写门 key（P1#2）：目标叶路径的 canonical 身份。
/// 拒绝工作区根自身（空相对路径）；仅在 `CODE_NOT_FOUND` 时沿工作区逐段
/// 检查已存在祖先，使用 `symlink_metadata` 拒绝悬空符号链接、`ELOOP` 与 `EACCES`。
fn restore_gate_key(workspace: &ProjectWorkspace, path: &[u8]) -> Result<PathBuf, ContractError> {
    let target = checked_relative_target(workspace, path)?;
    match leaf_identity_path(workspace, &target) {
        Ok(key) => Ok(key),
        Err(err) if err.code == CODE_NOT_FOUND => {
            let relative = target
                .strip_prefix(&workspace.workspace)
                .map_err(|_| io_escape("target path outside workspace"))?;
            let leaf = relative
                .file_name()
                .ok_or_else(|| invalid("target path has no file name"))?;
            let parent_rel = relative.parent().unwrap_or_else(|| Path::new(""));
            let mut canon_parent = workspace.workspace.clone();
            let components: Vec<_> = parent_rel.components().collect();
            for (idx, component) in components.iter().enumerate() {
                let std::path::Component::Normal(name) = component else {
                    return Err(io_escape("invalid path component in target"));
                };
                let probe = canon_parent.join(name);
                match std::fs::symlink_metadata(&probe) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        match std::fs::canonicalize(&probe) {
                            Ok(resolved) => {
                                require_within(&workspace.workspace, &resolved)?;
                                canon_parent = resolved;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                return Err(io_escape("dangling symlink in restore parent path"));
                            }
                            Err(e) => return Err(io_not_found_err(e)),
                        }
                    }
                    Ok(meta) if meta.is_dir() => {
                        let resolved = std::fs::canonicalize(&probe).map_err(io_not_found_err)?;
                        require_within(&workspace.workspace, &resolved)?;
                        canon_parent = resolved;
                    }
                    Ok(_) => {
                        let mut out = probe;
                        for rem in &components[idx + 1..] {
                            out = out.join(rem);
                        }
                        return Ok(out.join(leaf));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let mut out = probe;
                        for rem in &components[idx + 1..] {
                            out = out.join(rem);
                        }
                        return Ok(out.join(leaf));
                    }
                    Err(e) => return Err(io_not_found_err(e)),
                }
            }
            Ok(canon_parent.join(leaf))
        }
        Err(err) => Err(err),
    }
}

fn recovery_artifact_for(recovery_changes: &[AnchoredChange], path: &[u8]) -> Option<String> {
    recovery_changes
        .iter()
        .find(|anchored| anchored.change.path == path)
        .and_then(|anchored| anchored.change.artifact_id.clone())
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

    /// 单时刻锚定（测试播种）：播种条目是「一次触碰」，first = last = 给定
    /// 时刻（dsh r4 P1 三来源中的捕获时刻）。
    fn anchored_at(changes: Vec<CapturedFileChange>, at: &str) -> Vec<AnchoredChange> {
        changes
            .into_iter()
            .map(|change| AnchoredChange {
                change,
                first_occurred_at: at.to_owned(),
                last_occurred_at: at.to_owned(),
            })
            .collect()
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
        // 播种快照的排序锚 = 捕获时刻（状态即此刻；dsh r4 P1 三来源之
        // 一）——与旧 created_at 定序语义等价，既有回溯测试不受影响。
        // entry 级锚同取该时刻（一次触碰，first = last；dsh r5 P1）。
        let at = now_rfc3339();
        commit_turn_snapshot(
            store,
            project_id,
            Some((&run_id, &turn_id)),
            &at,
            &anchored_at(changes, &at),
        )
        .unwrap()
    }

    fn read_workspace_file(workspace: &std::path::Path, path: &str) -> Option<Vec<u8>> {
        std::fs::read(workspace.join(path)).ok()
    }

    /// 直读一条快照的排序锚（dsh r4 P1：变更时刻，非聚合提交时刻）。
    fn snapshot_order_key(rt: &Arc<PublicV1Runtime>, snapshot_id: &str) -> String {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT order_key FROM v1_file_snapshots WHERE snapshot_id = ?1",
            params![snapshot_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// entry 级锚（dsh r5 P1）：(first_occurred_at, last_occurred_at)。
    fn entry_anchors(rt: &Arc<PublicV1Runtime>, snapshot_id: &str, path: &str) -> (String, String) {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT first_occurred_at, last_occurred_at
             FROM v1_file_snapshot_entries WHERE snapshot_id = ?1 AND path_bytes = ?2",
            params![snapshot_id, path.as_bytes().to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// 表列存在性（迁移守卫测试用）。
    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        names.iter().any(|name| name == column)
    }

    /// 指定 run 归属台账行的 occurred_at（聚合前读取；条目锚的取值来源）。
    fn ledger_row_time(rt: &Arc<PublicV1Runtime>, run_id: &str, path: &[u8]) -> String {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT occurred_at FROM v1_file_capture_ledger
             WHERE run_id = ?1 AND path_bytes = ?2",
            params![run_id, path.to_vec()],
            |r| r.get(0),
        )
        .unwrap()
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

        // 两个操作的即时快照都在，且 replace 先于 delete（列表 order_key
        // ASC, snapshot_id ASC——A6b 快照的 order_key 即门内变更时刻，门序
        // 即落库序）。
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
    // dsh r4 P1：排序锚 order_key（before 回溯定序 = 变更时刻，非聚合时刻）
    // -----------------------------------------------------------------------

    /// dsh r4 P1 决定性交错：run 期间同一路径被 CLI 直写覆盖时，run 边界快
    /// 照的 created_at（聚合时刻，t2）晚于 CLI 快照（t1），但其 after 内容
    /// 是窗口内 driver 的写（t0）——早于 CLI 直写。before 回溯按
    /// (order_key, snapshot_id)（变更时刻）定序必须选 CLI 快照的 B，而不是
    /// run 快照的陈旧 A；旧（created_at 定序）实现必选 A——B 被静默丢弃
    /// 且 restore 报 success。
    ///
    /// 序列：t0 driver 写 f=A（run 台账行）→ t1 CLI 直写 f=B（S_cli）→
    /// t2 run 终态（S_run：created_at=t2、order_key=t0、after=A）→ t3 CLI
    /// 写 f=C（S_c）→ plan([S_c], before) 的目标 = S_cli 的 B。
    #[tokio::test]
    async fn before_restore_orders_by_change_time_not_capture_time() {
        let (_tmp, rt, workspace, project_id) = setup("snap-orderkey");
        let run_id = "run-orderkey-1";
        let rev_a = file_revision_of_bytes(b"A");
        let rev_b = file_revision_of_bytes(b"B");
        let rev_c = file_revision_of_bytes(b"C");

        // t0：driver 写 f=A（run 归属台账行，门内变更时刻 t0）。
        crate::public_v1::domains::files::driver_write_file(
            rt.store(),
            run_id,
            &project_id,
            b"f.txt",
            b"A",
        )
        .unwrap();
        let t0: String = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.query_row(
                "SELECT occurred_at FROM v1_file_capture_ledger WHERE run_id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .unwrap()
        };
        // 时间戳严格分离（occurred_at 与 created_at 同为 now_rfc3339 序
        // 列），交错形态确定成立。
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // t1：CLI 直写 f=B → A6b 即时快照 S_cli（created_at=order_key=t1，
        // after=B）。
        dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "f.txt",
                b"B",
                json!({"state": "revision", "revision": rev_a}),
            ),
        )
        .await;
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 1, "one A6b snapshot: {snapshots:?}");
        let s_cli = snapshots[0]["snapshotId"].as_str().unwrap().to_owned();
        let cli_created = snapshots[0]["createdAt"].as_str().unwrap().to_owned();
        let b_artifact = snapshots[0]["entries"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // t2：run 终态 → 边界聚合 S_run（created_at=t2、after=A、
        // order_key=窗口行的 max(occurred_at)=t0）。
        capture_run_boundary(rt.store(), &stored_run(&project_id, run_id)).unwrap();
        let snapshots = listed_snapshots(&rt, &project_id).await;
        let owned = snapshots
            .iter()
            .find(|s| s["runId"] == json!(run_id))
            .expect("the run-owned boundary snapshot");
        let s_run = owned["snapshotId"].as_str().unwrap().to_owned();
        let run_created = owned["createdAt"].as_str().unwrap().to_owned();
        let a_artifact = owned["entries"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // t3：CLI 写 f=C → S_c（created_at=order_key=t3，after=C）。
        dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "f.txt",
                b"C",
                json!({"state": "revision", "revision": rev_b}),
            ),
        )
        .await;
        let snapshots = listed_snapshots(&rt, &project_id).await;
        let s_c = snapshots
            .iter()
            .find(|s| {
                s["entries"]
                    .as_array()
                    .is_some_and(|entries| entries[0]["afterRevision"] == json!(rev_c))
            })
            .unwrap_or_else(|| panic!("the C snapshot: {snapshots:?}"))["snapshotId"]
            .as_str()
            .unwrap()
            .to_owned();

        // 交错形态成立（旧实现失败的决定性前提）：聚合时刻序 S_cli <
        // S_run，变更时刻序反转。
        assert!(
            cli_created < run_created,
            "capture order must put the CLI snapshot before the run snapshot"
        );
        assert_eq!(
            snapshot_order_key(&rt, &s_run),
            t0,
            "the run snapshot anchors at the window's change time (max occurred_at)"
        );
        assert!(
            snapshot_order_key(&rt, &s_run) < snapshot_order_key(&rt, &s_cli),
            "change-time order inverts the capture-time order"
        );
        // entry 级锚（dsh r5 P1）：单路径窗口的条目锚 = 该行 occurred_at；
        // 回溯与「最早/最后触碰」定序读它（快照级 order_key 只管显示序）。
        assert_eq!(
            entry_anchors(&rt, &s_run, "f.txt"),
            (t0.clone(), t0.clone()),
            "the run snapshot's f entry anchors at its own change time"
        );
        assert_eq!(
            entry_anchors(&rt, &s_cli, "f.txt"),
            (
                snapshot_order_key(&rt, &s_cli),
                snapshot_order_key(&rt, &s_cli)
            ),
            "the single-touch A6b entry anchor equals the write's change time"
        );

        // 决定性断言：plan([S_c], before) 选 S_cli 的 B（C 之前最后一次变
        // 更），不是聚合最晚的 S_run 的陈旧 A。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s_c],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(
            entries.len(),
            1,
            "f.txt is the only touched path: {entries:?}"
        );
        assert_eq!(
            entries[0]["artifactId"],
            json!(b_artifact),
            "the before target is the CLI snapshot's B, not the run snapshot's stale A"
        );
        assert_ne!(
            entries[0]["artifactId"],
            json!(a_artifact),
            "the stale run-window content must be skipped"
        );
        assert_eq!(entries[0]["action"], json!("replace"));
        assert!(
            plan["plan"]["conflicts"].as_array().unwrap().is_empty(),
            "no conflicts: {plan:?}"
        );

        // apply 后 f=B（旧 created_at 定序会写成 A，B 被静默丢弃）。
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "orderkey-before",
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if read_workspace_file(&workspace, "f.txt").as_deref() == Some(&b"B"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the before-restore never applied B"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 列表显示序 = 变更序：S_run（order_key=t0）排最前，尽管它聚合落库
        // 最晚（t2）；恢复快照与恢复落盘捕获（dsh r6 P1#1，锚=恢复提交时
        // 刻，>t3）排最后。
        let listed = listed_snapshots(&rt, &project_id).await;
        let ids: Vec<String> = listed
            .iter()
            .map(|s| s["snapshotId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            ids.len(),
            5,
            "S_run + S_cli + S_c + recovery + restore-capture: {ids:?}"
        );
        assert_eq!(
            ids[..3],
            [s_run.clone(), s_cli.clone(), s_c.clone()],
            "display order follows change time, not capture time"
        );
    }

    /// dsh r4 P1 单调性护栏：同路径两个 A6b 快照 + 一个 run 快照交错——
    /// 捕获顺序（= created_at 序）S_b1 → S_run → S_b2，变更时刻（=
    /// order_key）T0(run) < T1 < T2。before 回溯序按 (order_key,
    /// snapshot_id) 全史单调：
    /// - plan([S_b2], before) → S_b1 的 B1（紧前变更），不是聚合最晚的
    ///   S_run 的 A（旧 created_at 定序会选 A）；
    /// - plan([S_b1], before) → S_run 的 A（变更序更早的内容产物），不是
    ///   SNAPSHOT_UNAVAILABLE 冲突（旧定序下 S_run 聚合更晚，回溯扑空）。
    #[tokio::test]
    async fn before_backtrack_order_key_monotonicity_guard() {
        let (_tmp, rt, _workspace, project_id) = setup("snap-orderkey-mono");
        let run_id = "run-orderkey-mono";
        // 变更时刻（台账 occurred_at，显式可控 → 全确定）。
        let t0 = "2031-01-01T00:00:00+00:00";
        let t1 = "2031-01-01T00:00:01+00:00";
        let t2 = "2031-01-01T00:00:02+00:00";

        let bytes_a = b"mono-A".to_vec();
        let bytes_b1 = b"mono-B1".to_vec();
        let bytes_b2 = b"mono-B2".to_vec();
        let publish = |tag: &str, bytes: &[u8]| -> String {
            let artifact = publish_artifact(
                rt.store(),
                &project_id,
                "application/octet-stream",
                Some(&format!("mono:{tag}")),
                bytes,
                "snapshot-seed",
            )
            .unwrap();
            artifact["artifactId"].as_str().unwrap().to_owned()
        };
        let art_a = publish("a", &bytes_a);
        let art_b1 = publish("b1", &bytes_b1);
        let art_b2 = publish("b2", &bytes_b2);
        let rev_a = file_revision_of_bytes(&bytes_a);
        let rev_b1 = file_revision_of_bytes(&bytes_b1);
        let rev_b2 = file_revision_of_bytes(&bytes_b2);

        // 台账行插+逐批聚合（捕获顺序即 created_at 序）。全部行 action=
        // replace（before 方向要回溯内容产物，不触发 create 的缺失语义）。
        let insert_row = |owner: Option<&str>,
                          before: &str,
                          after: &str,
                          artifact: &str,
                          occurred: &str| {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                    "INSERT INTO v1_file_capture_ledger
                        (project_id, run_id, path_bytes, before_revision, after_revision, action, artifact_id, occurred_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'replace', ?6, ?7)",
                    params![
                        project_id,
                        owner,
                        b"f.txt".to_vec(),
                        before,
                        after,
                        artifact,
                        occurred,
                    ],
                )
                .unwrap();
        };
        let capture_unowned = || -> String {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            capture_unowned_entries(rt.store(), &conn, &project_id)
                .unwrap()
                .expect("the unowned batch aggregates")
        };

        // 捕获 1：A6b 快照 S_b1（变更 T1）。
        insert_row(None, &rev_a, &rev_b1, &art_b1, t1);
        let s_b1 = capture_unowned();
        // 捕获 2：run 边界快照 S_run（变更 T0，聚合最晚于 S_b1）。
        insert_row(Some(run_id), "rev-pre-run", &rev_a, &art_a, t0);
        capture_run_boundary(rt.store(), &stored_run(&project_id, run_id)).unwrap();
        let s_run = {
            let snapshots = listed_snapshots(&rt, &project_id).await;
            snapshots
                .iter()
                .find(|s| s["runId"] == json!(run_id))
                .expect("the run snapshot")["snapshotId"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        // 捕获 3：A6b 快照 S_b2（变更 T2）。
        insert_row(None, &rev_b1, &rev_b2, &art_b2, t2);
        let s_b2 = capture_unowned();

        // 三个来源的 order_key 取值：ledger 聚合 = max(occurred_at)。
        assert_eq!(snapshot_order_key(&rt, &s_run), t0);
        assert_eq!(snapshot_order_key(&rt, &s_b1), t1);
        assert_eq!(snapshot_order_key(&rt, &s_b2), t2);
        // entry 级锚（dsh r5 P1）：条目携带自身行的 occurred_at（首 = 末，
        // 单行窗口）；回溯候选与边界定序读它，快照级 order_key 只管显示序。
        assert_eq!(
            entry_anchors(&rt, &s_run, "f.txt"),
            (t0.to_string(), t0.to_string())
        );
        assert_eq!(
            entry_anchors(&rt, &s_b1, "f.txt"),
            (t1.to_string(), t1.to_string())
        );
        assert_eq!(
            entry_anchors(&rt, &s_b2, "f.txt"),
            (t2.to_string(), t2.to_string())
        );
        // 台账全消费。
        assert_eq!(ledger_count(&rt, None), 0);
        assert_eq!(ledger_count(&rt, Some(run_id)), 0);

        // 显示序 = 变更序：S_run 最前（尽管聚合落库晚于 S_b1）。
        let listed = listed_snapshots(&rt, &project_id).await;
        let ids: Vec<String> = listed
            .iter()
            .map(|s| s["snapshotId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            ids,
            [s_run.clone(), s_b1.clone(), s_b2.clone()],
            "display order follows change time, not capture time"
        );

        // 回溯序断言 1：plan([S_b2], before) → B1（紧前变更 S_b1）。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s_b2],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1, "f.txt only: {entries:?}");
        assert_eq!(
            entries[0]["artifactId"],
            json!(art_b1),
            "the backtrack picks the immediately preceding change (B1), not the latest-captured run snapshot (A)"
        );
        assert!(
            plan["plan"]["conflicts"].as_array().unwrap().is_empty(),
            "no conflicts: {plan:?}"
        );

        // 回溯序断言 2：plan([S_b1], before) → A（变更序更早的 run 快照
        // 内容），不是冲突。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s_b1],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1, "f.txt only: {entries:?}");
        assert_eq!(
            entries[0]["artifactId"],
            json!(art_a),
            "the backtrack crosses into the earlier-by-change-time run snapshot (A)"
        );
        assert!(
            plan["plan"]["conflicts"].as_array().unwrap().is_empty(),
            "the run snapshot is reachable by change time: {plan:?}"
        );
    }

    /// dsh r5 P1 决定性交错（评审序列）：多路径窗口的快照级锚会把窗口末时
    /// 刻（max occurred_at）套到**每条** entry 上，per-path 序倒置。
    ///
    /// 序列：t0 run 写 f=A → t2 CLI 直写 f=B（S_cli）→ t5 run 写 g=G →
    /// run 终态（S_run：order_key=max(t0,t5)=t5；f 条目锚=t0、g 条目锚
    /// =t5）→ t7 CLI 写 f=C（S_c）。实际变更序 A(t0)→B(t2)→C(t7)：
    /// - plan([S_c], before) 对 f 的候选按 entry 锚取「严格早于 t7 的末
    ///   次」→ S_cli 的 B（t2），不是 S_run 的 A——快照级 (order_key, id)
    /// 定序下 S_run 的 f 候选挂 t5，压过 S_cli 的 t2，恢复出陈旧 A；
    /// - plan([S_run, S_cli], before)：「最早选中触碰」按 entry 的
    ///   first_occurred_at 定位——f 的 touch 在 t0（S_run 的 create → 目
    ///   标缺失），g 的 touch 在 t5（create → 缺失）。快照级锚会把 f 的
    ///   touch 定位到 t5，S_cli 的 t2 反成「最早」→ 回溯扑空成
    ///   SNAPSHOT_UNAVAILABLE 冲突。
    #[tokio::test]
    async fn before_backtrack_uses_entry_anchors_in_multipath_windows() {
        let (_tmp, rt, workspace, project_id) = setup("snap-entry-anchor");
        let run_id = "run-entry-anchor-1";
        let rev_a = file_revision_of_bytes(b"A");
        let rev_b = file_revision_of_bytes(b"B");
        let rev_c = file_revision_of_bytes(b"C");

        // t0：run 写 f=A（run 归属台账行）。
        crate::public_v1::domains::files::driver_write_file(
            rt.store(),
            run_id,
            &project_id,
            b"f.txt",
            b"A",
        )
        .unwrap();
        let t0 = ledger_row_time(&rt, run_id, b"f.txt");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // t2：CLI 直写 f=B → A6b 即时快照 S_cli（order_key=锚=t2、after=B）。
        dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "f.txt",
                b"B",
                json!({"state": "revision", "revision": rev_a}),
            ),
        )
        .await;
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(
            snapshots.len(),
            1,
            "only the A6b snapshot so far: {snapshots:?}"
        );
        let s_cli = snapshots[0]["snapshotId"].as_str().unwrap().to_owned();
        let t2 = snapshot_order_key(&rt, &s_cli);
        let b_artifact = snapshots[0]["entries"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // t5：run 再写 g=G（同窗口第二路径）。
        crate::public_v1::domains::files::driver_write_file(
            rt.store(),
            run_id,
            &project_id,
            b"g.txt",
            b"G",
        )
        .unwrap();
        let t5 = ledger_row_time(&rt, run_id, b"g.txt");

        // run 终态 → 边界聚合 S_run（order_key=max(t0,t5)=t5；f 条目锚
        // =t0、g 条目锚=t5——per-path，不是窗口末时刻）。
        capture_run_boundary(rt.store(), &stored_run(&project_id, run_id)).unwrap();
        let snapshots = listed_snapshots(&rt, &project_id).await;
        let owned = snapshots
            .iter()
            .find(|s| s["runId"] == json!(run_id))
            .expect("the run-owned boundary snapshot");
        let s_run = owned["snapshotId"].as_str().unwrap().to_owned();
        let a_artifact = owned["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"]["relativePath"]["value"] == json!("f.txt"))
            .expect("the run snapshot touches f")["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // t7：CLI 写 f=C → S_c（order_key=锚=t7、after=C）。
        dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "f.txt",
                b"C",
                json!({"state": "revision", "revision": rev_b}),
            ),
        )
        .await;
        let snapshots = listed_snapshots(&rt, &project_id).await;
        let s_c = snapshots
            .iter()
            .find(|s| {
                s["entries"].as_array().is_some_and(|entries| {
                    entries[0]["path"]["relativePath"]["value"] == json!("f.txt")
                        && entries[0]["afterRevision"] == json!(rev_c)
                })
            })
            .unwrap_or_else(|| panic!("the C snapshot: {snapshots:?}"))["snapshotId"]
            .as_str()
            .unwrap()
            .to_owned();
        let t7 = snapshot_order_key(&rt, &s_c);

        // 交错形态成立：t0 < t2 < t5 < t7，且 S_run 的快照级锚 = 窗口末时刻
        // t5（把 f 的候选拉到 t2 之后的正是它——entry 级化的动因）。
        assert!(
            t0 < t2 && t2 < t5 && t5 < t7,
            "t0={t0} t2={t2} t5={t5} t7={t7}"
        );
        assert_eq!(
            snapshot_order_key(&rt, &s_run),
            t5,
            "the run snapshot's display anchor is the window's max change time"
        );
        assert_eq!(
            entry_anchors(&rt, &s_run, "f.txt"),
            (t0.clone(), t0.clone()),
            "f's touch anchors at its own change time (t0), not the window's end (t5)"
        );
        assert_eq!(
            entry_anchors(&rt, &s_run, "g.txt"),
            (t5.clone(), t5.clone()),
            "g's touch anchors at t5 — per-path anchors differ inside one snapshot"
        );

        // 决定性断言 1：plan([S_c], before) 选 S_cli 的 B（C 之前最后一次
        // 变更在 t2），不是 S_run 的陈旧 A（快照级 t5 排序会压过 t2）。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s_c],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(
            entries.len(),
            1,
            "f.txt is the only touched path: {entries:?}"
        );
        assert_eq!(
            entries[0]["artifactId"],
            json!(b_artifact),
            "the before target is the CLI snapshot's B, not the run snapshot's stale A"
        );
        assert_ne!(
            entries[0]["artifactId"],
            json!(a_artifact),
            "the stale run-window content must be skipped"
        );
        assert!(
            plan["plan"]["conflicts"].as_array().unwrap().is_empty(),
            "no conflicts: {plan:?}"
        );
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "entry-anchor-before",
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if read_workspace_file(&workspace, "f.txt").as_deref() == Some(&b"B"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the before-restore never applied B"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            read_workspace_file(&workspace, "g.txt").as_deref(),
            Some(&b"G"[..]),
            "g is untouched (S_c does not touch it)"
        );

        // 决定性断言 2：plan([S_run, S_cli], before) 的 earliest-touch 按
        // entry 首行时刻定位——f 的 touch 在 t0（S_run 的 create → 目标缺
        // 失），g 在 t5（create → 缺失）。快照级锚会把 f 定位到 t5 → S_cli
        // 的 t2 反成「最早触碰」→ 回溯在 t2 之前扑空 → f 报
        // SNAPSHOT_UNAVAILABLE。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s_run, s_cli],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 2, "f and g both rewind: {entries:?}");
        assert!(
            entries.iter().all(|e| e["action"] == json!("delete")),
            "both earliest touches are creates: {entries:?}"
        );
        assert!(
            plan["plan"]["conflicts"].as_array().unwrap().is_empty(),
            "no conflicts (the snapshot-level anchor would strand f as UNAVAILABLE): {plan:?}"
        );
    }

    // -----------------------------------------------------------------------
    // dsh r6 P1：restore 落盘捕获登记 + before 回溯链校验
    // -----------------------------------------------------------------------

    /// dsh r6 P1#1 决定性序列（评审复现）：t0 write f=B → t1 write f=A →
    /// t2 restore(after) 把 f 写回 B → t3 write f=C（before=revB）→
    /// plan([S_c], before) 恢复 **B**（旧代码恢复 A）。
    ///
    /// 修复前 restore 的 rename/删除只发 `files.changed` 事件、不登记捕获
    /// ——恢复快照 R 的 after 是**恢复前**内容（A），entry 级锚历史里「t2
    /// 之后 f 的内容」没有任何记录：t3 再写（before=revB）后 before 回溯
    /// 的最近候选是 R 的陈旧 after=A，静默恢复 A（t3 之前磁盘上是 B）。
    /// 修复 = 提交段落盘捕获 S_restore（before=revA（恢复前 current）、
    /// after=revB（计划目标 revision）、artifact 复用计划目标产物）——回溯
    /// 最近候选变成 S_restore，且链校验 revB == S_c.before=revB 通过。
    #[tokio::test]
    async fn restore_registers_capture_so_before_backtrack_sees_restored_content() {
        let (_tmp, rt, workspace, project_id) = setup("snap-restore-capture");

        // t0：write f=B → S_b（create，after=revB、artifact=artB）。
        let t0 = dispatch_ok(
            &rt,
            "files.write",
            write_input(&project_id, "f.txt", b"B", json!({"state": "absent"})),
        )
        .await;
        let rev_b = t0["entry"]["revision"].as_str().unwrap().to_owned();
        let snapshots = listed_snapshots(&rt, &project_id).await;
        assert_eq!(snapshots.len(), 1, "one A6b snapshot: {snapshots:?}");
        let s_b = snapshots[0]["snapshotId"].as_str().unwrap().to_owned();
        let art_b = snapshots[0]["entries"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();

        // t1：write f=A（precondition revB）→ S_a（replace，before=revB、
        // after=revA）。
        let t1 = dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "f.txt",
                b"A",
                json!({"state": "revision", "revision": rev_b}),
            ),
        )
        .await;
        let rev_a = t1["entry"]["revision"].as_str().unwrap().to_owned();

        // t2：restore([S_b], after) 把 f 写回 B；恢复快照 R 记录恢复前内容 A。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s_b],
                "direction": "after",
            }),
        )
        .await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "restore-capture-t2",
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if read_workspace_file(&workspace, "f.txt").as_deref() == Some(&b"B"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the t2 restore never applied B"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 决定性前提（修复缺失面）：restore 落盘捕获聚合成快照 S_restore
        // （f replace，before=revA、after=revB、artifact 复用 artB）。修复
        // 前历史里只有 S_b、S_a、R——t2 之后 f 的内容无记录（R 的 after 是
        // 恢复前的 A）。rename 与登记同在写门临界区内，磁盘可见 B 后登记
        // 仍在途，这里轮询快照出现。
        let restore_capture = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let snapshots = listed_snapshots(&rt, &project_id).await;
                let found = snapshots.iter().find(|s| {
                    s["entries"].as_array().is_some_and(|entries| {
                        entries.len() == 1
                            && entries[0]["path"]["relativePath"]["value"] == json!("f.txt")
                            && entries[0]["beforeRevision"] == json!(rev_a)
                            && entries[0]["afterRevision"] == json!(rev_b)
                    })
                });
                if let Some(snapshot) = found {
                    break snapshot.clone();
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the restore capture snapshot never appeared: {snapshots:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        assert!(
            restore_capture["runId"].is_null(),
            "the restore capture is an A6b immediate snapshot: {restore_capture:?}"
        );
        let capture_entry = &restore_capture["entries"][0];
        assert_eq!(capture_entry["action"], json!("replace"));
        assert_eq!(
            capture_entry["artifactId"],
            json!(art_b),
            "the capture reuses the plan target's artifact"
        );
        assert_eq!(ledger_count(&rt, None), 0, "the capture row is consumed");

        // t3：write f=C（precondition revB——磁盘是恢复后的 B）→ S_c（replace，
        // before=revB）。
        let t3 = dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "f.txt",
                b"C",
                json!({"state": "revision", "revision": rev_b}),
            ),
        )
        .await;
        let rev_c = t3["entry"]["revision"].as_str().unwrap().to_owned();
        let snapshots = listed_snapshots(&rt, &project_id).await;
        let s_c = snapshots
            .iter()
            .find(|s| {
                s["entries"].as_array().is_some_and(|entries| {
                    entries.iter().any(|e| {
                        e["path"]["relativePath"]["value"] == json!("f.txt")
                            && e["afterRevision"] == json!(rev_c)
                    })
                })
            })
            .unwrap_or_else(|| panic!("the C snapshot: {snapshots:?}"))["snapshotId"]
            .as_str()
            .unwrap()
            .to_owned();

        // 决定性断言：plan([S_c], before) 的目标 = 恢复后的 B（S_restore 的
        // after），不是恢复快照 R 的陈旧 A（修复前的时间线缺口）。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s_c],
                "direction": "before",
            }),
        )
        .await;
        let entries = plan["entries"].as_array().unwrap().clone();
        assert_eq!(
            entries.len(),
            1,
            "f.txt is the only touched path: {entries:?}"
        );
        assert_eq!(entries[0]["action"], json!("replace"));
        assert_eq!(
            entries[0]["artifactId"],
            json!(art_b),
            "the before target is the post-restore B, not the recovery snapshot's stale A"
        );
        assert!(
            plan["plan"]["conflicts"].as_array().unwrap().is_empty(),
            "no conflicts: {plan:?}"
        );

        // apply → f=B（旧实现写 A——恢复前内容被当作「之前」静默恢复）。
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        dispatch_submit(
            &rt,
            "snapshots.restore",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "restore-capture-apply",
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if read_workspace_file(&workspace, "f.txt").as_deref() == Some(&b"B"[..]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the before-restore never applied the post-restore B (old code wrote A)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// dsh r6 P1#2 链校验负例：before 回溯找到候选、但候选的 after_revision
    /// ≠ entry 的 before_revision（两次触碰之间的变更未被捕获——这里用绕
    /// 过网关的直写制造历史缺页）→ SNAPSHOT_UNAVAILABLE 明确冲突，不静默
    /// 恢复缺页前的陈旧内容（旧实现会恢复 t0 的 A）。
    #[tokio::test]
    async fn before_backtrack_chain_mismatch_surfaces_snapshot_unavailable() {
        let (_tmp, rt, workspace, project_id) = setup("snap-chain-gap");

        // t0：write f=A → S0（create，after=revA）。
        dispatch_ok(
            &rt,
            "files.write",
            write_input(&project_id, "f.txt", b"A", json!({"state": "absent"})),
        )
        .await;
        let rev_a = file_revision_of_bytes(b"A");

        // t0.5：绕过网关直写 f=B——捕获面看不到这次变更（历史缺页）。
        std::fs::write(workspace.join("f.txt"), b"B").unwrap();
        let rev_b = file_revision_of_bytes(b"B");
        assert_ne!(
            rev_a, rev_b,
            "the gap content differs from the captured t0 content"
        );

        // t1：write f=C（precondition revB——门内观测的当前态）→ S1（replace，
        // before=revB）。
        let t1 = dispatch_ok(
            &rt,
            "files.write",
            write_input(
                &project_id,
                "f.txt",
                b"C",
                json!({"state": "revision", "revision": rev_b}),
            ),
        )
        .await;
        let rev_c = t1["entry"]["revision"].as_str().unwrap().to_owned();
        let snapshots = listed_snapshots(&rt, &project_id).await;
        let s1 = snapshots
            .iter()
            .find(|s| {
                s["entries"].as_array().is_some_and(|entries| {
                    entries.iter().any(|e| {
                        e["path"]["relativePath"]["value"] == json!("f.txt")
                            && e["afterRevision"] == json!(rev_c)
                    })
                })
            })
            .unwrap_or_else(|| panic!("the C snapshot: {snapshots:?}"))["snapshotId"]
            .as_str()
            .unwrap()
            .to_owned();

        // plan([S1], before)：唯一更早候选 S0 的 after=revA ≠ S1 的
        // before=revB → 链断裂，明确冲突（历史缺页不伪装可恢复）。
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotIds": [s1],
                "direction": "before",
            }),
        )
        .await;
        let conflicts = plan["plan"]["conflicts"].as_array().unwrap().clone();
        assert_eq!(
            conflicts.len(),
            1,
            "exactly the gap path conflicts: {plan:?}"
        );
        assert_eq!(conflicts[0]["code"], json!("SNAPSHOT_UNAVAILABLE"));
        assert_eq!(conflicts[0]["resourceId"], json!("f.txt"));
        assert_eq!(conflicts[0]["observedRevision"], json!(rev_b));
        assert!(
            plan["entries"].as_array().unwrap().is_empty(),
            "a broken before-chain must not enter the plan: {plan:?}"
        );
    }

    // -----------------------------------------------------------------------
    // dsh r5-snap-wire P1-1：旧库升级列守卫
    // -----------------------------------------------------------------------

    /// pre-r4 旧形状（两表都无锚列）+ 新二进制：重新打开后守卫补列，旧数
    /// 据保留（order_key=''、entry 锚按父快照 order_key 回填 = ''），且
    /// files.write 照常成功——无守卫时 `INSERT ... order_key` 报 no such
    /// column，整事务回滚（台账行同丢），写「落盘却报 UNAVAILABLE」。
    #[tokio::test]
    async fn legacy_pre_r4_db_gains_order_key_and_entry_anchor_columns() {
        let (tmp, rt, _workspace, project_id) = setup("snap-migrate-prer4");
        let home = tmp.0.join("pd-home");
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute_batch("DROP TABLE v1_file_snapshot_entries;")
                .unwrap();
            conn.execute_batch("DROP TABLE v1_file_snapshots;").unwrap();
            // pre-r4 旧形状：无 order_key、无 entry 锚列。
            conn.execute_batch(
                "CREATE TABLE v1_file_snapshots (
                    snapshot_id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL,
                    revision TEXT NOT NULL,
                    run_id TEXT,
                    turn_id TEXT,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE v1_file_snapshot_entries (
                    snapshot_id TEXT NOT NULL,
                    path_bytes BLOB NOT NULL,
                    before_revision TEXT,
                    after_revision TEXT,
                    action TEXT NOT NULL,
                    artifact_id TEXT,
                    PRIMARY KEY (snapshot_id, path_bytes)
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO v1_file_snapshots
                 VALUES ('snap-legacy', ?1, 'rev-legacy', NULL, NULL, '2030-01-01T00:00:00+00:00')",
                params![project_id],
            )
            .unwrap();
            // X'6F6C642E747874' = "old.txt"。
            conn.execute(
                "INSERT INTO v1_file_snapshot_entries
                 VALUES ('snap-legacy', X'6F6C642E747874', NULL, 'rev-old', 'create', NULL)",
                [],
            )
            .unwrap();
        }

        // 旧库 + 新二进制：重新打开触发 init_schema → 列守卫迁移。
        let rt2 = open_runtime(&home);
        {
            let conn = rt2.store().conn.lock().expect("sqlite mutex");
            assert!(
                column_exists(&conn, "v1_file_snapshots", "order_key"),
                "the guard adds the missing order_key column"
            );
            assert!(column_exists(
                &conn,
                "v1_file_snapshot_entries",
                "first_occurred_at"
            ));
            assert!(column_exists(
                &conn,
                "v1_file_snapshot_entries",
                "last_occurred_at"
            ));
            // 旧数据保留；pre-r4 的 order_key 本身刚以 '' 补列，锚回填同为 ''。
            let order_key: String = conn
                .query_row(
                    "SELECT order_key FROM v1_file_snapshots WHERE snapshot_id = 'snap-legacy'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(order_key, "");
            let anchors: (String, String) = conn
                .query_row(
                    "SELECT first_occurred_at, last_occurred_at
                     FROM v1_file_snapshot_entries WHERE snapshot_id = 'snap-legacy'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(anchors, (String::new(), String::new()));
        }

        // files.write 成功 + 快照含 order_key / entry 锚（决定性：无守卫时
        // 这一步把快照域整个打死）。
        dispatch_ok(
            &rt2,
            "files.write",
            write_input(&project_id, "new.txt", b"hi", json!({"state": "absent"})),
        )
        .await;
        let snapshots = listed_snapshots(&rt2, &project_id).await;
        assert_eq!(snapshots.len(), 2, "legacy + fresh: {snapshots:?}");
        let fresh = snapshots
            .iter()
            .find(|s| s["snapshotId"] != json!("snap-legacy"))
            .expect("the fresh A6b snapshot");
        let sid = fresh["snapshotId"].as_str().unwrap().to_owned();
        let order_key = snapshot_order_key(&rt2, &sid);
        assert!(
            !order_key.is_empty(),
            "the fresh snapshot carries a real order_key"
        );
        assert_eq!(
            entry_anchors(&rt2, &sid, "new.txt"),
            (order_key.clone(), order_key),
            "the single-touch A6b entry anchors equal the write's change time"
        );
        assert_eq!(ledger_count(&rt2, None), 0, "the ledger row is consumed");
    }

    /// r4→r5 升级面：`v1_file_snapshots` 已有真实 order_key、entries 缺
    /// entry 锚列——迁移补列后旧条目的锚按**父快照 order_key** 回填（保持
    /// r4 定序语义），而非以 '' 锚在回溯里恒排所有真实时刻之前。幂等：再
    /// 次打开不重写已回填的锚。
    #[tokio::test]
    async fn legacy_r4_entries_backfill_anchors_from_parent_order_key() {
        let (tmp, rt, _workspace, project_id) = setup("snap-migrate-r4");
        let home = tmp.0.join("pd-home");
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            // r4 形状：snapshots 已有 order_key（r5 守卫对它是 no-op）。
            conn.execute_batch("DROP TABLE v1_file_snapshot_entries;")
                .unwrap();
            conn.execute_batch(
                "CREATE TABLE v1_file_snapshot_entries (
                    snapshot_id TEXT NOT NULL,
                    path_bytes BLOB NOT NULL,
                    before_revision TEXT,
                    after_revision TEXT,
                    action TEXT NOT NULL,
                    artifact_id TEXT,
                    PRIMARY KEY (snapshot_id, path_bytes)
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO v1_file_snapshots
                    (snapshot_id, project_id, revision, run_id, turn_id, created_at, order_key)
                 VALUES ('snap-r4', ?1, 'rev-r4', NULL, NULL,
                         '2030-01-01T00:00:00+00:00', '2030-01-02T00:00:00+00:00')",
                params![project_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO v1_file_snapshot_entries
                 VALUES ('snap-r4', X'6F6C642E747874', NULL, 'rev-old', 'create', NULL)",
                [],
            )
            .unwrap();
        }

        let rt2 = open_runtime(&home);
        {
            let conn = rt2.store().conn.lock().expect("sqlite mutex");
            let anchors: (String, String) = conn
                .query_row(
                    "SELECT first_occurred_at, last_occurred_at
                     FROM v1_file_snapshot_entries WHERE snapshot_id = 'snap-r4'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                anchors,
                (
                    "2030-01-02T00:00:00+00:00".to_owned(),
                    "2030-01-02T00:00:00+00:00".to_owned()
                ),
                "legacy entries keep the r4 anchor (parent order_key), not ''"
            );
        }
        // 幂等：再次打开不触碰已回填的锚（守卫只对**刚补**的列回填，列已
        // 在 → ALTER 与 UPDATE 都跳过）。
        let rt3 = open_runtime(&home);
        {
            let conn = rt3.store().conn.lock().expect("sqlite mutex");
            let anchors: (String, String) = conn
                .query_row(
                    "SELECT first_occurred_at, last_occurred_at
                     FROM v1_file_snapshot_entries WHERE snapshot_id = 'snap-r4'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(anchors.0, "2030-01-02T00:00:00+00:00");
        }
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

    async fn await_domain_op(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> (String, Value) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let row: (String, Option<String>) = {
                let conn = rt.store().conn.lock().expect("sqlite mutex");
                conn.query_row(
                    "SELECT state, completion_json FROM v1_domain_operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
            };
            if row.0 == "succeeded" || row.0 == "failed" {
                let json = row
                    .1
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(Value::Null);
                return (row.0, json);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "operation `{operation_id}` timed out"
            );
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
    }

    /// P0#1: symlink 越界读取（plan_restore/stat_current）与悬空/越界/空路径写门防护。
    #[cfg(unix)]
    #[tokio::test]
    async fn p0_restore_and_plan_block_symlink_escape_and_dangling() {
        let (tmp, rt, workspace, project_id) = setup("snap-p0-1-symlink");
        let outside = tmp.0.join("outside-secret-dir");
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, b"host-secret-bytes").unwrap();

        let snap = seed_snapshot(
            &rt,
            &project_id,
            "s1",
            &[("escape/secret.txt", b"snap-secret-bytes", "create")],
            &[],
        );

        // 1) plan_restore: 工作区内父目录被换成指向工作区外的 symlink → PATH_ESCAPE（不泄露外部 SHA-256）。
        std::os::unix::fs::symlink(&outside, workspace.join("escape")).unwrap();
        let err = dispatch_err(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotId": snap,
                "direction": "after",
            }),
            None,
        )
        .await;
        assert_eq!(err.code, CODE_PATH_ESCAPE);

        // 2) restore_gate_key: 拒绝空路径与悬空 symlink 父路径。
        let pw = resolve_project(rt.store(), &project_id).unwrap();
        let empty_err = restore_gate_key(&pw, b"").unwrap_err();
        assert_eq!(empty_err.code, CODE_INVALID_ARGUMENT);
        std::os::unix::fs::symlink(outside.join("nonexistent-dir"), workspace.join("dangling"))
            .unwrap();
        let dangling_err = restore_gate_key(&pw, b"dangling/file.txt").unwrap_err();
        assert_eq!(dangling_err.code, CODE_PATH_ESCAPE);

        // 3) execute_restore_inner TOCTOU: plan 时为真目录，plan 后替换为指向工作区外 symlink。
        let real_sub = workspace.join("sub");
        std::fs::create_dir_all(&real_sub).unwrap();
        let snap2 = seed_snapshot(
            &rt,
            &project_id,
            "s2",
            &[("sub/victim.txt", b"restored-payload", "create")],
            &[],
        );
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotId": snap2,
                "direction": "after",
            }),
        )
        .await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();

        std::fs::remove_dir_all(&real_sub).unwrap();
        std::os::unix::fs::symlink(&outside, &real_sub).unwrap();
        let err = execute_restore_inner(rt.store(), &plan_id).unwrap_err();
        assert_eq!(err.code, CODE_PATH_ESCAPE);
        assert!(
            !outside.join("victim.txt").exists(),
            "outside directory must remain untouched"
        );
        assert_eq!(
            std::fs::read(&outside_file).unwrap(),
            b"host-secret-bytes".to_vec()
        );
    }

    struct RollbackInjectionGuard;
    impl Drop for RollbackInjectionGuard {
        fn drop(&mut self) {
            injected_rollback_failure_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
    }

    /// P0#2: 回滚覆盖 Created / Replaced / Deleted 全动作，仅全回滚成功报 rolledBack、
    /// 过滤回滚路径的 files.changed 事件；回滚部分失败时 PARTIAL_FAILURE 保留 recoverySnapshotId。
    #[tokio::test]
    async fn p0_restore_rollback_tracks_all_ops_and_partial_failure_keeps_recovery_id() {
        // Part A: full rollback across Created, Replaced, Deleted + zero temp/event leftovers.
        {
            let (_tmp, rt, workspace, project_id) = setup("snap-p0-2a-rollback");
            std::fs::write(workspace.join("b_replace.txt"), b"orig-replace").unwrap();
            std::fs::write(workspace.join("d_delete.txt"), b"orig-delete").unwrap();
            // z_fail 在磁盘为非空目录（stat_current -> None），Step 3 renameat 覆盖目录必报 EISDIR/ENOTEMPTY。
            std::fs::create_dir_all(workspace.join("z_fail")).unwrap();
            std::fs::write(workspace.join("z_fail/hold.txt"), b"keep").unwrap();

            let s1 = seed_snapshot(
                &rt,
                &project_id,
                "s1",
                &[
                    ("a_create.txt", b"new-create", "create"),
                    ("b_replace.txt", b"new-replace", "replace"),
                    ("z_fail", b"will-fail-rename", "create"),
                ],
                &[("d_delete.txt", "frev-del")],
            );
            let plan = dispatch_ok(
                &rt,
                "snapshots.plan_restore",
                json!({
                    "projectId": project_id,
                    "snapshotId": s1,
                    "direction": "after",
                }),
            )
            .await;
            let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
            let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
            let receipt = dispatch_submit(
                &rt,
                "snapshots.restore",
                json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
                "p0-2a-key",
            )
            .await;
            let op_id = receipt["operationId"].as_str().unwrap().to_owned();
            let (state, completion) = await_domain_op(&rt, &op_id).await;
            assert_eq!(state, "succeeded");
            assert_eq!(completion["outcome"], json!("rolledBack"));
            assert_eq!(completion["restoredPaths"], json!([]));
            assert_eq!(
                completion["failedPaths"].as_array().unwrap().len(),
                1,
                "only z_fail failed commit: {completion:?}"
            );
            assert!(
                !workspace.join("a_create.txt").exists(),
                "Created op must roll back"
            );
            assert_eq!(
                read_workspace_file(&workspace, "b_replace.txt").as_deref(),
                Some(b"orig-replace".as_slice()),
                "Replaced op must roll back"
            );
            assert_eq!(
                read_workspace_file(&workspace, "d_delete.txt").as_deref(),
                Some(b"orig-delete".as_slice()),
                "Deleted op must roll back"
            );
            let temp_leftovers: Vec<_> = std::fs::read_dir(&workspace)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(".agentcpm-pd-restore-")
                })
                .collect();
            assert!(
                temp_leftovers.is_empty(),
                "all temp staging files must be removed"
            );
            let files_changed_count: i64 = {
                let conn = rt.store().conn.lock().expect("sqlite mutex");
                conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'files.changed'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(
                files_changed_count, 0,
                "rolledBack restore must emit zero files.changed events"
            );
        }

        // Part B: rollback fails partway → PARTIAL_FAILURE preserves recoverySnapshotId & details.
        {
            let (_tmp, rt, workspace, project_id) = setup("snap-p0-2b-partial");
            let _guard = RollbackInjectionGuard;
            injected_rollback_failure_registry()
                .lock()
                .unwrap()
                .push((project_id.clone(), b"b_replace.txt".to_vec()));

            std::fs::write(workspace.join("b_replace.txt"), b"orig-replace").unwrap();
            std::fs::create_dir_all(workspace.join("z_fail")).unwrap();
            std::fs::write(workspace.join("z_fail/hold.txt"), b"keep").unwrap();

            let s1 = seed_snapshot(
                &rt,
                &project_id,
                "s1",
                &[
                    ("a_create.txt", b"new-create", "create"),
                    ("b_replace.txt", b"new-replace", "replace"),
                    ("z_fail", b"will-fail-rename", "create"),
                ],
                &[],
            );
            let plan = dispatch_ok(
                &rt,
                "snapshots.plan_restore",
                json!({
                    "projectId": project_id,
                    "snapshotId": s1,
                    "direction": "after",
                }),
            )
            .await;
            let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
            let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
            let receipt = dispatch_submit(
                &rt,
                "snapshots.restore",
                json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
                "p0-2b-key",
            )
            .await;
            let op_id = receipt["operationId"].as_str().unwrap().to_owned();
            let (state, completion) = await_domain_op(&rt, &op_id).await;
            assert_eq!(state, "failed");
            assert_eq!(completion["errorCode"], json!(CODE_PARTIAL_FAILURE));
            assert_eq!(completion["outcome"], json!("partial"));
            assert!(
                completion["recoverySnapshotId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "PARTIAL_FAILURE must preserve recoverySnapshotId: {completion:?}"
            );
            assert!(
                !completion["rollbackFailedPaths"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "PARTIAL_FAILURE must report rollbackFailedPaths: {completion:?}"
            );
            assert!(
                !workspace.join("a_create.txt").exists(),
                "unfailed Created rollback still rolled back"
            );
        }
    }

    /// P1#7 & P1#8: snapshots.restore 并发同键幂等不误写 rejection + claim 第二道防线 + 暂存失败 RAII 清理。
    #[tokio::test]
    async fn p1_restore_concurrent_idempotency_claim_and_staging_cleanup() {
        let (_tmp, rt, workspace, project_id) = setup("snap-p1-7-8");
        std::fs::write(workspace.join("a.txt"), b"old-a").unwrap();
        let snap = seed_snapshot(
            &rt,
            &project_id,
            "s1",
            &[("a.txt", b"new-a", "replace")],
            &[],
        );
        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotId": snap,
                "direction": "after",
            }),
        )
        .await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        let input = json!({"planId": plan_id, "expectedPlanRevision": plan_rev});

        let (r1, r2) = tokio::join!(
            rt.dispatch(
                "snapshots.restore",
                input.clone(),
                Some("concurrent-restore-key"),
                None,
            ),
            rt.dispatch(
                "snapshots.restore",
                input.clone(),
                Some("concurrent-restore-key"),
                None,
            ),
        );
        let (_, rec1) = r1.expect("first submit must accept or deduplicate");
        let (_, rec2) = r2.expect("second submit must accept or deduplicate");
        assert_eq!(rec1["operationId"], rec2["operationId"]);
        assert_ne!(
            rec1["deduplicated"].as_bool().unwrap(),
            rec2["deduplicated"].as_bool().unwrap(),
            "exactly one must be fresh and one deduplicated"
        );
        let op_id = rec1["operationId"].as_str().unwrap().to_owned();
        let (state, completion) = await_domain_op(&rt, &op_id).await;
        assert_eq!(state, "succeeded");
        assert_eq!(completion["outcome"], json!("complete"));
        assert_eq!(
            read_workspace_file(&workspace, "a.txt").as_deref(),
            Some(b"new-a".as_slice())
        );

        // claim 第二道防线：重入 execute_restore 返回 UNAVAILABLE，不覆写终态结果。
        let reclaim_err = execute_restore(rt.store(), &op_id, &plan_id).unwrap_err();
        assert_eq!(reclaim_err.code, CODE_UNAVAILABLE);
        let (state_after, completion_after) = await_domain_op(&rt, &op_id).await;
        assert_eq!(state_after, "succeeded");
        assert_eq!(completion_after, completion);

        // Pre-commit staging failure cleanup: existing regular file blocks parent directory creation.
        std::fs::write(workspace.join("blocked_parent"), b"not-a-dir").unwrap();
        let snap2 = seed_snapshot(
            &rt,
            &project_id,
            "s2",
            &[
                ("ok.txt", b"ok-staged", "create"),
                ("blocked_parent/child.txt", b"cannot-stage", "create"),
            ],
            &[],
        );
        let plan2 = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotId": snap2,
                "direction": "after",
            }),
        )
        .await;
        let plan2_id = plan2["plan"]["planId"].as_str().unwrap().to_owned();
        let outcome = execute_restore_inner(rt.store(), &plan2_id).unwrap();
        assert_eq!(outcome["outcome"], json!("partial"));
        assert_eq!(outcome["restoredPaths"], json!([]));
        assert!(!workspace.join("ok.txt").exists(), "abort before commit");
        let temp_leftovers: Vec<_> = std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".agentcpm-pd-restore-")
            })
            .collect();
        assert!(temp_leftovers.is_empty());
    }

    /// P1#9: plan_restore(after) 对非 delete 且缺产物的目录/符号链接条目报告 SNAPSHOT_UNAVAILABLE。
    #[tokio::test]
    async fn p1_plan_restore_after_non_delete_without_artifact_conflicts() {
        let (_tmp, rt, workspace, project_id) = setup("snap-p1-9-dir");
        std::fs::write(workspace.join("moved_dir"), b"regular-keep-safe").unwrap();
        std::fs::write(workspace.join("ephemeral.txt"), b"should-delete").unwrap();

        let at = now_rfc3339();
        let snap_id = commit_turn_snapshot(
            rt.store(),
            &project_id,
            Some(("run-p1-9", "turn-p1-9")),
            &at,
            &anchored_at(
                vec![
                    CapturedFileChange {
                        path: b"moved_dir".to_vec(),
                        before_revision: None,
                        after_revision: Some("drev-dir-1".to_owned()),
                        action: "create",
                        artifact_id: None,
                    },
                    CapturedFileChange {
                        path: b"ephemeral.txt".to_vec(),
                        before_revision: None,
                        after_revision: None,
                        action: "create",
                        artifact_id: None,
                    },
                ],
                &at,
            ),
        )
        .unwrap();

        let plan = dispatch_ok(
            &rt,
            "snapshots.plan_restore",
            json!({
                "projectId": project_id,
                "snapshotId": snap_id,
                "direction": "after",
            }),
        )
        .await;
        let targets = plan["targets"].as_array().unwrap();
        let conflicts = plan["conflicts"].as_array().unwrap();
        assert!(
            targets
                .iter()
                .all(|t| t["path"]["relativePath"]["value"] != json!("moved_dir")),
            "directory entry without artifact must never become delete target: {plan:?}"
        );
        assert!(
            targets.iter().any(
                |t| t["path"]["relativePath"]["value"] == json!("ephemeral.txt")
                    && t["action"] == json!("delete")
            ),
            "folded net-zero absent create must remain delete target: {plan:?}"
        );
        assert!(
            conflicts.iter().any(|c| {
                c["path"]["relativePath"]["value"] == json!("moved_dir")
                    && c["code"] == json!(CODE_SNAPSHOT_UNAVAILABLE)
            }),
            "missing-artifact non-delete entry must conflict with SNAPSHOT_UNAVAILABLE: {plan:?}"
        );
    }
}
