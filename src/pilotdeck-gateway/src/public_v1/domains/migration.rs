//! `migration.*` — 旧数据迁移闭环（t020，S4；capability a039）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `migration.stage` / `verify`
//! / `commit` / `recover` 四条（`migration.plan` 是 CLI local 操作，落在
//! `agentcpm-pd-cli` 的 local_ops，经 `data_backup.rs` 的共享扫描核心复用同
//! 一套分类）；设计正本是 `docs/plan/008_t020-legacy-data-migration.md`。
//!
//! ## 六步闭环中的四步（008 §3/§4）
//!
//! - `stage`（async）：受理前复验（备份登记存在、源根再算 revision →
//!   `PLAN_STALE`、写者活跃 `WRITER_ACTIVE`、备份与计划的 sourceRevision 同
//!   链）；worker 在**隔离暂存目录** `<home>/v1/staging/<stagingId>/`
//!   （staging.sqlite + idmap.jsonl + staging.json）做转换——**不写
//!   `<home>/v1/state.sqlite`**，源根绝不改写；受理后 worker 再复验一次源
//!   revision（受理窗口内的变化 → 操作失败，零目标副作用）。
//! - `verify`（async）：六项核对（008 §7：计数 / ID 映射 / 引用 / 内容
//!   hash / 目标可读性 / unsupported 对账）→ `verified` + `failures[]` +
//!   `reportArtifactId`；`verified=false` 不是操作失败，但挡住 commit
//!   （`VERIFICATION_REQUIRED`）。
//! - `commit`（async）：目标 revision CAS（`REVISION_CONFLICT`）+ 活跃 run
//!   拒绝（`WRITER_ACTIVE`）+ 匹配 staging 的成功核验（否则
//!   `VERIFICATION_REQUIRED`）；持 store 互斥（单写者锁）在一个事务里把
//!   staging 行合并进 `state.sqlite` + 写 `legacy_id_map` + 递增
//!   `meta.revision`，并物化 `compat/legacy-id-map.json`（0600）+ `.sha256`
//!   导出副本；发布前目标状态记入恢复制品（`recoveryArtifactId`）。
//!   commit 后不启动对源根的任何写（a039「不允许新旧双写同一数据根」）。
//! - `recover`（async）：`discardUnpublished`（只删本 staging 自己的隔离目
//!   录；不删源数据或未归属资源）与 `restorePreviousTarget`（commit 后回
//!   滚；目标有新写入 → `recovered=false` + 人工 `recoveryInstructions`，
//!   不强回滚）。未知 stagingId → `RESOURCE_NOT_OWNED`。
//!
//! ## 转换面（008 §6，双段式：T2 采纳建议方案）
//!
//! stage 转换：项目登记（`.cwd` 为权威）、会话（含子代理转写，同面转
//! 换）、历史消息（accepted_input / assistant_message / durable_message →
//! history_entries 的 text 块投影）、run 记录（registry.json 终态导入）、
//! cron 任务（tasks.json → v1_schedules 行，session 绑定留给用户重绑）。
//! skills / memory / 凭据 / 配置**不并入** stage（plan 的 changes 以
//! `manual-via-<operation>` 预告），commit 后由用户经各自领域操作显式导
//! 入；always-on 整类 unsupported，运行期杂项整类 skip。
//!
//! ## 存储
//!
//! - `legacy_id_map`（reg.schema；DDL = 008 §5.3 原文）：commit 是唯一写
//!   入方；`compat/legacy-id-map.json` 是 shim 只读的导出副本（0600 + 同
//!   目录 sha256）。
//! - `v1_migration_stagings`（reg.schema）：staging 状态机（staging →
//!   staged → verified → committed → restored / discarded / failed）。
//! - 域操作受理/完成走 `v1_domain_operations` seam（schedules 域共享）。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::bootstrap::atomic_write_file;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::data_backup::{
    self, ScanDepth, load_registered_backup, scan_source_root, target_identity,
};
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 本实现接受的目标格式版本（staging 一致性 / plan 校验同值）。
pub const TARGET_FORMAT_VERSION: &str = "agentcpm-pd.v1/1";

const DDL_LEGACY_ID_MAP: &str = "CREATE TABLE IF NOT EXISTS legacy_id_map (
    legacy_kind  TEXT NOT NULL,
    legacy_scope TEXT NOT NULL,
    legacy_id    TEXT NOT NULL,
    legacy_storage_key TEXT NOT NULL,
    new_id       TEXT NOT NULL,
    source_revision  TEXT NOT NULL,
    migration_id TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (legacy_kind, legacy_scope, legacy_id)
)";
const DDL_LEGACY_ID_MAP_INDEX: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_legacy_id_map_new ON legacy_id_map(new_id)";
const DDL_STAGINGS: &str = "CREATE TABLE IF NOT EXISTS v1_migration_stagings (
    staging_id TEXT PRIMARY KEY,
    plan_id TEXT NOT NULL,
    plan_hash TEXT NOT NULL,
    backup_artifact_id TEXT NOT NULL,
    source_identity TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    target_format_version TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

