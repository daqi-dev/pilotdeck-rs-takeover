//! `work_cycles.*` — 工作周期应用与归档（cli.task.automation，S3 W6）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `work_cycles.*` 条目。TS
//! 基线（cc7c4e6）语义参考：`DiscoveryPlanService.queueCycleApply` /
//! `archiveCycle` / `updateCycleExecution`（ui/server/discovery-plans.js 的
//! 编排）与 `WorkspaceApply.applyWorktreeToProject`；旧 Rust 线
//! `runtime/always_on/cycles.rs`（只读参考）。
//!
//! ## v1 薄实现的选择（差异在任务报告 §4 登记）
//!
//! * **周期存储**：v1 自管表 `v1_work_cycles`（周期 = 基线 `WorkCycleRecord`
//!   的合同化重述：`revision`/`state`/`changeSummary` + 待应用变更集）。
//!   基线的 worktree-merge 应用（`applyWorktreeChanges`：git worktree → 项
//!   目根的合并）在 v1 无 worktree runtime 对应物，替换为**显式变更集应
//!   用**：生产者缝隙 [`record_work_cycle`] 登记逐文件变更
//!   （`{path, content}`），`work_cycles.apply` 把它们原子写入项目工作区。
//! * **两重 CAS**：`expectedRevision` 绑周期记录版本（「周期过期」）；
//!   `expectedWorkspaceRevision` 绑变更集路径的**内容派生**工作区版本
//!   （「工作区变化」）。调用方从周期生产者或上次 apply 的完成结果获得
//!   期望值（v1 无公开读操作直接暴露该值——登记为薄实现限制）。
//! * **并发应用**：per-cycle 受理串行 + 状态 CAS `active→applying`（0 行
//!   → RESOURCE_BUSY）；「同请求不重复修改文件」由受理幂等（同 key 重放
//!   receipt）+ CAS 共同保证。
//! * **恢复材料**：应用前把每个目标路径的原始字节备份为不可变产物
//!   （`recoveryArtifactId`）；中途失败保留该产物并把周期退回 `active`
//!   （基线 `updateCycleExecution('failed')` 的状态语义）。
//! * **归档**：只改状态与记录（`workCycle.archived` 事件），不应用未完成
//!   变更、不删除项目或工作树；`applying` 中的周期按状态拒绝。
//! * **关联 session/run**：每次 apply 在 per-cycle session 提交一个 run 记
//!   录应用行为（S2 受理机；与 schedules/discovery 同款缝）。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{ContractError, EffectStatus, RetrySpec};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::artifacts::publish_artifact;
use crate::public_v1::domains::automation::{
    self, CODE_VIRTUAL_PROJECT, REV_PREFIX, accept_operation, current_automation_rev, domain_db,
    domain_not_found, ensure_session, next_automation_rev, parse_limit, parse_offset_cursor,
    resource_busy, revision_conflict, sql_err, submit_text_run, wait_run_terminal,
};
use crate::public_v1::domains::discovery::resolve_real_workspace;
use crate::public_v1::domains::files::{ProjectResolveError, resolve_project};
use crate::public_v1::domains::schedules::{
    DomainDb, finish_domain_operation, mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339, run_state_str};

pub(crate) const DDL_WORK_CYCLES: &str = "CREATE TABLE IF NOT EXISTS v1_work_cycles (
    project_id TEXT NOT NULL,
    cycle_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    state TEXT NOT NULL,
    session_id TEXT,
    run_id TEXT,
    change_summary TEXT NOT NULL,
    workspace_revision TEXT NOT NULL,
    changes_json TEXT NOT NULL,
    last_apply_json TEXT,
    applied_at TEXT,
    archived_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (project_id, cycle_id)
);";

/// apply 后 run 的等待上界。
const RUN_WAIT_BOUND: Duration = Duration::from_secs(120);

/// MERGE_CONFLICT：合同错误表列明（本域的合并冲突面 = 目标路径被目录占
/// 据、路径越出工作区等无法落盘的结构性冲突）。
const CODE_MERGE_CONFLICT: &str = "MERGE_CONFLICT";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_work_cycles", DDL_WORK_CYCLES);
    reg.handler("work_cycles.list", |rt, input, _key, _rev| {
        Box::pin(work_cycles_list(rt, input))
    });
    reg.handler("work_cycles.apply", |rt, input, key, _rev| {
        Box::pin(work_cycles_apply(rt, input, key))
    });
    reg.handler("work_cycles.archive", |rt, input, _key, _rev| {
        Box::pin(work_cycles_archive(rt, input))
    });
}

// ---------------------------------------------------------------------------
// 周期行
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct CycleChange {
    /// 工作区相对路径（lexical 归一后必须仍在工作区内）。
    pub(crate) path: String,
    /// 目标内容（原始字节经 base64 存储）。
    pub(crate) content_base64: String,
}

#[derive(Clone)]
pub(crate) struct CycleRow {
    pub(crate) project_id: String,
    pub(crate) cycle_id: String,
    pub(crate) revision: String,
    pub(crate) state: String,
    pub(crate) session_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) change_summary: String,
    #[allow(dead_code)]
    pub(crate) workspace_revision: String,
    pub(crate) changes: Vec<CycleChange>,
}

