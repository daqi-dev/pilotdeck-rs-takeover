//! `agent_workflow.*` — 外部请求到项目与 Git 产物的组合流程
//! （cli.task.automation，S3 W6）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `agent_workflow.*` 条目。TS
//! 基线（cc7c4e6）语义参考：`ui/server/routes/agent.js` 的 POST /api/agent
//! （克隆/登记/运行 + 可选分支/提交/推送/PR）。旧 Rust 线无对应（按合同）。
//!
//! ## v1 薄实现的选择（差异在任务报告 §4 登记）
//!
//! * **组合验收锚**：`agent_workflow.submit` 的执行 = 逐 step `rt.dispatch`
//!   既有公共操作，不绕过：`workspaces.prepare`（clone，目的 = t012 default
//!   根下 `agent-workflows/<workflowId>`，基线 `~/.pilotdeck/external-
//!   projects` 的产品自有目录对应物）→ `session.create` + `run.submit`（等
//!   run 终态）→ `git.create_branch` / `git.commit` / `git.push`（每步先经
//!   公开 `git.status` 观察 stateToken）。子操作经 `operations.get` 等待终
//!   态；父操作完成结果包含子 operationId/runId，无第二套 run owner。
//! * **两段式（应用与发布显式）**：`plan` 只登记预览（TTL 10 分钟，git
//!   restore 计划同款）；`submit` 复验 planHash（不一致/过期 → PLAN_STALE）
//!   并要求发布效果（`push`）出现在 `authorizedEffects`，否则
//!   EFFECT_APPROVAL_REQUIRED——「未明确授权不创建PR或推送」。
//! * **pullRequest 工件**：v1 的 285 个公开操作中没有 PR 创建操作（git
//!   域只有 push/publish=推送并设上游）；plan 对 `pullRequest` 明确
//!   UNAVAILABLE（预览不留一个注定失败的步骤），PR 上游通道由集成域后
//!   续装配（UPSTREAM_ERROR 保留给真实上游失败）。
//! * **默认无 bypassPermissions**：run 步骤经 S2 受理机提交（权限模型与
//!   run.submit 一致），不存在基线 agent.js 的 bypass 路由。
//! * **清理面**：cleanup 只移除本 workflow 登记的临时克隆目录（绝对路径
//!   记录在案），运行中的资源保留并给 reasonCode（`retainedReasons`），
//!   既有项目永不默删；已发布远端引用不属于清理范围（publishedReferences
//!   只报告不移除）。
//! * **中断语义**：worker 中途失败保留已完成步骤状态（步骤行 + 事件），
//!   不清理已克隆项目（「中途失败不误清理既有项目」），恢复经同一
//!   idempotencyKey 重放受理（不重复克隆/提交/推送）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{ContractError, EffectStatus, RetrySpec};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::automation::{
    accept_operation, domain_db, domain_not_found, domain_unavailable, invalid_arg, sql_err,
    wait_run_terminal,
};
use crate::public_v1::domains::schedules::{
    DomainDb, finish_domain_operation, mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339, run_state_str};

pub(crate) const DDL_WORKFLOW_PLANS: &str = "CREATE TABLE IF NOT EXISTS v1_agent_workflow_plans (
    plan_id TEXT PRIMARY KEY,
    plan_hash TEXT NOT NULL,
    project_id TEXT,
    repository_url TEXT,
    base_ref TEXT,
    prompt TEXT NOT NULL,
    model_id TEXT,
    artifacts_json TEXT NOT NULL,
    credential_ref TEXT,
    steps_json TEXT NOT NULL,
    temporary_resources_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);";

pub(crate) const DDL_WORKFLOWS: &str = "CREATE TABLE IF NOT EXISTS v1_agent_workflows (
    workflow_id TEXT PRIMARY KEY,
    plan_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    session_id TEXT,
    run_id TEXT,
    state TEXT NOT NULL,
    steps_json TEXT NOT NULL,
    resources_json TEXT NOT NULL,
    published_json TEXT NOT NULL,
    artifacts_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

/// 两段式 plan 的过期窗口（git restore 计划同款）。
const PLAN_TTL_SECS: i64 = 600;
/// 子操作（clone）等待上界。
const CHILD_OPERATION_BOUND: Duration = Duration::from_secs(180);
/// run 步骤等待上界。
const RUN_WAIT_BOUND: Duration = Duration::from_secs(120);
/// git 子操作确认等待上界（run_mutation 在 dispatch 内同步完成，这里只做
/// 终态确认与完成结果采集）。
const GIT_STEP_BOUND: Duration = Duration::from_secs(30);

