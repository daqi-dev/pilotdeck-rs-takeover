//! `discovery.*` — 项目探索计划与报告（cli.task.automation，S3 W6）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `discovery.*` 条目。TS
//! 基线（cc7c4e6）语义参考：`ui/server/discovery-plans.js` +
//! `src/always-on/web/{DiscoveryPlanService,DiscoveryPlanContext}.ts`
//! （上下文聚合、计划目录、报告读取）与旧 Rust 线
//! `runtime/always_on/{cycles,paths,report}.rs`（只读参考）。
//!
//! ## v1 薄实现的选择（差异在任务报告 §4 登记）
//!
//! * **计划存储**：v1 自管表 `v1_automation_plans`（automation.rs 所有，
//!   本域消费）。旧线/基线的 on-disk 计划目录（`always-on/projects/<id>/
//!   plans/index.json` + `<planId>.md`）没有合同概念的逐计划固定
//!   `revision`、`lastRunId` 与执行↔run↔报告产物关联，两头映射有损
//!   （memory.rs 对 EdgeClaw 的同款结论）；生产者缝隙
//!   [`record_discovery_plan`] 是发现 runtime 的写入面（未装配时表为空，
//!   读操作如实返回空）。
//! * **执行 = v1 run + 报告产物**：`discovery.execute` 的一次执行 =
//!   per-(project,plan) session 里的一个 run（S2 受理机；离线 driver 默认
//!   25ms 成功，`[pd-driver:*]` 指令可注入失败），成功后把报告 markdown 发
//!   布为不可变产物（t013 `artifacts` 域跨域 API）并登记
//!   `reportArtifactId`——「报告生成关联本次 run 和计划版本」。
//! * **在途栅栏**：同 (project, plan) 只允许一个未终态执行
//!   （RESOURCE_BUSY）。per-plan 异步锁串行「幂等查找 + 校验 + 受理 + 执行
//!   行落位」，行状态是受理后的第二道观察面。
//! * **上下文聚合**：`get_context` 采集基线 `collectWorkspaceSignals` 的
//!   工作区信号（git status --short 前 20 行、最近提交前 12 行，git 不
//!   可用时省略对应行）+ activePlanIds（state ∈ ready/running）。
//! * **虚拟/缺工作区**：未声明授权根 → VIRTUAL_PROJECT；工作区目录缺
//!   失 → WORKSPACE_UNAVAILABLE（get_context/execute；list_plans 错误表
//!   无该码，虚拟照报 VIRTUAL_PROJECT，目录缺失不阻塞目录读取）。
//! * **get_report**：执行记录与产物是权威（不触工作区）。无执行、或执行
//!   未曾提交 run（受理后基础设施失败）→ NOT_FOUND；未终态执行如实报
//!   running；失败执行报 failed（无报告不冒充成功）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{ContractError, EffectStatus, RetrySpec};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::artifacts::{publish_artifact, read_artifact_bytes};
use crate::public_v1::domains::automation::{
    self, CODE_VIRTUAL_PROJECT, CODE_WORKSPACE_UNAVAILABLE, PlanRow, accept_operation,
    current_automation_rev, domain_db, domain_not_found, ensure_session, parse_limit,
    parse_offset_cursor, plan_empty, plan_row, plans_for_project, resource_busy, revision_conflict,
    set_plan_execution, sql_err, submit_text_run, wait_run_terminal,
};
use crate::public_v1::domains::files::{ProjectResolveError, resolve_project};
use crate::public_v1::domains::schedules::{
    DomainDb, finish_domain_operation, mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{StoredRun, now_rfc3339, run_state_str};

pub(crate) const DDL_DISCOVERY_EXECUTIONS: &str =
    "CREATE TABLE IF NOT EXISTS v1_discovery_executions (
    execution_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    plan_revision TEXT NOT NULL,
    session_id TEXT,
    run_id TEXT,
    state TEXT NOT NULL,
    report_artifact_id TEXT,
    error_code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

/// 执行等待上界（automation 同款）。
const RUN_WAIT_BOUND: Duration = Duration::from_secs(120);

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_discovery_executions", DDL_DISCOVERY_EXECUTIONS);
    reg.handler("discovery.get_context", |rt, input, _key, _rev| {
        Box::pin(discovery_get_context(rt, input))
    });
    reg.handler("discovery.list_plans", |rt, input, _key, _rev| {
        Box::pin(discovery_list_plans(rt, input))
    });
    reg.handler("discovery.execute", |rt, input, key, _rev| {
        Box::pin(discovery_execute(rt, input, key))
    });
    reg.handler("discovery.get_report", |rt, input, _key, _rev| {
        Box::pin(discovery_get_report(rt, input))
    });
}