const CYCLE_COLUMNS: &str = "project_id, cycle_id, revision, state, session_id, run_id, change_summary, \
     workspace_revision, changes_json";

fn cycle_row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<CycleRow> {
    let changes_json: String = row.get(8)?;
    let changes = serde_json::from_str::<Vec<Value>>(&changes_json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            Some(CycleChange {
                path: entry.get("path")?.as_str()?.to_owned(),
                content_base64: entry.get("contentBase64")?.as_str()?.to_owned(),
            })
        })
        .collect();
    Ok(CycleRow {
        project_id: row.get(0)?,
        cycle_id: row.get(1)?,
        revision: row.get(2)?,
        state: row.get(3)?,
        session_id: row.get(4)?,
        run_id: row.get(5)?,
        change_summary: row.get(6)?,
        workspace_revision: row.get(7)?,
        changes,
    })
}

fn cycle_row(
    conn: &Connection,
    project_id: &str,
    cycle_id: &str,
) -> Result<Option<CycleRow>, ContractError> {
    conn.query_row(
        &format!(
            "SELECT {CYCLE_COLUMNS} FROM v1_work_cycles
             WHERE project_id = ?1 AND cycle_id = ?2"
        ),
        params![project_id, cycle_id],
        cycle_row_of,
    )
    .optional()
    .map_err(sql_err)
}

fn cycles_for_project(conn: &Connection, project_id: &str) -> Result<Vec<CycleRow>, ContractError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {CYCLE_COLUMNS} FROM v1_work_cycles
             WHERE project_id = ?1 ORDER BY created_at, cycle_id"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![project_id], cycle_row_of)
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

fn cycle_json(row: &CycleRow) -> Value {
    json!({
        "cycleId": row.cycle_id,
        "projectId": row.project_id,
        "revision": row.revision,
        "state": row.state,
        "sessionId": row.session_id,
        "runId": row.run_id,
        "changeSummary": row.change_summary,
    })
}

fn append_cycle_event(
    store: &Arc<V1Store>,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let placeholder = format!("tmpcycle-{nanos}-{seq}");
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.event/1', ?5, ?5, ?6)",
        params![placeholder, resource_id, session_id, event_type, payload_str, now_rfc3339()],
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
// 工作区版本（变更集路径的内容派生 revision）
// ---------------------------------------------------------------------------

/// 变更集路径的当前内容身份（files.rs 同式：sha256("file"||len||bytes)，
/// 缺失 → "absent"）。
fn path_identity(workspace: &Path, relative: &str) -> String {
    match std::fs::read(workspace.join(relative)) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(b"file:");
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
            format!("file:{:x}", hasher.finalize())
        }
        Err(_) => "absent".to_owned(),
    }
}

/// 工作区在变更集路径上的内容派生 revision（排序后逐路径哈希）。
fn workspace_revision_of(workspace: &Path, changes: &[CycleChange]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"wsrev:");
    let mut sorted: Vec<&CycleChange> = changes.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    for change in sorted {
        let identity = path_identity(workspace, &change.path);
        hasher.update((change.path.len() as u64).to_le_bytes());
        hasher.update(change.path.as_bytes());
        hasher.update((identity.len() as u64).to_le_bytes());
        hasher.update(identity.as_bytes());
    }
    format!("wsrev-{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// 变更应用（原子写 + 恢复备份）
// ---------------------------------------------------------------------------

/// 原始字节备份（应用前全量采集；缺失路径记 null）。
fn backup_entries(workspace: &Path, changes: &[CycleChange]) -> Vec<Value> {
    changes
        .iter()
        .map(|change| {
            let original = std::fs::read(workspace.join(&change.path))
                .ok()
                .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));
            json!({"path": change.path, "originalBase64": original})
        })
        .collect()
}

fn merge_conflict(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_MERGE_CONFLICT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// 目标路径的最深已存在祖先必须 canonical 化后仍在工作区内（防符号链接
/// 父目录逃逸；workspace_dirs.rs `decode_server_path` 同款边界）。
fn ensure_ancestor_within(workspace: &Path, target: &Path) -> Result<(), ContractError> {
    let mut current = target.to_path_buf();
    loop {
        let Some(parent) = current.parent() else {
            return Ok(());
        };
        if parent == workspace {
            return Ok(());
        }
        match std::fs::symlink_metadata(parent) {
            Ok(_) => {
                let canonical = std::fs::canonicalize(parent).map_err(|err| {
                    merge_conflict(format!("cannot canonicalize `{}`: {err}", parent.display()))
                })?;
                if !canonical.starts_with(workspace) {
                    return Err(merge_conflict(format!(
                        "change path `{}` escapes the project workspace via `{}`",
                        target.display(),
                        parent.display()
                    )));
                }
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                current = parent.to_path_buf();
            }
            Err(err) => {
                return Err(merge_conflict(format!(
                    "cannot stat `{}`: {err}",
                    parent.display()
                )));
            }
        }
    }
}

/// 原子写入：同目录暂存 → fsync → rename（files.rs 写入同纪律）。
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), ContractError> {
    let io_err =
        |err: std::io::Error| merge_conflict(format!("cannot write `{}`: {err}", target.display()));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            merge_conflict(format!(
                "cannot create parent of `{}`: {err}",
                target.display()
            ))
        })?;
    }
    if target.is_dir() {
        return Err(merge_conflict(format!(
            "`{}` is a directory; cannot apply a file change onto it",
            target.display()
        )));
    }
    static STAGING_SEQ: AtomicI64 = AtomicI64::new(0);
    let staging = target.with_file_name(format!(
        ".pd-cycle-staging-{}-{}",
        std::process::id(),
        STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(io_err)?;
        file.write_all(bytes).map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
    }
    std::fs::rename(&staging, target).map_err(|err| {
        let _ = std::fs::remove_file(&staging);
        io_err(err)
    })
}