/// 隔离 staging 内的转换库（v1 行形状的镜像，commit 原样搬入 state.sqlite）。
const STAGING_DDL: &str = "
CREATE TABLE IF NOT EXISTS stg_projects (
    project_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root_id TEXT NOT NULL,
    absolute_path TEXT NOT NULL,
    legacy_project_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS stg_sessions (
    session_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    tags_json TEXT NOT NULL,
    legacy_session_key TEXT NOT NULL,
    legacy_project_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS stg_history (
    session_id TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    turn_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (session_id, entry_id)
);
CREATE TABLE IF NOT EXISTS stg_runs (
    run_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    state TEXT NOT NULL,
    legacy_run_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS stg_schedules (
    schedule_id TEXT PRIMARY KEY,
    legacy_task_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    expression TEXT NOT NULL,
    timezone TEXT NOT NULL,
    message TEXT NOT NULL,
    enabled INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS stg_id_map (
    legacy_kind TEXT NOT NULL,
    legacy_scope TEXT NOT NULL,
    legacy_id TEXT NOT NULL,
    legacy_storage_key TEXT NOT NULL,
    new_id TEXT NOT NULL,
    PRIMARY KEY (legacy_kind, legacy_scope, legacy_id)
);
";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("legacy_id_map", DDL_LEGACY_ID_MAP);
    reg.schema("v1_migration_stagings", DDL_STAGINGS);

    reg.handler("migration.stage", |rt, input, key, _rev| {
        Box::pin(migration_stage(rt, input, key))
    });
    reg.handler("migration.verify", |rt, input, key, _rev| {
        Box::pin(migration_verify(rt, input, key))
    });
    reg.handler("migration.commit", |rt, input, key, _rev| {
        Box::pin(migration_commit(rt, input, key))
    });
    reg.handler("migration.recover", |rt, input, key, _rev| {
        Box::pin(migration_recover(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 错误构造
// ---------------------------------------------------------------------------

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn io_unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message,
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn integrity_error(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "INTEGRITY_ERROR",
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        "NOT_FOUND",
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn plan_stale(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "PLAN_STALE",
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn verification_required(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "VERIFICATION_REQUIRED",
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict(expected: &str, actual: &str) -> ContractError {
    ContractError::new(
        "REVISION_CONFLICT",
        format!(
            "expected target revision `{expected}` does not match current `{actual}`; re-read the target and retry"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn writer_active(detail: impl std::fmt::Display) -> ContractError {
    ContractError::new(
        "WRITER_ACTIVE",
        format!(
            "the source data root still has an active writer ({detail}); stop it before migrating"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn target_writer_active() -> ContractError {
    ContractError::new(
        "WRITER_ACTIVE",
        "the target instance has non-terminal runs; let them finish or stop the service before switching",
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn resource_not_owned(staging_id: &str) -> ContractError {
    ContractError::new(
        "RESOURCE_NOT_OWNED",
        format!("staging `{staging_id}` is not owned by this instance"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("migration worker task failed: {err}"),
        data: None,
    }
}

fn sql_err(context: &str, err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("{context}: {err}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn push_failure(failures: &mut Vec<Value>, entry: &str, code: &str, message: String) {
    failures.push(json!({"entryId": entry, "code": code, "message": message}));
}

// ---------------------------------------------------------------------------
// 目标 revision / 活跃 run（impl V1Store 块，本域私有方法）
// ---------------------------------------------------------------------------

impl V1Store {
    /// 目标全局 revision（`meta.rev_counter`）与是否存在非终态 run
    /// （commit/recover 的 WRITER_ACTIVE 判据）。
    pub(crate) fn migration_target_state(&self) -> Result<(String, bool), ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let revision = current_target_revision(&conn)?;
        let active: i64 = conn
            .query_row("SELECT COUNT(*) FROM runs WHERE terminal = 0", [], |r| {
                r.get(0)
            })
            .map_err(|e| sql_err("count active runs", e))?;
        Ok((revision, active > 0))
    }
}

fn current_target_revision(conn: &Connection) -> Result<String, ContractError> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| sql_err("read rev_counter", e))?;
    Ok(format!("rev-{:08}", current.unwrap_or(1)))
}

fn bump_target_revision(conn: &Connection) -> Result<String, ContractError> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| sql_err("read rev_counter", e))?;
    let next = current.unwrap_or(1) + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(|e| sql_err("bump rev_counter", e))?;
    Ok(format!("rev-{next:08}"))
}

fn set_target_revision(conn: &Connection, value: &str) -> Result<(), ContractError> {
    let parsed: i64 = value
        .strip_prefix("rev-")
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![parsed.to_string()],
    )
    .map_err(|e| sql_err("restore rev_counter", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 域单例（op seam + 自有连接）
// ---------------------------------------------------------------------------

struct MigrationDomain {
    /// 域操作受理/完成 seam（`v1_domain_operations`，schedules 共享纪律）。
    ops: DomainDb,
    /// 本域自有连接（staging 登记表；commit 事务用 store 自己的连接）。
    conn: Mutex<Connection>,
}

fn migration_domain(home: &Path) -> Result<Arc<MigrationDomain>, ContractError> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<MigrationDomain>>>> = OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let v1 = home.join("v1");
    std::fs::create_dir_all(&v1)
        .map_err(|e| io_unavailable(format!("mkdir {}: {e}", v1.display())))?;
    let conn =
        Connection::open(v1.join("state.sqlite")).map_err(|e| sql_err("open state db", e))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(|e| sql_err("pragma state db", e))?;
    conn.execute_batch(DDL_STAGINGS)
        .map_err(|e| sql_err("staging registry ddl", e))?;
    conn.execute_batch(DDL_LEGACY_ID_MAP)
        .map_err(|e| sql_err("legacy id map ddl", e))?;
    conn.execute_batch(DDL_LEGACY_ID_MAP_INDEX)
        .map_err(|e| sql_err("legacy id map index", e))?;
    let domain = Arc::new(MigrationDomain {
        ops: DomainDb::open(home, &[DDL_DOMAIN_OPERATIONS])?,
        conn: Mutex::new(conn),
    });
    registry.insert(home.to_path_buf(), Arc::downgrade(&domain));
    Ok(domain)
}

/// staging.json 写互斥（worker 之间串行化 staging 目录内的清单更新）。
fn staging_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// staging 状态行。
#[derive(Clone)]
struct StagingRow {
    staging_id: String,
    plan_id: String,
    plan_hash: String,
    backup_artifact_id: String,
    source_identity: String,
    source_revision: String,
    state: String,
}

fn get_staging_row(
    conn: &Connection,
    staging_id: &str,
) -> Result<Option<StagingRow>, ContractError> {
    let row = conn
        .query_row(
            "SELECT staging_id, plan_id, plan_hash, backup_artifact_id, source_identity,
                    source_revision, state
             FROM v1_migration_stagings WHERE staging_id = ?1",
            params![staging_id],
            |r| {
                Ok(StagingRow {
                    staging_id: r.get(0)?,
                    plan_id: r.get(1)?,
                    plan_hash: r.get(2)?,
                    backup_artifact_id: r.get(3)?,
                    source_identity: r.get(4)?,
                    source_revision: r.get(5)?,
                    state: r.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|e| sql_err("read staging row", e))?;
    Ok(row)
}

fn set_staging_state(
    conn: &Connection,
    staging_id: &str,
    state: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_migration_stagings SET state = ?2, updated_at = ?3 WHERE staging_id = ?1",
        params![staging_id, state, now_rfc3339()],
    )
    .map_err(|e| sql_err("update staging state", e))?;
    Ok(())
}

fn upsert_staging_row(conn: &Connection, row: &StagingRow) -> Result<(), ContractError> {
    conn.execute(
        "INSERT INTO v1_migration_stagings
            (staging_id, plan_id, plan_hash, backup_artifact_id, source_identity,
             source_revision, target_format_version, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
         ON CONFLICT(staging_id) DO UPDATE SET
            state = excluded.state,
            updated_at = excluded.updated_at",
        params![
            row.staging_id,
            row.plan_id,
            row.plan_hash,
            row.backup_artifact_id,
            row.source_identity,
            row.source_revision,
            TARGET_FORMAT_VERSION,
            row.state,
            now_rfc3339(),
        ],
    )
    .map_err(|e| sql_err("write staging row", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// staging 目录（隔离目标的落盘形态）
// ---------------------------------------------------------------------------

fn staging_dir(home: &Path, staging_id: &str) -> PathBuf {
    home.join("v1").join("staging").join(staging_id)
}

/// staging.json：staging 的自描述清单。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StagingManifest {
    staging_id: String,
    plan_id: String,
    plan_hash: String,
    backup_artifact_id: String,
    source_identity: String,
    source_revision: String,
    target_format_version: String,
    backup_checksum: String,
    backup_entry_count: u64,
    converted_entries: u64,
    diagnostics: Vec<Value>,
    unsupported: Vec<String>,
    id_map_artifact: Value,
    verifications: Vec<Value>,
    #[serde(default)]
    commit: Option<Value>,
    created_at: String,
}

fn load_staging_manifest(dir: &Path) -> Result<StagingManifest, ContractError> {
    let path = dir.join("staging.json");
    let raw = std::fs::read_to_string(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            integrity_error(format!("staging manifest missing at {}", path.display()))
        } else {
            io_unavailable(format!("read {}: {err}", path.display()))
        }
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| integrity_error(format!("staging manifest corrupt: {e}")))
}

fn write_staging_manifest(dir: &Path, manifest: &StagingManifest) -> Result<(), ContractError> {
    let path = dir.join("staging.json");
    let bytes = serde_json::to_vec(manifest)
        .map_err(|e| io_unavailable(format!("serialize staging manifest: {e}")))?;
    atomic_write_file(&path, &bytes, false)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// 转换（008 §6）
// ---------------------------------------------------------------------------

/// 旧 run state → v1 终态 run state（008 §6 runs 行；非终态旧记录按
/// interrupted 诚实导入——「只读导入为终态 run」）。
fn map_run_state(legacy: &str) -> &'static str {
    match legacy {
        "completed" | "succeeded" => "succeeded",
        "aborted" | "cancelled" => "cancelled",
        "failed" => "failed",
        "result_unknown" => "result_unknown",
        _ => "interrupted",
    }
}

/// 备份条目 → (rel_path, kind) 对（`data_backup::unsupported_projection`
/// 的输入形态）。
fn entry_pairs(entries: &[data_backup::BackupEntryRecord]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|entry| (entry.entry_id.clone(), entry.kind.clone()))
        .collect()
}

/// 把一个备份转换进隔离 staging。全部写入都在 staging 目录内。
#[allow(clippy::too_many_arguments)]
fn convert_backup_into_staging(
    store: &Arc<V1Store>,
    home: &Path,
    staging_id: &str,
    backup_artifact_id: &str,
    backup_destination: &Path,
    backup_manifest: &Value,
    entries: &[data_backup::BackupEntryRecord],
    plan_id: &str,
    plan_hash: &str,
) -> Result<StagingManifest, ContractError> {
    let dir = staging_dir(home, staging_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| io_unavailable(format!("rebuild staging dir: {e}")))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| io_unavailable(format!("mkdir staging: {e}")))?;

    let conn =
        Connection::open(dir.join("staging.sqlite")).map_err(|e| sql_err("open staging db", e))?;
    conn.execute_batch(STAGING_DDL)
        .map_err(|e| sql_err("staging ddl", e))?;

    let mut diagnostics: Vec<Value> = Vec::new();
    let push_diag = |diagnostics: &mut Vec<Value>, code: &str, message: String| {
        diagnostics.push(json!({"code": code, "message": message}));
    };

    // 1) 项目（A3/A4：.cwd 为权威；缺失 → 诊断 + 空 root 回退）。
    let mut project_dirs: BTreeMap<String, ()> = BTreeMap::new();
    for entry in entries {
        if let Some(rest) = entry.entry_id.strip_prefix("projects/")
            && let Some(project) = rest.split('/').next()
        {
            project_dirs.insert(project.to_owned(), ());
        }
    }
    let mut new_project_of: HashMap<String, String> = HashMap::new();
    for legacy_project in project_dirs.keys() {
        let cwd_entry = entries
            .iter()
            .find(|e| e.entry_id == format!("projects/{legacy_project}/.cwd"));
        let root_path = match cwd_entry {
            Some(record) => {
                let bytes = data_backup::backup_entry_bytes(backup_destination, record)?;
                String::from_utf8_lossy(&bytes).trim().to_owned()
            }
            None => {
                push_diag(
                    &mut diagnostics,
                    "project-cwd-missing",
                    format!(
                        "project `{legacy_project}` has no .cwd marker; its registration carries no root path"
                    ),
                );
                String::new()
            }
        };
        let new_project_id = store.next_id();
        let root_id = store.next_id();
        conn.execute(
            "INSERT INTO stg_projects (project_id, name, root_id, absolute_path, legacy_project_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                new_project_id,
                legacy_project,
                root_id,
                root_path,
                legacy_project
            ],
        )
        .map_err(|e| sql_err("insert stg project", e))?;
        conn.execute(
            "INSERT INTO stg_id_map (legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id)
             VALUES ('project', '', ?1, ?2, ?3)",
            params![
                legacy_project,
                format!("projects/{legacy_project}"),
                new_project_id
            ],
        )
        .map_err(|e| sql_err("insert stg id map (project)", e))?;
        new_project_of.insert(legacy_project.clone(), new_project_id);
    }

    // 2) 会话（A5/A7/A8；子代理转写同面转换——008 §6「同上，按子会话归档」）。
    let mut session_keys_seen: HashSet<String> = HashSet::new();
    // legacy sessionKey → (new_session_id, new_project_id)。
    let mut staged_sessions: HashMap<String, (String, String)> = HashMap::new();
    for entry in entries {
        if entry.kind != "session-transcript" && entry.kind != "subagent-transcript" {
            continue;
        }
        let raw = data_backup::backup_entry_bytes(backup_destination, entry)?;
        let legacy_project = entry
            .entry_id
            .strip_prefix("projects/")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default()
            .to_owned();
        let Some(new_project_id) = new_project_of.get(&legacy_project).cloned() else {
            push_diag(
                &mut diagnostics,
                "session-project-unknown",
                format!(
                    "transcript `{}` references unregistered project `{legacy_project}`",
                    entry.entry_id
                ),
            );
            continue;
        };
        // 投影与 plan 的计数同源（data_backup::project_transcript）。
        let projection = data_backup::project_transcript(&raw);
        let Some(session_key) = projection.session_key else {
            push_diag(
                &mut diagnostics,
                "transcript-session-id-missing",
                format!("transcript `{}` carries no sessionId", entry.entry_id),
            );
            continue;
        };
        if !session_keys_seen.insert(session_key.clone()) {
            push_diag(
                &mut diagnostics,
                "slug-collision",
                format!(
                    "session key `{session_key}` appears in more than one transcript (`{}` skipped)",
                    entry.entry_id
                ),
            );
            continue;
        }
        let new_session_id = store.next_id();
        let tags = projection.tag.map(|t| vec![t]).unwrap_or_default();
        let created_at = projection
            .messages
            .first()
            .and_then(|message| {
                (!message.created_at.is_empty()).then(|| message.created_at.clone())
            })
            .unwrap_or_else(now_rfc3339);
        conn.execute(
            "INSERT INTO stg_sessions (session_id, project_id, name, tags_json, legacy_session_key, legacy_project_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                new_session_id,
                new_project_id,
                projection
                    .title
                    .clone()
                    .unwrap_or_else(|| entry.entry_id.clone()),
                serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into()),
                session_key,
                legacy_project,
                created_at,
            ],
        )
        .map_err(|e| sql_err("insert stg session", e))?;
        for (index, message) in projection.messages.iter().enumerate() {
            let entry_id = store.next_id();
            conn.execute(
                "INSERT INTO stg_history (session_id, entry_id, seq, turn_id, role, content_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    new_session_id,
                    entry_id,
                    (index + 1) as i64,
                    message.turn_id,
                    message.role,
                    serde_json::to_string(&message.content).unwrap_or_else(|_| "[]".into()),
                    if message.created_at.is_empty() {
                        now_rfc3339()
                    } else {
                        message.created_at.clone()
                    },
                ],
            )
            .map_err(|e| sql_err("insert stg history", e))?;
        }
        conn.execute(
            "INSERT INTO stg_id_map (legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id)
             VALUES ('session', ?1, ?2, ?3, ?4)",
            params![legacy_project, session_key, entry.entry_id, new_session_id],
        )
        .map_err(|e| sql_err("insert stg id map (session)", e))?;
        staged_sessions.insert(session_key, (new_session_id, new_project_id));
    }

    // 3) run 记录（A10/A11：registry.json 终态导入）。
    if let Some(record) = entries.iter().find(|e| e.entry_id == "runs/registry.json") {
        let raw = data_backup::backup_entry_bytes(backup_destination, record)?;
        let parsed: Value = serde_json::from_slice(&raw)
            .map_err(|e| integrity_error(format!("runs/registry.json is corrupt: {e}")))?;
        match parsed["runs"].as_object() {
            Some(runs_map) => {
                for (legacy_run_id, run) in runs_map {
                    let session_key = run["sessionKey"].as_str().unwrap_or_default();
                    let Some((new_session_id, new_project_id)) = staged_sessions.get(session_key)
                    else {
                        push_diag(
                            &mut diagnostics,
                            "run-session-unknown",
                            format!(
                                "run `{legacy_run_id}` references session key `{session_key}` with no staged session; skipped"
                            ),
                        );
                        continue;
                    };
                    let new_run_id = store.next_id();
                    let state = map_run_state(run["state"].as_str().unwrap_or_default()).to_owned();
                    conn.execute(
                        "INSERT INTO stg_runs (run_id, session_id, project_id, state, legacy_run_id, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            new_run_id,
                            new_session_id,
                            new_project_id,
                            state,
                            legacy_run_id,
                            run["acceptedAt"].as_str().unwrap_or_default(),
                            run["updatedAt"].as_str().unwrap_or_default(),
                        ],
                    )
                    .map_err(|e| sql_err("insert stg run", e))?;
                    conn.execute(
                        "INSERT INTO stg_id_map (legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id)
                         VALUES ('run', ?1, ?2, ?3, ?4)",
                        params![
                            run["projectKey"].as_str().unwrap_or_default(),
                            legacy_run_id,
                            format!("runs/registry.json#{legacy_run_id}"),
                            new_run_id
                        ],
                    )
                    .map_err(|e| sql_err("insert stg id map (run)", e))?;
                }
            }
            None => push_diag(
                &mut diagnostics,
                "run-registry-shape",
                "runs/registry.json has no `runs` object; no run records imported".to_owned(),
            ),
        }
    }

    // 4) cron 任务（A15：tasks.json → v1_schedules 行；session 绑定留空由
    //    用户重绑，008 §6 automation 行）。
    for entry in entries.iter().filter(|e| e.kind == "cron-task") {
        let raw = data_backup::backup_entry_bytes(backup_destination, entry)?;
        let Ok(parsed) = serde_json::from_slice::<Value>(&raw) else {
            push_diag(
                &mut diagnostics,
                "cron-tasks-corrupt",
                format!("`{}` is not valid JSON; skipped", entry.entry_id),
            );
            continue;
        };
        let legacy_project = entry
            .entry_id
            .strip_prefix("cron/projects/")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default()
            .to_owned();
        let Some(new_project_id) = new_project_of.get(&legacy_project).cloned() else {
            push_diag(
                &mut diagnostics,
                "cron-project-unknown",
                format!(
                    "cron tasks `{}` reference unregistered project `{legacy_project}`",
                    entry.entry_id
                ),
            );
            continue;
        };
        let Some(tasks) = parsed["tasks"].as_array() else {
            push_diag(
                &mut diagnostics,
                "cron-tasks-shape",
                format!("`{}` has no `tasks` array; skipped", entry.entry_id),
            );
            continue;
        };
        for task in tasks {
            let legacy_task_id = task["taskId"].as_str().unwrap_or_default();
            if legacy_task_id.is_empty() {
                push_diag(
                    &mut diagnostics,
                    "cron-task-id-missing",
                    format!("a task in `{}` has no taskId; skipped", entry.entry_id),
                );
                continue;
            }
            let schedule = &task["schedule"];
            let schedule_type = schedule["type"].as_str().unwrap_or("cron");
            let (kind, expression) = if schedule_type == "once" {
                (
                    "once",
                    schedule["at"]
                        .as_str()
                        .or(schedule["runAt"].as_str())
                        .unwrap_or_default(),
                )
            } else {
                (
                    "cron",
                    schedule["expression"]
                        .as_str()
                        .or(schedule["at"].as_str())
                        .unwrap_or_default(),
                )
            };
            if expression.is_empty() {
                push_diag(
                    &mut diagnostics,
                    "cron-schedule-missing",
                    format!("task `{legacy_task_id}` has no usable schedule; skipped"),
                );
                continue;
            }
            let status = task["status"].as_str().unwrap_or("active");
            let enabled = !matches!(status, "paused" | "disabled" | "cancelled");
            let new_schedule_id = store.next_id();
            conn.execute(
                "INSERT INTO stg_schedules (schedule_id, legacy_task_id, project_id, kind, expression, timezone, message, enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    new_schedule_id,
                    legacy_task_id,
                    new_project_id,
                    kind,
                    expression,
                    task["timezone"]
                        .as_str()
                        .or(schedule["timezone"].as_str())
                        .unwrap_or("UTC"),
                    task["message"].as_str().unwrap_or_default(),
                    if enabled { 1 } else { 0 },
                ],
            )
            .map_err(|e| sql_err("insert stg schedule", e))?;
            conn.execute(
                "INSERT INTO stg_id_map (legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id)
                 VALUES ('task', ?1, ?2, ?3, ?4)",
                params![
                    legacy_project,
                    legacy_task_id,
                    format!("{}#{legacy_task_id}", entry.entry_id),
                    new_schedule_id
                ],
            )
            .map_err(|e| sql_err("insert stg id map (task)", e))?;
        }
    }

    // 5) idMap 制品（008 §5：staging 内唯一新 ID 分配记录）。
    let id_map_rows: Vec<(String, String, String, String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id
                 FROM stg_id_map ORDER BY legacy_kind, legacy_scope, legacy_id",
            )
            .map_err(|e| sql_err("read stg id map", e))?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| sql_err("read stg id map", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| sql_err("read stg id map", e))?
    };
    let id_map_artifact_id = store.next_id();
    let mut idmap_jsonl = String::new();
    for (kind, scope, legacy_id, storage_key, new_id) in &id_map_rows {
        idmap_jsonl.push_str(
            &serde_json::to_string(&json!({
                "legacyKind": kind,
                "legacyScope": scope,
                "legacyId": legacy_id,
                "legacyStorageKey": storage_key,
                "newId": new_id,
                "sourceRevision": backup_manifest["sourceRevision"],
                "migrationId": staging_id,
                "createdAt": now_rfc3339(),
            }))
            .unwrap_or_default(),
        );
        idmap_jsonl.push('\n');
    }
    let idmap_bytes = idmap_jsonl.into_bytes();
    let idmap_sha = sha256_hex(&idmap_bytes);
    atomic_write_file(&dir.join("idmap.jsonl"), &idmap_bytes, false).map_err(|error| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write idmap.jsonl: {}", error.message),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;
    let id_map_artifact = json!({
        "artifactId": id_map_artifact_id,
        "length": idmap_bytes.len(),
        "sha256": idmap_sha,
        "mediaType": "application/jsonl",
    });

    // 转换计数（convertedEntries = Σ plan.changes 的 convert 类计数，008 §7 check 1）。
    let count = |table: &str| -> Result<u64, ContractError> {
        let total: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .map_err(|e| sql_err(&format!("count {table}"), e))?;
        Ok(total.max(0) as u64)
    };
    let converted_entries = count("stg_projects")?
        + count("stg_sessions")?
        + count("stg_history")?
        + count("stg_runs")?
        + count("stg_schedules")?;

    let manifest = StagingManifest {
        staging_id: staging_id.to_owned(),
        plan_id: plan_id.to_owned(),
        plan_hash: plan_hash.to_owned(),
        backup_artifact_id: backup_artifact_id.to_owned(),
        source_identity: backup_manifest["sourceIdentity"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        source_revision: backup_manifest["sourceRevision"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        target_format_version: TARGET_FORMAT_VERSION.to_owned(),
        backup_checksum: backup_manifest["checksum"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        backup_entry_count: entries.len() as u64,
        converted_entries,
        diagnostics,
        unsupported: data_backup::unsupported_projection(backup_destination, &entry_pairs(entries)),
        id_map_artifact,
        verifications: Vec::new(),
        commit: None,
        created_at: now_rfc3339(),
    };
    write_staging_manifest(&dir, &manifest)?;
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// migration.stage（async / idempotentSubmit）
// ---------------------------------------------------------------------------

async fn migration_stage(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let home = rt.store().home().to_path_buf();
    let domain = migration_domain(&home).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let plan_hash = input["planHash"].as_str().unwrap_or_default().to_owned();
    let backup_artifact_id = input["backupArtifactId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let source_revision = input["sourceRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if plan_id.is_empty() || plan_hash.is_empty() || backup_artifact_id.is_empty() {
        return Err(invalid("planId, planHash and backupArtifactId are required").into());
    }

    // 幂等查找优先（008 §4 第 6 步「同键重放返回同一 stagingId」）：
    // 重放直接回 stored receipt，不进入下面的受理前复验——第一次受理后
    // 源根再变化不影响重放。
    if let Some(receipt) = data_backup::replay_idempotent_receipt(
        &domain.ops,
        "migration.stage",
        &input,
        key.as_deref(),
    )? {
        return Ok((ResponseOutcome::Accepted, receipt));
    }

    // 受理前复验（幂等重放路径已在上面返回）：
    // 备份存在 → 源根再算 revision（PLAN_STALE）→ 写者（WRITER_ACTIVE）
    // → 备份与计划 sourceRevision 同链。
    tokio::task::spawn_blocking({
        let home = home.clone();
        let backup_artifact_id = backup_artifact_id.clone();
        let source_revision = source_revision.clone();
        move || -> Result<(), ContractError> {
            let (record, backup_manifest, _entries) =
                load_registered_backup(&home, &backup_artifact_id)?;
            if backup_manifest["sourceRevision"].as_str() != Some(source_revision.as_str()) {
                return Err(plan_stale(format!(
                    "backup `{backup_artifact_id}` was taken at sourceRevision `{}`, not `{source_revision}`; re-run migration.plan and re-back-up",
                    backup_manifest["sourceRevision"].as_str().unwrap_or_default()
                )));
            }
            let scan = match scan_source_root(Path::new(&record.data_root), ScanDepth::Full) {
                Ok(scan) => scan,
                Err(_) => {
                    return Err(plan_stale(format!(
                        "source root `{}` can no longer be verified; re-run migration.plan",
                        record.data_root
                    )));
                }
            };
            if let Some(writer) = &scan.active_writer {
                return Err(writer_active(writer));
            }
            if scan.revision != source_revision {
                return Err(plan_stale(format!(
                    "source revision `{source_revision}` does not match the live `{}`; re-run migration.plan",
                    scan.revision
                )));
            }
            Ok(())
        }
    })
    .await
    .map_err(join_err)?
    .map_err(DispatchError::Application)?;

    let input_for_accept = input.clone();
    let operation = accept_domain_operation(
        &domain.ops,
        "migration.stage",
        &input_for_accept,
        key.as_deref(),
        || format!("op-{}", store.next_id()),
        || Ok(()),
    )?;
    let operation_id = operation.operation_id.clone();
    let staging_id = format!("stg-{}", store.next_id());

    let runner = domain.clone();
    tokio::task::spawn_blocking(move || {
        let _ = mark_domain_operation_running(&runner.ops, &operation_id, "");
        let outcome = execute_stage(
            &store,
            &home,
            &runner,
            &staging_id,
            &plan_id,
            &plan_hash,
            &backup_artifact_id,
            &source_revision,
        );
        match &outcome {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "succeeded",
                    completion,
                );
            }
            Err(error) => {
                if let Ok(guard) = runner.conn.lock() {
                    let _ = set_staging_state(&guard, &staging_id, "failed");
                }
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"errorCode": error.code, "message": error.message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// stage worker：受理窗口内的源变化复验（PLAN_STALE，零目标副作用）→ 转换
/// → 登记（staged）。
#[allow(clippy::too_many_arguments)]
fn execute_stage(
    store: &Arc<V1Store>,
    home: &Path,
    domain: &MigrationDomain,
    staging_id: &str,
    plan_id: &str,
    plan_hash: &str,
    backup_artifact_id: &str,
    source_revision: &str,
) -> Result<Value, ContractError> {
    let (record, backup_manifest, entries) = load_registered_backup(home, backup_artifact_id)?;
    // 受理后源复验（「源版本变化拒绝旧计划」的窗口闭合）。
    let scan = match scan_source_root(Path::new(&record.data_root), ScanDepth::Full) {
        Ok(scan) => scan,
        Err(_) => {
            return Err(plan_stale(format!(
                "source root `{}` can no longer be verified after acceptance",
                record.data_root
            )));
        }
    };
    if let Some(writer) = &scan.active_writer {
        return Err(writer_active(writer));
    }
    if scan.revision != source_revision {
        return Err(plan_stale(format!(
            "source revision `{source_revision}` changed to `{}` after acceptance",
            scan.revision
        )));
    }
    // 备份条目完整性（INTEGRITY_ERROR）。
    let destination = Path::new(&record.destination);
    for entry in &entries {
        let _ = data_backup::backup_entry_bytes(destination, entry)?;
    }
    let manifest = convert_backup_into_staging(
        store,
        home,
        staging_id,
        backup_artifact_id,
        destination,
        &backup_manifest,
        &entries,
        plan_id,
        plan_hash,
    )?;
    {
        let guard = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = StagingRow {
            staging_id: staging_id.to_owned(),
            plan_id: plan_id.to_owned(),
            plan_hash: plan_hash.to_owned(),
            backup_artifact_id: backup_artifact_id.to_owned(),
            source_identity: manifest.source_identity.clone(),
            source_revision: manifest.source_revision.clone(),
            state: "staged".to_owned(),
        };
        upsert_staging_row(&guard, &row)?;
    }
    Ok(json!({
        "stagingId": staging_id,
        "planHash": plan_hash,
        "sourceRevision": manifest.source_revision,
        "targetFormatVersion": manifest.target_format_version,
        "idMapArtifactId": manifest.id_map_artifact["artifactId"],
        "convertedEntries": manifest.converted_entries,
        "diagnostics": manifest.diagnostics,
    }))
}

// ---------------------------------------------------------------------------
// migration.verify（async / idempotentSubmit）
// ---------------------------------------------------------------------------

async fn migration_verify(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let home = rt.store().home().to_path_buf();
    let domain = migration_domain(&home).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let staging_id = input["stagingId"].as_str().unwrap_or_default().to_owned();
    let plan_hash = input["planHash"].as_str().unwrap_or_default().to_owned();
    if staging_id.is_empty() || plan_hash.is_empty() {
        return Err(invalid("stagingId and planHash are required").into());
    }

    // 幂等查找优先（008 §4 第 7 步：同键重放回 stored receipt）——重放
    // 不得被下面的受理前校验（如 staging 已推进状态）抢拒。
    if let Some(receipt) = data_backup::replay_idempotent_receipt(
        &domain.ops,
        "migration.verify",
        &input,
        key.as_deref(),
    )? {
        return Ok((ResponseOutcome::Accepted, receipt));
    }

    // 受理前校验：staging 存在 / planHash 匹配 / 状态可核验。
    {
        let conn = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = get_staging_row(&conn, &staging_id)?
            .ok_or_else(|| not_found("staging", &staging_id))?;
        if row.plan_hash != plan_hash {
            return Err(integrity_error(format!(
                "staging `{staging_id}` was staged for plan hash `{}`, not `{plan_hash}`",
                row.plan_hash
            ))
            .into());
        }
        if row.state != "staged" && row.state != "verified" {
            return Err(integrity_error(format!(
                "staging `{staging_id}` is `{}`; only a staged staging can be verified",
                row.state
            ))
            .into());
        }
    }

    let input_for_accept = input.clone();
    let operation = accept_domain_operation(
        &domain.ops,
        "migration.verify",
        &input_for_accept,
        key.as_deref(),
        || format!("op-{}", store.next_id()),
        || Ok(()),
    )?;
    let operation_id = operation.operation_id.clone();
    let verification_id = format!("verif-{}", store.next_id());
    let report_artifact_id = format!("rprt-{}", store.next_id());

    let runner = domain.clone();
    tokio::task::spawn_blocking(move || {
        let _ = mark_domain_operation_running(&runner.ops, &operation_id, "");
        let outcome = execute_verify(
            home,
            &runner,
            &staging_id,
            &plan_hash,
            &verification_id,
            &report_artifact_id,
        );
        match &outcome {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "succeeded",
                    completion,
                );
            }
            Err(error) => {
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"errorCode": error.code, "message": error.message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// 核对辅助：一行未映射/悬垂即计一条 failure。
fn verify_id_map_coverage(
    conn: &Connection,
    table: &str,
    column: &str,
    kind: &str,
    staging_id: &str,
    failures: &mut Vec<Value>,
) -> Result<(), ContractError> {
    let missing: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM {table} t WHERE NOT EXISTS (
                     SELECT 1 FROM stg_id_map m WHERE m.new_id = t.{column})"
            ),
            [],
            |r| r.get(0),
        )
        .map_err(|e| integrity_error(format!("id map completeness check `{kind}`: {e}")))?;
    if missing > 0 {
        push_failure(
            failures,
            staging_id,
            "id-map-missing",
            format!("{missing} `{kind}` rows have no id map entry"),
        );
    }
    Ok(())
}

fn verify_dangling(
    conn: &Connection,
    sql: &str,
    label: &str,
    staging_id: &str,
    failures: &mut Vec<Value>,
) -> Result<(), ContractError> {
    let missing: i64 = conn
        .query_row(sql, [], |r| r.get(0))
        .map_err(|e| integrity_error(format!("reference check `{label}`: {e}")))?;
    if missing > 0 {
        push_failure(
            failures,
            staging_id,
            "dangling-reference",
            format!("{missing} {label} rows reference a missing target"),
        );
    }
    Ok(())
}

/// verify worker：六项核对（008 §7）→ 报告制品 + 核验记录。
fn execute_verify(
    home: PathBuf,
    domain: &MigrationDomain,
    staging_id: &str,
    plan_hash: &str,
    verification_id: &str,
    report_artifact_id: &str,
) -> Result<Value, ContractError> {
    let dir = staging_dir(&home, staging_id);
    let manifest = load_staging_manifest(&dir)?;
    if manifest.plan_hash != plan_hash {
        return Err(integrity_error(format!(
            "staging `{staging_id}` carries plan hash `{}`, not `{plan_hash}`",
            manifest.plan_hash
        )));
    }
    let mut failures: Vec<Value> = Vec::new();

    let staging_db = dir.join("staging.sqlite");
    let conn = Connection::open(&staging_db)
        .map_err(|e| integrity_error(format!("staging db unreadable: {e}")))?;
    let count = |table: &str| -> Result<i64, ContractError> {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .map_err(|e| integrity_error(format!("staging table `{table}` unreadable: {e}")))
    };
    let (projects, sessions, history, runs, schedules, id_map) = (
        count("stg_projects")?,
        count("stg_sessions")?,
        count("stg_history")?,
        count("stg_runs")?,
        count("stg_schedules")?,
        count("stg_id_map")?,
    );

    // 1) 计数对账（convertedEntries == 实际 staged 行合计）。
    let recounted = (projects + sessions + history + runs + schedules) as u64;
    if recounted != manifest.converted_entries {
        push_failure(
            &mut failures,
            staging_id,
            "count-mismatch",
            format!(
                "staging records {recounted} converted entries, manifest says {}",
                manifest.converted_entries
            ),
        );
    }

    // 2) ID 映射完整性（new_id 唯一；每个 staged 行恰有一条映射）。
    {
        let mut new_ids: HashSet<String> = HashSet::new();
        let mut duplicate = false;
        let mut stmt = conn
            .prepare("SELECT new_id FROM stg_id_map")
            .map_err(|e| integrity_error(format!("read stg id map: {e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| integrity_error(format!("read stg id map: {e}")))?;
        for row in rows {
            let new_id = row.map_err(|e| integrity_error(format!("read stg id map: {e}")))?;
            if !new_ids.insert(new_id) {
                duplicate = true;
            }
        }
        if duplicate {
            push_failure(
                &mut failures,
                staging_id,
                "id-map-duplicate",
                "two legacy ids map to the same new id".to_owned(),
            );
        }
    }
    verify_id_map_coverage(
        &conn,
        "stg_projects",
        "project_id",
        "project",
        staging_id,
        &mut failures,
    )?;
    verify_id_map_coverage(
        &conn,
        "stg_sessions",
        "session_id",
        "session",
        staging_id,
        &mut failures,
    )?;
    verify_id_map_coverage(
        &conn,
        "stg_runs",
        "run_id",
        "run",
        staging_id,
        &mut failures,
    )?;
    verify_id_map_coverage(
        &conn,
        "stg_schedules",
        "schedule_id",
        "task",
        staging_id,
        &mut failures,
    )?;

    // 3) 引用一致性（run→session、session→project、schedule→project）。
    verify_dangling(
        &conn,
        "SELECT COUNT(*) FROM stg_runs r WHERE NOT EXISTS (
            SELECT 1 FROM stg_sessions s WHERE s.session_id = r.session_id)",
        "run",
        staging_id,
        &mut failures,
    )?;
    verify_dangling(
        &conn,
        "SELECT COUNT(*) FROM stg_sessions s WHERE NOT EXISTS (
            SELECT 1 FROM stg_projects p WHERE p.project_id = s.project_id)",
        "session",
        staging_id,
        &mut failures,
    )?;
    verify_dangling(
        &conn,
        "SELECT COUNT(*) FROM stg_schedules x WHERE NOT EXISTS (
            SELECT 1 FROM stg_projects p WHERE p.project_id = x.project_id)",
        "schedule",
        staging_id,
        &mut failures,
    )?;

    // 4) 内容 hash：备份逐条目 + idMap 制品三元组。
    match load_registered_backup(&home, &manifest.backup_artifact_id) {
        Ok((record, _, entries)) => {
            let destination = Path::new(&record.destination);
            for entry in &entries {
                if let Err(error) = data_backup::backup_entry_bytes(destination, entry) {
                    push_failure(
                        &mut failures,
                        &entry.entry_id,
                        "checksum-mismatch",
                        error.message,
                    );
                }
            }
            let idmap = std::fs::read(dir.join("idmap.jsonl"))
                .map_err(|e| integrity_error(format!("read idmap.jsonl: {e}")))?;
            let actual = sha256_hex(&idmap);
            let expected = manifest.id_map_artifact["sha256"]
                .as_str()
                .unwrap_or_default();
            if actual != expected {
                push_failure(
                    &mut failures,
                    "idmap.jsonl",
                    "checksum-mismatch",
                    format!("id map artifact hash `{expected}` does not match `{actual}`"),
                );
            }
        }
        Err(error) => {
            push_failure(
                &mut failures,
                &manifest.backup_artifact_id,
                "checksum-mismatch",
                format!("the source backup is no longer readable: {}", error.message),
            );
        }
    }

    // 5) 目标可读性（staging 可开已在上方；manifest 格式版本核对）。
    if manifest.target_format_version != TARGET_FORMAT_VERSION {
        push_failure(
            &mut failures,
            staging_id,
            "staging-unreadable",
            format!(
                "staging target format `{}` is not `{TARGET_FORMAT_VERSION}`",
                manifest.target_format_version
            ),
        );
    }
    let checked_entries = (projects + sessions + history + runs + schedules + id_map) as u64
        + manifest.backup_entry_count;

    // 6) unsupported 对账（备份不可变 → 共享函数重算必须与 stage 记录一致）。
    if let Ok((record, _, entries)) = load_registered_backup(&home, &manifest.backup_artifact_id) {
        let observed = data_backup::unsupported_projection(
            Path::new(&record.destination),
            &entry_pairs(&entries),
        );
        if observed != manifest.unsupported {
            push_failure(
                &mut failures,
                staging_id,
                "unsupported-drift",
                format!(
                    "unsupported entries drifted since staging (recorded {}, observed {})",
                    manifest.unsupported.len(),
                    observed.len()
                ),
            );
        }
    }

    let verified = failures.is_empty();
    // 报告制品（staging 目录内，$defs.artifact 三元组随核验记录保存）。
    let report = json!({
        "verificationId": verification_id,
        "stagingId": staging_id,
        "planHash": plan_hash,
        "verified": verified,
        "checkedEntries": checked_entries,
        "failures": failures,
        "generatedAt": now_rfc3339(),
    });
    let report_bytes = serde_json::to_vec(&report)
        .map_err(|e| io_unavailable(format!("serialize report: {e}")))?;
    let report_path = dir.join(format!("report-{report_artifact_id}.json"));
    atomic_write_file(&report_path, &report_bytes, false).map_err(|error| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write report artifact: {}", error.message),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;
    let report_artifact = json!({
        "artifactId": report_artifact_id,
        "length": report_bytes.len(),
        "sha256": sha256_hex(&report_bytes),
        "mediaType": "application/json",
    });

    // 核验记录进 staging 清单（读改写全程持锁）+ 登记表状态。
    {
        let _guard = staging_write_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut manifest = load_staging_manifest(&dir)?;
        manifest.verifications.push(json!({
            "verificationId": verification_id,
            "verified": verified,
            "checkedEntries": checked_entries,
            "reportArtifact": report_artifact,
        }));
        write_staging_manifest(&dir, &manifest)?;
    }
    if verified {
        let guard = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        set_staging_state(&guard, staging_id, "verified")?;
    }
    Ok(json!({
        "stagingId": staging_id,
        "verificationId": verification_id,
        "verified": verified,
        "reportArtifactId": report_artifact_id,
        "checkedEntries": checked_entries,
        "failures": failures,
    }))
}

// ---------------------------------------------------------------------------
// migration.commit（async / idempotentSubmit）
// ---------------------------------------------------------------------------

async fn migration_commit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let home = rt.store().home().to_path_buf();
    let domain = migration_domain(&home).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let staging_id = input["stagingId"].as_str().unwrap_or_default().to_owned();
    let verification_id = input["verificationId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let plan_hash = input["planHash"].as_str().unwrap_or_default().to_owned();
    let expected_target_revision = input["expectedTargetRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if staging_id.is_empty()
        || verification_id.is_empty()
        || plan_hash.is_empty()
        || expected_target_revision.is_empty()
    {
        return Err(invalid(
            "stagingId, verificationId, planHash and expectedTargetRevision are required",
        )
        .into());
    }

    // 幂等查找优先（008 §4 第 8 步「中断重入：同幂等键重放」）：第一次
    // 受理成功后 targetRevision 已被推进，重放必须回 stored receipt，而不
    // 是被下面的 REVISION_CONFLICT 抢拒。
    if let Some(receipt) = data_backup::replay_idempotent_receipt(
        &domain.ops,
        "migration.commit",
        &input,
        key.as_deref(),
    )? {
        return Ok((ResponseOutcome::Accepted, receipt));
    }

    // 受理前校验：staging → 匹配成功核验 → 目标 CAS → 活跃 run。
    {
        let conn = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = get_staging_row(&conn, &staging_id)?.ok_or_else(|| {
            verification_required(format!(
                "staging `{staging_id}` does not exist; there is no matching verification to publish"
            ))
        })?;
        if row.plan_hash != plan_hash {
            return Err(verification_required(format!(
                "staging `{staging_id}` was staged for plan hash `{}`, not `{plan_hash}`",
                row.plan_hash
            ))
            .into());
        }
        if row.state != "staged" && row.state != "verified" {
            return Err(verification_required(format!(
                "staging `{staging_id}` is `{}`; only a staged/verified staging can be committed",
                row.state
            ))
            .into());
        }
        let dir = staging_dir(&home, &staging_id);
        let manifest = load_staging_manifest(&dir).map_err(DispatchError::Application)?;
        let matched = manifest.verifications.iter().any(|verification| {
            verification["verificationId"].as_str() == Some(verification_id.as_str())
                && verification["verified"].as_bool() == Some(true)
        });
        if !matched {
            return Err(verification_required(format!(
                "verification `{verification_id}` is not a successful verification of staging `{staging_id}`"
            ))
            .into());
        }
    }
    let (current_revision, has_active_runs) = store
        .migration_target_state()
        .map_err(DispatchError::Application)?;
    if current_revision != expected_target_revision {
        return Err(revision_conflict(&expected_target_revision, &current_revision).into());
    }
    if has_active_runs {
        return Err(target_writer_active().into());
    }

    let input_for_accept = input.clone();
    let operation = accept_domain_operation(
        &domain.ops,
        "migration.commit",
        &input_for_accept,
        key.as_deref(),
        || format!("op-{}", store.next_id()),
        || Ok(()),
    )?;
    let operation_id = operation.operation_id.clone();
    let recovery_artifact_id = format!("rcvr-{}", store.next_id());

    let runner = domain.clone();
    tokio::task::spawn_blocking(move || {
        let _ = mark_domain_operation_running(&runner.ops, &operation_id, "");
        let outcome = execute_commit(
            &store,
            &home,
            &runner,
            &staging_id,
            &plan_hash,
            &verification_id,
            &expected_target_revision,
            &recovery_artifact_id,
        );
        match &outcome {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "succeeded",
                    completion,
                );
            }
            Err(error) => {
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"errorCode": error.code, "message": error.message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// staging 行读取（commit/recover 用；staging.sqlite 是唯一转换产物源）。
fn read_staging_rows(source: &Connection) -> Result<Value, ContractError> {
    fn rows_of(
        source: &Connection,
        sql: &str,
        columns: &[&str],
    ) -> Result<Vec<Value>, ContractError> {
        let mut stmt = source
            .prepare(sql)
            .map_err(|e| sql_err("read staging rows", e))?;
        let names: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
        let rows = stmt
            .query_map([], |r| {
                let mut row = serde_json::Map::new();
                for (index, name) in names.iter().enumerate() {
                    let value: rusqlite::types::Value = r.get(index)?;
                    row.insert(
                        name.clone(),
                        match value {
                            rusqlite::types::Value::Null => Value::Null,
                            rusqlite::types::Value::Integer(i) => json!(i),
                            rusqlite::types::Value::Real(f) => json!(f),
                            rusqlite::types::Value::Text(t) => json!(t),
                            rusqlite::types::Value::Blob(b) => json!(b),
                        },
                    );
                }
                Ok(Value::Object(row))
            })
            .map_err(|e| sql_err("read staging rows", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| sql_err("read staging rows", e))?;
        Ok(rows)
    }
    Ok(json!({
        "projects": rows_of(
            source,
            "SELECT project_id, name, root_id, absolute_path, legacy_project_id FROM stg_projects",
            &["projectId", "name", "rootId", "absolutePath", "legacyProjectId"],
        )?,
        "sessions": rows_of(
            source,
            "SELECT session_id, project_id, name, tags_json, created_at FROM stg_sessions",
            &["sessionId", "projectId", "name", "tagsJson", "createdAt"],
        )?,
        "history": rows_of(
            source,
            "SELECT session_id, entry_id, seq, turn_id, role, content_json, created_at FROM stg_history",
            &["sessionId", "entryId", "seq", "turnId", "role", "contentJson", "createdAt"],
        )?,
        "runs": rows_of(
            source,
            "SELECT run_id, session_id, project_id, state, legacy_run_id, created_at, updated_at FROM stg_runs",
            &["runId", "sessionId", "projectId", "state", "legacyRunId", "createdAt", "updatedAt"],
        )?,
        "schedules": rows_of(
            source,
            "SELECT schedule_id, legacy_task_id, project_id, kind, expression, timezone, message, enabled FROM stg_schedules",
            &["scheduleId", "legacyTaskId", "projectId", "kind", "expression", "timezone", "message", "enabled"],
        )?,
        "idMap": rows_of(
            source,
            "SELECT legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id FROM stg_id_map",
            &["legacyKind", "legacyScope", "legacyId", "legacyStorageKey", "newId"],
        )?,
    }))
}

/// commit worker：持 store 互斥（单写者锁）→ 恢复材料 → 单事务合并 →
/// compat 导出。
#[allow(clippy::too_many_arguments)]
fn execute_commit(
    store: &Arc<V1Store>,
    home: &Path,
    domain: &MigrationDomain,
    staging_id: &str,
    plan_hash: &str,
    verification_id: &str,
    expected_target_revision: &str,
    recovery_artifact_id: &str,
) -> Result<Value, ContractError> {
    // worker 侧复验（受理窗口内的变化 → 失败，零已发生副作用）。
    let (current_revision, has_active_runs) = store.migration_target_state()?;
    if current_revision != expected_target_revision {
        return Err(revision_conflict(
            expected_target_revision,
            &current_revision,
        ));
    }
    if has_active_runs {
        return Err(target_writer_active());
    }
    let dir = staging_dir(home, staging_id);
    let manifest = load_staging_manifest(&dir)?;
    if manifest.plan_hash != plan_hash {
        return Err(verification_required(format!(
            "staging `{staging_id}` carries plan hash `{}`, not `{plan_hash}`",
            manifest.plan_hash
        )));
    }
    if !manifest.verifications.iter().any(|verification| {
        verification["verificationId"].as_str() == Some(verification_id)
            && verification["verified"].as_bool() == Some(true)
    }) {
        return Err(verification_required(format!(
            "verification `{verification_id}` is not a successful verification of staging `{staging_id}`"
        )));
    }
    let staging_db = dir.join("staging.sqlite");
    let source = Connection::open(&staging_db)
        .map_err(|e| integrity_error(format!("staging db unreadable: {e}")))?;
    let staged = read_staging_rows(&source)?;

    // 单事务合并（store 互斥 = 单写者锁，008 §4 第 7 步）。
    let recovery = {
        let mut target = store.conn.lock().expect("sqlite mutex");
        let tx = target
            .transaction()
            .map_err(|e| sql_err("begin commit tx", e))?;
        let pre_revision = current_target_revision(&tx)?;
        let instance_id = store.meta().instance_id.clone();
        let now = now_rfc3339();
        let mut inserted_roots: Vec<String> = Vec::new();
        let mut inserted_projects: Vec<String> = Vec::new();
        let mut inserted_sessions: Vec<String> = Vec::new();
        let mut inserted_runs: Vec<String> = Vec::new();
        let mut inserted_schedules: Vec<String> = Vec::new();

        for project in staged["projects"].as_array().cloned().unwrap_or_default() {
            let project_id = project["projectId"].as_str().unwrap_or_default().to_owned();
            let name = project["name"].as_str().unwrap_or_default().to_owned();
            let root_id = project["rootId"].as_str().unwrap_or_default().to_owned();
            let absolute_path = project["absolutePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if absolute_path.is_empty() {
                return Err(invalid(format!(
                    "project `{project_id}` has no root path (missing .cwd in the legacy source)"
                )));
            }
            tx.execute(
                "INSERT OR IGNORE INTO authorized_roots (root_id, instance_id, absolute_path, source, created_at)
                 VALUES (?1, ?2, ?3, 'legacy-migration', ?4)",
                params![root_id, instance_id, absolute_path, now],
            )
            .map_err(|e| sql_err("insert authorized root", e))?;
            let root_json = json!({
                "instanceId": instance_id,
                "rootId": root_id,
                "relativePath": "",
            });
            tx.execute(
                "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
                 VALUES (?1, ?2, ?3, 'registered', 'local', ?4, ?5)",
                params![
                    project_id,
                    name,
                    serde_json::to_string(&root_json).unwrap_or_default(),
                    store.next_id(),
                    now,
                ],
            )
            .map_err(|e| sql_err("insert project", e))?;
            inserted_roots.push(root_id);
            inserted_projects.push(project_id);
        }

        for session in staged["sessions"].as_array().cloned().unwrap_or_default() {
            let session_id = session["sessionId"].as_str().unwrap_or_default().to_owned();
            tx.execute(
                "INSERT INTO sessions (session_id, project_id, name, tags_json, revision, active_run_id, session_state, queue_paused, pause_reason, queue_revision, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'open', 0, NULL, ?5, ?6)",
                params![
                    session_id,
                    session["projectId"].as_str().unwrap_or_default(),
                    session["name"].as_str().unwrap_or_default(),
                    session["tagsJson"].as_str().unwrap_or("[]"),
                    store.next_id(),
                    session["createdAt"].as_str().unwrap_or_default(),
                ],
            )
            .map_err(|e| sql_err("insert session", e))?;
            inserted_sessions.push(session_id);
        }

        for entry in staged["history"].as_array().cloned().unwrap_or_default() {
            tx.execute(
                "INSERT INTO history_entries (session_id, entry_id, seq, turn_id, role, content_json, run_id, tool_call_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7)",
                params![
                    entry["sessionId"].as_str().unwrap_or_default(),
                    entry["entryId"].as_str().unwrap_or_default(),
                    entry["seq"].as_i64().unwrap_or_default(),
                    entry["turnId"].as_str().unwrap_or_default(),
                    entry["role"].as_str().unwrap_or_default(),
                    entry["contentJson"].as_str().unwrap_or_default(),
                    entry["createdAt"].as_str().unwrap_or_default(),
                ],
            )
            .map_err(|e| sql_err("insert history", e))?;
        }

        for run in staged["runs"].as_array().cloned().unwrap_or_default() {
            let run_id = run["runId"].as_str().unwrap_or_default().to_owned();
            let input_json = json!({
                "legacy": {
                    "runId": run["legacyRunId"],
                    "stagedSessionId": run["sessionId"],
                    "origin": "legacy-migration",
                }
            });
            tx.execute(
                "INSERT INTO runs (run_id, session_id, project_id, state, revision, cancel_requested, waiting_for_json, effect_status, config_json, final_cursor, continued_from, outcome_code, result_json, input_json, terminal, queue_paused, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, '[]', 'completed', '{}', NULL, NULL, 'LEGACY_IMPORT', NULL, ?6, 1, 0, ?7, ?8)",
                params![
                    run_id,
                    run["sessionId"].as_str().unwrap_or_default(),
                    run["projectId"].as_str().unwrap_or_default(),
                    run["state"].as_str().unwrap_or_default(),
                    store.next_id(),
                    serde_json::to_string(&input_json).unwrap_or_default(),
                    run["createdAt"].as_str().unwrap_or_default(),
                    run["updatedAt"].as_str().unwrap_or_default(),
                ],
            )
            .map_err(|e| sql_err("insert run", e))?;
            inserted_runs.push(run_id);
        }

        for schedule in staged["schedules"].as_array().cloned().unwrap_or_default() {
            let schedule_id = schedule["scheduleId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            tx.execute(
                "INSERT INTO v1_schedules (schedule_id, task_id, project_id, session_id, kind, expression, timezone, enabled, message, channel_id, revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, ?10)",
                params![
                    schedule_id,
                    schedule["legacyTaskId"].as_str().unwrap_or_default(),
                    schedule["projectId"].as_str().unwrap_or_default(),
                    schedule["kind"].as_str().unwrap_or_default(),
                    schedule["expression"].as_str().unwrap_or_default(),
                    schedule["timezone"].as_str().unwrap_or_default(),
                    if schedule["enabled"].as_i64().unwrap_or(0) != 0 { 1 } else { 0 },
                    schedule["message"].as_str().unwrap_or_default(),
                    format!("srev-legacy-{}", schedule["legacyTaskId"].as_str().unwrap_or_default()),
                    now,
                ],
            )
            .map_err(|e| sql_err("insert schedule", e))?;
            inserted_schedules.push(schedule_id);
        }

        // legacy_id_map：先捕获既有冲突行（恢复材料），再写入本迁移的行。
        let mut prior_rows: Vec<Value> = Vec::new();
        for mapping in staged["idMap"].as_array().cloned().unwrap_or_default() {
            let kind = mapping["legacyKind"].as_str().unwrap_or_default();
            let scope = mapping["legacyScope"].as_str().unwrap_or_default();
            let legacy_id = mapping["legacyId"].as_str().unwrap_or_default();
            let existing = tx
                .query_row(
                    "SELECT legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id, source_revision, migration_id, created_at
                     FROM legacy_id_map WHERE legacy_kind = ?1 AND legacy_scope = ?2 AND legacy_id = ?3",
                    params![kind, scope, legacy_id],
                    |r| {
                        Ok(json!({
                            "legacyKind": r.get::<_, String>(0)?,
                            "legacyScope": r.get::<_, String>(1)?,
                            "legacyId": r.get::<_, String>(2)?,
                            "legacyStorageKey": r.get::<_, String>(3)?,
                            "newId": r.get::<_, String>(4)?,
                            "sourceRevision": r.get::<_, String>(5)?,
                            "migrationId": r.get::<_, String>(6)?,
                            "createdAt": r.get::<_, String>(7)?,
                        }))
                    },
                )
                .optional()
                .map_err(|e| sql_err("read prior id map row", e))?;
            if let Some(prior) = existing {
                prior_rows.push(prior);
            }
            tx.execute(
                "INSERT INTO legacy_id_map (legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id, source_revision, migration_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(legacy_kind, legacy_scope, legacy_id) DO UPDATE SET
                    legacy_storage_key = excluded.legacy_storage_key,
                    new_id = excluded.new_id,
                    source_revision = excluded.source_revision,
                    migration_id = excluded.migration_id,
                    created_at = excluded.created_at",
                params![
                    kind,
                    scope,
                    legacy_id,
                    mapping["legacyStorageKey"].as_str().unwrap_or_default(),
                    mapping["newId"].as_str().unwrap_or_default(),
                    manifest.source_revision,
                    staging_id,
                    now,
                ],
            )
            .map_err(|e| sql_err("insert legacy id map", e))?;
        }

        let post_revision = bump_target_revision(&tx)?;
        tx.commit().map_err(|e| sql_err("commit migration tx", e))?;

        json!({
            "preRevision": pre_revision,
            "postRevision": post_revision,
            "inserted": {
                "rootIds": inserted_roots,
                "projectIds": inserted_projects,
                "sessionIds": inserted_sessions,
                "runIds": inserted_runs,
                "scheduleIds": inserted_schedules,
            },
            "priorLegacyIdMap": prior_rows,
        })
    };

    // 恢复制品（发布前目标状态；restorePreviousTarget 的材料）。
    let recovery_doc = json!({
        "recoveryArtifactId": recovery_artifact_id,
        "stagingId": staging_id,
        "targetIdentity": target_identity(home),
        "recovery": recovery.clone(),
    });
    let recovery_bytes = serde_json::to_vec(&recovery_doc)
        .map_err(|e| io_unavailable(format!("serialize recovery: {e}")))?;
    let recovery_path = dir.join(format!("recovery-{recovery_artifact_id}.json"));
    atomic_write_file(&recovery_path, &recovery_bytes, false).map_err(|error| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write recovery artifact: {}", error.message),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;

    // compat 导出（commit 完成路径内同步物化，008 §5.3/§9）。
    materialize_compat_export(home)?;

    // staging 清单 + 登记表。
    {
        let _guard = staging_write_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut manifest = load_staging_manifest(&dir)?;
        manifest.commit = Some(json!({
            "verificationId": verification_id,
            "recoveryArtifactId": recovery_artifact_id,
            "targetRevision": recovery["postRevision"].clone(),
            "committedAt": now_rfc3339(),
        }));
        write_staging_manifest(&dir, &manifest)?;
    }
    {
        let guard = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        set_staging_state(&guard, staging_id, "committed")?;
    }
    Ok(json!({
        "stagingId": staging_id,
        "targetIdentity": target_identity(home),
        "targetRevision": recovery["postRevision"].clone(),
        "published": true,
        "recoveryArtifactId": recovery_artifact_id,
    }))
}

/// 从 legacy_id_map 全表物化 `compat/legacy-id-map.json`（0600）+ 同目录
/// `.sha256`；表空时移除两个文件。
fn materialize_compat_export(home: &Path) -> Result<(), ContractError> {
    let conn = Connection::open(home.join("v1").join("state.sqlite"))
        .map_err(|e| sql_err("open state db for export", e))?;
    let mut stmt = conn
        .prepare(
            "SELECT legacy_kind, legacy_scope, legacy_id, new_id, migration_id, source_revision
             FROM legacy_id_map ORDER BY legacy_kind, legacy_scope, legacy_id",
        )
        .map_err(|e| sql_err("read legacy id map for export", e))?;
    let rows: Vec<(String, String, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .map_err(|e| sql_err("read legacy id map for export", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| sql_err("read legacy id map for export", e))?;
    drop(stmt);
    let compat_dir = home.join("compat");
    let export_path = compat_dir.join("legacy-id-map.json");
    let sha_path = compat_dir.join("legacy-id-map.sha256");
    if rows.is_empty() {
        let _ = std::fs::remove_file(&export_path);
        let _ = std::fs::remove_file(&sha_path);
        return Ok(());
    }
    let latest_migration = rows
        .iter()
        .map(|(_, _, _, _, migration, _)| migration.clone())
        .max()
        .unwrap_or_default();
    let latest_revision = rows
        .iter()
        .find(|(_, _, _, _, migration, _)| migration == &latest_migration)
        .map(|(_, _, _, _, _, revision)| revision.clone())
        .unwrap_or_default();
    let export = json!({
        "formatVersion": "agentcpm-pd.legacy-id-map/1",
        "migrationId": latest_migration,
        "sourceRevision": latest_revision,
        "items": rows
            .iter()
            .map(|(kind, scope, legacy, new_id, _, _)| {
                json!({
                    "legacyKind": kind,
                    "legacyScope": scope,
                    "legacyId": legacy,
                    "newId": new_id,
                })
            })
            .collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&export)
        .map_err(|e| io_unavailable(format!("serialize compat export: {e}")))?;
    atomic_write_file(&export_path, &bytes, true).map_err(|error| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write compat export: {}", error.message),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;
    atomic_write_file(&sha_path, sha256_hex(&bytes).as_bytes(), false).map_err(|error| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write compat export sha: {}", error.message),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// migration.recover（async / idempotentSubmit）
// ---------------------------------------------------------------------------

async fn migration_recover(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let home = rt.store().home().to_path_buf();
    let domain = migration_domain(&home).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let staging_id = input["stagingId"].as_str().unwrap_or_default().to_owned();
    let action = input["action"].as_str().unwrap_or_default().to_owned();
    let expected_target_revision = input["expectedTargetRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if staging_id.is_empty() || expected_target_revision.is_empty() {
        return Err(invalid("stagingId and expectedTargetRevision are required").into());
    }
    if action != "discardUnpublished" && action != "restorePreviousTarget" {
        return Err(invalid(format!(
            "action must be `discardUnpublished` or `restorePreviousTarget`, got `{action}`"
        ))
        .into());
    }

    // 幂等查找优先（008 §4 第 9 步：同键重放回 stored receipt）——重放
    // 不得因 staging 状态或 targetRevision 已被第一次受理改变而抢拒。
    if let Some(receipt) = data_backup::replay_idempotent_receipt(
        &domain.ops,
        "migration.recover",
        &input,
        key.as_deref(),
    )? {
        return Ok((ResponseOutcome::Accepted, receipt));
    }

    // 受理前校验。
    {
        let conn = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row =
            get_staging_row(&conn, &staging_id)?.ok_or_else(|| resource_not_owned(&staging_id))?;
        match action.as_str() {
            "discardUnpublished" => {
                if !matches!(
                    row.state.as_str(),
                    "staged" | "verified" | "failed" | "staging"
                ) {
                    return Err(invalid(format!(
                        "staging `{staging_id}` is `{}`; only an unpublished staging can be discarded",
                        row.state
                    ))
                    .into());
                }
            }
            "restorePreviousTarget" => {
                if row.state != "committed" {
                    return Err(invalid(format!(
                        "staging `{staging_id}` is `{}`; nothing was published from it",
                        row.state
                    ))
                    .into());
                }
            }
            _ => unreachable!("action validated above"),
        }
    }
    let (current_revision, has_active_runs) = store
        .migration_target_state()
        .map_err(DispatchError::Application)?;
    if current_revision != expected_target_revision {
        return Err(revision_conflict(&expected_target_revision, &current_revision).into());
    }
    if action == "restorePreviousTarget" && has_active_runs {
        return Err(target_writer_active().into());
    }

    let input_for_accept = input.clone();
    let operation = accept_domain_operation(
        &domain.ops,
        "migration.recover",
        &input_for_accept,
        key.as_deref(),
        || format!("op-{}", store.next_id()),
        || Ok(()),
    )?;
    let operation_id = operation.operation_id.clone();

    let runner = domain.clone();
    tokio::task::spawn_blocking(move || {
        let _ = mark_domain_operation_running(&runner.ops, &operation_id, "");
        let outcome = execute_recover(
            &store,
            &home,
            &runner,
            &staging_id,
            &action,
            &expected_target_revision,
        );
        match &outcome {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "succeeded",
                    completion,
                );
            }
            Err(error) => {
                let _ = finish_domain_operation(
                    &runner.ops,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"errorCode": error.code, "message": error.message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// recover worker。
fn execute_recover(
    store: &Arc<V1Store>,
    home: &Path,
    domain: &MigrationDomain,
    staging_id: &str,
    action: &str,
    expected_target_revision: &str,
) -> Result<Value, ContractError> {
    // worker 侧 revision 复验。
    let (current_revision, has_active_runs) = store.migration_target_state()?;
    if current_revision != expected_target_revision {
        return Err(revision_conflict(
            expected_target_revision,
            &current_revision,
        ));
    }
    if action == "restorePreviousTarget" && has_active_runs {
        return Err(target_writer_active());
    }
    let dir = staging_dir(home, staging_id);
    let manifest = load_staging_manifest(&dir)?;

    if action == "discardUnpublished" {
        // 只删本 staging 自己的隔离目录与 staging 制品（008 §4 第 8 步）；
        // 源数据与未归属项目不触碰。
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| io_unavailable(format!("remove staging dir: {e}")))?;
        }
        let guard = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        set_staging_state(&guard, staging_id, "discarded")?;
        return Ok(json!({
            "stagingId": staging_id,
            "action": action,
            "recovered": true,
            "targetRevision": current_revision,
            "retainedSourceIdentity": manifest.source_identity,
            "recoveryInstructions": [
                format!("staging `{staging_id}` discarded; the migration never modified the source root"),
                "re-run `migration plan` when you are ready to start over",
            ],
        }));
    }

    // restorePreviousTarget：恢复制品 + 目标新写入检查。
    let commit = manifest.commit.clone().ok_or_else(|| {
        invalid(format!(
            "staging `{staging_id}` has no commit record to restore from"
        ))
    })?;
    let recovery_artifact_id = commit["recoveryArtifactId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let recovery_path = dir.join(format!("recovery-{recovery_artifact_id}.json"));
    let recovery_raw = std::fs::read_to_string(&recovery_path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            integrity_error(format!(
                "recovery artifact `{recovery_artifact_id}` is missing from staging `{staging_id}`"
            ))
        } else {
            io_unavailable(format!("read {}: {err}", recovery_path.display()))
        }
    })?;
    let recovery_doc: Value = serde_json::from_str(&recovery_raw)
        .map_err(|e| integrity_error(format!("recovery artifact is corrupt: {e}")))?;
    let recovery = &recovery_doc["recovery"];
    let post_revision = recovery["postRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    if post_revision != current_revision {
        // 目标发布后有新写入：拒绝直接回滚，给人工步骤（008 §4 第 8 步）。
        return Ok(json!({
            "stagingId": staging_id,
            "action": action,
            "recovered": false,
            "targetRevision": current_revision,
            "retainedSourceIdentity": manifest.source_identity,
            "recoveryInstructions": [
                format!(
                    "the target has moved on since the commit (committed at `{post_revision}`, now `{current_revision}`); an automatic rollback was refused"
                ),
                format!(
                    "to roll back manually, remove the rows listed in `{}` (inserted ids) and restore the prior legacy_id_map rows it records",
                    recovery_path.display()
                ),
                "alternatively keep the migrated state and fix forward",
            ],
        }));
    }

    // 回滚：删除本迁移插入的行，恢复既有 legacy_id_map 行，还原 revision。
    {
        let mut target = store.conn.lock().expect("sqlite mutex");
        let tx = target
            .transaction()
            .map_err(|e| sql_err("begin restore tx", e))?;
        let inserted = &recovery["inserted"];
        for session_id in inserted["sessionIds"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(session) = session_id.as_str() {
                tx.execute(
                    "DELETE FROM history_entries WHERE session_id = ?1",
                    params![session],
                )
                .map_err(|e| sql_err("delete migrated history", e))?;
                tx.execute(
                    "DELETE FROM sessions WHERE session_id = ?1",
                    params![session],
                )
                .map_err(|e| sql_err("delete migrated session", e))?;
            }
        }
        for run_id in inserted["runIds"].as_array().cloned().unwrap_or_default() {
            if let Some(run) = run_id.as_str() {
                tx.execute("DELETE FROM runs WHERE run_id = ?1", params![run])
                    .map_err(|e| sql_err("delete migrated run", e))?;
            }
        }
        for schedule_id in inserted["scheduleIds"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(schedule) = schedule_id.as_str() {
                tx.execute(
                    "DELETE FROM v1_schedules WHERE schedule_id = ?1",
                    params![schedule],
                )
                .map_err(|e| sql_err("delete migrated schedule", e))?;
            }
        }
        for project_id in inserted["projectIds"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(project) = project_id.as_str() {
                tx.execute(
                    "DELETE FROM projects WHERE project_id = ?1",
                    params![project],
                )
                .map_err(|e| sql_err("delete migrated project", e))?;
            }
        }
        for root_id in inserted["rootIds"].as_array().cloned().unwrap_or_default() {
            if let Some(root) = root_id.as_str() {
                tx.execute(
                    "DELETE FROM authorized_roots WHERE root_id = ?1",
                    params![root],
                )
                .map_err(|e| sql_err("delete migrated root", e))?;
            }
        }
        tx.execute(
            "DELETE FROM legacy_id_map WHERE migration_id = ?1",
            params![staging_id],
        )
        .map_err(|e| sql_err("delete migrated id map rows", e))?;
        for prior in recovery["priorLegacyIdMap"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            tx.execute(
                "INSERT OR REPLACE INTO legacy_id_map (legacy_kind, legacy_scope, legacy_id, legacy_storage_key, new_id, source_revision, migration_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    prior["legacyKind"].as_str().unwrap_or_default(),
                    prior["legacyScope"].as_str().unwrap_or_default(),
                    prior["legacyId"].as_str().unwrap_or_default(),
                    prior["legacyStorageKey"].as_str().unwrap_or_default(),
                    prior["newId"].as_str().unwrap_or_default(),
                    prior["sourceRevision"].as_str().unwrap_or_default(),
                    prior["migrationId"].as_str().unwrap_or_default(),
                    prior["createdAt"].as_str().unwrap_or_default(),
                ],
            )
            .map_err(|e| sql_err("restore prior id map row", e))?;
        }
        let pre_revision = recovery["preRevision"].as_str().unwrap_or_default();
        if !pre_revision.is_empty() {
            set_target_revision(&tx, pre_revision)?;
        }
        tx.commit().map_err(|e| sql_err("commit restore tx", e))?;
    }
    // 导出副本随表内容重写。
    materialize_compat_export(home)?;
    {
        let guard = domain.conn.lock().unwrap_or_else(|e| e.into_inner());
        set_staging_state(&guard, staging_id, "restored")?;
    }
    Ok(json!({
        "stagingId": staging_id,
        "action": action,
        "recovered": true,
        "targetRevision": recovery["preRevision"].clone(),
        "retainedSourceIdentity": manifest.source_identity,
        "recoveryInstructions": [
            format!(
                "target restored to the pre-migration state (`{}`)",
                recovery["preRevision"].as_str().unwrap_or_default()
            ),
            "the source root was never modified; re-run `migration plan` to restart",
        ],
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::domains::data_backup::{
        dispatch_err, seed_legacy_root, wait_domain_operation,
    };
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

    async fn submit(rt: &Arc<PublicV1Runtime>, method: &str, input: Value, key: &str) -> Value {
        let (_, receipt) = rt.dispatch(method, input, Some(key), None).await.unwrap();
        OperationRegistry::global()
            .validate_result(method, &receipt)
            .unwrap_or_else(|e| panic!("`{method}` receipt schema: {e:?}"));
        receipt
    }

    async fn wait_completion(rt: &Arc<PublicV1Runtime>, method: &str, receipt: &Value) -> Value {
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_domain_operation(rt, &operation_id).await;
        assert_eq!(
            operation["state"],
            json!("succeeded"),
            "`{method}` failed: {operation}"
        );
        operation["result"].clone()
    }

    /// 播种一个旧根（含 cron 任务）并走完一次 create_backup。
    async fn seed_backup(
        rt: &Arc<PublicV1Runtime>,
        tmp: &std::path::Path,
        tag: &str,
    ) -> (String, String, String) {
        let root = seed_legacy_root(tmp);
        std::fs::create_dir_all(root.join("cron").join("projects").join("demo-proj")).unwrap();
        std::fs::write(
            root.join("cron")
                .join("projects")
                .join("demo-proj")
                .join("tasks.json"),
            r#"{"tasks":[{"taskId":"22222222-2222-2222-2222-222222222222","message":"daily","schedule":{"type":"cron","expression":"0 9 * * *","timezone":"UTC"},"status":"active","sessionKey":"tui:project=/tmp/demo:default","channelKey":"tui"}]}"#,
        )
        .unwrap();
        let destination = tmp.join(format!("backups-{tag}"));
        let receipt = submit(
            rt,
            "data.create_backup",
            json!({
                "dataRoot": root.to_string_lossy(),
                "destination": destination.to_string_lossy(),
            }),
            &format!("key-backup-{tag}"),
        )
        .await;
        let completion = wait_completion(rt, "data.create_backup", &receipt).await;
        (
            completion["artifactId"].as_str().unwrap().to_owned(),
            completion["sourceRevision"].as_str().unwrap().to_owned(),
            root.to_string_lossy().into_owned(),
        )
    }

    async fn full_stage(
        rt: &Arc<PublicV1Runtime>,
        tag: &str,
        artifact_id: &str,
        source_revision: &str,
    ) -> String {
        let receipt = submit(
            rt,
            "migration.stage",
            json!({
                "planId": format!("plan-{tag}"),
                "planHash": format!("phash-{tag}"),
                "backupArtifactId": artifact_id,
                "sourceRevision": source_revision,
            }),
            &format!("key-stage-{tag}"),
        )
        .await;
        let completion = wait_completion(rt, "migration.stage", &receipt).await;
        completion["stagingId"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn stage_verify_commit_and_restore_roundtrip() {
        let tmp = TempDir::new("migration-loop");
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        let (artifact_id, source_revision, _root) = seed_backup(&rt, &tmp.0, "loop").await;

        // stage → 隔离目录，不写 state.sqlite 的业务表。
        let staging_id = full_stage(&rt, "loop", &artifact_id, &source_revision).await;
        assert!(staging_dir(&home, &staging_id).exists());
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let projects: i64 = conn
                .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
                .unwrap();
            assert_eq!(projects, 0, "stage does not publish into the target");
        }

        // verify：verified=true。
        let receipt = submit(
            &rt,
            "migration.verify",
            json!({"stagingId": staging_id, "planHash": "phash-loop"}),
            "key-verify-loop",
        )
        .await;
        let verification = wait_completion(&rt, "migration.verify", &receipt).await;
        assert_eq!(verification["verified"], json!(true), "{verification}");
        assert!(verification["checkedEntries"].as_u64().unwrap() > 0);
        let verification_id = verification["verificationId"].as_str().unwrap().to_owned();

        // 未知 verificationId → VERIFICATION_REQUIRED。
        let err = dispatch_err(
            &rt,
            "migration.commit",
            json!({
                "stagingId": staging_id,
                "verificationId": "verif-bogus",
                "planHash": "phash-loop",
                "expectedTargetRevision": "rev-00000000",
            }),
            Some("key-commit-bogus"),
        )
        .await;
        assert_eq!(err.code, "VERIFICATION_REQUIRED");

        // 错误 expectedTargetRevision → REVISION_CONFLICT。
        let (current, _) = rt.store().migration_target_state().unwrap();
        let err = dispatch_err(
            &rt,
            "migration.commit",
            json!({
                "stagingId": staging_id,
                "verificationId": verification_id,
                "planHash": "phash-loop",
                "expectedTargetRevision": "rev-00000000",
            }),
            Some("key-commit-stale"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");

        // 正确 commit。
        let receipt = submit(
            &rt,
            "migration.commit",
            json!({
                "stagingId": staging_id,
                "verificationId": verification_id,
                "planHash": "phash-loop",
                "expectedTargetRevision": current,
            }),
            "key-commit-loop",
        )
        .await;
        let commit = wait_completion(&rt, "migration.commit", &receipt).await;
        assert_eq!(commit["published"], json!(true));
        let post_revision = commit["targetRevision"].as_str().unwrap().to_owned();

        // 目标数据落位。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
            assert!(count("SELECT COUNT(*) FROM projects") >= 1);
            assert!(count("SELECT COUNT(*) FROM sessions") >= 1);
            assert!(count("SELECT COUNT(*) FROM history_entries") >= 2);
            assert!(
                count("SELECT COUNT(*) FROM runs WHERE terminal = 1 AND state = 'succeeded'") >= 1
            );
            assert!(count("SELECT COUNT(*) FROM v1_schedules") >= 1);
            assert!(count("SELECT COUNT(*) FROM legacy_id_map") >= 3);
        }
        // compat 导出。
        let export = std::fs::read_to_string(home.join("compat").join("legacy-id-map.json"))
            .expect("compat export");
        let export_json: Value = serde_json::from_str(&export).unwrap();
        assert!(!export_json["items"].as_array().unwrap().is_empty());
        assert!(home.join("compat").join("legacy-id-map.sha256").exists());

        // restore：无新写入 → 回滚成功，行消失、导出收回。
        let receipt = submit(
            &rt,
            "migration.recover",
            json!({
                "stagingId": staging_id,
                "action": "restorePreviousTarget",
                "expectedTargetRevision": post_revision,
            }),
            "key-restore-loop",
        )
        .await;
        let restore = wait_completion(&rt, "migration.recover", &receipt).await;
        assert_eq!(restore["recovered"], json!(true), "{restore}");
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
            assert_eq!(count("SELECT COUNT(*) FROM legacy_id_map"), 0);
            assert_eq!(count("SELECT COUNT(*) FROM sessions"), 0);
        }
        assert!(
            !home.join("compat").join("legacy-id-map.json").exists(),
            "compat export removed after restore"
        );

        // 提交后新写入 → restore 拒绝直回滚（recovered=false + 人工步骤）。
        // （重走一遍 stage→verify→commit，再制造一次目标写入。）
        let receipt = submit(
            &rt,
            "data.create_backup",
            json!({
                "dataRoot": _root,
                "destination": tmp.0.join("backups-loop-2").to_string_lossy(),
            }),
            "key-backup-loop-2",
        )
        .await;
        let completion = wait_completion(&rt, "data.create_backup", &receipt).await;
        let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();
        let source_revision = completion["sourceRevision"].as_str().unwrap().to_owned();
        let staging_id = full_stage(&rt, "loop2", &artifact_id, &source_revision).await;
        let receipt = submit(
            &rt,
            "migration.verify",
            json!({"stagingId": staging_id, "planHash": "phash-loop2"}),
            "key-verify-loop2",
        )
        .await;
        let verification = wait_completion(&rt, "migration.verify", &receipt).await;
        assert_eq!(verification["verified"], json!(true));
        let verification_id = verification["verificationId"].as_str().unwrap().to_owned();
        let (current, _) = rt.store().migration_target_state().unwrap();
        let receipt = submit(
            &rt,
            "migration.commit",
            json!({
                "stagingId": staging_id,
                "verificationId": verification_id,
                "planHash": "phash-loop2",
                "expectedTargetRevision": current,
            }),
            "key-commit-loop2",
        )
        .await;
        let commit = wait_completion(&rt, "migration.commit", &receipt).await;
        let post_revision = commit["targetRevision"].as_str().unwrap().to_owned();
        // 制造目标新写入（一个项目 + revision 递增）。
        let store = rt.store().clone();
        {
            let mut conn = store.conn.lock().expect("sqlite mutex");
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
                 VALUES ('post-write', 'Post Write', '{}', 'registered', 'local', 'x', '2026-11-19T00:00:00Z')",
                [],
            )
            .unwrap();
            bump_target_revision(&tx).unwrap();
            tx.commit().unwrap();
        }
        // 调用方重读当前 revision 后再请求恢复 → 受理过 CAS，但 commit 之后
        // 有新写入 → recovered=false + 人工步骤（008 §4 第 8 步）。
        let (after_write, _) = rt.store().migration_target_state().unwrap();
        assert_ne!(after_write, post_revision);
        let receipt = submit(
            &rt,
            "migration.recover",
            json!({
                "stagingId": staging_id,
                "action": "restorePreviousTarget",
                "expectedTargetRevision": after_write,
            }),
            "key-restore-refused",
        )
        .await;
        let refused = wait_completion(&rt, "migration.recover", &receipt).await;
        assert_eq!(refused["recovered"], json!(false), "{refused}");
        assert!(
            !refused["recoveryInstructions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn plan_stale_and_discard_flow() {
        let tmp = TempDir::new("migration-stale");
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        let (artifact_id, source_revision, root) = seed_backup(&rt, &tmp.0, "stale").await;

        // 源变化 → PLAN_STALE（受理前拒绝）。
        std::fs::write(
            std::path::Path::new(&root)
                .join("runs")
                .join("registry.json"),
            "{}",
        )
        .unwrap();
        let err = dispatch_err(
            &rt,
            "migration.stage",
            json!({
                "planId": "plan-stale",
                "planHash": "phash-stale",
                "backupArtifactId": artifact_id,
                "sourceRevision": source_revision,
            }),
            Some("key-stage-stale"),
        )
        .await;
        assert_eq!(err.code, "PLAN_STALE");

        // 未知 staging → RESOURCE_NOT_OWNED / NOT_FOUND。
        let (current, _) = rt.store().migration_target_state().unwrap();
        let err = dispatch_err(
            &rt,
            "migration.recover",
            json!({
                "stagingId": "stg-unknown",
                "action": "discardUnpublished",
                "expectedTargetRevision": current,
            }),
            Some("key-recover-unknown"),
        )
        .await;
        assert_eq!(err.code, "RESOURCE_NOT_OWNED");
        let err = dispatch_err(
            &rt,
            "migration.verify",
            json!({"stagingId": "stg-unknown", "planHash": "phash-x"}),
            Some("key-verify-unknown"),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");

        // 恢复源内容，重新备份后 stage 成功 → discard：目录删除、源未动。
        std::fs::write(
            std::path::Path::new(&root).join("runs").join("registry.json"),
            r#"{"schemaVersion":1,"runs":{"11111111-1111-1111-1111-111111111111":{"sessionKey":"tui:project=/tmp/demo:default","state":"completed","revision":"r1","acceptedAt":"2026-11-01T00:00:00Z","updatedAt":"2026-11-01T00:00:05Z"}}}"#,
        )
        .unwrap();
        let receipt = submit(
            &rt,
            "data.create_backup",
            json!({
                "dataRoot": root,
                "destination": tmp.0.join("backups-stale-2").to_string_lossy(),
            }),
            "key-backup-stale-2",
        )
        .await;
        let completion = wait_completion(&rt, "data.create_backup", &receipt).await;
        let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();
        let source_revision = completion["sourceRevision"].as_str().unwrap().to_owned();
        let staging_id = full_stage(&rt, "stale2", &artifact_id, &source_revision).await;

        let receipt = submit(
            &rt,
            "migration.recover",
            json!({
                "stagingId": staging_id,
                "action": "discardUnpublished",
                "expectedTargetRevision": current,
            }),
            "key-discard",
        )
        .await;
        let discard = wait_completion(&rt, "migration.recover", &receipt).await;
        assert_eq!(discard["recovered"], json!(true));
        assert!(!staging_dir(&home, &staging_id).exists());
        assert!(
            std::path::Path::new(&root)
                .join("runs")
                .join("registry.json")
                .exists(),
            "source untouched"
        );

        // 未发布 staging 上的 restore 门 → INVALID_ARGUMENT。
        let receipt = submit(
            &rt,
            "migration.stage",
            json!({
                "planId": "plan-stale3",
                "planHash": "phash-stale3",
                "backupArtifactId": artifact_id,
                "sourceRevision": source_revision,
            }),
            "key-stage-stale3",
        )
        .await;
        let completion = wait_completion(&rt, "migration.stage", &receipt).await;
        let staging_id = completion["stagingId"].as_str().unwrap().to_owned();
        let (current, _) = rt.store().migration_target_state().unwrap();
        let err = dispatch_err(
            &rt,
            "migration.recover",
            json!({
                "stagingId": staging_id,
                "action": "restorePreviousTarget",
                "expectedTargetRevision": current,
            }),
            Some("key-restore-unpublished"),
        )
        .await;
        assert_eq!(err.code, "INVALID_ARGUMENT");
    }
}
