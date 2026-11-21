//! `automation.*` + 本任务四域共享核心（cli.task.automation，S3 W6）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `automation.*` 条目（按
//! id 检索）。TS 基线（cc7c4e6）语义参考：`ui/server/routes/projects.js`
//! 的 always-on 路由、`src/always-on/runtime/AlwaysOnRuntime.ts` 的
//! `applyCycle`/`abortRun`/`rerunPlan` 与旧 Rust 线
//! `runtime/always_on/manager.rs` 的同名控制面（只读参考；深执行管线
//! DiscoveryFire 在旧线即未移植，v1 按合同薄实现）。
//!
//! ## v1 薄实现的选择（差异在任务报告 §4 登记）
//!
//! * **计划注册表**：`automation.apply` 与 `discovery.*` 共用同一计划空间
//!   （`v1_automation_plans`，能力 a034 的来源清单含 discovery-plans 路
//!   由）。v1 的 285 个公开操作中没有「写入探索计划」的操作（基线里计
//!   划由发现 agent 的执行管线产出）；注册表行由发现 runtime 的生产者缝
//!   隙 [`crate::public_v1::domains::discovery::record_discovery_plan`] 写
//!   入，未装配时为空表（读操作如实返回空，执行操作 NOT_FOUND）。
//! * **automation 行的产生**：`automationId` 由调用方选择；`automation.apply`
//!   是唯一的 automation 写操作——受理时创建/更新绑定（planId+planRevision）
//!   并执行一次计划版本（「执行指定版本」）；持续语义 = 绑定与执行历史
//!   持久存在，可 apply 新版本 / rerun_plan / abort。
//! * **automation.get 未配置**：项目没有任何 automation 行时按能力验收要
//!   求（「未配置返回明确不可用而非空成功」）返回 UNAVAILABLE，不伪造空
//!   计划成功；指定 automationId 不存在 → NOT_FOUND。
//! * **执行 = v1 run**：一次计划执行就是 S2 受理机里的一个 run
//!   （`V1Store::submit_run` + `start_session_worker`，schedules.trigger 同
//!   款缝），per-automation 固定 session。abort = 取消该 run（终态如实返
//!   回）；rerun_plan = 新 run 且保留 previousRunId 关联。
//! * **自动化域不要求真实工作区**：其错误表没有 VIRTUAL_PROJECT /
//!   WORKSPACE_UNAVAILABLE；受理权威是计划注册表。真实项目约束由
//!   discovery.*/work_cycles.*（错误表明确列出该两码）承担。
//! * OUT_OF_SCOPE（StaffDeck SOP 范围外）在 v1 无对应输入面，不发射。
//!
//! 存储沿用 memory.rs 的分工：本域表走共享 `V1Store` 连接（S3 底座 §1.4），
//! 异步操作的幂等受理/完成记录走 `v1_domain_operations` 缝（schedules.rs
//! 所有，`operations.get/list` 因此可见本域操作）。
//!
//! 锁纪律：`V1Store::conn` 的 guard 持有期间绝不调用会再次锁该连接的
//! store 方法（`get_run`/`cancel_run`/`submit_run` 等）——需要 run 状态时
//! 用同连接直查 `runs` 表（[`run_is_live_conn`]）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetryMode, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::schedules::{
    DomainDb, DomainOperationLookup, finish_domain_operation, lookup_domain_operation,
    mark_domain_operation_running, record_domain_operation_accept,
    record_domain_operation_rejection,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{SubmitDisposition, V1Store, now_rfc3339, run_state_str};

/// 合同错误表中本域族专属（protocol crate 尚无公共常量）的错误码。
pub(crate) const CODE_PLAN_EMPTY: &str = "PLAN_EMPTY";
pub(crate) const CODE_VIRTUAL_PROJECT: &str = "VIRTUAL_PROJECT";
pub(crate) const CODE_WORKSPACE_UNAVAILABLE: &str = "WORKSPACE_UNAVAILABLE";

/// 本任务四域共享的 revision 前缀（独立于核心 `rev-…` 与 schedules 的
/// `srev-…` 空间；计数器在 `v1_automation_meta`）。
pub(crate) const REV_PREFIX: &str = "arev-";

/// 计划注册表（discovery 计划与通用持续计划共用）。
pub(crate) const DDL_PLANS: &str = "CREATE TABLE IF NOT EXISTS v1_automation_plans (
    project_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    state TEXT NOT NULL,
    revision TEXT NOT NULL,
    last_run_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (project_id, plan_id)
);";

const DDL_AUTOMATIONS: &str = "CREATE TABLE IF NOT EXISTS v1_automations (
    project_id TEXT NOT NULL,
    automation_id TEXT NOT NULL,
    plan_id TEXT,
    plan_revision TEXT,
    state TEXT NOT NULL,
    generation TEXT,
    active_run_id TEXT,
    session_id TEXT NOT NULL,
    last_result TEXT,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (project_id, automation_id)
);";

const DDL_AUTOMATION_EXECUTIONS: &str = "CREATE TABLE IF NOT EXISTS v1_automation_executions (
    execution_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    automation_id TEXT NOT NULL,
    run_id TEXT,
    previous_run_id TEXT,
    state TEXT NOT NULL,
    error_code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

pub(crate) const DDL_META: &str = "CREATE TABLE IF NOT EXISTS v1_automation_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

/// 四域共用的全部 DDL（各自 `reg.schema` 之外，`DomainDb::open` 也带着，
/// 生产者缝隙单独开店时表可用）。discovery / work_cycles / agent_workflow
/// 的表由各自文件追加到该清单。
pub(crate) const ALL_DOMAIN_DDL: &[&str] = &[
    DDL_PLANS,
    DDL_AUTOMATIONS,
    DDL_AUTOMATION_EXECUTIONS,
    DDL_META,
    // discovery.rs（同任务领域；IF NOT EXISTS 双注册安全）
    crate::public_v1::domains::discovery::DDL_DISCOVERY_EXECUTIONS,
    // work_cycles.rs
    crate::public_v1::domains::work_cycles::DDL_WORK_CYCLES,
    // agent_workflow.rs
    crate::public_v1::domains::agent_workflow::DDL_WORKFLOW_PLANS,
    crate::public_v1::domains::agent_workflow::DDL_WORKFLOWS,
];