/// 依变更集把字节写进工作区（越出工作区的相对路径 → MERGE_CONFLICT）。
/// 返回 (已完成路径, 错误)：部分失败时已完成路径如实报告（完成记录与
/// last_apply 的恢复语义依据）。
fn apply_changes(
    workspace: &Path,
    changes: &[CycleChange],
) -> (Vec<String>, Option<ContractError>) {
    let mut changed_paths = Vec::new();
    for change in changes {
        let relative = Path::new(&change.path);
        if relative.is_absolute() || change.path.split('/').any(|part| part == "..") {
            return (
                changed_paths,
                Some(merge_conflict(format!(
                    "change path `{}` escapes the project workspace",
                    change.path
                ))),
            );
        }
        let target = workspace.join(relative);
        if let Err(error) = ensure_ancestor_within(workspace, &target) {
            return (changed_paths, Some(error));
        }
        let bytes = match base64::engine::general_purpose::STANDARD.decode(&change.content_base64) {
            Ok(bytes) => bytes,
            Err(err) => {
                return (
                    changed_paths,
                    Some(ContractError::new(
                        "INVALID_ARGUMENT",
                        format!("change `{}` has invalid base64 content: {err}", change.path),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )),
                );
            }
        };
        if let Err(error) = atomic_write(&target, &bytes) {
            return (changed_paths, Some(error));
        }
        changed_paths.push(change.path.clone());
    }
    (changed_paths, None)
}

// ---------------------------------------------------------------------------
// work_cycles.list
// ---------------------------------------------------------------------------

fn project_row_exists(rt: &Arc<PublicV1Runtime>, project_id: &str) -> Result<bool, ContractError> {
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM projects WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(exists.is_some())
}

async fn work_cycles_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let cycle_filter = input
        .get("cycleId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let cursor = parse_offset_cursor(input.get("cursor").and_then(Value::as_str), "wc")?;
    let limit = parse_limit(&input);

    // 只读不触发应用或归档；周期归属真实项目（虚拟 → VIRTUAL_PROJECT）。
    if !project_row_exists(&rt, &project_id)? {
        return Err(domain_not_found("project", &project_id).into());
    }
    if let Err(ProjectResolveError::Virtual(message)) = resolve_project(rt.store(), &project_id) {
        return Err(ContractError::new(
            CODE_VIRTUAL_PROJECT,
            format!("virtual project has no real workspace: {message}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    let (mut cycles, snapshot_revision) = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        (
            cycles_for_project(&conn, &project_id)?,
            current_automation_rev(&conn)?,
        )
    };
    if let Some(cycle_id) = &cycle_filter {
        cycles.retain(|cycle| &cycle.cycle_id == cycle_id);
    }
    let mut next_cursor = None;
    let mut items = Vec::new();
    for (index, cycle) in cycles.iter().enumerate().skip(cursor) {
        if items.len() >= limit {
            next_cursor = Some(format!("wc-{index}"));
            break;
        }
        items.push(cycle_json(cycle));
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "nextCursor": next_cursor,
            "snapshotRevision": snapshot_revision,
        }),
    ))
}

// ---------------------------------------------------------------------------
// work_cycles.apply
// ---------------------------------------------------------------------------

/// 一次 apply 的受理快照。
struct ApplyRequest {
    project_id: String,
    cycle: CycleRow,
    session_id: String,
    expected_revision: String,
    expected_workspace_revision: String,
}

/// per-cycle 受理串行（并发 apply 的第一道边界；CAS 是第二道）。
async fn lock_cycle(project_id: &str, cycle_id: &str) -> OwnedMutexGuard<()> {
    #[allow(clippy::type_complexity)]
    static LOCKS: OnceLock<Mutex<HashMap<(String, String), Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let key = (project_id.to_owned(), cycle_id.to_owned());
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mutex = {
        let mut map = locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(key).or_default().clone()
    };
    mutex.lock_owned().await
}