/// 合同错误表中本域专属（protocol crate 尚无公共常量）的错误码。
const CODE_PLAN_STALE: &str = "PLAN_STALE";
const CODE_EFFECT_APPROVAL_REQUIRED: &str = "EFFECT_APPROVAL_REQUIRED";
const CODE_RESOURCE_NOT_OWNED: &str = "RESOURCE_NOT_OWNED";
const CODE_UPSTREAM_ERROR: &str = "UPSTREAM_ERROR";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_agent_workflow_plans", DDL_WORKFLOW_PLANS);
    reg.schema("v1_agent_workflows", DDL_WORKFLOWS);
    reg.handler("agent_workflow.plan", |rt, input, _key, _rev| {
        Box::pin(agent_workflow_plan(rt, input))
    });
    reg.handler("agent_workflow.submit", |rt, input, key, _rev| {
        Box::pin(agent_workflow_submit(rt, input, key))
    });
    reg.handler("agent_workflow.cleanup", |rt, input, key, _rev| {
        Box::pin(agent_workflow_cleanup(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 错误折返（DispatchError → ContractError，子步失败统一记步骤错误）
// ---------------------------------------------------------------------------

fn dispatch_error_to_contract(error: DispatchError) -> ContractError {
    match error {
        DispatchError::Application(err) => err,
        DispatchError::Rpc {
            code,
            message,
            data,
        } => {
            // dispatch 公共前置（缺 idempotencyKey 等）的 Rpc 错误内嵌合同错
            // 误结构；无结构时按 UNAVAILABLE 折返。
            if let Some(data) = data
                && let Ok(err) = serde_json::from_value::<ContractError>(data)
            {
                err
            } else {
                ContractError::new(
                    "UNAVAILABLE",
                    format!("rpc {code}: {message}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            }
        }
    }
}

/// dispatch 一个子公共操作；失败映射为步骤错误。
async fn dispatch_child(
    rt: &Arc<PublicV1Runtime>,
    method: &str,
    input: Value,
    key: Option<&str>,
) -> Result<(ResponseOutcome, Value), ContractError> {
    rt.dispatch(method, input, key, None)
        .await
        .map_err(dispatch_error_to_contract)
}

/// 等待一个子操作终态（operations.get 轮询），返回其 operation JSON。
async fn wait_child_operation(
    rt: &Arc<PublicV1Runtime>,
    operation_id: &str,
    bound: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + bound;
    loop {
        if let Ok((_, value)) = rt
            .dispatch(
                "operations.get",
                json!({"operationId": operation_id}),
                None,
                None,
            )
            .await
            && let Some(operation) = value.get("operation")
        {
            let state = operation["state"].as_str().unwrap_or_default();
            if matches!(state, "succeeded" | "failed" | "cancelled") {
                return Some(operation.clone());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 子操作终态确认（失败/超时 → UPSTREAM_ERROR / UNAVAILABLE）。
async fn confirm_child_operation(
    rt: &Arc<PublicV1Runtime>,
    operation_id: &str,
    bound: Duration,
    step: &str,
) -> Result<Value, ContractError> {
    match wait_child_operation(rt, operation_id, bound).await {
        Some(operation) if operation["state"] == json!("succeeded") => Ok(operation),
        Some(operation) => Err(ContractError::new(
            CODE_UPSTREAM_ERROR,
            format!(
                "step `{step}` failed: {}",
                operation["error"]["message"]
                    .as_str()
                    .unwrap_or("child operation failed")
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        None => Err(domain_unavailable(format!(
            "step `{step}` operation never reached a terminal state"
        ))),
    }
}

// ---------------------------------------------------------------------------
// 计划登记
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WorkflowPlan {
    plan_id: String,
    plan_hash: String,
    project_id: Option<String>,
    repository_url: Option<String>,
    base_ref: Option<String>,
    prompt: String,
    #[allow(dead_code)]
    model_id: Option<String>,
    artifacts: Vec<String>,
    credential_ref: Option<String>,
    steps: Vec<Value>,
    temporary_resources: Vec<String>,
    expires_at: String,
}

fn plan_row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowPlan> {
    let artifacts: Vec<String> =
        serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default();
    let steps: Vec<Value> = serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default();
    let temporary_resources: Vec<String> =
        serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default();
    Ok(WorkflowPlan {
        plan_id: row.get(0)?,
        plan_hash: row.get(1)?,
        project_id: row.get(2)?,
        repository_url: row.get(3)?,
        base_ref: row.get(4)?,
        prompt: row.get(5)?,
        model_id: row.get(6)?,
        artifacts,
        credential_ref: row.get(8)?,
        steps,
        temporary_resources,
        expires_at: row.get(12)?,
    })
}

const PLAN_COLUMNS: &str = "plan_id, plan_hash, project_id, repository_url, base_ref, prompt, \
     model_id, artifacts_json, credential_ref, steps_json, temporary_resources_json, created_at, \
     expires_at";

fn load_plan(conn: &Connection, plan_id: &str) -> Result<Option<WorkflowPlan>, ContractError> {
    conn.query_row(
        &format!("SELECT {PLAN_COLUMNS} FROM v1_agent_workflow_plans WHERE plan_id = ?1"),
        params![plan_id],
        plan_row_of,
    )
    .optional()
    .map_err(sql_err)
}

fn rfc3339_plus(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(seconds)).to_rfc3339()
}

/// 计划身份哈希：对全部受理语义输入（排序后的 artifacts）与步骤表签名。
fn plan_hash_of(
    project_id: Option<&str>,
    repository_url: Option<&str>,
    base_ref: Option<&str>,
    prompt: &str,
    model_id: Option<&str>,
    artifacts: &[String],
    credential_ref: Option<&str>,
    steps: &[Value],
) -> String {
    let mut artifacts = artifacts.to_vec();
    artifacts.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"agent-workflow-plan:");
    for part in [
        project_id.unwrap_or_default(),
        repository_url.unwrap_or_default(),
        base_ref.unwrap_or_default(),
        prompt,
        model_id.unwrap_or_default(),
        credential_ref.unwrap_or_default(),
    ] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.update(
        serde_json::to_string(&artifacts)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(serde_json::to_string(steps).unwrap_or_default().as_bytes());
    format!("awhash-{:x}", hasher.finalize())
}

/// 组合计划的步骤表（`operationId` = 将要 dispatch 的公开操作 method）。
fn build_steps(artifacts: &[String], repository_url: Option<&str>) -> Vec<Value> {
    let mut steps = Vec::new();
    if repository_url.is_some() {
        steps.push(json!({
            "stepId": "clone",
            "operationId": "workspaces.prepare",
            "effects": ["clone"],
            "requiresAuthorization": false,
        }));
    }
    steps.push(json!({
        "stepId": "run",
        "operationId": "run.submit",
        "effects": ["run"],
        "requiresAuthorization": false,
    }));
    if artifacts.contains(&"branch".to_owned()) {
        steps.push(json!({
            "stepId": "branch",
            "operationId": "git.create_branch",
            "effects": ["branch"],
            "requiresAuthorization": false,
        }));
    }
    if artifacts.contains(&"commit".to_owned()) {
        steps.push(json!({
            "stepId": "commit",
            "operationId": "git.commit",
            "effects": ["commit"],
            "requiresAuthorization": false,
        }));
    }
    if artifacts.contains(&"push".to_owned()) {
        steps.push(json!({
            "stepId": "push",
            "operationId": "git.push",
            "effects": ["push"],
            "requiresAuthorization": true,
        }));
    }
    steps
}

// ---------------------------------------------------------------------------
// agent_workflow.plan
// ---------------------------------------------------------------------------

async fn agent_workflow_plan(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let repository_url = input
        .get("repositoryUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let base_ref = input
        .get("baseRef")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let prompt = input["prompt"].as_str().unwrap_or_default().to_owned();
    let model_id = input
        .get("modelId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let artifacts: Vec<String> = input
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let credential_ref = input
        .get("credentialRef")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    // projectId 与 repositoryUrl 恰选其一（验收锚）。
    match (project_id, repository_url) {
        (Some(_), Some(_)) => {
            return Err(invalid_arg(
                "projectId and repositoryUrl are mutually exclusive; pass exactly one",
            )
            .into());
        }
        (None, None) => {
            return Err(invalid_arg("one of projectId or repositoryUrl is required").into());
        }
        _ => {}
    }
    if prompt.trim().is_empty() {
        return Err(invalid_arg("`prompt` must not be empty").into());
    }
    // PR 产物无公开操作对应（报告登记的差异）；预览不留注定失败的步骤。
    if artifacts.iter().any(|a| a == "pullRequest") {
        return Err(domain_unavailable(
            "the pullRequest artifact has no assembled public operation in this instance; \
             branch/commit/push are available",
        )
        .into());
    }
    // projectId 计划：目标项目必须已登记（预览不接注定失败的 submit）。
    if let Some(project_id) = project_id {
        let registered = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.query_row(
                "SELECT 1 FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql_err)?
            .is_some()
        };
        if !registered {
            return Err(invalid_arg(format!(
                "project `{project_id}` is not registered in this instance"
            ))
            .into());
        }
    }

    let steps = build_steps(&artifacts, repository_url);
    let temporary_resources: Vec<String> = if repository_url.is_some() {
        vec!["temp-clone".to_owned()]
    } else {
        Vec::new()
    };
    let plan_hash = plan_hash_of(
        project_id,
        repository_url,
        base_ref,
        &prompt,
        model_id,
        &artifacts,
        credential_ref,
        &steps,
    );
    let plan_id = format!("wfp-{}", rt.store().next_id());
    let required_permissions: Vec<String> = steps
        .iter()
        .filter_map(|step| step.get("operationId").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect();
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO v1_agent_workflow_plans
                (plan_id, plan_hash, project_id, repository_url, base_ref, prompt, model_id,
                 artifacts_json, credential_ref, steps_json, temporary_resources_json,
                 created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                plan_id,
                plan_hash,
                project_id,
                repository_url,
                base_ref,
                prompt,
                model_id,
                serde_json::to_string(&artifacts).unwrap_or_else(|_| "[]".into()),
                credential_ref,
                serde_json::to_string(&steps).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&temporary_resources).unwrap_or_else(|_| "[]".into()),
                now,
                rfc3339_plus(PLAN_TTL_SECS),
            ],
        )
        .map_err(sql_err)?;
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "planId": plan_id,
            "planHash": plan_hash,
            "steps": steps,
            "temporaryResources": temporary_resources,
            "requiredPermissions": required_permissions,
        }),
    ))
}

// ---------------------------------------------------------------------------
// workflow 行维护
// ---------------------------------------------------------------------------

fn update_workflow(
    conn: &Connection,
    workflow_id: &str,
    state: &str,
    steps: &[Value],
    run_id: Option<&str>,
    session_id: Option<&str>,
    resources: Option<&Value>,
    published: Option<&Value>,
    artifacts: Option<&Value>,
) {
    let _ = conn.execute(
        "UPDATE v1_agent_workflows
         SET state = ?2, steps_json = ?3, run_id = ?4, session_id = ?5,
             resources_json = COALESCE(?6, resources_json),
             published_json = COALESCE(?7, published_json),
             artifacts_json = COALESCE(?8, artifacts_json),
             updated_at = ?9
         WHERE workflow_id = ?1",
        params![
            workflow_id,
            state,
            serde_json::to_string(steps).unwrap_or_else(|_| "[]".into()),
            run_id,
            session_id,
            resources.map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into())),
            published.map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into())),
            artifacts.map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into())),
            now_rfc3339()
        ],
    );
}

/// 事件（workflow.stepStarted/stepCompleted/stepFailed；合同事件表）。
fn append_workflow_event(
    store: &Arc<V1Store>,
    resource_id: &str,
    event_type: &str,
    payload: &Value,
) {
    use std::sync::atomic::{AtomicI64, Ordering};
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let placeholder = format!("tmpwf-{nanos}-{seq}");
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    let conn = store.conn.lock().expect("sqlite mutex");
    if conn
        .execute(
            "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
             VALUES (?1, ?2, '', ?3, 'agentcpm-pd.event/1', ?4, ?4, ?5)",
            params![placeholder, resource_id, event_type, payload_str, now_rfc3339()],
        )
        .is_err()
    {
        return;
    }
    let seq = conn.last_insert_rowid();
    let _ = conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![crate::public_v1::store::format_cursor(seq), seq],
    );
}

// ---------------------------------------------------------------------------
// agent_workflow.submit
// ---------------------------------------------------------------------------

/// 从 git.status 观察当前 stateToken（公开操作观察面）。
async fn observe_state_token(
    rt: &Arc<PublicV1Runtime>,
    project_id: &str,
) -> Result<String, ContractError> {
    let (_, value) =
        dispatch_child(rt, "git.status", json!({"projectId": project_id}), None).await?;
    value["state"]["stateToken"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| domain_unavailable("git.status returned no stateToken"))
}

/// submit 的 worker：逐 step 编排公共操作。
async fn execute_workflow(
    rt: Arc<PublicV1Runtime>,
    db: Arc<DomainDb>,
    operation_id: String,
    workflow_id: String,
    plan: WorkflowPlan,
) {
    let _ = mark_domain_operation_running(&db, &operation_id, "");
    let mut steps: Vec<Value> = plan.steps.clone();
    for step in steps.iter_mut() {
        step["state"] = json!("pending");
    }
    let mut run_id: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut resources: Vec<Value> = Vec::new();
    let mut published: Vec<String> = Vec::new();
    let mut artifact_ids: Vec<String> = Vec::new();
    let mut branch_name = String::new();
    // 目标项目：repositoryUrl 由 clone 步产生；projectId 计划直接使用。
    let mut project_id = plan.project_id.clone().unwrap_or_default();
    let mut failed: Option<String> = None;

    let set_step = |steps: &mut [Value], step_id: &str, state: &str| {
        if let Some(step) = steps.iter_mut().find(|s| s["stepId"] == json!(step_id)) {
            step["state"] = json!(state);
        }
    };

    for step in plan.steps.iter() {
        let step_id = step["stepId"].as_str().unwrap_or_default().to_owned();
        append_workflow_event(
            rt.store(),
            &workflow_id,
            "workflow.stepStarted",
            &json!({"workflowId": workflow_id, "stepId": step_id}),
        );
        set_step(&mut steps, &step_id, "running");
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            update_workflow(
                &conn,
                &workflow_id,
                "running",
                &steps,
                run_id.as_deref(),
                session_id.as_deref(),
                None,
                None,
                None,
            );
        }
        let result: Result<Value, ContractError> = match step_id.as_str() {
            "clone" => {
                let child_key = format!("wf-clone-{workflow_id}");
                let mut input = json!({
                    "mode": "clone",
                    "destination": {
                        "authorizedRootId": "default",
                        "relativePath": {
                            "encoding": "utf8",
                            "value": format!("agent-workflows/{workflow_id}"),
                        },
                    },
                    "repositoryUrl": plan.repository_url,
                    "displayName": format!("agent workflow {workflow_id}"),
                });
                if let Some(base_ref) = &plan.base_ref {
                    input["branch"] = json!(base_ref);
                }
                if let Some(credential_ref) = &plan.credential_ref {
                    input["credentialRef"] = json!(credential_ref);
                }
                match dispatch_child(&rt, "workspaces.prepare", input, Some(&child_key)).await {
                    Ok((_, receipt)) => {
                        let child_op = receipt["operationId"].as_str().unwrap_or_default();
                        match confirm_child_operation(&rt, child_op, CHILD_OPERATION_BOUND, "clone")
                            .await
                        {
                            Ok(operation) => {
                                let completion = operation["result"].clone();
                                let cloned_project = completion["projectId"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_owned();
                                if cloned_project.is_empty() {
                                    Err(domain_unavailable(
                                        "workspaces.prepare completion carries no projectId",
                                    ))
                                } else {
                                    project_id = cloned_project;
                                    // 临时克隆目录（default 根下；cleanup 的移除目标）。
                                    let absolute =
                                        rt.store().resolve_authorized_root("default").ok().map(
                                            |root| {
                                                root.join(format!("agent-workflows/{workflow_id}"))
                                            },
                                        );
                                    resources.push(json!({
                                        "id": "clone",
                                        "kind": "temp-clone",
                                        "projectId": project_id,
                                        "serverPath": completion["serverPath"],
                                        "absolutePath": absolute
                                            .as_ref()
                                            .map(|path| path.display().to_string()),
                                    }));
                                    Ok(Value::Null)
                                }
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            "run" => {
                let session = format!("s-wf-{project_id}-{workflow_id}");
                let create = json!({
                    "sessionId": session,
                    "projectId": project_id,
                    "name": format!("agent workflow {workflow_id}"),
                });
                // 同名 session 已存在（前次部分执行残留）视作就绪。
                let session_ready = match dispatch_child(&rt, "session.create", create, None).await
                {
                    Ok(_) => Ok(()),
                    Err(ref err) if err.code == "ALREADY_EXISTS" => Ok(()),
                    Err(error) => Err(error),
                };
                match session_ready {
                    Ok(()) => {
                        let child_key = format!("wf-run-{workflow_id}");
                        let input = json!({
                            "sessionId": session,
                            "input": [{"kind": "text", "text": plan.prompt}],
                        });
                        match dispatch_child(&rt, "run.submit", input, Some(&child_key)).await {
                            Ok((_, receipt)) => {
                                let child_run =
                                    receipt["runId"].as_str().unwrap_or_default().to_owned();
                                session_id = Some(session);
                                if child_run.is_empty() {
                                    Err(domain_unavailable("run.submit receipt carries no runId"))
                                } else {
                                    run_id = Some(child_run.clone());
                                    match wait_run_terminal(&rt, &child_run, RUN_WAIT_BOUND).await {
                                        Some(run) if run.terminal => {
                                            let state = run_state_str(run.state);
                                            if state == "succeeded" {
                                                Ok(json!({"runId": child_run}))
                                            } else {
                                                Err(ContractError::new(
                                                    CODE_UPSTREAM_ERROR,
                                                    format!("run step ended `{state}`"),
                                                    RetrySpec::never(),
                                                    EffectStatus::NotApplied,
                                                ))
                                            }
                                        }
                                        _ => Err(domain_unavailable(
                                            "run never reached a terminal state",
                                        )),
                                    }
                                }
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            "branch" => {
                branch_name = format!("agent-workflow/{workflow_id}");
                match observe_state_token(&rt, &project_id).await {
                    Ok(token) => {
                        let input = json!({
                            "projectId": project_id,
                            "expectedStateToken": token,
                            "branchName": branch_name,
                            "checkout": true,
                        });
                        let key = format!("wf-branch-{workflow_id}");
                        match dispatch_child(&rt, "git.create_branch", input, Some(&key)).await {
                            Ok((_, receipt)) => {
                                let child_op = receipt["operationId"].as_str().unwrap_or_default();
                                confirm_child_operation(&rt, child_op, GIT_STEP_BOUND, "branch")
                                    .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            "commit" => match observe_state_token(&rt, &project_id).await {
                Ok(token) => {
                    let message = format!(
                        "agent workflow: {}",
                        plan.prompt.lines().next().unwrap_or_default()
                    );
                    let input = json!({
                        "projectId": project_id,
                        "expectedStateToken": token,
                        "selection": {"mode": "all"},
                        "message": message,
                    });
                    let key = format!("wf-commit-{workflow_id}");
                    match dispatch_child(&rt, "git.commit", input, Some(&key)).await {
                        Ok((_, receipt)) => {
                            let child_op = receipt["operationId"].as_str().unwrap_or_default();
                            confirm_child_operation(&rt, child_op, GIT_STEP_BOUND, "commit")
                                .await
                                .map(|operation| {
                                    if let Some(artifact) = operation["result"]["outputArtifactId"]
                                        .as_str()
                                        .filter(|a| !a.is_empty())
                                    {
                                        artifact_ids.push(artifact.to_owned());
                                    }
                                    operation
                                })
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            },
            "push" => {
                match observe_state_token(&rt, &project_id).await {
                    Ok(token) => {
                        let destination = if branch_name.is_empty() {
                            // 无 branch 工件：推送当前分支（git 域 HEAD 语义）。
                            "HEAD".to_owned()
                        } else {
                            format!("refs/heads/{branch_name}")
                        };
                        let input = json!({
                            "projectId": project_id,
                            "expectedStateToken": token,
                            "remoteName": "origin",
                            "sourceRef": "HEAD",
                            "destinationRef": destination,
                            "expectedRemoteOid": "",
                        });
                        let key = format!("wf-push-{workflow_id}");
                        match dispatch_child(&rt, "git.push", input, Some(&key)).await {
                            Ok((_, receipt)) => {
                                let child_op = receipt["operationId"].as_str().unwrap_or_default();
                                confirm_child_operation(&rt, child_op, GIT_STEP_BOUND, "push")
                                    .await
                                    .map(|operation| {
                                        if let Some(refs) =
                                            operation["result"]["changedRefs"].as_array()
                                        {
                                            for changed in refs {
                                                if let Some(name) = changed["refName"].as_str() {
                                                    published.push(format!("origin/{name}"));
                                                }
                                            }
                                        }
                                        if let Some(artifact) =
                                            operation["result"]["outputArtifactId"]
                                                .as_str()
                                                .filter(|a| !a.is_empty())
                                        {
                                            artifact_ids.push(artifact.to_owned());
                                        }
                                        operation
                                    })
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            other => Err(domain_unavailable(format!(
                "unknown workflow step `{other}`"
            ))),
        };
        match result {
            Ok(_) => {
                set_step(&mut steps, &step_id, "succeeded");
                append_workflow_event(
                    rt.store(),
                    &workflow_id,
                    "workflow.stepCompleted",
                    &json!({"workflowId": workflow_id, "stepId": step_id}),
                );
            }
            Err(error) => {
                set_step(&mut steps, &step_id, "failed");
                append_workflow_event(
                    rt.store(),
                    &workflow_id,
                    "workflow.stepFailed",
                    &json!({
                        "workflowId": workflow_id,
                        "stepId": step_id,
                        "code": error.code,
                        "message": error.message,
                    }),
                );
                failed = Some(format!("{}: {}", step_id, error.code));
                break;
            }
        }
    }
    // 未执行到的步骤标 skipped（部分失败保存步骤状态）。
    if failed.is_some() {
        for step in steps.iter_mut() {
            if step["state"] == json!("pending") {
                step["state"] = json!("skipped");
            }
        }
    }
    let final_state = if failed.is_some() {
        "failed"
    } else {
        "succeeded"
    };
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        update_workflow(
            &conn,
            &workflow_id,
            final_state,
            &steps,
            run_id.as_deref(),
            session_id.as_deref(),
            Some(&Value::Array(resources.clone())),
            Some(&Value::Array(
                published.iter().map(|p| json!(p)).collect::<Vec<_>>(),
            )),
            Some(&Value::Array(
                artifact_ids.iter().map(|a| json!(a)).collect::<Vec<_>>(),
            )),
        );
    }
    let owned_resource_ids: Vec<String> = resources
        .iter()
        .filter_map(|resource| resource["id"].as_str().map(ToOwned::to_owned))
        .collect();
    // completionResult 的 steps 只带 {stepId, operationId, state}（合同
    // additionalProperties: false；域内 steps_json 保留完整执行面）。
    let completion_steps: Vec<Value> = steps
        .iter()
        .map(|step| {
            json!({
                "stepId": step["stepId"],
                "operationId": step["operationId"],
                "state": step["state"],
            })
        })
        .collect();
    let completion = json!({
        "workflowId": workflow_id,
        "projectId": project_id,
        "runId": run_id,
        "steps": completion_steps,
        "artifactIds": artifact_ids,
        "publishedReferences": published,
        "ownedTemporaryResourceIds": owned_resource_ids,
    });
    let _ = finish_domain_operation(
        &db,
        &operation_id,
        session_id.as_deref().unwrap_or(""),
        final_state,
        &completion,
    );
}

async fn agent_workflow_submit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = domain_db(&rt)?;
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let plan_hash = input["planHash"].as_str().unwrap_or_default().to_owned();
    let authorized_effects: Vec<String> = input
        .get("authorizedEffects")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let snapshot = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        load_plan(&conn, &plan_id)?
            .ok_or_else(|| domain_not_found("agent workflow plan", &plan_id))?
    };
    let validate = || -> Result<(), ContractError> {
        let stale = |message: String| {
            ContractError::new(
                CODE_PLAN_STALE,
                message,
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        };
        if plan_hash != snapshot.plan_hash {
            return Err(stale(format!(
                "planHash `{plan_hash}` does not match plan `{}` (`{}`); re-plan",
                snapshot.plan_id, snapshot.plan_hash
            )));
        }
        // 发布效果显式授权（应用与发布显式锚）。
        for step in &snapshot.steps {
            if step["requiresAuthorization"].as_bool() == Some(true)
                && let Some(effects) = step["effects"].as_array()
            {
                for effect in effects {
                    let name = effect.as_str().unwrap_or_default();
                    if !authorized_effects.contains(&name.to_owned()) {
                        return Err(ContractError::new(
                            CODE_EFFECT_APPROVAL_REQUIRED,
                            format!(
                                "step `{}` requires effect `{name}` to be authorized",
                                step["stepId"].as_str().unwrap_or_default()
                            ),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                }
            }
        }
        // 一次性消费（过期 + 消费共用 expires_at；CAS 0 行 = 已消费/已过
        // 期，memory 域两段式同款纪律）。
        let now = now_rfc3339();
        if snapshot.expires_at < now {
            return Err(stale(format!(
                "plan `{}` expired at `{}`; re-plan",
                snapshot.plan_id, snapshot.expires_at
            )));
        }
        let consumed = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE v1_agent_workflow_plans SET expires_at = ?2
                 WHERE plan_id = ?1 AND expires_at > ?3",
                params![snapshot.plan_id, "1970-01-01T00:00:00Z", now],
            )
            .map_err(sql_err)?
        };
        if consumed == 0 {
            return Err(stale(format!(
                "plan `{}` is expired or already consumed; re-plan",
                snapshot.plan_id
            )));
        }
        Ok(())
    };
    let operation = accept_operation(
        &rt,
        &db,
        "agent_workflow.submit",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        validate,
    )?;
    // 重放（deduplicated）到此为止：直接回受理 receipt，不重复编排。
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    // workflow 行落位（克隆资源/步骤状态由 worker 维护）。
    let workflow_id = format!("wf-{}", rt.store().next_id());
    {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO v1_agent_workflows
                (workflow_id, plan_id, project_id, session_id, run_id, state, steps_json,
                 resources_json, published_json, artifacts_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, NULL, NULL, 'queued', '[]', '[]', '[]', '[]', ?4, ?4)",
            params![
                workflow_id,
                snapshot.plan_id,
                snapshot.project_id.clone().unwrap_or_default(),
                now
            ],
        )
        .map_err(sql_err)?;
    }
    let runner_rt = rt.clone();
    let runner_db = db.clone();
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        execute_workflow(runner_rt, runner_db, operation_id, workflow_id, snapshot).await;
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// agent_workflow.cleanup
// ---------------------------------------------------------------------------

async fn agent_workflow_cleanup(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = domain_db(&rt)?;
    let workflow_id = input["workflowId"].as_str().unwrap_or_default().to_owned();
    let resource_ids: Vec<String> = input
        .get("resourceIds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let (workflow_run_id, resources): (Option<String>, Vec<Value>) = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let row: Option<(Option<String>, String)> = conn
            .query_row(
                "SELECT run_id, resources_json FROM v1_agent_workflows WHERE workflow_id = ?1",
                params![workflow_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((run_id, resources_json)) = row else {
            return Err(domain_not_found("agent workflow", &workflow_id).into());
        };
        (
            run_id,
            serde_json::from_str::<Vec<Value>>(&resources_json).unwrap_or_default(),
        )
    };
    // 归属校验：resourceIds 必须全部是本 workflow 登记的资源。
    let owned_resources: Vec<Value> = resources
        .iter()
        .filter(|resource| {
            resource["id"]
                .as_str()
                .is_some_and(|id| resource_ids.iter().any(|wanted| wanted == id))
        })
        .cloned()
        .collect();
    if owned_resources.len() != resource_ids.len() {
        let foreign: Vec<String> = resource_ids
            .iter()
            .filter(|wanted| {
                !resources
                    .iter()
                    .any(|resource| resource["id"].as_str() == Some(wanted.as_str()))
            })
            .cloned()
            .collect();
        return Err(ContractError::new(
            CODE_RESOURCE_NOT_OWNED,
            format!(
                "resourceIds not owned by workflow `{workflow_id}`: {}",
                foreign.join(", ")
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    let operation = accept_operation(
        &rt,
        &db,
        "agent_workflow.cleanup",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || Ok(()),
    )?;
    // 重放（deduplicated）到此为止：直接回受理 receipt，不重复清理。
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    let runner_rt = rt.clone();
    let runner_db = db.clone();
    let operation_id = operation.operation_id.clone();
    let workflow_run_id = workflow_run_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner_db, &operation_id, "");
        // 运行占用检查（run 在途 → 保留，不移除）。
        let run_live = workflow_run_id
            .as_deref()
            .map(|run_id| {
                runner_rt
                    .store()
                    .get_run(run_id)
                    .map(|run| !run.terminal)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let mut removed: Vec<String> = Vec::new();
        let mut retained: Vec<String> = Vec::new();
        let mut reasons: Vec<Value> = Vec::new();
        for resource in owned_resources {
            let id = resource["id"].as_str().unwrap_or_default().to_owned();
            if run_live {
                retained.push(id.clone());
                reasons.push(json!({"resourceId": id, "reasonCode": "run-active"}));
                continue;
            }
            let target = resource["absolutePath"]
                .as_str()
                .map(std::path::PathBuf::from);
            match target {
                Some(path) if path.is_dir() => match std::fs::remove_dir_all(&path) {
                    Ok(()) => removed.push(id),
                    Err(_) => {
                        retained.push(id.clone());
                        reasons.push(json!({"resourceId": id, "reasonCode": "not-removable"}));
                    }
                },
                _ => {
                    retained.push(id.clone());
                    reasons.push(json!({"resourceId": id, "reasonCode": "not-present"}));
                }
            }
        }
        let completion = json!({
            "workflowId": workflow_id,
            "removedResourceIds": removed,
            "retainedResourceIds": retained,
            "retainedReasons": reasons,
        });
        let _ = finish_domain_operation(&runner_db, &operation_id, "", "succeeded", &completion);
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::domains::automation::tests::{
        dispatch_err, dispatch_ok, fixture, git_present, git_sync, wait_domain_operation,
    };
    use agentcpm_pd_protocol::registry::OperationRegistry;

    #[tokio::test]
    async fn plan_validates_target_choice_and_pr_artifact() {
        let fx = fixture("wf-plan-validation").await;
        crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;

        // projectId 与 repositoryUrl 恰选其一。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "proj-1", "repositoryUrl": "https://example.invalid/x.git",
                   "prompt": "hi"}),
            None,
        )
        .await;
        assert_eq!(err.code, "INVALID_ARGUMENT");
        let err = dispatch_err(&fx.rt, "agent_workflow.plan", json!({"prompt": "hi"}), None).await;
        assert_eq!(err.code, "INVALID_ARGUMENT");
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "proj-1"}),
            None,
        )
        .await;
        assert_eq!(err.code, "INVALID_ARGUMENT", "prompt 必填");

        // pullRequest 工件无公开操作对应 → UNAVAILABLE。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "proj-1", "prompt": "hi", "artifacts": ["pullRequest"]}),
            None,
        )
        .await;
        assert_eq!(err.code, "UNAVAILABLE");

        // 未登记项目 → INVALID_ARGUMENT（预览不接注定失败的 submit）。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "missing", "prompt": "hi"}),
            None,
        )
        .await;
        assert_eq!(err.code, "INVALID_ARGUMENT");

        // 未显式要求发布动作时计划不含 push 步骤（验收锚）。
        let plan = dispatch_ok(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "proj-1", "prompt": "do something"}),
            None,
        )
        .await;
        let steps = plan["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["stepId"], "run");
        assert_eq!(steps[0]["requiresAuthorization"], false);
        assert_eq!(plan["temporaryResources"], json!([]));
        assert_eq!(
            plan["requiredPermissions"],
            json!(["run.submit"]),
            "requiredPermissions 列出组合的公开操作"
        );
        assert!(
            OperationRegistry::global()
                .validate_result("agent_workflow.plan", &plan)
                .is_ok(),
            "plan 结果过 schema"
        );
        // 显式 push：步骤表含发布动作且 requiresAuthorization=true。
        let plan = dispatch_ok(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "proj-1", "prompt": "do something", "artifacts": ["push"]}),
            None,
        )
        .await;
        let steps = plan["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1]["stepId"], "push");
        assert_eq!(steps[1]["requiresAuthorization"], true);
    }

    #[tokio::test]
    async fn submit_orchestrates_public_operations_and_reports_steps() {
        if !git_present() {
            // git 缺席的环境由 e2e 侧跳过逻辑覆盖；单元测试直接跳过。
            return;
        }
        let fx = fixture("wf-submit").await;
        let workspace =
            crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;
        // 工作区留一个未提交修改（commit 步的素材；离线 driver 不落盘）。
        std::fs::write(workspace.join("wf-change.txt"), "workflow change\n").unwrap();

        let plan = dispatch_ok(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "proj-1", "prompt": "make a change", "artifacts": ["branch", "commit"]}),
            None,
        )
        .await;
        let plan_id = plan["planId"].as_str().unwrap().to_owned();
        let plan_hash = plan["planHash"].as_str().unwrap().to_owned();

        // planHash 不一致 → PLAN_STALE。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.submit",
            json!({"planId": plan_id, "planHash": "wrong-hash", "authorizedEffects": []}),
            Some("k-wf-stale"),
        )
        .await;
        assert_eq!(err.code, "PLAN_STALE");

        let receipt = dispatch_ok(
            &fx.rt,
            "agent_workflow.submit",
            json!({"planId": plan_id, "planHash": plan_hash, "authorizedEffects": []}),
            Some("k-wf-1"),
        )
        .await;
        assert_eq!(receipt["kind"], "operation");
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        let workflow_id = completion["workflowId"].as_str().unwrap().to_owned();
        assert!(!workflow_id.is_empty());
        assert_eq!(completion["projectId"], "proj-1");
        let run_id = completion["runId"].as_str().expect("run linked");
        assert!(!run_id.is_empty());
        let steps = completion["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);
        for step in steps {
            assert_eq!(step["state"], "succeeded", "全部步骤成功：{step}");
        }
        assert_eq!(completion["ownedTemporaryResourceIds"], json!([]));
        assert_eq!(completion["publishedReferences"], json!([]));
        // 父操作包含子 operationId/runId（无第二套 run owner）：run 属于该
        // 项目（run.get 可核对）。
        let run = dispatch_ok(&fx.rt, "run.get", json!({"runId": run_id}), None).await;
        assert_eq!(run["run"]["projectId"], "proj-1");

        // git 效果：分支存在 + 提交存在（公开 git 操作语义落盘）。
        git_sync(
            &workspace,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/agent-workflow/{workflow_id}"),
            ],
        );
        let log = std::process::Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(&workspace)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        let subject = String::from_utf8_lossy(&log.stdout);
        assert!(
            subject.contains("agent workflow:"),
            "commit 步产生提交：{subject}"
        );

        // 中途失败不误清理既有项目：同一计划再提交（新 key）会因 plan 过期
        // 拒绝（PLAN_STALE——两段式的显式边界）。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.submit",
            json!({"planId": plan_id, "planHash": plan_hash, "authorizedEffects": []}),
            Some("k-wf-2"),
        )
        .await;
        assert_eq!(err.code, "PLAN_STALE", "计划消费后过期");
    }

    #[tokio::test]
    async fn submit_requires_explicit_authorization_for_push() {
        let fx = fixture("wf-authz").await;
        crate::public_v1::domains::automation::tests::seed_real_project(&fx, "proj-1").await;
        let plan = dispatch_ok(
            &fx.rt,
            "agent_workflow.plan",
            json!({"projectId": "proj-1", "prompt": "publish it", "artifacts": ["push"]}),
            None,
        )
        .await;
        // 未授权 push 效果 → EFFECT_APPROVAL_REQUIRED（受理前拒绝，零副作用）。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.submit",
            json!({"planId": plan["planId"], "planHash": plan["planHash"], "authorizedEffects": []}),
            Some("k-wf-authz"),
        )
        .await;
        assert_eq!(err.code, "EFFECT_APPROVAL_REQUIRED");
        // 同 key 重试同一拒绝（终局拒绝重放）。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.submit",
            json!({"planId": plan["planId"], "planHash": plan["planHash"], "authorizedEffects": []}),
            Some("k-wf-authz"),
        )
        .await;
        assert_eq!(err.code, "EFFECT_APPROVAL_REQUIRED");
    }

    #[tokio::test]
    async fn repository_workflow_clones_runs_and_cleanup_removes_owned_clone() {
        if !git_present() {
            return;
        }
        let fx = fixture("wf-clone").await;
        // 造一个可克隆的本地 bare 源（file://，确定性无网络）。
        let source_workspace = fx._tmp.0.join("source");
        std::fs::create_dir_all(&source_workspace).unwrap();
        git_sync(&source_workspace, &["init", "-q", "-b", "main"]);
        git_sync(
            &source_workspace,
            &["config", "user.email", "src@example.invalid"],
        );
        git_sync(&source_workspace, &["config", "user.name", "Source"]);
        std::fs::write(source_workspace.join("README.md"), "# source\n").unwrap();
        git_sync(&source_workspace, &["add", "-A"]);
        git_sync(&source_workspace, &["commit", "-q", "-m", "init"]);
        let bare = fx._tmp.0.join("source.git");
        git_sync(
            &source_workspace,
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );
        let url = format!("file://{}", bare.display());

        let plan = dispatch_ok(
            &fx.rt,
            "agent_workflow.plan",
            json!({"repositoryUrl": url, "prompt": "inspect the repo"}),
            None,
        )
        .await;
        assert_eq!(plan["temporaryResources"], json!(["temp-clone"]));
        let steps = plan["steps"].as_array().unwrap();
        assert_eq!(steps[0]["stepId"], "clone");
        assert_eq!(steps[0]["operationId"], "workspaces.prepare");
        assert_eq!(steps[1]["stepId"], "run");

        let receipt = dispatch_ok(
            &fx.rt,
            "agent_workflow.submit",
            json!({"planId": plan["planId"], "planHash": plan["planHash"], "authorizedEffects": []}),
            Some("k-wf-clone-1"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        let workflow_id = completion["workflowId"].as_str().unwrap().to_owned();
        let cloned_project = completion["projectId"].as_str().unwrap().to_owned();
        assert!(!cloned_project.is_empty(), "克隆项目已登记");
        assert_eq!(
            completion["ownedTemporaryResourceIds"],
            json!(["clone"]),
            "克隆目录登记为 workflow 拥有的临时资源"
        );
        let clone_dir = {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            let resources: Value = serde_json::from_str(
                &conn
                    .query_row(
                        "SELECT resources_json FROM v1_agent_workflows WHERE workflow_id = ?1",
                        params![workflow_id],
                        |r| r.get::<_, String>(0),
                    )
                    .unwrap(),
            )
            .unwrap();
            resources[0]["absolutePath"].as_str().unwrap().to_owned()
        };
        assert!(std::path::Path::new(&clone_dir).is_dir(), "克隆目录在盘");

        // 外来资源 → RESOURCE_NOT_OWNED。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.cleanup",
            json!({"workflowId": workflow_id, "resourceIds": ["not-ours"]}),
            Some("k-wf-cleanup-foreign"),
        )
        .await;
        assert_eq!(err.code, "RESOURCE_NOT_OWNED");
        // 未知 workflow → NOT_FOUND。
        let err = dispatch_err(
            &fx.rt,
            "agent_workflow.cleanup",
            json!({"workflowId": "wf-missing", "resourceIds": ["clone"]}),
            Some("k-wf-cleanup-missing"),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");

        // cleanup 移除本 workflow 的临时克隆（既有项目不动）。
        let receipt = dispatch_ok(
            &fx.rt,
            "agent_workflow.cleanup",
            json!({"workflowId": workflow_id, "resourceIds": ["clone"]}),
            Some("k-wf-cleanup-1"),
        )
        .await;
        let operation =
            wait_domain_operation(&fx.rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        assert_eq!(completion["removedResourceIds"], json!(["clone"]));
        assert_eq!(completion["retainedResourceIds"], json!([]));
        assert!(!std::path::Path::new(&clone_dir).exists(), "克隆目录已移除");
        // 克隆登记的项目行仍在（既有项目永不默删）。
        let project = dispatch_ok(
            &fx.rt,
            "project.get",
            json!({"projectId": cloned_project}),
            None,
        )
        .await;
        assert_eq!(project["project"]["projectId"], cloned_project);
    }
}