// ---------------------------------------------------------------------------
// 工作区解析（错误码按各操作错误表映射）
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

/// 解析项目的真实工作区：项目行缺失 → NOT_FOUND；无宿主声明授权根 →
/// VIRTUAL_PROJECT（虚拟 General 同类）；工作区目录缺失 →
/// WORKSPACE_UNAVAILABLE；其余解析错误原样透传。
pub(crate) fn resolve_real_workspace(
    rt: &Arc<PublicV1Runtime>,
    project_id: &str,
) -> Result<PathBuf, ContractError> {
    if !project_row_exists(rt, project_id)? {
        return Err(domain_not_found("project", project_id));
    }
    match resolve_project(rt.store(), project_id) {
        Ok(workspace) => Ok(workspace.workspace),
        Err(ProjectResolveError::Virtual(message)) => Err(ContractError::new(
            CODE_VIRTUAL_PROJECT,
            format!("virtual project has no real workspace: {message}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        Err(ProjectResolveError::Contract(err)) => Err(err),
        Err(ProjectResolveError::NotFound(message)) => Err(ContractError::new(
            CODE_WORKSPACE_UNAVAILABLE,
            format!("project workspace unavailable: {message}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
    }
}

/// 虚拟项目拒绝（list_plans：不触盘、只验「真实项目」身份）。
fn require_real_project(rt: &Arc<PublicV1Runtime>, project_id: &str) -> Result<(), ContractError> {
    if !project_row_exists(rt, project_id)? {
        return Err(domain_not_found("project", project_id));
    }
    match resolve_project(rt.store(), project_id) {
        Ok(_) => Ok(()),
        Err(ProjectResolveError::Virtual(message)) => Err(ContractError::new(
            CODE_VIRTUAL_PROJECT,
            format!("virtual project has no real workspace: {message}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        // 目录缺失不阻塞目录读取（错误表无 WORKSPACE_UNAVAILABLE）。
        Err(ProjectResolveError::NotFound(_)) => Ok(()),
        Err(ProjectResolveError::Contract(err)) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// 执行记录
// ---------------------------------------------------------------------------

struct ExecutionRow {
    execution_id: String,
    plan_id: String,
    #[allow(dead_code)]
    plan_revision: String,
    run_id: Option<String>,
    state: String,
    report_artifact_id: Option<String>,
    #[allow(dead_code)]
    error_code: Option<String>,
}

fn execution_row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExecutionRow> {
    Ok(ExecutionRow {
        execution_id: row.get(0)?,
        plan_id: row.get(1)?,
        plan_revision: row.get(2)?,
        run_id: row.get(3)?,
        state: row.get(4)?,
        report_artifact_id: row.get(5)?,
        error_code: row.get(6)?,
    })
}

const EXECUTION_COLUMNS: &str =
    "execution_id, plan_id, plan_revision, run_id, state, report_artifact_id, error_code";

fn begin_execution(
    conn: &Connection,
    execution_id: &str,
    project_id: &str,
    plan: &PlanRow,
    session_id: &str,
) -> Result<(), ContractError> {
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO v1_discovery_executions
            (execution_id, project_id, plan_id, plan_revision, session_id, run_id,
             state, report_artifact_id, error_code, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'queued', NULL, NULL, ?6, ?6)",
        params![
            execution_id,
            project_id,
            plan.plan_id,
            plan.revision,
            session_id,
            now
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 把执行行置为失败（行缺失时补一行失败记录：受理后基础设施失败也要可
/// 追溯；`finish_discovery_failure` 的幂等落点）。
fn fail_execution(
    conn: &Connection,
    execution_id: &str,
    project_id: &str,
    plan: &PlanRow,
    session_id: &str,
    run_id: Option<&str>,
    error_code: &str,
) {
    let now = now_rfc3339();
    let _ = conn.execute(
        "INSERT INTO v1_discovery_executions
            (execution_id, project_id, plan_id, plan_revision, session_id, run_id,
             state, report_artifact_id, error_code, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'failed', NULL, ?7, ?8, ?8)
         ON CONFLICT(execution_id) DO UPDATE SET
            run_id = excluded.run_id,
            state = 'failed',
            error_code = excluded.error_code,
            updated_at = excluded.updated_at",
        params![
            execution_id,
            project_id,
            plan.plan_id,
            plan.revision,
            session_id,
            run_id,
            error_code,
            now
        ],
    );
}

fn update_execution(
    conn: &Connection,
    execution_id: &str,
    run_id: Option<&str>,
    state: &str,
    error_code: Option<&str>,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_discovery_executions
         SET run_id = ?2, state = ?3, error_code = ?4, updated_at = ?5
         WHERE execution_id = ?1",
        params![execution_id, run_id, state, error_code, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn set_execution_report(
    conn: &Connection,
    execution_id: &str,
    report_artifact_id: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE v1_discovery_executions
         SET report_artifact_id = ?2, updated_at = ?3
         WHERE execution_id = ?1",
        params![execution_id, report_artifact_id, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn latest_execution(
    conn: &Connection,
    project_id: &str,
    plan_id: &str,
) -> Result<Option<ExecutionRow>, ContractError> {
    conn.query_row(
        &format!(
            "SELECT {EXECUTION_COLUMNS} FROM v1_discovery_executions
             WHERE project_id = ?1 AND plan_id = ?2
             ORDER BY created_at DESC, execution_id DESC LIMIT 1"
        ),
        params![project_id, plan_id],
        execution_row_of,
    )
    .optional()
    .map_err(sql_err)
}

fn execution_by_id(
    conn: &Connection,
    project_id: &str,
    plan_id: &str,
    execution_id: &str,
) -> Result<Option<ExecutionRow>, ContractError> {
    conn.query_row(
        &format!(
            "SELECT {EXECUTION_COLUMNS} FROM v1_discovery_executions
             WHERE project_id = ?1 AND plan_id = ?2 AND execution_id = ?3"
        ),
        params![project_id, plan_id, execution_id],
        execution_row_of,
    )
    .optional()
    .map_err(sql_err)
}

fn execution_in_flight(conn: &Connection, project_id: &str, plan_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM v1_discovery_executions
         WHERE project_id = ?1 AND plan_id = ?2 AND state IN ('queued', 'running') LIMIT 1",
        params![project_id, plan_id],
        |_| Ok(()),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// per-(project, plan) 受理串行（同 plan 并发 execute 的 RESOURCE_BUSY
/// 栅栏；git.rs 的 repo 锁同款）。
async fn lock_plan_execution(project_id: &str, plan_id: &str) -> OwnedMutexGuard<()> {
    #[allow(clippy::type_complexity)]
    static LOCKS: OnceLock<Mutex<HashMap<(String, String), Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let key = (project_id.to_owned(), plan_id.to_owned());
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mutex = {
        let mut map = locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(key).or_default().clone()
    };
    mutex.lock_owned().await
}

// ---------------------------------------------------------------------------
// discovery.get_context
// ---------------------------------------------------------------------------

/// 基线 `collectWorkspaceSignals`（DiscoveryPlanContext.ts:137-156）的 v1
/// 薄版：git status 前 20 行 + 最近提交前 12 行；git 不可用时省略对应行。
async fn collect_workspace_signals(workspace: &std::path::Path) -> Vec<String> {
    let mut signals = vec![format!("Project root: {}", workspace.display())];
    if let Ok(status) = super::git::git_run_str(workspace, &["status", "--short"]).await
        && !status.is_empty()
    {
        let text = String::from_utf8_lossy(&status).into_owned();
        let lines: Vec<&str> = text.lines().take(20).collect();
        signals.push(format!("Git status:\n{}", lines.join("\n")));
    }
    if let Ok(commit) = super::git::git_run_str(
        workspace,
        &["log", "-1", "--stat", "--oneline", "--decorate=no"],
    )
    .await
        && !commit.is_empty()
    {
        let text = String::from_utf8_lossy(&commit).into_owned();
        let lines: Vec<&str> = text.lines().take(12).collect();
        signals.push(format!("Latest commit:\n{}", lines.join("\n")));
    }
    signals
}

async fn discovery_get_context(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let workspace = resolve_real_workspace(&rt, &project_id)?;
    let signals = collect_workspace_signals(&workspace).await;
    let (revision, active_plan_ids) = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let revision = current_automation_rev(&conn)?;
        let active = plans_for_project(&conn, &project_id)?
            .into_iter()
            .filter(|plan| plan.state == "ready" || plan.state == "running")
            .map(|plan| plan.plan_id)
            .collect::<Vec<_>>();
        (revision, active)
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "projectId": project_id,
            "revision": revision,
            "workspaceAvailable": true,
            "context": signals.join("\n\n"),
            "activePlanIds": active_plan_ids,
        }),
    ))
}

// ---------------------------------------------------------------------------
// discovery.list_plans
// ---------------------------------------------------------------------------

async fn discovery_list_plans(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let plan_filter = input
        .get("planId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    require_real_project(&rt, &project_id)?;
    let cursor = parse_offset_cursor(input.get("cursor").and_then(Value::as_str), "dp")?;
    let limit = parse_limit(&input);

    let (mut plans, snapshot_revision) = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        (
            plans_for_project(&conn, &project_id)?,
            current_automation_rev(&conn)?,
        )
    };
    if let Some(plan_id) = &plan_filter {
        plans.retain(|plan| &plan.plan_id == plan_id);
    }
    let mut next_cursor = None;
    let mut items = Vec::new();
    for (index, plan) in plans.iter().enumerate().skip(cursor) {
        if items.len() >= limit {
            next_cursor = Some(format!("dp-{index}"));
            break;
        }
        items.push(json!({
            "planId": plan.plan_id,
            "revision": plan.revision,
            "title": plan.title,
            "content": plan.content,
            "state": plan.state,
            "lastRunId": plan.last_run_id,
        }));
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
// discovery.execute
// ---------------------------------------------------------------------------

/// 一次探索执行的全部参数（受理快照）。
struct DiscoveryRequest {
    project_id: String,
    session_id: String,
    plan: PlanRow,
}

/// 受理快照构建：计划存在 + 版本匹配（重放在 accept 内先行，不受计划后
/// 续变化影响）。
fn build_discovery_request(
    rt: &Arc<PublicV1Runtime>,
    project_id: &str,
    plan_id: &str,
    expected_revision: &str,
) -> Result<DiscoveryRequest, ContractError> {
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let plan =
        plan_row(&conn, project_id, plan_id)?.ok_or_else(|| domain_not_found("plan", plan_id))?;
    if expected_revision != plan.revision {
        return Err(revision_conflict(format!(
            "expectedRevision `{expected_revision}` does not match plan revision `{}`",
            plan.revision
        )));
    }
    Ok(DiscoveryRequest {
        session_id: format!("s-disc-{project_id}-{plan_id}"),
        project_id: project_id.to_owned(),
        plan,
    })
}

fn validate_discovery_execution(
    rt: &Arc<PublicV1Runtime>,
    request: &DiscoveryRequest,
) -> Result<(), ContractError> {
    resolve_real_workspace(rt, &request.project_id)?;
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let current = plan_row(&conn, &request.project_id, &request.plan.plan_id)?
        .ok_or_else(|| domain_not_found("plan", &request.plan.plan_id))?;
    if current.revision != request.plan.revision {
        return Err(revision_conflict(format!(
            "expectedRevision `{}` does not match plan revision `{}`",
            request.plan.revision, current.revision
        )));
    }
    if current.content.trim().is_empty() {
        return Err(plan_empty(&request.plan.plan_id));
    }
    if execution_in_flight(&conn, &request.project_id, &request.plan.plan_id) {
        return Err(resource_busy(format!(
            "plan `{}` already has an in-flight execution",
            request.plan.plan_id
        )));
    }
    Ok(())
}

/// worker 失败收尾：执行行置 failed + 操作行置 failed（完成结果仍带完整
/// 字段，错误码进 `error`）。
fn finish_discovery_failure(
    rt: &Arc<PublicV1Runtime>,
    db: &Arc<DomainDb>,
    operation_id: &str,
    execution_id: &str,
    request: &DiscoveryRequest,
    run_id: Option<&str>,
    error: &ContractError,
) {
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        fail_execution(
            &conn,
            execution_id,
            &request.project_id,
            &request.plan,
            &request.session_id,
            run_id,
            &error.code,
        );
    }
    let completion = json!({
        "projectId": request.project_id,
        "planId": request.plan.plan_id,
        "planRevision": request.plan.revision,
        "executionId": execution_id,
        // completionResult 的 runId 是非空 string：未及提交 run 的失败用
        // 空串占位（执行行保持 NULL，get_report 对其报 NOT_FOUND）。
        "runId": run_id.unwrap_or_default(),
        "reportArtifactId": Value::Null,
    });
    let _ = finish_domain_operation(db, operation_id, &request.session_id, "failed", &completion);
}

/// 受理后的 worker：提交 run → 等终态 → 报告产物 → 完成。
async fn execute_discovery_plan(
    rt: Arc<PublicV1Runtime>,
    db: Arc<DomainDb>,
    operation_id: String,
    execution_id: String,
    request: DiscoveryRequest,
) {
    let _ = mark_domain_operation_running(&db, &operation_id, &request.session_id);
    if let Err(error) = ensure_session(
        rt.store(),
        &request.session_id,
        &request.project_id,
        &format!("discovery {}", request.plan.plan_id),
    ) {
        finish_discovery_failure(
            &rt,
            &db,
            &operation_id,
            &execution_id,
            &request,
            None,
            &error,
        );
        return;
    }
    let run_id = match submit_text_run(
        &rt,
        &request.session_id,
        &request.plan.content,
        &format!("discovery-exec-{execution_id}"),
    ) {
        Ok(run_id) => run_id,
        Err(error) => {
            finish_discovery_failure(
                &rt,
                &db,
                &operation_id,
                &execution_id,
                &request,
                None,
                &error,
            );
            return;
        }
    };
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let _ = update_execution(&conn, &execution_id, Some(&run_id), "running", None);
        let _ = set_plan_execution(
            &conn,
            &request.project_id,
            &request.plan.plan_id,
            &run_id,
            "running",
        );
    }
    let finished = wait_run_terminal(&rt, &run_id, RUN_WAIT_BOUND).await;
    let state = finished
        .as_ref()
        .map(|run| run_state_str(run.state).to_owned())
        .unwrap_or_else(|| "result_unknown".to_owned());
    // 成功 → 报告产物（关联本次 run 与计划版本）；产物失败按失败收尾。
    let report_artifact_id = if state == "succeeded" {
        let report = build_report_markdown(&request, &execution_id, &run_id, finished.as_ref());
        match publish_artifact(
            rt.store(),
            &request.project_id,
            "text/markdown",
            Some(&format!("discovery-report-{execution_id}.md")),
            &report.into_bytes(),
            "discovery-report",
        ) {
            Ok(artifact) => {
                let artifact_id = artifact["artifactId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let conn = rt.store().conn.lock().expect("sqlite mutex");
                let _ = set_execution_report(&conn, &execution_id, &artifact_id);
                Some(artifact_id)
            }
            Err(error) => {
                finish_discovery_failure(
                    &rt,
                    &db,
                    &operation_id,
                    &execution_id,
                    &request,
                    Some(&run_id),
                    &error,
                );
                return;
            }
        }
    } else {
        None
    };
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let _ = update_execution(
            &conn,
            &execution_id,
            Some(&run_id),
            &state,
            finished
                .as_ref()
                .and_then(|run| run.outcome_code.as_deref()),
        );
        let _ = set_plan_execution(
            &conn,
            &request.project_id,
            &request.plan.plan_id,
            &run_id,
            "ready",
        );
    }
    let completion = json!({
        "projectId": request.project_id,
        "planId": request.plan.plan_id,
        "planRevision": request.plan.revision,
        "executionId": execution_id,
        "runId": run_id,
        "reportArtifactId": report_artifact_id,
    });
    let op_state = if state == "succeeded" {
        "succeeded"
    } else {
        "failed"
    };
    let _ = finish_domain_operation(
        &db,
        &operation_id,
        &request.session_id,
        op_state,
        &completion,
    );
}

/// 报告 markdown（薄格式：执行身份 + run 结果；内容由 get_report 原样返回）。
fn build_report_markdown(
    request: &DiscoveryRequest,
    execution_id: &str,
    run_id: &str,
    finished: Option<&StoredRun>,
) -> String {
    let result = finished
        .and_then(|run| run.result.as_ref())
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
        .unwrap_or_else(|| "null".to_owned());
    format!(
        "# Discovery report\n\n\
         - Plan: {} ({})\n\
         - Plan revision: {}\n\
         - Execution: {execution_id}\n\
         - Run: {run_id}\n\n\
         ## Run result\n\n```json\n{result}\n```\n",
        request.plan.title, request.plan.plan_id, request.plan.revision
    )
}

async fn discovery_execute(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = domain_db(&rt)?;
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let expected_revision = input["expectedRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    // 在途栅栏：同 plan 的「幂等查找 + 校验 + 受理 + 执行行落位」串行。
    let _guard = lock_plan_execution(&project_id, &plan_id).await;
    let operation = accept_operation(
        &rt,
        &db,
        "discovery.execute",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || {
            build_discovery_request(&rt, &project_id, &plan_id, &expected_revision)
                .and_then(|request| validate_discovery_execution(&rt, &request).map(|_| ()))
        },
    )?;
    // 重放（deduplicated）到此为止：直接回受理 receipt，不再触达计划状态。
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    // 受理后快照（罕见竞态：计划在受理与快照间消失 → 操作显式失败收尾）。
    let snapshot = match build_discovery_request(&rt, &project_id, &plan_id, &expected_revision) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = finish_domain_operation(
                &db,
                &operation.operation_id,
                "",
                "failed",
                &json!({
                    "projectId": project_id,
                    "planId": plan_id,
                    "planRevision": expected_revision,
                    "executionId": "",
                    "runId": "",
                    "reportArtifactId": Value::Null,
                }),
            );
            return Err(error.into());
        }
    };
    // 执行行同步落位（worker 侧任何失败路径都会终态化该行）。
    let execution_id = format!("ex-{}", rt.store().next_id());
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let _ = begin_execution(
            &conn,
            &execution_id,
            &snapshot.project_id,
            &snapshot.plan,
            &snapshot.session_id,
        );
    }
    drop(_guard);
    let runner_rt = rt.clone();
    let runner_db = db.clone();
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        execute_discovery_plan(runner_rt, runner_db, operation_id, execution_id, snapshot).await;
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// discovery.get_report
// ---------------------------------------------------------------------------

async fn discovery_get_report(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let execution_filter = input
        .get("executionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    let execution = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        if plan_row(&conn, &project_id, &plan_id)?.is_none() {
            return Err(domain_not_found("plan", &plan_id).into());
        }
        let execution = match execution_filter.as_deref() {
            Some(execution_id) => execution_by_id(&conn, &project_id, &plan_id, execution_id)?,
            None => latest_execution(&conn, &project_id, &plan_id)?,
        };
        execution.ok_or_else(|| {
            domain_not_found(
                "discovery execution",
                &format!("(plan {plan_id}; run discovery.execute first)"),
            )
        })?
    };
    // 未曾提交 run 的执行（受理后基础设施失败）没有可读报告：NOT_FOUND。
    let Some(run_id) = execution.run_id.clone() else {
        return Err(domain_not_found(
            "discovery execution report",
            &format!("(execution {} never started a run)", execution.execution_id),
        )
        .into());
    };
    // 未完成执行：区分 running（无完成报告，不冒充）。
    let (report_artifact_id, content) =
        if execution.state == "queued" || execution.state == "running" {
            (Value::Null, Value::Null)
        } else {
            match &execution.report_artifact_id {
                Some(artifact_id) => {
                    let bytes = read_artifact_bytes(rt.store(), artifact_id)?;
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    (json!(artifact_id), json!(text))
                }
                None => (Value::Null, Value::Null),
            }
        };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "planId": execution.plan_id,
            "executionId": execution.execution_id,
            "runId": run_id,
            "state": execution.state,
            "reportArtifactId": report_artifact_id,
            "content": content,
        }),
    ))
}

// ---------------------------------------------------------------------------
// 生产者缝隙（发现 runtime 的写入面；e2e 播种同入口）
// ---------------------------------------------------------------------------

/// 写入（或重录）一条探索计划，返回其固定 revision。发现 runtime 落计划
/// 的唯一入口；e2e 播种同款（t013 `declare_authorized_root` 先例）。
pub fn record_discovery_plan(
    home: &std::path::Path,
    project_id: &str,
    plan_id: &str,
    title: &str,
    content: &str,
) -> Result<String, ContractError> {
    let store = automation::open_store_for_seeding(home)?;
    let mut conn = store.conn.lock().expect("sqlite mutex");
    automation::upsert_plan(&mut conn, project_id, plan_id, title, content, "ready")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::domains::automation::tests::{
        Fixture, dispatch_err, dispatch_ok, fixture, git_present, seed_plan, wait_domain_operation,
    };
    use agentcpm_pd_protocol::registry::OperationRegistry;

    /// 等待一次执行完成（get_report 报终态）并返回其报告结果。
    async fn wait_report(rt: &Arc<PublicV1Runtime>, project_id: &str, plan_id: &str) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let report = dispatch_ok(
                rt,
                "discovery.get_report",
                json!({"projectId": project_id, "planId": plan_id}),
                None,
            )
            .await;
            let state = report["state"].as_str().unwrap_or_default();
            if state != "queued" && state != "running" {
                return report;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "execution of `{plan_id}` never became terminal"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn seed_virtual_project(fx: &Fixture, project_id: &str) {
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": fx.instance_id,
                    "projectId": project_id,
                    "name": "Virtual",
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": "undeclared", "relativePath": "ws"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn get_context_reports_workspace_signals_and_virtual_projects_rejected() {
        let fx = fixture("disc-ctx").await;
        let workspace =
            crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;
        // 基线只在 `git status --short` 输出非空时投影 `Git status:` 块
        // （DiscoveryPlanContext.ts `if (gitStatus)`）；干净仓库会省略该
        // 信号，所以留一个未提交改动让它出现。
        std::fs::write(workspace.join("scratch.txt"), "uncommitted\n").unwrap();
        let context = dispatch_ok(
            &fx.rt,
            "discovery.get_context",
            json!({"projectId": "proj-1"}),
            None,
        )
        .await;
        assert_eq!(context["projectId"], "proj-1");
        assert_eq!(context["workspaceAvailable"], true);
        assert!(context["revision"].as_str().is_some());
        let text = context["context"].as_str().expect("context text");
        assert!(text.contains("Project root:"), "signals present: {text}");
        if git_present() {
            assert!(text.contains("Git status:"), "git signals: {text}");
        }
        assert_eq!(context["activePlanIds"], json!([]));
        assert!(
            OperationRegistry::global()
                .validate_result("discovery.get_context", &context)
                .is_ok(),
            "get_context 结果过 schema"
        );
        assert!(workspace.exists());

        // 虚拟项目（无宿主声明根）→ VIRTUAL_PROJECT；未知项目 → NOT_FOUND。
        seed_virtual_project(&fx, "proj-virtual").await;
        let err = dispatch_err(
            &fx.rt,
            "discovery.get_context",
            json!({"projectId": "proj-virtual"}),
            None,
        )
        .await;
        assert_eq!(err.code, "VIRTUAL_PROJECT");
        let err = dispatch_err(
            &fx.rt,
            "discovery.get_context",
            json!({"projectId": "missing"}),
            None,
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn list_plans_pages_content_and_keeps_virtual_projects_out() {
        let fx = fixture("disc-list").await;
        crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;
        let rev_a = seed_plan(&fx, "proj-1", "plan-a", "alpha body");
        let rev_b = seed_plan(&fx, "proj-1", "plan-b", "beta body");

        let page = dispatch_ok(
            &fx.rt,
            "discovery.list_plans",
            json!({"projectId": "proj-1", "limit": 1}),
            None,
        )
        .await;
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert!(page["nextCursor"].as_str().is_some(), "分页游标");
        assert!(page["snapshotRevision"].as_str().is_some());
        // 选定 planId：返回正文与固定 revision。
        let selected = dispatch_ok(
            &fx.rt,
            "discovery.list_plans",
            json!({"projectId": "proj-1", "planId": "plan-b"}),
            None,
        )
        .await;
        let items = selected["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["planId"], "plan-b");
        assert_eq!(items[0]["content"], "beta body");
        assert_eq!(items[0]["revision"], rev_b);
        assert_eq!(items[0]["lastRunId"], Value::Null);
        assert!(
            OperationRegistry::global()
                .validate_result("discovery.list_plans", &selected)
                .is_ok(),
            "list_plans 结果过 schema"
        );
        // 重录同内容计划：revision 固定不变。
        let rev_a2 = seed_plan(&fx, "proj-1", "plan-a", "alpha body");
        assert_eq!(rev_a, rev_a2, "同内容重录保持固定 revision");

        // 虚拟项目拒绝；空项目目录为空表。
        seed_virtual_project(&fx, "proj-virtual").await;
        let err = dispatch_err(
            &fx.rt,
            "discovery.list_plans",
            json!({"projectId": "proj-virtual"}),
            None,
        )
        .await;
        assert_eq!(err.code, "VIRTUAL_PROJECT");
        crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-empty").await;
        let empty = dispatch_ok(
            &fx.rt,
            "discovery.list_plans",
            json!({"projectId": "proj-empty"}),
            None,
        )
        .await;
        assert_eq!(empty["items"], json!([]));
        assert_eq!(empty["nextCursor"], Value::Null);
    }

    #[tokio::test]
    async fn execute_runs_plan_and_report_links_run_and_plan_version() {
        let fx = fixture("disc-exec").await;
        crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;
        let revision = seed_plan(&fx, "proj-1", "plan-1", "explore the workspace");

        // 版本不匹配 / 未知计划 / 虚拟项目在受理前拒绝。
        let err = dispatch_err(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "plan-1", "expectedRevision": "wrong"}),
            Some("k-disc-bad"),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");
        let err = dispatch_err(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "missing", "expectedRevision": "r"}),
            Some("k-disc-missing"),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");

        let receipt = dispatch_ok(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "plan-1", "expectedRevision": revision}),
            Some("k-disc-1"),
        )
        .await;
        assert_eq!(receipt["kind"], "operation");
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        assert_eq!(completion["projectId"], "proj-1");
        assert_eq!(completion["planId"], "plan-1");
        assert_eq!(completion["planRevision"], revision);
        let run_id = completion["runId"].as_str().unwrap().to_owned();
        assert!(!run_id.is_empty());
        let execution_id = completion["executionId"].as_str().unwrap().to_owned();
        assert!(!execution_id.is_empty());
        let artifact_id = completion["reportArtifactId"]
            .as_str()
            .expect("report artifact");

        // 报告关联本次 run 与计划版本；get_report 返回内容与产物。
        let report = wait_report(&fx.rt, "proj-1", "plan-1").await;
        assert_eq!(report["executionId"], execution_id);
        assert_eq!(report["runId"], run_id);
        assert_eq!(report["state"], "succeeded");
        assert_eq!(report["reportArtifactId"], artifact_id);
        let content = report["content"].as_str().expect("report content");
        assert!(content.contains(&run_id), "报告关联本次 run：{content}");
        assert!(content.contains(&revision));
        assert!(
            OperationRegistry::global()
                .validate_result("discovery.get_report", &report)
                .is_ok(),
            "get_report 结果过 schema"
        );
        // 指定 executionId 的读取与最新一致。
        let by_id = dispatch_ok(
            &fx.rt,
            "discovery.get_report",
            json!({"projectId": "proj-1", "planId": "plan-1", "executionId": execution_id}),
            None,
        )
        .await;
        assert_eq!(by_id["executionId"], execution_id);
        // 未知 executionId → NOT_FOUND。
        let err = dispatch_err(
            &fx.rt,
            "discovery.get_report",
            json!({"projectId": "proj-1", "planId": "plan-1", "executionId": "ex-missing"}),
            None,
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");

        // 计划行记录 lastRunId（关联真实）。
        let selected = dispatch_ok(
            &fx.rt,
            "discovery.list_plans",
            json!({"projectId": "proj-1", "planId": "plan-1"}),
            None,
        )
        .await;
        assert_eq!(selected["items"][0]["lastRunId"], run_id);

        // 同 key 重放受理（不重复执行）。
        let replay = dispatch_ok(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "plan-1", "expectedRevision": revision}),
            Some("k-disc-1"),
        )
        .await;
        assert_eq!(replay["deduplicated"], true);
    }

    #[tokio::test]
    async fn execute_rejects_in_flight_duplicate_and_reports_failure_without_report() {
        let fx = fixture("disc-busy").await;
        crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;
        let revision = seed_plan(&fx, "proj-1", "plan-hold", "hold [pd-driver:sleep=60000]");
        let _receipt = dispatch_ok(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "plan-hold", "expectedRevision": revision}),
            Some("k-hold-1"),
        )
        .await;
        // 等报告进入 running，再验证重复在途执行拒绝。受理与 run 提交
        // 之间的窗口里 get_report 如实 NOT_FOUND（「run 未开始」，不冒充
        // 在途状态），所以轮询到 state 进入 queued/running。
        let report = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let polled = fx
                    .rt
                    .dispatch(
                        "discovery.get_report",
                        json!({"projectId": "proj-1", "planId": "plan-hold"}),
                        None,
                        None,
                    )
                    .await;
                if let Ok((_, report)) = polled
                    && matches!(report["state"].as_str(), Some("running") | Some("queued"))
                {
                    break report;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "execution of `plan-hold` never entered running"
                );
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        };
        assert!(
            report["state"] == json!("running") || report["state"] == json!("queued"),
            "在途执行的报告如实报运行中：{report}"
        );
        assert_eq!(report["content"], Value::Null);
        assert_eq!(report["reportArtifactId"], Value::Null);
        let err = dispatch_err(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "plan-hold", "expectedRevision": revision}),
            Some("k-hold-2"),
        )
        .await;
        assert_eq!(err.code, "RESOURCE_BUSY");

        // 空正文计划显式不可执行。
        let empty_rev = seed_plan(&fx, "proj-1", "plan-empty", "");
        let err = dispatch_err(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "plan-empty", "expectedRevision": empty_rev}),
            Some("k-empty"),
        )
        .await;
        assert_eq!(err.code, "PLAN_EMPTY");

        // 虚拟项目拒绝。
        seed_virtual_project(&fx, "proj-virtual").await;
        let vrev = {
            let mut conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            crate::public_v1::domains::automation::upsert_plan(
                &mut conn,
                "proj-virtual",
                "plan-v",
                "v",
                "v",
                "ready",
            )
            .unwrap()
        };
        let err = dispatch_err(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-virtual", "planId": "plan-v", "expectedRevision": vrev}),
            Some("k-virtual"),
        )
        .await;
        assert_eq!(err.code, "VIRTUAL_PROJECT");

        // 取消在途 run，让 fixture 收尾（drop 不阻塞）。
        let run_id = report["runId"].as_str().unwrap().to_owned();
        crate::public_v1::domains::automation::cancel_run_and_signal(&fx.rt, &run_id);
    }

    #[tokio::test]
    async fn failed_run_yields_failed_execution_without_report() {
        let fx = fixture("disc-fail").await;
        crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;
        let revision = seed_plan(&fx, "proj-1", "plan-fail", "boom [pd-driver:fail]");
        let receipt = dispatch_ok(
            &fx.rt,
            "discovery.execute",
            json!({"projectId": "proj-1", "planId": "plan-fail", "expectedRevision": revision}),
            Some("k-fail-1"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "failed");
        let report = wait_report(&fx.rt, "proj-1", "plan-fail").await;
        assert_eq!(report["state"], "failed");
        assert_eq!(report["reportArtifactId"], Value::Null);
        assert_eq!(report["content"], Value::Null);
        // 无执行的计划 → NOT_FOUND（不冒充）。
        let err = dispatch_err(
            &fx.rt,
            "discovery.get_report",
            json!({"projectId": "proj-1", "planId": "plan-none"}),
            None,
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");
    }
}