fn validate_apply(
    rt: &Arc<PublicV1Runtime>,
    request: &ApplyRequest,
    workspace: &Path,
) -> Result<(), ContractError> {
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let current = cycle_row(&conn, &request.project_id, &request.cycle.cycle_id)?
        .ok_or_else(|| domain_not_found("work cycle", &request.cycle.cycle_id))?;
    if current.revision != request.expected_revision {
        return Err(revision_conflict(format!(
            "expectedRevision `{}` does not match cycle revision `{}`",
            request.expected_revision, current.revision
        )));
    }
    match current.state.as_str() {
        "active" => {}
        "applying" => {
            return Err(resource_busy(format!(
                "work cycle `{}` is currently being applied",
                current.cycle_id
            )));
        }
        other => {
            return Err(revision_conflict(format!(
                "work cycle `{}` is `{other}` (stale); only an active cycle can be applied",
                current.cycle_id
            )));
        }
    }
    // 工作区变化：变更集路径的实时内容版本必须等于调用方期望。
    let live = workspace_revision_of(workspace, &current.changes);
    if live != request.expected_workspace_revision {
        return Err(revision_conflict(format!(
            "expectedWorkspaceRevision `{}` does not match the live workspace `{live}`",
            request.expected_workspace_revision
        )));
    }
    Ok(())
}

/// CAS active→applying（0 行 → 并发应用 RESOURCE_BUSY）。
fn claim_applying(
    conn: &Connection,
    project_id: &str,
    cycle_id: &str,
) -> Result<bool, ContractError> {
    let changed = conn
        .execute(
            "UPDATE v1_work_cycles SET state = 'applying', updated_at = ?3
             WHERE project_id = ?1 AND cycle_id = ?2 AND state = 'active'",
            params![project_id, cycle_id, now_rfc3339()],
        )
        .map_err(sql_err)?;
    Ok(changed > 0)
}

fn revert_to_active(conn: &Connection, project_id: &str, cycle_id: &str) {
    let _ = conn.execute(
        "UPDATE v1_work_cycles SET state = 'active', updated_at = ?3
         WHERE project_id = ?1 AND cycle_id = ?2",
        params![project_id, cycle_id, now_rfc3339()],
    );
}

fn record_last_apply(conn: &Connection, project_id: &str, cycle_id: &str, payload: &Value) {
    let _ = conn.execute(
        "UPDATE v1_work_cycles SET last_apply_json = ?3, updated_at = ?4
         WHERE project_id = ?1 AND cycle_id = ?2",
        params![
            project_id,
            cycle_id,
            serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
            now_rfc3339()
        ],
    );
}