/// automation 执行的有界等待（离线 driver 默认 25ms 完成；生产 turn 机的
/// 超时由其自身的运行窗口承担，worker 超时后按 result_unknown 收尾）。
const RUN_WAIT_BOUND: Duration = Duration::from_secs(120);

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_automation_plans", DDL_PLANS);
    reg.schema("v1_automations", DDL_AUTOMATIONS);
    reg.schema("v1_automation_executions", DDL_AUTOMATION_EXECUTIONS);
    reg.schema("v1_automation_meta", DDL_META);
    reg.handler("automation.get", |rt, input, _key, _rev| {
        Box::pin(automation_get(rt, input))
    });
    reg.handler("automation.apply", |rt, input, key, _rev| {
        Box::pin(automation_apply(rt, input, key))
    });
    reg.handler("automation.abort", |rt, input, key, _rev| {
        Box::pin(automation_abort(rt, input, key))
    });
    reg.handler("automation.rerun_plan", |rt, input, key, _rev| {
        Box::pin(automation_rerun_plan(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 共享错误/工具助手（discovery / work_cycles / agent_workflow 复用）
// ---------------------------------------------------------------------------

pub(crate) fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("automation domain storage error: {err}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

pub(crate) fn invalid_arg(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

pub(crate) fn domain_not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

pub(crate) fn revision_conflict(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

pub(crate) fn resource_busy(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "RESOURCE_BUSY",
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

pub(crate) fn domain_unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

pub(crate) fn plan_empty(plan_id: &str) -> ContractError {
    ContractError::new(
        CODE_PLAN_EMPTY,
        format!("discovery plan `{plan_id}` has no body and cannot execute"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// 单条 offset 游标解析（`<prefix>-<n>`；schedules 同款）。
pub(crate) fn parse_offset_cursor(
    cursor: Option<&str>,
    prefix: &str,
) -> Result<usize, ContractError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let rest = if prefix.ends_with('-') {
        cursor.strip_prefix(prefix)
    } else {
        cursor
            .strip_prefix(prefix)
            .and_then(|s| s.strip_prefix('-'))
    };
    rest.and_then(|rest| rest.parse::<usize>().ok())
        .ok_or_else(|| invalid_arg(format!("malformed cursor `{cursor}`")))
}

pub(crate) fn parse_limit(input: &Value) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize
}

// ---------------------------------------------------------------------------
// revision 计数器（v1_automation_meta，共享连接上的事务递增）
// ---------------------------------------------------------------------------

pub(crate) fn current_automation_rev(conn: &Connection) -> Result<String, ContractError> {
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM v1_automation_meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(current.unwrap_or_else(|| format!("{REV_PREFIX}00000000")))
}

pub(crate) fn next_automation_rev_in_conn(conn: &Connection) -> Result<String, ContractError> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM v1_automation_meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let next = current.unwrap_or(0) + 1;
    conn.execute(
        "INSERT INTO v1_automation_meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("{REV_PREFIX}{next:08}"))
}

pub(crate) fn next_automation_rev(conn: &mut Connection) -> Result<String, ContractError> {
    let tx = conn.transaction().map_err(sql_err)?;
    let rev = next_automation_rev_in_conn(&tx)?;
    tx.commit().map_err(sql_err)?;
    Ok(rev)
}

// ---------------------------------------------------------------------------
// 共享 DomainDb（异步操作幂等缝，schedules.rs 的 v1_domain_operations）
// ---------------------------------------------------------------------------

pub(crate) fn domain_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
    let home = rt.store().home().to_path_buf();
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let db = Arc::new(DomainDb::open(&home, ALL_DOMAIN_DDL)?);
    registry.insert(home, Arc::downgrade(&db));
    Ok(db)
}

/// 已受理的异步操作（receipt 形态）。
pub(crate) struct AcceptedOperation {
    pub(crate) operation_id: String,
    pub(crate) receipt: Value,
}

/// 幂等受理（memory.rs `submit_memory_operation` 同纪律；acceptedRevision
/// 用本域 arev）。同 key 同输入重放 receipt，不同输入 IDEMPOTENCY_CONFLICT，
/// 终局拒绝重放其错误码；`validate` 只做受理前校验，不得留下无法回收的
/// 副作用（受理失败即零业务效果）。
pub(crate) fn accept_operation(
    rt: &Arc<PublicV1Runtime>,
    db: &Arc<DomainDb>,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    new_operation_id: impl FnOnce() -> String,
    validate: impl FnOnce() -> Result<(), ContractError>,
) -> Result<AcceptedOperation, ContractError> {
    match lookup_domain_operation(db, method_id, input, idempotency_key)? {
        DomainOperationLookup::Replay(receipt) => {
            return Ok(AcceptedOperation {
                operation_id: receipt["operationId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                receipt,
            });
        }
        DomainOperationLookup::Conflict => {
            return Err(ContractError::new(
                CODE_IDEMPOTENCY_CONFLICT,
                format!("idempotencyKey already used for {method_id} with different input"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        DomainOperationLookup::Rejected(code) => {
            return Err(ContractError::new(
                code,
                format!("{method_id} request previously rejected definitively"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        DomainOperationLookup::Fresh => {}
    }
    if let Err(error) = validate() {
        if error.retry.mode == RetryMode::Never {
            record_domain_operation_rejection(db, method_id, input, idempotency_key, &error.code)?;
        }
        return Err(error);
    }
    let operation_id = new_operation_id();
    let accepted_revision = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        current_automation_rev(&conn)?
    };
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": accepted_revision,
        "deduplicated": false,
        "state": "queued",
    });
    record_domain_operation_accept(
        db,
        method_id,
        input,
        idempotency_key,
        &operation_id,
        &receipt,
    )?;
    Ok(AcceptedOperation {
        operation_id,
        receipt,
    })
}

// ---------------------------------------------------------------------------
// 计划注册表（共享连接）
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct PlanRow {
    #[allow(dead_code)]
    pub(crate) project_id: String,
    pub(crate) plan_id: String,
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) state: String,
    pub(crate) revision: String,
    pub(crate) last_run_id: Option<String>,
}

fn plan_row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanRow> {
    Ok(PlanRow {
        project_id: row.get(0)?,
        plan_id: row.get(1)?,
        title: row.get(2)?,
        content: row.get(3)?,
        state: row.get(4)?,
        revision: row.get(5)?,
        last_run_id: row.get(6)?,
    })
}

const PLAN_COLUMNS: &str = "project_id, plan_id, title, content, state, revision, last_run_id";

pub(crate) fn plan_row(
    conn: &Connection,
    project_id: &str,
    plan_id: &str,
) -> Result<Option<PlanRow>, ContractError> {
    conn.query_row(
        &format!(
            "SELECT {PLAN_COLUMNS} FROM v1_automation_plans
             WHERE project_id = ?1 AND plan_id = ?2"
        ),
        params![project_id, plan_id],
        plan_row_of,
    )
    .optional()
    .map_err(sql_err)
}

pub(crate) fn plans_for_project(
    conn: &Connection,
    project_id: &str,
) -> Result<Vec<PlanRow>, ContractError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {PLAN_COLUMNS} FROM v1_automation_plans
             WHERE project_id = ?1 ORDER BY created_at, plan_id"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![project_id], plan_row_of)
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

/// 生产者缝隙：写入（或重录）一条计划，返回其固定 revision（重录内容不
/// 变则 revision 不变；新计划 → 新 revision）。不清理 lastRunId（发现
/// runtime 重录计划正文不视为新计划身份）。
pub(crate) fn upsert_plan(
    conn: &mut Connection,
    project_id: &str,
    plan_id: &str,
    title: &str,
    content: &str,
    state: &str,
) -> Result<String, ContractError> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT revision FROM v1_automation_plans WHERE project_id = ?1 AND plan_id = ?2",
            params![project_id, plan_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let revision = match existing {
        Some(revision) => revision,
        None => next_automation_rev(conn)?,
    };
    conn.execute(
        "INSERT INTO v1_automation_plans
            (project_id, plan_id, title, content, state, revision, last_run_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?7)
         ON CONFLICT(project_id, plan_id) DO UPDATE SET
            title = excluded.title,
            content = excluded.content,
            state = excluded.state,
            updated_at = excluded.updated_at",
        params![project_id, plan_id, title, content, state, revision, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(revision)
}

/// 计划执行后的登记（lastRunId 指向最近一次执行；state 反映就绪/执行中）。
pub(crate) fn set_plan_execution(
    conn: &Connection,
    project_id: &str,
    plan_id: &str,
    last_run_id: &str,
    state: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_automation_plans SET last_run_id = ?3, state = ?4, updated_at = ?5
         WHERE project_id = ?1 AND plan_id = ?2",
        params![project_id, plan_id, last_run_id, state, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// run 助手（四域共享）
// ---------------------------------------------------------------------------

/// run 是否仍在途（同连接直查 runs 表；锁纪律见模块 doc）。
pub(crate) fn run_is_live_conn(conn: &Connection, run_id: &str) -> bool {
    conn.query_row(
        "SELECT terminal FROM runs WHERE run_id = ?1",
        params![run_id],
        |r| r.get::<_, i64>(0),
    )
    .map(|terminal| terminal == 0)
    .unwrap_or(false)
}

/// 会话存在性（同连接直查）。
pub(crate) fn session_exists(conn: &Connection, session_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sessions WHERE session_id = ?1",
        params![session_id],
        |_| Ok(()),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// 确保 session 存在（不存在则建；存在则复用）。
pub(crate) fn ensure_session(
    store: &Arc<V1Store>,
    session_id: &str,
    project_id: &str,
    name: &str,
) -> Result<(), ContractError> {
    let exists = {
        let conn = store.conn.lock().expect("sqlite mutex");
        session_exists(&conn, session_id)
    };
    if exists {
        return Ok(());
    }
    let input = json!({"sessionId": session_id, "projectId": project_id, "name": name});
    match store.create_session(&input) {
        Ok(_) => Ok(()),
        // 并发路径：另一个 worker 先建了同一 session。
        Err(err) if err.code == "ALREADY_EXISTS" => Ok(()),
        Err(err) => Err(err),
    }
}

/// 把一段文本提交为当前 session 的 run（schedules.trigger 同款缝），
/// 返回 runId。
pub(crate) fn submit_text_run(
    rt: &Arc<PublicV1Runtime>,
    session_id: &str,
    text: &str,
    child_key: &str,
) -> Result<String, ContractError> {
    let input = json!({
        "sessionId": session_id,
        "input": [{"kind": "text", "text": text}],
    });
    match rt.store().submit_run(&input, Some(child_key))? {
        SubmitDisposition::Deduplicated { .. } => Err(domain_unavailable(
            "run submission deduplicated under a fresh child key",
        )),
        SubmitDisposition::NewRun {
            run_id,
            start_immediately,
            ..
        } => {
            if start_immediately {
                rt.start_session_worker(run_id.clone());
            }
            Ok(run_id)
        }
    }
}

/// 有界等待 run 终态（run.wait 同款 notify 循环；超时返回最后观察值）。
pub(crate) async fn wait_run_terminal(
    rt: &Arc<PublicV1Runtime>,
    run_id: &str,
    bound: Duration,
) -> Option<crate::public_v1::store::StoredRun> {
    let deadline = Instant::now() + bound;
    // notifier（Arc<Notify>）必须先绑定再 .notified()：临时 Arc 会在语句
    // 结束时释放，而 Notified 借用它会活到 select（run.rs run_wait 同款）。
    let notifier = rt.store().state_notifier();
    loop {
        let notified = notifier.notified();
        match rt.store().get_run(run_id) {
            Ok(run) if run.terminal => return Some(run),
            Ok(_) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return rt.store().get_run(run_id).ok();
        }
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

/// 取消一个 run 并在需要时唤醒 worker（锁外调用）。
pub(crate) fn cancel_run_and_signal(rt: &Arc<PublicV1Runtime>, run_id: &str) {
    if let Ok(outcome) = rt.store().cancel_run(run_id, None)
        && outcome.should_notify_worker
    {
        rt.signal_run_cancel(run_id);
    }
}

// ---------------------------------------------------------------------------
// automation 行 / 执行历史
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AutomationRow {
    #[allow(dead_code)]
    project_id: String,
    automation_id: String,
    plan_id: Option<String>,
    plan_revision: Option<String>,
    #[allow(dead_code)]
    state: String,
    generation: Option<String>,
    active_run_id: Option<String>,
    #[allow(dead_code)]
    session_id: String,
    last_result: Option<String>,
    #[allow(dead_code)]
    revision: String,
    #[allow(dead_code)]
    updated_at: String,
}

const AUTOMATION_COLUMNS: &str = "project_id, automation_id, plan_id, plan_revision, state, \
     generation, active_run_id, session_id, last_result, revision, updated_at";

fn automation_row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutomationRow> {
    Ok(AutomationRow {
        project_id: row.get(0)?,
        automation_id: row.get(1)?,
        plan_id: row.get(2)?,
        plan_revision: row.get(3)?,
        state: row.get(4)?,
        generation: row.get(5)?,
        active_run_id: row.get(6)?,
        session_id: row.get(7)?,
        last_result: row.get(8)?,
        revision: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn automation_row(
    conn: &Connection,
    project_id: &str,
    automation_id: &str,
) -> Result<Option<AutomationRow>, ContractError> {
    conn.query_row(
        &format!(
            "SELECT {AUTOMATION_COLUMNS} FROM v1_automations
             WHERE project_id = ?1 AND automation_id = ?2"
        ),
        params![project_id, automation_id],
        automation_row_of,
    )
    .optional()
    .map_err(sql_err)
}

fn latest_automation_for_project(
    conn: &Connection,
    project_id: &str,
) -> Result<Option<AutomationRow>, ContractError> {
    conn.query_row(
        &format!(
            "SELECT {AUTOMATION_COLUMNS} FROM v1_automations
             WHERE project_id = ?1 ORDER BY updated_at DESC, automation_id LIMIT 1"
        ),
        params![project_id],
        automation_row_of,
    )
    .optional()
    .map_err(sql_err)
}

/// automation 的 per-automation session id（项目内唯一、跨执行复用）。
fn automation_session_id(project_id: &str, automation_id: &str) -> String {
    format!("s-aw-{project_id}-{automation_id}")
}

#[derive(Clone)]
struct AutomationExecution {
    #[allow(dead_code)]
    execution_id: String,
    #[allow(dead_code)]
    automation_id: String,
    #[allow(dead_code)]
    run_id: Option<String>,
    #[allow(dead_code)]
    previous_run_id: Option<String>,
    #[allow(dead_code)]
    state: String,
}

fn automation_execution_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutomationExecution> {
    Ok(AutomationExecution {
        execution_id: row.get(0)?,
        automation_id: row.get(1)?,
        run_id: row.get(2)?,
        previous_run_id: row.get(3)?,
        state: row.get(4)?,
    })
}

fn automation_execution_for_run(
    conn: &Connection,
    automation_id: &str,
    run_id: &str,
) -> Result<Option<AutomationExecution>, ContractError> {
    conn.query_row(
        "SELECT execution_id, automation_id, run_id, previous_run_id, state
         FROM v1_automation_executions
         WHERE automation_id = ?1 AND run_id = ?2 ORDER BY created_at DESC LIMIT 1",
        params![automation_id, run_id],
        automation_execution_of,
    )
    .optional()
    .map_err(sql_err)
}

fn begin_automation_execution(
    conn: &Connection,
    execution_id: &str,
    project_id: &str,
    automation_id: &str,
    previous_run_id: Option<&str>,
) -> Result<(), ContractError> {
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO v1_automation_executions
            (execution_id, project_id, automation_id, run_id, previous_run_id, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, NULL, ?4, 'queued', ?5, ?5)",
        params![execution_id, project_id, automation_id, previous_run_id, now],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn finish_automation_execution(
    conn: &Connection,
    execution_id: &str,
    run_id: Option<&str>,
    state: &str,
    error_code: Option<&str>,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_automation_executions
         SET run_id = ?2, state = ?3, error_code = ?4, updated_at = ?5
         WHERE execution_id = ?1",
        params![execution_id, run_id, state, error_code, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// CAS：抢占 automation 的活跃 run 槽（空槽或指向非在途 run 的陈旧 pin 可
/// 回收；指向在途 run 则 0 行，防止双跑）。
#[cfg(test)]
fn claim_automation_run(
    conn: &Connection,
    project_id: &str,
    automation_id: &str,
    run_id: &str,
) -> Result<bool, ContractError> {
    let changed = conn
        .execute(
            "UPDATE v1_automations SET active_run_id = ?3, updated_at = ?4
             WHERE project_id = ?1 AND automation_id = ?2
               AND (active_run_id IS NULL
                    OR active_run_id = ''
                    OR NOT EXISTS (
                        SELECT 1 FROM runs
                        WHERE run_id = v1_automations.active_run_id AND terminal = 0
                    ))",
            params![project_id, automation_id, run_id, now_rfc3339()],
        )
        .map_err(sql_err)?;
    Ok(changed > 0)
}

/// 摘掉本执行 claim 到的 run 槽标记。scoped 守卫 `AND active_run_id = ?`
/// （store.rs run.cancel 清 sessions.active_run_id pin 的同款先例）：只摘
/// 自己的标记——并发输家、后到的 abort、终态后才收尾的执行，都不能抹掉
/// 别人的在途标记（t022 复审 P1：输家臂曾无条件清空，第三个受理即可
/// 击穿「单 automation 单在途 run」）。
fn clear_automation_run(
    conn: &Connection,
    project_id: &str,
    automation_id: &str,
    run_id: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_automations SET active_run_id = NULL, updated_at = ?4
         WHERE project_id = ?1 AND automation_id = ?2 AND active_run_id = ?3",
        params![project_id, automation_id, run_id, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 只回收「指向已非在途 run」的陈旧槽标记：受理校验只挡 live run、claim
/// 只认空槽，崩溃或被吞掉的 clear 残留下的终态标记若无人回收，后续每次
/// apply 都会受理后走输家臂失败。陈旧判据与 [`run_is_live_conn`] 同口径
/// （run 行缺失或 terminal=1）；在途赢家的标记绝不触碰。
fn clear_stale_automation_run(
    conn: &Connection,
    project_id: &str,
    automation_id: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_automations SET active_run_id = NULL, updated_at = ?3
         WHERE project_id = ?1 AND automation_id = ?2
           AND active_run_id IS NOT NULL AND active_run_id != ''
           AND NOT EXISTS (SELECT 1 FROM runs
                           WHERE run_id = v1_automations.active_run_id AND terminal = 0)",
        params![project_id, automation_id, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn set_automation_last_result(
    conn: &Connection,
    project_id: &str,
    automation_id: &str,
    last_result: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_automations SET last_result = ?3, updated_at = ?4
         WHERE project_id = ?1 AND automation_id = ?2",
        params![project_id, automation_id, last_result, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// automation.get
// ---------------------------------------------------------------------------

fn automation_status_json(conn: &Connection, row: &AutomationRow) -> Result<Value, ContractError> {
    // 绑定计划仍可用：存在且 revision 一致（计划被删除 → 不可用，不伪成功）。
    let (available, reason_code) = match (&row.plan_id, &row.plan_revision) {
        (Some(plan_id), Some(plan_revision)) => match plan_row(conn, &row.project_id, plan_id)? {
            Some(plan) if &plan.revision == plan_revision => (true, None),
            Some(_) => (false, Some("PLAN_STALE".to_owned())),
            None => (false, Some("PLAN_NOT_FOUND".to_owned())),
        },
        _ => (false, Some("PLAN_NOT_CONFIGURED".to_owned())),
    };
    let active_run_id = row
        .active_run_id
        .as_deref()
        .filter(|run_id| run_is_live_conn(conn, run_id))
        .map(ToOwned::to_owned);
    let state = if active_run_id.is_some() {
        "running"
    } else {
        "active"
    };
    Ok(json!({
        "automationId": row.automation_id,
        "status": {
            "state": state,
            "available": available,
            "reasonCode": reason_code,
            "generation": row.generation,
        },
        "planId": row.plan_id,
        "planRevision": row.plan_revision,
        "activeRunId": active_run_id,
        "lastResult": row.last_result,
    }))
}

async fn automation_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let automation_id = input
        .get("automationId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let result = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        match automation_id.as_deref() {
            Some(automation_id) => {
                let row = automation_row(&conn, &project_id, automation_id)?
                    .ok_or_else(|| domain_not_found("automation", automation_id))?;
                automation_status_json(&conn, &row)?
            }
            None => {
                // 项目级查询：无任何 automation = 能力未对该项目装配 →
                // UNAVAILABLE（能力验收：「未配置返回明确不可用而非空成功」）。
                let row = latest_automation_for_project(&conn, &project_id)?.ok_or_else(|| {
                    domain_unavailable(format!(
                        "project `{project_id}` has no automation configured; \
                             the capability is not assembled for it"
                    ))
                })?;
                automation_status_json(&conn, &row)?
            }
        }
    };
    Ok((ResponseOutcome::Succeeded, result))
}

// ---------------------------------------------------------------------------
// automation.apply / rerun_plan 共享受理与执行
// ---------------------------------------------------------------------------

/// 一次 automation 计划执行的全部参数（受理校验通过后快照）。
struct AutomationRunRequest {
    project_id: String,
    automation_id: String,
    plan: PlanRow,
    previous_run_id: Option<String>,
    session_id: String,
}

/// 受理校验 + 受理快照：计划存在且版本匹配、正文非空；已有 automation
/// 时无在途 run；rerun 的 previousRunId 归属本 automation。幂等重放在
/// `accept_operation` 内先行（重放不受计划后续变化影响）。
fn build_automation_request(
    rt: &Arc<PublicV1Runtime>,
    project_id: &str,
    automation_id: &str,
    plan_id: &str,
    expected_revision: &str,
    previous_run_id: Option<String>,
) -> Result<AutomationRunRequest, ContractError> {
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let plan =
        plan_row(&conn, project_id, plan_id)?.ok_or_else(|| domain_not_found("plan", plan_id))?;
    if expected_revision != plan.revision {
        return Err(revision_conflict(format!(
            "expectedRevision `{expected_revision}` does not match plan revision `{}`",
            plan.revision
        )));
    }
    if plan.content.trim().is_empty() {
        return Err(invalid_arg(format!("plan `{plan_id}` has an empty body")));
    }
    if let Some(row) = automation_row(&conn, project_id, automation_id)?
        && let Some(active) = &row.active_run_id
        && run_is_live_conn(&conn, active)
    {
        return Err(resource_busy(format!(
            "automation `{automation_id}` already has a live run; abort it before applying again"
        )));
    }
    if let Some(previous) = &previous_run_id
        && automation_execution_for_run(&conn, automation_id, previous)?.is_none()
    {
        return Err(domain_not_found(
            "automation run",
            &format!("{previous} (automation {automation_id})"),
        ));
    }
    Ok(AutomationRunRequest {
        session_id: automation_session_id(project_id, automation_id),
        project_id: project_id.to_owned(),
        automation_id: automation_id.to_owned(),
        plan,
        previous_run_id,
    })
}

fn automation_lock(
    home: &Path,
    project_id: &str,
    automation_id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let key = format!("{}|{}|{}", home.display(), project_id, automation_id);
    let mut reg = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reg.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        reg.insert(key, Arc::downgrade(&lock));
        lock
    })
}

/// 单事务抢占与登记：仅当无在途 active_run_id（空或陈旧非在途）时，原子
/// 写入 `v1_automations` 绑定与 `active_run_id`、执行记录及计划运行状态；
/// 并发输家不触碰 `v1_automations` 或 `v1_automation_plans`。
fn claim_and_arm_automation_run(
    conn: &mut Connection,
    request: &AutomationRunRequest,
    execution_id: &str,
    run_id: &str,
) -> Result<bool, ContractError> {
    let tx = conn.transaction().map_err(sql_err)?;
    let existing_active: Option<String> = tx
        .query_row(
            "SELECT active_run_id FROM v1_automations
             WHERE project_id = ?1 AND automation_id = ?2",
            params![request.project_id, request.automation_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .flatten();
    if let Some(active) = existing_active.as_deref()
        && !active.is_empty()
        && run_is_live_conn(&tx, active)
    {
        return Ok(false);
    }

    let rev = next_automation_rev_in_conn(&tx)?;
    let now = now_rfc3339();
    tx.execute(
        "INSERT INTO v1_automations
            (project_id, automation_id, plan_id, plan_revision, state, generation,
             active_run_id, session_id, last_result, revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, NULL, ?8, ?9, ?9)
         ON CONFLICT(project_id, automation_id) DO UPDATE SET
            plan_id = excluded.plan_id,
            plan_revision = excluded.plan_revision,
            state = 'active',
            generation = excluded.generation,
            active_run_id = excluded.active_run_id,
            session_id = excluded.session_id,
            revision = excluded.revision,
            updated_at = excluded.updated_at",
        params![
            request.project_id,
            request.automation_id,
            request.plan.plan_id,
            request.plan.revision,
            rev,
            run_id,
            request.session_id,
            rev,
            now,
        ],
    )
    .map_err(sql_err)?;
    begin_automation_execution(
        &tx,
        execution_id,
        &request.project_id,
        &request.automation_id,
        request.previous_run_id.as_deref(),
    )?;
    finish_automation_execution(&tx, execution_id, Some(run_id), "running", None)?;
    set_plan_execution(
        &tx,
        &request.project_id,
        &request.plan.plan_id,
        run_id,
        "running",
    )?;
    tx.commit().map_err(sql_err)?;
    Ok(true)
}

/// worker：per-automation 串行抢占槽位 → 提交 run → 单事务 CAS 登记 →
/// 有界等终态 → 落执行历史/lastResult → 完成操作。
async fn execute_automation_run(
    rt: Arc<PublicV1Runtime>,
    db: Arc<DomainDb>,
    operation_id: String,
    request: AutomationRunRequest,
) {
    let session_for_events = request.session_id.clone();
    let _ = mark_domain_operation_running(&db, &operation_id, &session_for_events);
    let execution_id = format!("ex-{}", rt.store().next_id());

    let lock = automation_lock(
        rt.store().home(),
        &request.project_id,
        &request.automation_id,
    );
    let _guard = lock.lock_owned().await;

    if ensure_session(
        rt.store(),
        &request.session_id,
        &request.project_id,
        &format!("automation {}", request.automation_id),
    )
    .is_err()
    {
        let completion = json!({
            "automationId": request.automation_id,
            "planId": request.plan.plan_id,
            "planRevision": request.plan.revision,
            "runId": Value::Null,
            "state": "failed",
            "artifactIds": [],
        });
        let _ = finish_domain_operation(
            &db,
            &operation_id,
            &session_for_events,
            "failed",
            &completion,
        );
        return;
    }

    // 在 submit_text_run 与触碰 v1_automations/v1_automation_plans 前先检查在途槽位。
    let live_owner = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        automation_row(&conn, &request.project_id, &request.automation_id)
            .ok()
            .flatten()
            .and_then(|row| row.active_run_id)
            .filter(|id| !id.is_empty() && run_is_live_conn(&conn, id))
    };
    if live_owner.is_some() {
        let completion = json!({
            "automationId": request.automation_id,
            "planId": request.plan.plan_id,
            "planRevision": request.plan.revision,
            "runId": Value::Null,
            "state": "failed",
            "artifactIds": [],
        });
        let _ = finish_domain_operation(
            &db,
            &operation_id,
            &session_for_events,
            "failed",
            &completion,
        );
        return;
    }

    let run_id = submit_text_run(
        &rt,
        &request.session_id,
        &request.plan.content,
        &format!("automation-exec-{execution_id}"),
    );
    let run_id = match run_id {
        Ok(run_id) => run_id,
        Err(_) => {
            {
                let conn = rt.store().conn.lock().expect("sqlite mutex");
                let _ =
                    clear_stale_automation_run(&conn, &request.project_id, &request.automation_id);
            }
            let completion = json!({
                "automationId": request.automation_id,
                "planId": request.plan.plan_id,
                "planRevision": request.plan.revision,
                "runId": Value::Null,
                "state": "failed",
                "artifactIds": [],
            });
            let _ = finish_domain_operation(
                &db,
                &operation_id,
                &session_for_events,
                "failed",
                &completion,
            );
            return;
        }
    };

    let claimed = {
        let mut conn = rt.store().conn.lock().expect("sqlite mutex");
        claim_and_arm_automation_run(&mut conn, &request, &execution_id, &run_id).unwrap_or(false)
    };
    if !claimed {
        // 竞争落败：撤销本执行刚提交的 run；绝不覆写赢家的 v1_automations 绑定或计划状态。
        cancel_run_and_signal(&rt, &run_id);
        let completion = json!({
            "automationId": request.automation_id,
            "planId": request.plan.plan_id,
            "planRevision": request.plan.revision,
            "runId": run_id,
            "state": "failed",
            "artifactIds": [],
        });
        let _ = finish_domain_operation(
            &db,
            &operation_id,
            &session_for_events,
            "failed",
            &completion,
        );
        return;
    }
    drop(_guard);
    let finished = wait_run_terminal(&rt, &run_id, RUN_WAIT_BOUND).await;
    let state = finished
        .as_ref()
        .map(|run| run_state_str(run.state).to_owned())
        .unwrap_or_else(|| "result_unknown".to_owned());
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let _ = finish_automation_execution(
            &conn,
            &execution_id,
            Some(&run_id),
            &state,
            finished
                .as_ref()
                .and_then(|run| run.outcome_code.as_deref()),
        );
        let _ =
            set_automation_last_result(&conn, &request.project_id, &request.automation_id, &state);
        // 只摘本执行 claim 到的标记（scoped，见 clear_automation_run）。
        let _ = clear_automation_run(&conn, &request.project_id, &request.automation_id, &run_id);
        let _ = set_plan_execution(
            &conn,
            &request.project_id,
            &request.plan.plan_id,
            &run_id,
            "ready",
        );
    }
    let mut completion = json!({
        "automationId": request.automation_id,
        "planId": request.plan.plan_id,
        "planRevision": request.plan.revision,
        "runId": run_id,
        "state": state,
        "artifactIds": [],
    });
    if let Some(previous) = &request.previous_run_id {
        completion["previousRunId"] = json!(previous);
    }
    let op_state = if state == "succeeded" {
        "succeeded"
    } else {
        "failed"
    };
    let _ = finish_domain_operation(
        &db,
        &operation_id,
        &session_for_events,
        op_state,
        &completion,
    );
}

/// automation.apply / rerun_plan 的公共受理：accept（校验闭包内重验计划
/// 版本/在途/归属）→ 受理后快照 → spawn worker。
async fn submit_automation_run(
    rt: &Arc<PublicV1Runtime>,
    method_id: &'static str,
    input: Value,
    key: Option<String>,
    previous_run_id: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = domain_db(rt)?;
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let automation_id = input["automationId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let expected_revision = input["expectedRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let operation = accept_operation(
        rt,
        &db,
        method_id,
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || {
            build_automation_request(
                rt,
                &project_id,
                &automation_id,
                &plan_id,
                &expected_revision,
                previous_run_id.clone(),
            )
            .map(|_| ())
        },
    )?;
    // 重放（deduplicated）到此为止：直接回受理 receipt，不再触达计划状态。
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    // 受理后快照（罕见竞态：计划在受理与快照间消失 → 操作显式失败收尾，
    // 受理记录不悬空）。
    let snapshot = match build_automation_request(
        rt,
        &project_id,
        &automation_id,
        &plan_id,
        &expected_revision,
        previous_run_id,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let session_for_events = automation_session_id(&project_id, &automation_id);
            let _ = finish_domain_operation(
                &db,
                &operation.operation_id,
                &session_for_events,
                "failed",
                &json!({
                    "automationId": automation_id,
                    "planId": plan_id,
                    "planRevision": expected_revision,
                    "runId": Value::Null,
                    "state": "failed",
                    "artifactIds": [],
                }),
            );
            return Err(error.into());
        }
    };
    let runner_rt = rt.clone();
    let runner_db = db.clone();
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        execute_automation_run(runner_rt, runner_db, operation_id, snapshot).await;
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

async fn automation_apply(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    submit_automation_run(&rt, "automation.apply", input, key, None).await
}

async fn automation_rerun_plan(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let previous_run_id = input["previousRunId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    submit_automation_run(
        &rt,
        "automation.rerun_plan",
        input,
        key,
        Some(previous_run_id),
    )
    .await
}

// ---------------------------------------------------------------------------
// automation.abort
// ---------------------------------------------------------------------------

/// `automation.abort` — 终止指定归属 run；已完成返回终态，不借 abort 改
/// 计划或其他会话。
async fn automation_abort(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = domain_db(&rt)?;
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let automation_id = input["automationId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();

    let operation = accept_operation(
        &rt,
        &db,
        "automation.abort",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let row = automation_row(&conn, &project_id, &automation_id)?
                .ok_or_else(|| domain_not_found("automation", &automation_id))?;
            if automation_execution_for_run(&conn, &row.automation_id, &run_id)?.is_none() {
                return Err(domain_not_found(
                    "automation run",
                    &format!("{run_id} (automation {automation_id})"),
                ));
            }
            Ok(())
        },
    )?;
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    let session_for_events = automation_session_id(&project_id, &automation_id);
    let runner_rt = rt.clone();
    let runner_db = db.clone();
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner_db, &operation_id, &session_for_events);
        let mut state = runner_rt
            .store()
            .get_run(&run_id)
            .map(|run| run_state_str(run.state).to_owned())
            .unwrap_or_else(|_| "not_found".to_owned());
        let live = {
            let conn = runner_rt.store().conn.lock().expect("sqlite mutex");
            run_is_live_conn(&conn, &run_id)
        };
        if live {
            cancel_run_and_signal(&runner_rt, &run_id);
            if let Some(finished) =
                wait_run_terminal(&runner_rt, &run_id, Duration::from_secs(30)).await
            {
                state = run_state_str(finished.state).to_owned();
            }
            let conn = runner_rt.store().conn.lock().expect("sqlite mutex");
            // scoped：只摘被中止 run 自己的标记（abort 旧 run 不得抹掉
            // 后来者已 claim 的在途标记）。
            let _ = clear_automation_run(&conn, &project_id, &automation_id, &run_id);
            let _ = set_automation_last_result(
                &conn,
                &project_id,
                &automation_id,
                &format!("aborted:{state}"),
            );
        }
        let completion = json!({
            "automationId": automation_id,
            "runId": run_id,
            "state": state,
        });
        let op_state = if state == "cancelled" || state == "succeeded" {
            "succeeded"
        } else {
            "failed"
        };
        let _ = finish_domain_operation(
            &runner_db,
            &operation_id,
            &session_for_events,
            op_state,
            &completion,
        );
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// 生产者缝隙（发现 runtime 的写入面；e2e 播种同入口）
// ---------------------------------------------------------------------------

/// 打开（或复用）一个 home 的 V1Store，仅供域表写入（生产者缝隙与 e2e
/// 播种；运行中的 gateway 持有另一连接，SQLite WAL 容忍并发短事务）。
/// t013 `declare_authorized_root` 同款先例。
pub(crate) fn open_store_for_seeding(home: &Path) -> Result<Arc<V1Store>, ContractError> {
    let (file, _token) = crate::public_v1::bootstrap::load_local_instance_and_token(home, None)?;
    let meta = crate::public_v1::store::InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-seed".to_owned(),
        server_revision: "srv-seed".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };
    V1Store::open_and_recover(home, meta)
}

// ---------------------------------------------------------------------------
// Tests（夹具供 discovery / work_cycles / agent_workflow 的测试模块复用）
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use std::path::Path;

    pub(crate) struct Fixture {
        pub(crate) _tmp: TempDir,
        pub(crate) rt: Arc<PublicV1Runtime>,
        pub(crate) home: PathBuf,
        pub(crate) instance_id: String,
    }

    pub(crate) async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        let instance_id = rt.store().meta().instance_id.clone();
        Fixture {
            _tmp: tmp,
            rt,
            home,
            instance_id,
        }
    }

    pub(crate) fn open_runtime(root_dir: &Path) -> Arc<PublicV1Runtime> {
        let (file, _token) =
            crate::public_v1::bootstrap::load_local_instance_and_token(root_dir, None).unwrap();
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

    /// 注册一个带真实授权根 + git 仓库工作区的项目（git 不可用时跳过 git
    /// 部分——只有工作区信号的断言依赖它）。
    pub(crate) async fn seed_real_project(fx: &Fixture, project_id: &str) -> PathBuf {
        let root_id = format!("root-{project_id}");
        let declared_root = fx._tmp.0.join(&root_id);
        let workspace = declared_root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        fx.rt
            .store()
            .declare_authorized_root(&root_id, &declared_root)
            .unwrap();
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": fx.instance_id,
                    "projectId": project_id,
                    "name": "Automation Fixture",
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": root_id, "relativePath": "ws"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        git_sync(&workspace, &["init", "-q", "-b", "main"]);
        git_sync(
            &workspace,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git_sync(&workspace, &["config", "user.name", "Fixture"]);
        std::fs::write(workspace.join("README.md"), "# fixture\n").unwrap();
        git_sync(&workspace, &["add", "-A"]);
        git_sync(&workspace, &["commit", "-q", "-m", "fixture init"]);
        workspace
    }

    /// 隔离 git 环境的系统 git（与 gateway 的 spawn 同款隔离）。
    pub(crate) fn git_sync(cwd: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("spawn git")
            .success();
        assert!(ok, "git {} failed in {}", args.join(" "), cwd.display());
    }

    pub(crate) fn git_present() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// 域表直写播种：一条探索/持续计划（生产者缝隙的同构入口）。
    pub(crate) fn seed_plan(
        fx: &Fixture,
        project_id: &str,
        plan_id: &str,
        content: &str,
    ) -> String {
        let mut conn = fx.rt.store().conn.lock().expect("sqlite mutex");
        upsert_plan(
            &mut conn,
            project_id,
            plan_id,
            "Fixture plan",
            content,
            "ready",
        )
        .unwrap()
    }

    pub(crate) async fn dispatch_ok(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: Option<&str>,
    ) -> Value {
        let (_, value) = rt
            .dispatch(method, input, key, None)
            .await
            .unwrap_or_else(|err| panic!("`{method}` failed: {err:?}"));
        value
    }

    pub(crate) async fn dispatch_err(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: Option<&str>,
    ) -> ContractError {
        match rt.dispatch(method, input, key, None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    /// 等待一个异步域操作终态并返回其完成记录（operations.get 的 result）。
    pub(crate) async fn wait_domain_operation(
        rt: &Arc<PublicV1Runtime>,
        operation_id: &str,
    ) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let operation = dispatch_ok(
                rt,
                "operations.get",
                json!({"operationId": operation_id}),
                None,
            )
            .await["operation"]
                .clone();
            let state = operation["state"].as_str().unwrap_or_default();
            if matches!(state, "succeeded" | "failed" | "cancelled") {
                return operation;
            }
            assert!(
                Instant::now() < deadline,
                "operation `{operation_id}` never became terminal"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    // -------------------------------------------------------------------
    // automation.* 合同正反例
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn automation_get_without_configuration_is_unavailable() {
        let fx = fixture("aw-get-empty").await;
        seed_real_project(&fx, "proj-1").await;
        let err = dispatch_err(
            &fx.rt,
            "automation.get",
            json!({"projectId": "proj-1"}),
            None,
        )
        .await;
        assert_eq!(err.code, "UNAVAILABLE", "未配置必须显式不可用：{err:?}");
        // 指定未知 automationId → NOT_FOUND。
        let err = dispatch_err(
            &fx.rt,
            "automation.get",
            json!({"projectId": "proj-1", "automationId": "missing"}),
            None,
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn automation_apply_binds_executes_and_get_reports() {
        let fx = fixture("aw-apply").await;
        seed_real_project(&fx, "proj-1").await;
        let revision = seed_plan(&fx, "proj-1", "plan-1", "hello automation");

        // 版本不匹配 / 未知计划在受理前拒绝。
        let err = dispatch_err(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": "wrong"}),
            Some("k-apply-bad"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");
        let err = dispatch_err(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "missing",
                   "expectedRevision": revision}),
            Some("k-apply-missing"),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");

        // 受理 → receipt（合同 receipt 形态）→ 完成记录。
        let receipt = dispatch_ok(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": revision}),
            Some("k-apply-1"),
        )
        .await;
        assert_eq!(receipt["kind"], "operation");
        assert_eq!(receipt["state"], "queued");
        assert_eq!(receipt["deduplicated"], false);
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_domain_operation(&fx.rt, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        assert_eq!(completion["automationId"], "aw-1");
        assert_eq!(completion["planId"], "plan-1");
        assert_eq!(completion["planRevision"], revision);
        let run_id = completion["runId"].as_str().unwrap().to_owned();
        assert!(!run_id.is_empty());
        assert_eq!(completion["state"], "succeeded");

        // automation.get 报告绑定与结果（计划、run 与 owner 关联真实）。
        let status = dispatch_ok(
            &fx.rt,
            "automation.get",
            json!({"projectId": "proj-1", "automationId": "aw-1"}),
            None,
        )
        .await;
        assert_eq!(status["automationId"], "aw-1");
        assert_eq!(status["status"]["available"], true);
        assert_eq!(status["status"]["reasonCode"], Value::Null);
        assert_eq!(status["planId"], "plan-1");
        assert_eq!(status["planRevision"], revision);
        assert_eq!(status["activeRunId"], Value::Null);
        assert_eq!(status["lastResult"], "succeeded");
        assert!(
            OperationRegistry::global()
                .validate_result("automation.get", &status)
                .is_ok(),
            "automation.get 结果过 schema"
        );

        // 同 key 同输入重放受理（不重复应用）。
        let replay = dispatch_ok(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": revision}),
            Some("k-apply-1"),
        )
        .await;
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"], operation_id);

        // 同 key 不同输入 → IDEMPOTENCY_CONFLICT。
        let err = dispatch_err(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": "other"}),
            Some("k-apply-1"),
        )
        .await;
        assert_eq!(err.code, "IDEMPOTENCY_CONFLICT");
    }

    #[tokio::test]
    async fn automation_rerun_links_previous_and_rejects_foreign_runs() {
        let fx = fixture("aw-rerun").await;
        seed_real_project(&fx, "proj-1").await;
        let revision = seed_plan(&fx, "proj-1", "plan-1", "rerun me");
        let receipt = dispatch_ok(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": revision}),
            Some("k-rerun-apply"),
        )
        .await;
        let first_run = wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap())
            .await["result"]["runId"]
            .as_str()
            .unwrap()
            .to_owned();

        // 外来 runId（不属于本 automation）→ NOT_FOUND。
        let err = dispatch_err(
            &fx.rt,
            "automation.rerun_plan",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "previousRunId": "r-foreign", "expectedRevision": revision}),
            Some("k-rerun-foreign"),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");

        // 显式重跑：新 run 且保留前后关联。
        let receipt = dispatch_ok(
            &fx.rt,
            "automation.rerun_plan",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "previousRunId": first_run, "expectedRevision": revision}),
            Some("k-rerun-1"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        let new_run = completion["runId"].as_str().unwrap().to_owned();
        assert_ne!(new_run, first_run, "重跑必须显式创建新 run");
        assert_eq!(completion["previousRunId"], first_run);
    }

    #[tokio::test]
    async fn automation_abort_cancels_live_run_and_reports_terminal() {
        let fx = fixture("aw-abort").await;
        seed_real_project(&fx, "proj-1").await;
        // 长 run（driver sleep）保证 abort 时仍在途。
        let revision = seed_plan(&fx, "proj-1", "plan-1", "hold [pd-driver:sleep=60000]");
        let receipt = dispatch_ok(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": revision}),
            Some("k-abort-apply"),
        )
        .await;
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        // 等 automation.get 报出活跃 run。apply 是异步受理：receipt 先回，
        // worker 随后才 arm 出 automation 行——行未建成前 get 如实报
        // NOT_FOUND，属正常窗口，继续轮询；其余错误仍然失败。
        let run_id = {
            let deadline = Instant::now() + Duration::from_secs(20);
            let get_input = || json!({"projectId": "proj-1", "automationId": "aw-1"});
            loop {
                let status = match fx
                    .rt
                    .dispatch("automation.get", get_input(), None, None)
                    .await
                {
                    Ok((_, status)) => status,
                    Err(DispatchError::Application(err)) if err.code == "NOT_FOUND" => {
                        assert!(
                            Instant::now() < deadline,
                            "automation row never became visible"
                        );
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        continue;
                    }
                    Err(other) => panic!("`automation.get` failed: {other:?}"),
                };
                if let Some(run) = status["activeRunId"].as_str() {
                    break run.to_owned();
                }
                assert!(Instant::now() < deadline, "automation run never started");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        // 在途的 apply 操作随 abort 失败收尾（run 被取消）。
        let abort_receipt = dispatch_ok(
            &fx.rt,
            "automation.abort",
            json!({"projectId": "proj-1", "automationId": "aw-1", "runId": run_id}),
            Some("k-abort-1"),
        )
        .await;
        let abort_op =
            wait_domain_operation(&fx.rt, abort_receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(abort_op["state"], "succeeded");
        assert_eq!(abort_op["result"]["state"], "cancelled");
        let apply_op = wait_domain_operation(&fx.rt, &operation_id).await;
        assert_eq!(apply_op["state"], "failed");

        // 已终态 run 的 abort：完成记录直接回报终态。
        let abort_receipt = dispatch_ok(
            &fx.rt,
            "automation.abort",
            json!({"projectId": "proj-1", "automationId": "aw-1", "runId": run_id}),
            Some("k-abort-2"),
        )
        .await;
        let abort_op =
            wait_domain_operation(&fx.rt, abort_receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(abort_op["result"]["state"], "cancelled");
        // abort 不改计划与其他会话：automation 绑定仍在。
        let status = dispatch_ok(
            &fx.rt,
            "automation.get",
            json!({"projectId": "proj-1", "automationId": "aw-1"}),
            None,
        )
        .await;
        assert_eq!(status["planId"], "plan-1");
    }

    // -------------------------------------------------------------------
    // t022 并发复审 P1-1 回归：run 槽标记的 scoped 清理
    // -------------------------------------------------------------------

    /// 播种一行 automation（测试直写域表；生产者缝隙的同构入口）。
    fn seed_automation_row(
        conn: &Connection,
        project_id: &str,
        automation_id: &str,
        active_run_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO v1_automations
                (project_id, automation_id, state, active_run_id, session_id, revision,
                 created_at, updated_at)
             VALUES (?1, ?2, 'active', ?3, ?4, 'arev-seed', ?5, ?5)",
            params![
                project_id,
                automation_id,
                active_run_id,
                automation_session_id(project_id, automation_id),
                now_rfc3339()
            ],
        )
        .unwrap();
    }

    fn active_run_id(conn: &Connection, project_id: &str, automation_id: &str) -> Option<String> {
        automation_row(conn, project_id, automation_id)
            .unwrap()
            .and_then(|row| row.active_run_id)
    }

    /// 提交一个排队中的 run（排队即 terminal=0，`run_is_live_conn` 视为在途）。
    fn submit_queued_run(fx: &Fixture, session: &str, key: &str) -> String {
        match fx
            .rt
            .store()
            .submit_run(
                &json!({"sessionId": session,
                        "input": [{"kind": "text", "text": "automation slot race fixture"}]}),
                Some(key),
            )
            .unwrap()
        {
            SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("expected a new run, got {other:?}"),
        }
    }

    /// 并发双受理只有一个赢家的标记存活（t022 复审 P1-1）。按 wp4fix 队列
    /// pin 回归的同款方式顺序构造交错：两个受理都通过校验（标记为空）→
    /// 赢家 claim 成功 → 输家 claim 落空并收尾 → 第三个受理的 claim 也
    /// 必须落空（单 run 不变量）。修复前输家臂无条件 clear，会在第 3 步
    /// 抹掉赢家的标记，第 4 步即双跑。
    #[tokio::test]
    async fn race_loser_never_wipes_the_live_winners_run_slot() {
        let fx = fixture("aw-slot-race").await;
        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": "proj-1",
                "name": "Slot Race",
                "root": {"instanceId": fx.instance_id, "rootId": "root-1",
                         "relativePath": "ws"},
            }))
            .unwrap();
        let session = automation_session_id("proj-1", "aw-1");
        ensure_session(fx.rt.store(), &session, "proj-1", "slot race").unwrap();
        let run_a = submit_queued_run(&fx, &session, "slot-race-a");
        let run_b = submit_queued_run(&fx, &session, "slot-race-b");
        let run_c = submit_queued_run(&fx, &session, "slot-race-c");

        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            seed_automation_row(&conn, "proj-1", "aw-1", None);
            // 1. 赢家 A 抢到槽位。
            assert!(claim_automation_run(&conn, "proj-1", "aw-1", &run_a).unwrap());
            // 2. 输家 B 的 claim 落空（CAS 只认空槽）。
            assert!(!claim_automation_run(&conn, "proj-1", "aw-1", &run_b).unwrap());
            // 3. 输家臂收尾：只回收陈旧标记，赢家的在途标记必须存活。
            clear_stale_automation_run(&conn, "proj-1", "aw-1").unwrap();
            assert_eq!(
                active_run_id(&conn, "proj-1", "aw-1").as_deref(),
                Some(run_a.as_str()),
                "输家收尾后，赢家的在途标记必须存活"
            );
            // 4. 第三个受理（K3）的 claim 也必须落空：单 run 不变量。
            assert!(!claim_automation_run(&conn, "proj-1", "aw-1", &run_c).unwrap());
            // 5. 外来 run_id 的 scoped clear 是 no-op（修复前的无守卫 clear
            //    抹掉的正是这里的赢家标记）。
            clear_automation_run(&conn, "proj-1", "aw-1", &run_c).unwrap();
            assert_eq!(
                active_run_id(&conn, "proj-1", "aw-1").as_deref(),
                Some(run_a.as_str())
            );
            // 6. 赢家自己的 scoped clear 才能摘掉标记。
            clear_automation_run(&conn, "proj-1", "aw-1", &run_a).unwrap();
            assert_eq!(active_run_id(&conn, "proj-1", "aw-1"), None);
        }

        // 崩溃自愈不回归：标记指向已终态 run（run_b 已被取消）时，陈旧
        // 回收必须能摘——否则下一次 apply 将永远走输家臂失败。
        fx.rt.store().cancel_run(&run_b, None).unwrap();
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            assert!(!run_is_live_conn(&conn, &run_b), "取消后 run_b 已终态");
            assert!(claim_automation_run(&conn, "proj-1", "aw-1", &run_b).unwrap());
            clear_stale_automation_run(&conn, "proj-1", "aw-1").unwrap();
            assert_eq!(active_run_id(&conn, "proj-1", "aw-1"), None);
        }
    }

    /// 陈旧 pin 自愈：崩溃残留的陈旧标记（指向已不在途的 run）在首次 apply
    /// 时即由 `claim_and_arm_automation_run` / `claim_automation_run` 自动回收，
    /// 不得先自杀式失败一次。
    #[tokio::test]
    async fn loser_arm_recycles_a_stale_run_slot_and_stays_usable() {
        let fx = fixture("aw-slot-stale").await;
        seed_real_project(&fx, "proj-1").await;
        let revision = seed_plan(&fx, "proj-1", "plan-1", "stale slot probe");
        // 播种崩溃残留：标记指向重启恢复后已不在途（这里：从未存在）的 run。
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            seed_automation_row(&conn, "proj-1", "aw-1", Some("r-crashed"));
        }

        let receipt = dispatch_ok(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": revision}),
            Some("k-slot-stale-1"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(
            operation["state"], "succeeded",
            "stale active_run_id pin must be recycled directly on first apply: {operation}"
        );
        assert_eq!(operation["result"]["state"], "succeeded");

        let status = dispatch_ok(
            &fx.rt,
            "automation.get",
            json!({"projectId": "proj-1", "automationId": "aw-1"}),
            None,
        )
        .await;
        assert_eq!(status["activeRunId"], Value::Null);
        let receipt = dispatch_ok(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-1",
                   "expectedRevision": revision}),
            Some("k-slot-stale-2"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        assert_eq!(operation["result"]["state"], "succeeded");
    }

    #[tokio::test]
    async fn concurrent_apply_with_different_plans_preserves_winner_plan_and_abort_replays() {
        let fx = fixture("aw-concurrent-plans").await;
        seed_real_project(&fx, "proj-1").await;
        let rev_a = seed_plan(&fx, "proj-1", "plan-a", "[pd-driver:sleep=600]");
        let rev_b = seed_plan(&fx, "proj-1", "plan-b", "[pd-driver:sleep=600]");

        let (rec_a, rec_b) = tokio::join!(
            dispatch_ok(
                &fx.rt,
                "automation.apply",
                json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-a",
                       "expectedRevision": rev_a}),
                Some("k-conc-a"),
            ),
            dispatch_ok(
                &fx.rt,
                "automation.apply",
                json!({"projectId": "proj-1", "automationId": "aw-1", "planId": "plan-b",
                       "expectedRevision": rev_b}),
                Some("k-conc-b"),
            ),
        );
        let (op_a, op_b) = tokio::join!(
            wait_domain_operation(&fx.rt, rec_a["operationId"].as_str().unwrap()),
            wait_domain_operation(&fx.rt, rec_b["operationId"].as_str().unwrap()),
        );
        let (winner_plan, loser_plan) = match (
            op_a["state"].as_str().unwrap_or_default(),
            op_b["state"].as_str().unwrap_or_default(),
        ) {
            ("succeeded", "failed") => ("plan-a", "plan-b"),
            ("failed", "succeeded") => ("plan-b", "plan-a"),
            other => panic!("expected exactly 1 winner and 1 loser, got {other:?}"),
        };
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            let row = automation_row(&conn, "proj-1", "aw-1").unwrap().unwrap();
            assert_eq!(
                row.plan_id.as_deref(),
                Some(winner_plan),
                "loser must never overwrite winner's planId on v1_automations"
            );
            let loser_row = plan_row(&conn, "proj-1", loser_plan).unwrap().unwrap();
            assert_ne!(
                loser_row.state, "running",
                "loser's plan must not stay stuck in 'running'"
            );
        }

        // automation.abort 幂等重放短路。
        let rev_abort = seed_plan(&fx, "proj-1", "plan-abort", "[pd-driver:sleep=5000]");
        let apply_rec = dispatch_ok(
            &fx.rt,
            "automation.apply",
            json!({"projectId": "proj-1", "automationId": "aw-abort", "planId": "plan-abort",
                   "expectedRevision": rev_abort}),
            Some("k-abort-apply"),
        )
        .await;
        let run_id = {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let st = match fx
                    .rt
                    .dispatch(
                        "automation.get",
                        json!({"projectId": "proj-1", "automationId": "aw-abort"}),
                        None,
                        None,
                    )
                    .await
                {
                    Ok((_, st)) => st,
                    Err(DispatchError::Application(err)) if err.code == "NOT_FOUND" => {
                        assert!(
                            Instant::now() < deadline,
                            "automation row never became visible"
                        );
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        continue;
                    }
                    Err(other) => panic!("`automation.get` failed: {other:?}"),
                };
                if let Some(id) = st["activeRunId"].as_str() {
                    break id.to_owned();
                }
                assert!(Instant::now() < deadline, "active run never started");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        let abort_req = json!({
            "projectId": "proj-1",
            "automationId": "aw-abort",
            "runId": run_id,
        });
        let ab1 = dispatch_ok(
            &fx.rt,
            "automation.abort",
            abort_req.clone(),
            Some("k-ab-1"),
        )
        .await;
        let ab1_op = ab1["operationId"].as_str().unwrap().to_owned();
        wait_domain_operation(&fx.rt, &ab1_op).await;
        wait_domain_operation(&fx.rt, apply_rec["operationId"].as_str().unwrap()).await;

        let ab2 = dispatch_ok(&fx.rt, "automation.abort", abort_req, Some("k-ab-1")).await;
        assert_eq!(ab2["deduplicated"], json!(true));
        assert_eq!(ab2["operationId"], json!(ab1_op));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
        let started_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE resource_id = ?1 AND event_type = 'operation.started'",
                params![ab1_op],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            started_events, 1,
            "abort replay must not spawn a second task"
        );
    }
}