/// apply 的 worker：CAS → 备份 → 写入 → run → 落状态。
async fn execute_cycle_apply(
    rt: Arc<PublicV1Runtime>,
    db: Arc<DomainDb>,
    operation_id: String,
    request: ApplyRequest,
    workspace: PathBuf,
) {
    let _ = mark_domain_operation_running(&db, &operation_id, &request.session_id);
    // 失败收尾：周期退回 active + last_apply 记录 + 恢复材料保留。
    let fail = |error: &ContractError,
                run_id: Option<&str>,
                changed: &[String],
                recovery: Option<&str>| {
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            revert_to_active(&conn, &request.project_id, &request.cycle.cycle_id);
            record_last_apply(
                &conn,
                &request.project_id,
                &request.cycle.cycle_id,
                &json!({
                    "error": {"code": error.code, "message": error.message},
                    "runId": run_id,
                    "changedPaths": changed,
                    "recoveryArtifactId": recovery,
                }),
            );
        }
        let completion = json!({
            "projectId": request.project_id,
            "cycleId": request.cycle.cycle_id,
            "cycleRevision": request.expected_revision,
            "workspaceRevision": request.expected_workspace_revision,
            "sessionId": request.session_id,
            "runId": run_id,
            "changedPaths": changed,
            "recoveryArtifactId": recovery,
        });
        let _ = finish_domain_operation(
            &db,
            &operation_id,
            &request.session_id,
            "failed",
            &completion,
        );
    };
    // CAS：并发应用明确拒绝。
    let claimed = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        match claim_applying(&conn, &request.project_id, &request.cycle.cycle_id) {
            Ok(claimed) => claimed,
            Err(error) => {
                fail(&error, None, &[], None);
                return;
            }
        }
    };
    if !claimed {
        let error = resource_busy(format!(
            "work cycle `{}` is currently being applied",
            request.cycle.cycle_id
        ));
        fail(&error, None, &[], None);
        return;
    }
    // 备份（恢复材料；备份失败不落任何写入）。
    let backup = backup_entries(&workspace, &request.cycle.changes);
    let recovery_payload = serde_json::to_string(&json!({
        "cycleId": request.cycle.cycle_id,
        "projectId": request.project_id,
        "entries": backup,
    }))
    .unwrap_or_else(|_| "{}".into());
    let recovery_artifact_id = match publish_artifact(
        rt.store(),
        &request.project_id,
        "application/json",
        Some(&format!(
            "work-cycle-recovery-{}.json",
            request.cycle.cycle_id
        )),
        recovery_payload.as_bytes(),
        "work-cycle-recovery",
    ) {
        Ok(artifact) => artifact["artifactId"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        Err(error) => {
            fail(&error, None, &[], None);
            return;
        }
    };
    // 写入（部分失败保留恢复材料，周期退回 active）。
    let (changed_paths, write_error) = apply_changes(&workspace, &request.cycle.changes);
    if let Some(error) = write_error {
        fail(&error, None, &changed_paths, Some(&recovery_artifact_id));
        return;
    }
    // 应用行为记录为 run（关联 session/run 可核对）。
    let run_result = (|| -> Result<String, ContractError> {
        ensure_session(
            rt.store(),
            &request.session_id,
            &request.project_id,
            &format!("work cycle {}", request.cycle.cycle_id),
        )?;
        let child_key = format!(
            "cycle-apply-{}-{}",
            request.cycle.cycle_id,
            rt.store().next_id()
        );
        submit_text_run(
            &rt,
            &request.session_id,
            &format!(
                "apply work cycle {}: {}",
                request.cycle.cycle_id, request.cycle.change_summary
            ),
            &child_key,
        )
    })();
    let run_id = match run_result {
        Ok(run_id) => run_id,
        Err(error) => {
            fail(&error, None, &changed_paths, Some(&recovery_artifact_id));
            return;
        }
    };
    let finished = wait_run_terminal(&rt, &run_id, RUN_WAIT_BOUND).await;
    let run_state = finished
        .as_ref()
        .map(|run| run_state_str(run.state).to_owned())
        .unwrap_or_else(|| "result_unknown".to_owned());
    let new_workspace_revision = workspace_revision_of(&workspace, &request.cycle.changes);
    let new_cycle_revision = {
        let mut conn = rt.store().conn.lock().expect("sqlite mutex");
        let rev =
            next_automation_rev(&mut conn).unwrap_or_else(|_| format!("{REV_PREFIX}00000000"));
        let _ = conn.execute(
            "UPDATE v1_work_cycles
             SET state = 'applied', revision = ?3, run_id = ?4, session_id = ?5,
                 applied_at = ?6, updated_at = ?6
             WHERE project_id = ?1 AND cycle_id = ?2",
            params![
                request.project_id,
                request.cycle.cycle_id,
                rev,
                run_id,
                request.session_id,
                now_rfc3339()
            ],
        );
        rev
    };
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        record_last_apply(
            &conn,
            &request.project_id,
            &request.cycle.cycle_id,
            &json!({
                "runId": run_id,
                "runState": run_state,
                "changedPaths": changed_paths,
                "recoveryArtifactId": recovery_artifact_id,
            }),
        );
    }
    let completion = json!({
        "projectId": request.project_id,
        "cycleId": request.cycle.cycle_id,
        "cycleRevision": new_cycle_revision,
        "workspaceRevision": new_workspace_revision,
        "sessionId": request.session_id,
        "runId": run_id,
        "changedPaths": changed_paths,
        "recoveryArtifactId": recovery_artifact_id,
    });
    let _ = finish_domain_operation(
        &db,
        &operation_id,
        &request.session_id,
        "succeeded",
        &completion,
    );
}

async fn work_cycles_apply(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = domain_db(&rt)?;
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let cycle_id = input["cycleId"].as_str().unwrap_or_default().to_owned();
    let expected_revision = input["expectedRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let expected_workspace_revision = input["expectedWorkspaceRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    // 受理串行（校验闭包内做全部确定性检查；重放不受周期后续变化影响）。
    let _guard = lock_cycle(&project_id, &cycle_id).await;
    let operation = accept_operation(
        &rt,
        &db,
        "work_cycles.apply",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || {
            // 缺工作区在受理前拒绝（错误表列 WORKSPACE_UNAVAILABLE）。
            let workspace = resolve_real_workspace(&rt, &project_id)?;
            let request = {
                let conn = rt.store().conn.lock().expect("sqlite mutex");
                let cycle = cycle_row(&conn, &project_id, &cycle_id)?
                    .ok_or_else(|| domain_not_found("work cycle", &cycle_id))?;
                if expected_revision != cycle.revision {
                    return Err(revision_conflict(format!(
                        "expectedRevision `{expected_revision}` does not match cycle revision `{}`",
                        cycle.revision
                    )));
                }
                ApplyRequest {
                    session_id: format!("s-cycle-{project_id}-{cycle_id}"),
                    project_id: project_id.clone(),
                    cycle,
                    expected_revision: expected_revision.clone(),
                    expected_workspace_revision: expected_workspace_revision.clone(),
                }
            };
            validate_apply(&rt, &request, &workspace).map(|_| ())
        },
    )?;
    // 重放（deduplicated）到此为止：直接回受理 receipt，不再触达周期状态。
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    // 受理后快照（罕见竞态 → 操作显式失败收尾，受理记录不悬空）。
    let (request, workspace) = match (|| -> Result<(ApplyRequest, PathBuf), ContractError> {
        let workspace = resolve_real_workspace(&rt, &project_id)?;
        let cycle = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let cycle = cycle_row(&conn, &project_id, &cycle_id)?
                .ok_or_else(|| domain_not_found("work cycle", &cycle_id))?;
            if expected_revision != cycle.revision {
                return Err(revision_conflict(format!(
                    "expectedRevision `{expected_revision}` does not match cycle revision `{}`",
                    cycle.revision
                )));
            }
            cycle
        };
        Ok((
            ApplyRequest {
                session_id: format!("s-cycle-{project_id}-{cycle_id}"),
                project_id: project_id.clone(),
                cycle,
                expected_revision: expected_revision.clone(),
                expected_workspace_revision: expected_workspace_revision.clone(),
            },
            workspace,
        ))
    })() {
        Ok(pair) => pair,
        Err(error) => {
            let session_for_events = format!("s-cycle-{project_id}-{cycle_id}");
            let _ = finish_domain_operation(
                &db,
                &operation.operation_id,
                &session_for_events,
                "failed",
                &json!({
                    "projectId": project_id,
                    "cycleId": cycle_id,
                    "cycleRevision": Value::Null,
                    "workspaceRevision": Value::Null,
                    "sessionId": Value::Null,
                    "runId": Value::Null,
                    "changedPaths": [],
                    "recoveryArtifactId": Value::Null,
                }),
            );
            return Err(error.into());
        }
    };
    drop(_guard);
    let runner_rt = rt.clone();
    let runner_db = db.clone();
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        execute_cycle_apply(runner_rt, runner_db, operation_id, request, workspace).await;
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// work_cycles.archive
// ---------------------------------------------------------------------------

async fn work_cycles_archive(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let cycle_id = input["cycleId"].as_str().unwrap_or_default().to_owned();
    let expected_revision = input["expectedRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let (archived_cycle, revision) = {
        let mut conn = rt.store().conn.lock().expect("sqlite mutex");
        let cycle = cycle_row(&conn, &project_id, &cycle_id)?
            .ok_or_else(|| domain_not_found("work cycle", &cycle_id))?;
        if expected_revision != cycle.revision {
            return Err(revision_conflict(format!(
                "expectedRevision `{expected_revision}` does not match cycle revision `{}`",
                cycle.revision
            ))
            .into());
        }
        if cycle.state == "applying" {
            return Err(resource_busy(format!(
                "work cycle `{}` is currently being applied and cannot be archived",
                cycle_id
            ))
            .into());
        }
        let revision = next_automation_rev(&mut conn)?;
        conn.execute(
            "UPDATE v1_work_cycles
             SET state = 'archived', revision = ?3, archived_at = ?4, updated_at = ?4
             WHERE project_id = ?1 AND cycle_id = ?2",
            params![project_id, cycle_id, revision, now_rfc3339()],
        )
        .map_err(sql_err)?;
        let archived = cycle_row(&conn, &project_id, &cycle_id)?
            .ok_or_else(|| domain_not_found("work cycle", &cycle_id))?;
        (archived, revision)
    };
    // workCycle.archived（合同事件表）。
    let _ = append_cycle_event(
        rt.store(),
        &cycle_id,
        archived_cycle.session_id.as_deref().unwrap_or(""),
        "workCycle.archived",
        &json!({
            "cycleId": cycle_id,
            "projectId": project_id,
            "revision": revision,
        }),
    );
    Ok((
        ResponseOutcome::Succeeded,
        json!({"cycle": cycle_json(&archived_cycle)}),
    ))
}

// ---------------------------------------------------------------------------
// 生产者缝隙（发现 runtime 的周期写入面；e2e 播种同入口）
// ---------------------------------------------------------------------------

/// 登记一个工作周期：待应用变更集 + 内容派生 `workspaceRevision`（调用方
/// 把该值作为后续 `work_cycles.apply` 的 `expectedWorkspaceRevision`）。
/// 返回 `{cycleId, revision, workspaceRevision}`。
pub fn record_work_cycle(
    home: &std::path::Path,
    project_id: &str,
    cycle_id: &str,
    change_summary: &str,
    changes: &[(String, Vec<u8>)],
) -> Result<Value, ContractError> {
    // 工作区必须已可解析（workspaceRevision 按当前工作区内容计算）。
    let store = automation::open_store_for_seeding(home)?;
    let workspace = match resolve_project(&store, project_id) {
        Ok(ws) => ws.workspace,
        Err(ProjectResolveError::Virtual(message)) => {
            return Err(ContractError::new(
                CODE_VIRTUAL_PROJECT,
                format!("virtual project has no real workspace: {message}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Err(ProjectResolveError::NotFound(message)) => {
            return Err(ContractError::new(
                "WORKSPACE_UNAVAILABLE",
                format!("project workspace unavailable: {message}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Err(ProjectResolveError::Contract(err)) => return Err(err),
    };
    let changes: Vec<CycleChange> = changes
        .iter()
        .map(|(path, content)| CycleChange {
            path: path.clone(),
            content_base64: base64::engine::general_purpose::STANDARD.encode(content),
        })
        .collect();
    let workspace_revision = workspace_revision_of(&workspace, &changes);
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let revision = next_automation_rev(&mut conn)?;
    let changes_json = serde_json::to_string(
        &changes
            .iter()
            .map(|change| json!({"path": change.path, "contentBase64": change.content_base64}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO v1_work_cycles
            (project_id, cycle_id, revision, state, session_id, run_id, change_summary,
             workspace_revision, changes_json, last_apply_json, applied_at, archived_at,
             created_at, updated_at)
         VALUES (?1, ?2, ?3, 'active', NULL, NULL, ?4, ?5, ?6, NULL, NULL, NULL, ?7, ?7)
         ON CONFLICT(project_id, cycle_id) DO UPDATE SET
            change_summary = excluded.change_summary,
            workspace_revision = excluded.workspace_revision,
            changes_json = excluded.changes_json,
            state = 'active',
            updated_at = excluded.updated_at",
        params![
            project_id,
            cycle_id,
            revision,
            change_summary,
            workspace_revision,
            changes_json,
            now_rfc3339()
        ],
    )
    .map_err(sql_err)?;
    Ok(json!({
        "cycleId": cycle_id,
        "revision": revision,
        "workspaceRevision": workspace_revision,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::domains::automation::tests::{
        dispatch_err, dispatch_ok, fixture, wait_domain_operation,
    };
    use agentcpm_pd_protocol::registry::OperationRegistry;

    async fn seed_project(
        fx: &crate::public_v1::domains::automation::tests::Fixture,
        tag: &str,
    ) -> PathBuf {
        crate::public_v1::domains::automation::tests::seed_real_project(fx, tag).await
    }

    fn seed_cycle(
        fx: &crate::public_v1::domains::automation::tests::Fixture,
        project_id: &str,
        cycle_id: &str,
        files: &[(&str, &str)],
    ) -> Value {
        let changes: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(path, content)| ((*path).to_owned(), content.as_bytes().to_vec()))
            .collect();
        record_work_cycle(
            fx.home.as_path(),
            project_id,
            cycle_id,
            "fixture cycle",
            &changes,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn list_apply_and_archive_round_trip() {
        let fx = fixture("wc-roundtrip").await;
        let workspace = seed_project(&fx, "proj-1").await;
        let seeded = seed_cycle(
            &fx,
            "proj-1",
            "cycle-1",
            &[("docs/summary.md", "applied content")],
        );
        let cycle_revision = seeded["revision"].as_str().unwrap().to_owned();
        let workspace_revision = seeded["workspaceRevision"].as_str().unwrap().to_owned();

        // list：周期与项目关系真实；schema。
        let page = dispatch_ok(
            &fx.rt,
            "work_cycles.list",
            json!({"projectId": "proj-1"}),
            None,
        )
        .await;
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["cycleId"], "cycle-1");
        assert_eq!(items[0]["projectId"], "proj-1");
        assert_eq!(items[0]["state"], "active");
        assert_eq!(items[0]["changeSummary"], "fixture cycle");
        assert!(
            OperationRegistry::global()
                .validate_result("work_cycles.list", &page)
                .is_ok(),
            "work_cycles.list 结果过 schema"
        );

        // 工作区变化 → REVISION_CONFLICT（先动一个非目标文件不算；动目标
        // 文件才算——版本只覆盖变更集路径）。
        std::fs::write(workspace.join("docs/summary.md"), "drift").unwrap();
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.apply",
            json!({"projectId": "proj-1", "cycleId": "cycle-1",
                   "expectedRevision": cycle_revision,
                   "expectedWorkspaceRevision": workspace_revision}),
            Some("k-wc-drift"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");
        // 恢复原始内容后重新登记工作区版本（模拟调用方重新观察）。
        std::fs::write(workspace.join("docs/summary.md"), "applied content").unwrap();
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.apply",
            json!({"projectId": "proj-1", "cycleId": "cycle-1",
                   "expectedRevision": cycle_revision,
                   "expectedWorkspaceRevision": "stale-wsrev"}),
            Some("k-wc-stale"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");

        // 受理 → 应用：变更落盘 + run/session 关联 + 恢复材料。
        let receipt = dispatch_ok(
            &fx.rt,
            "work_cycles.apply",
            json!({"projectId": "proj-1", "cycleId": "cycle-1",
                   "expectedRevision": cycle_revision,
                   "expectedWorkspaceRevision": workspace_revision}),
            Some("k-wc-1"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        assert_eq!(completion["projectId"], "proj-1");
        assert_eq!(completion["cycleId"], "cycle-1");
        assert_eq!(
            completion["changedPaths"],
            json!(["docs/summary.md"]),
            "changedPaths 如实报告"
        );
        let run_id = completion["runId"].as_str().expect("apply run");
        assert!(!run_id.is_empty());
        assert!(completion["sessionId"].as_str().is_some());
        assert!(
            completion["recoveryArtifactId"].as_str().is_some(),
            "恢复材料登记"
        );
        assert!(completion["workspaceRevision"].as_str().is_some());
        // 文件系统效果。
        let applied = std::fs::read_to_string(workspace.join("docs/summary.md")).unwrap();
        assert_eq!(applied, "applied content");
        // 周期状态。
        let page = dispatch_ok(
            &fx.rt,
            "work_cycles.list",
            json!({"projectId": "proj-1", "cycleId": "cycle-1"}),
            None,
        )
        .await;
        assert_eq!(page["items"][0]["state"], "applied");
        assert_eq!(page["items"][0]["runId"], run_id);
        assert!(page["items"][0]["sessionId"].as_str().is_some());

        // 应用后的周期再 apply → 周期过期（REVISION_CONFLICT）。
        let new_revision = page["items"][0]["revision"].as_str().unwrap().to_owned();
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.apply",
            json!({"projectId": "proj-1", "cycleId": "cycle-1",
                   "expectedRevision": new_revision,
                   "expectedWorkspaceRevision": completion["workspaceRevision"]}),
            Some("k-wc-2"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT", "applied 周期不可再应用");

        // 归档：状态迁移 + 事件 + 不删除工作区。
        let archived = dispatch_ok(
            &fx.rt,
            "work_cycles.archive",
            json!({"projectId": "proj-1", "cycleId": "cycle-1", "expectedRevision": new_revision}),
            None,
        )
        .await;
        assert_eq!(archived["cycle"]["state"], "archived");
        assert_eq!(archived["cycle"]["cycleId"], "cycle-1");
        assert!(
            OperationRegistry::global()
                .validate_result("work_cycles.archive", &archived)
                .is_ok(),
            "work_cycles.archive 结果过 schema"
        );
        assert!(workspace.exists(), "归档不删除工作区");
        // 事件已落（workCycle.archived）。
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'workCycle.archived' AND resource_id = ?1",
                    params!["cycle-1"],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "workCycle.archived 事件恰好一条");
        }
        // 版本不匹配归档拒绝；未知周期 NOT_FOUND。
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.archive",
            json!({"projectId": "proj-1", "cycleId": "cycle-1", "expectedRevision": new_revision}),
            None,
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.archive",
            json!({"projectId": "proj-1", "cycleId": "missing", "expectedRevision": "r"}),
            None,
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn apply_partial_failure_keeps_recovery_material_and_reverts() {
        let fx = fixture("wc-partial").await;
        let workspace = seed_project(&fx, "proj-1").await;
        // 第二个目标是目录 → 写入失败（MERGE_CONFLICT），第一个已写入。
        std::fs::create_dir_all(workspace.join("blocked")).unwrap();
        let seeded = seed_cycle(
            &fx,
            "proj-1",
            "cycle-p",
            &[("first.md", "first"), ("blocked", "not a file")],
        );
        let receipt = dispatch_ok(
            &fx.rt,
            "work_cycles.apply",
            json!({"projectId": "proj-1", "cycleId": "cycle-p",
                   "expectedRevision": seeded["revision"],
                   "expectedWorkspaceRevision": seeded["workspaceRevision"]}),
            Some("k-wc-partial"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "failed");
        let completion = &operation["result"];
        assert!(
            completion["recoveryArtifactId"].as_str().is_some(),
            "部分失败保留恢复材料"
        );
        // 周期退回 active（可再处理），已写文件如实存在。
        let page = dispatch_ok(
            &fx.rt,
            "work_cycles.list",
            json!({"projectId": "proj-1", "cycleId": "cycle-p"}),
            None,
        )
        .await;
        assert_eq!(page["items"][0]["state"], "active");
        assert_eq!(
            std::fs::read_to_string(workspace.join("first.md")).unwrap(),
            "first"
        );
        // 恢复材料可读（原始内容备份）。
        let artifact_id = completion["recoveryArtifactId"].as_str().unwrap();
        let bytes =
            crate::public_v1::domains::artifacts::read_artifact_bytes(fx.rt.store(), artifact_id)
                .unwrap();
        let recovery: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(recovery["cycleId"], "cycle-p");
    }

    #[tokio::test]
    async fn virtual_project_rejected_and_missing_cycle_not_found() {
        let fx = fixture("wc-virtual").await;
        // 虚拟项目（无授权根）拒绝 list/apply。
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": fx.instance_id,
                    "projectId": "proj-virtual",
                    "name": "Virtual",
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": "undeclared", "relativePath": "ws"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.list",
            json!({"projectId": "proj-virtual"}),
            None,
        )
        .await;
        assert_eq!(err.code, "VIRTUAL_PROJECT");
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.apply",
            json!({"projectId": "proj-virtual", "cycleId": "c", "expectedRevision": "r",
                   "expectedWorkspaceRevision": "w"}),
            Some("k-wc-virtual"),
        )
        .await;
        assert_eq!(err.code, "VIRTUAL_PROJECT");
        // 真实项目未知周期 → NOT_FOUND；缺工作区（虚拟结构）在 apply 受理前拒。
        seed_project(&fx, "proj-1").await;
        let err = dispatch_err(
            &fx.rt,
            "work_cycles.apply",
            json!({"projectId": "proj-1", "cycleId": "missing", "expectedRevision": "r",
                   "expectedWorkspaceRevision": "w"}),
            Some("k-wc-missing"),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");
    }
}
