//! `workspaces.prepare` — 登记已有目录、创建空工作区或克隆仓库
//! （`cli.task.clone`，S3）。
//!
//! 合同正本：`state/cli-v1-operations.json` 的 `workspaces.prepare`
//! （execution=async、retry=idempotentSubmit；completionResult 定义业务结
//! 果，事件 operations.progress / projects.changed / git.changed）。TS 基线
//! （ui/server/routes/projects.js:379-512 的 create-workspace、537-675 的
//! clone-progress）是语义参考：existing 只登记已存在的目录；new 建目录
//! 且非空目标即拒；clone 失败清理本次创建的目录，而「克隆已完成但登记
//! 失败」报告残留路径、不删除数据（clone-progress.js:643 同语义）。
//!
//! 受理形态（「受理即返回 operation receipt」）：
//! 1. 幂等 intake（`idempotency_records`）：同 key 同输入重放原
//!    receipt（`deduplicated=true`，state 反映操作当前实况）；同 key 不同
//!    输入 `IDEMPOTENCY_CONFLICT`；预检拒绝记为 definitivelyRejected，
//!    同 key 重试得到同一拒绝而不是重新执行。
//! 2. 预检（受理前、零副作用 fail-fast）：授权根与路径收容（t012 的
//!    `decode_server_path`）、existing 的目录存在性、new/clone 的非空目
//!    标拒绝（「不覆盖非空目录」）、clone 的 URL 白名单与凭据形态。
//! 3. `V1Store::begin_long_operation`（collections 长操作缝：`operations`
//!    表 + operation.queued 事件；`operations.get/list/cancel` 直接可见）
//!    记录幂等 acceptance 后 `tokio::spawn` 后台执行——断开连接只停止
//!    等待，不影响已受理操作（合同 cancellation 条款；background_tasks
//!    同姿势）。
//!
//! 取消清理边界（「取消只清本次创建内容」）：执行期间的内存 manifest
//! （t013 recoveryArtifact 先例）记录本次新建/占位的绝对路径清单——
//! clone 的 staging 目录（`<root>/.pd-staging-<operationId>/payload`，
//! 本次操作拥有、同根保证 rename 原子）、目录链与移入的目标内容——成功
//! 路径把清单发布为 recovery artifact 供审计；取消/失败按清单倒序回收，
//! 清单之外（既有）的任何文件不触碰。唯一保留残留的分支是「克隆已完
//! 成但登记失败」：destination 是完整的克隆结果，删除会丢数据，改为在
//! 错误消息报告残留路径，由人决定去留。
//!
//! 凭据脱敏（「URL与凭据引用分离」）：repositoryUrl 只接受声明的传输
//! scheme（https/git/ssh/file；明文 http 与无 scheme 的 scp-like 形态拒
//! 绝），内嵌 userinfo（`user:pass@host`）直接 `INVALID_REPOSITORY_URL`
//! ——凭据从一开始就不进入 URL/argv/日志面；credentialRef 是引用，非
//! 空时与 git 域 R3 决策一致地拒绝（注入未接线，不静默忽略，git.rs 的
//! `credential_ref_unsupported` 同界）；错误消息全部为自构造短语，不透
//! 传 git stderr（stderr 只用于字符串分类），也不内嵌 URL 文本。git
//! clone 经 argv 直接 spawn（无 shell）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_RESULT_UNKNOWN,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::operations_ops::LongOperationOutcome;
use crate::public_v1::domains::workspace_dirs::{append_domain_event_in_tx, decode_server_path};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

// ---------------------------------------------------------------------------
// 合同错误码（`workspaces.prepare` 声明、protocol crate 尚无常量）。
// ---------------------------------------------------------------------------

const CODE_INVALID_REPOSITORY_URL: &str = "INVALID_REPOSITORY_URL";
const CODE_AUTHENTICATION_FAILED: &str = "AUTHENTICATION_FAILED";
const CODE_REMOTE_UNREACHABLE: &str = "REMOTE_UNREACHABLE";
const CODE_PARTIAL_FAILURE: &str = "PARTIAL_FAILURE";

/// 网络克隆时限（对齐 git 域 `GIT_NETWORK_TIMEOUT` 的 300 s）。
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
/// 取消意图轮询间隔（clone 子进程期间）。
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(150);
/// 幂等记录保留窗口（24 h，对齐 git 域受理类）。
const RETENTION_SECS: i64 = 24 * 60 * 60;
/// staging 目录前缀（授权根内的隐藏目录，directories.list 不展示点前缀）。
const STAGING_PREFIX: &str = ".pd-staging-";
/// repositoryUrl 的最大长度（防无界 argv）。
const MAX_URL_BYTES: usize = 8192;
/// completionResult 的 schema 标识（`operations.get` 的 resultSchemaId）。
const COMPLETION_SCHEMA_ID: &str = "agentcpm-pd.workspaces.prepare.completion/1";
/// recovery manifest artifact 的 schema 标识。
const MANIFEST_SCHEMA_ID: &str = "agentcpm-pd.workspaces.prepare.manifest/1";

const METHOD_ID: &str = "workspaces.prepare";

pub(super) fn register(reg: &mut DomainRegistry) {
    // 本域不新增表：operations（长操作缝）、idempotency_records、projects、
    // events、artifacts 均由 S2 / collections / t012 / t013 登记 DDL。
    reg.handler(METHOD_ID, |rt, input, key, _rev| {
        Box::pin(workspaces_prepare(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 受理（dispatch）。
// ---------------------------------------------------------------------------

async fn workspaces_prepare(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();

    // 幂等 intake：同 key 同输入重放受理结果（「再次相同提交不重复创
    // 建」）；同 key 不同输入冲突。
    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = replay_accepted_receipt(&store, key, &input)?
    {
        return Ok((ResponseOutcome::Accepted, replay));
    }

    // 预检：受理前完成全部确定性检查（零副作用）。
    let plan = match precheck(&store, &input) {
        Ok(plan) => plan,
        Err(err) => {
            // 预检拒绝是终局性的：登记后同 key 重试得到同一拒绝。
            if let Some(key) = idempotency_key.as_deref() {
                record_rejection(&store, key, &input, &err.code);
            }
            return Err(err.into());
        }
    };

    // 受理：长操作缝登记（operations 表 + operation.queued 事件），幂等
    // acceptance 记录，后台执行。
    let (operation_id, revision) = store.begin_long_operation(
        METHOD_ID,
        &json!({"kind": "instance"}),
        // 克隆/创建可中断；existing 的登记本身不可分（一步 SQLite 事
        // 务），取消语义按阶段边界生效。
        true,
    )?;
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": revision,
        "deduplicated": false,
        "state": "queued",
    });
    if let Some(key) = idempotency_key.as_deref()
        && let Err(err) = record_acceptance(&store, key, &input, &operation_id, &receipt)
    {
        // 操作行已存在但 acceptance 未能持久化：把它显式终态化（失败）
        // 而不是留一个无人驱动的 queued 行，再如实报告受理失败。
        let _ = store.finish_long_operation(
            &operation_id,
            LongOperationOutcome::Failed {
                code: err.code.clone(),
                message: err.message.clone(),
            },
        );
        return Err(err.into());
    }

    let job = PrepareJob {
        operation_id: operation_id.clone(),
        mode: plan.mode,
        root_id: plan.root_id,
        root: plan.root,
        relative: plan.relative,
        relative_str: plan.relative_str,
        repository_url: plan.repository_url,
        branch: plan.branch,
        name: plan.name,
        idempotency_key,
    };
    let exec_store = store.clone();
    tokio::spawn(async move {
        execute_prepare(exec_store, job).await;
    });
    Ok((ResponseOutcome::Accepted, receipt))
}

/// 一次已受理请求的全部执行参数（dispatch 侧预检的产出）。
struct PreparePlan {
    mode: String,
    root_id: String,
    root: PathBuf,
    relative: PathBuf,
    relative_str: String,
    repository_url: Option<String>,
    branch: Option<String>,
    name: String,
}

/// 受理前预检。全部错误为 NotApplied（「目标存在且非空 → 明确错误」等
/// 验收锚在受理期生效，受理即零业务副作用）。
fn precheck(store: &V1Store, input: &Value) -> Result<PreparePlan, ContractError> {
    let mode = input["mode"].as_str().unwrap_or_default().to_owned();
    // decode_server_path：授权根声明（未知 root → FORBIDDEN）、路径收容
    // （词法 + canonical 化，符号链接越界 → PATH_ESCAPE）。existing 要
    // 求目标已存在（NOT_FOUND / NOT_A_DIRECTORY），new/clone 允许不存在。
    let rooted = decode_server_path(store, &input["destination"], mode == "existing")?;
    // 根本身不是合法目标（登记到 root 会把授权根变成项目工作区）。
    if rooted.relative.as_os_str().is_empty() {
        return Err(invalid(
            "destination.relativePath must name a path inside the authorized root, not the root itself",
        ));
    }
    // 登记服务的 root.relativePath 是 JSON string：非 UTF-8 路径无法无损
    // 进入（BytePath 的 base64 形态仅服务传输），禁止有损替换 → 明确拒绝。
    let Some(relative_str) = rooted.relative.to_str().map(str::to_owned) else {
        return Err(invalid(
            "destination.relativePath must be valid UTF-8: the registration service records the relative path as a JSON string",
        ));
    };

    let destination = rooted.root.join(&rooted.relative);
    let fallback_name = rooted
        .relative
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_owned());
    let name = input
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&fallback_name)
        .to_owned();

    let (repository_url, branch) = match mode.as_str() {
        "existing" => (None, None),
        "new" => {
            refuse_nonempty_destination(&destination)?;
            (None, None)
        }
        "clone" => {
            refuse_nonempty_destination(&destination)?;
            let url = input["repositoryUrl"].as_str().unwrap_or_default();
            validate_repository_url(url)?;
            // credentialRef 是引用形态（secret 不内嵌）：注入未接线，与
            // git 域 R3 决策一致地拒绝而不是静默忽略。
            if input
                .get("credentialRef")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                return Err(ContractError::new(
                    CODE_UNAVAILABLE,
                    "credentialRef injection is not wired yet; omit credentialRef to use the system git credential resolution",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let branch = input.get("branch").and_then(Value::as_str);
            if let Some(branch) = branch
                && !valid_branch_name(branch)
            {
                return Err(invalid(format!(
                    "branch `{branch}` is not a valid ref name"
                )));
            }
            (
                Some(url.to_owned()),
                branch.filter(|b| !b.is_empty()).map(ToOwned::to_owned),
            )
        }
        other => {
            return Err(invalid(format!(
                "mode must be one of existing/new/clone (got `{other}`)"
            )));
        }
    };

    Ok(PreparePlan {
        mode,
        root_id: rooted.root_id,
        root: rooted.root,
        relative: rooted.relative,
        relative_str,
        repository_url,
        branch,
        name,
    })
}

/// 「不覆盖非空目录」：目标已存在且（是文件或）非空 → ALREADY_EXISTS；
/// 已存在的空目录可接受（内容移入，不删除目录本身）。
fn refuse_nonempty_destination(destination: &Path) -> Result<(), ContractError> {
    match std::fs::symlink_metadata(destination) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(unavailable(format!(
            "cannot stat destination `{}`: {e}",
            destination.display()
        ))),
        Ok(_) => {
            let is_dir = std::fs::metadata(destination)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if !is_dir {
                return Err(ContractError::new(
                    CODE_ALREADY_EXISTS,
                    format!(
                        "destination `{}` already exists and is not a directory",
                        destination.display()
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let is_empty = std::fs::read_dir(destination)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if !is_empty {
                return Err(ContractError::new(
                    CODE_ALREADY_EXISTS,
                    format!(
                        "destination `{}` already exists and is not empty; refusing to overwrite",
                        destination.display()
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            Ok(())
        }
    }
}

/// clone 源校验（「只支持声明的传输协议」；「URL与凭据引用分离」）。
/// 白名单 https/git/ssh/file；拒绝明文 http、无 scheme 的 scp-like 形态
/// 与内嵌 userinfo（凭据不进 URL/argv）。错误消息不含 URL 文本（脱敏）。
fn validate_repository_url(url: &str) -> Result<(), ContractError> {
    let reject = |message: String| {
        ContractError::new(
            CODE_INVALID_REPOSITORY_URL,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    if url.is_empty() {
        return Err(reject(
            "repositoryUrl is required for clone mode".to_owned(),
        ));
    }
    if url.len() > MAX_URL_BYTES {
        return Err(reject(format!(
            "repositoryUrl exceeds the supported maximum of {MAX_URL_BYTES} bytes"
        )));
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(reject(
            "repositoryUrl must use a supported transport: https://, git://, ssh:// or file://"
                .to_owned(),
        ));
    };
    match scheme {
        "https" | "git" | "ssh" => {
            let authority = rest.split('/').next().unwrap_or("");
            if authority.is_empty() {
                return Err(reject("repositoryUrl is missing its host".to_owned()));
            }
            // URL 内嵌凭据拒绝（「URL与凭据引用分离」）：userinfo 带密码
            // （`user:pass@host`）直接拒绝。纯 username（`git@host`，ssh 的
            // 常规形态）不是机密，放行——git 侧仍无终端提示，认证失败分
            // 类兜底。
            if let Some((userinfo, _)) = authority.rsplit_once('@')
                && userinfo.contains(':')
            {
                return Err(reject(
                    "credentials embedded in repositoryUrl are not supported; use a credentialRef instead"
                        .to_owned(),
                ));
            }
        }
        "file" => {
            if !rest.starts_with('/') {
                return Err(reject(
                    "file:// repositoryUrl requires an absolute path".to_owned(),
                ));
            }
        }
        other => {
            return Err(reject(format!(
                "unsupported repositoryUrl transport `{other}` (supported: https, git, ssh, file)"
            )));
        }
    }
    Ok(())
}

/// 分支名校验（TS 基线 git.js:51-102 的移植语义，与 git 域一致）。
fn valid_branch_name(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'_' | b'-'))
}

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// 幂等受理记录（`idempotency_records`，与 run.submit / git 域同一张表）。
// ---------------------------------------------------------------------------

/// 同 key 重放：digest 一致 → 返回原 receipt（state 刷新为操作实况，
/// `deduplicated=true`）；不一致 → IDEMPOTENCY_CONFLICT；此前被拒绝 → 原
/// 拒绝码。无记录 → Ok(None)。
fn replay_accepted_receipt(
    store: &V1Store,
    key: &str,
    input: &Value,
) -> Result<Option<Value>, ContractError> {
    let digest = compute_input_digest(METHOD_ID, input);
    let conn = store.conn.lock().expect("sqlite mutex");
    let found: Option<(String, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT input_digest, request_state, receipt_json, rejection_code
             FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, METHOD_ID],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("sqlite storage error: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            )
        })?;
    let Some((stored_digest, request_state, receipt_json, rejection_code)) = found else {
        return Ok(None);
    };
    if stored_digest != digest {
        return Err(ContractError::new(
            CODE_IDEMPOTENCY_CONFLICT,
            format!("idempotencyKey `{key}` already used for {METHOD_ID} with different input"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if request_state == "accepted"
        && let Some(raw) = receipt_json
        && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
        && let Some(obj) = receipt.as_object_mut()
    {
        // 重放反映当前实况（后台执行推进 state）。
        if let Some(operation_id) = obj.get("operationId").and_then(Value::as_str)
            && let Ok(Some(state)) = conn
                .query_row(
                    "SELECT state FROM operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
        {
            obj.insert("state".to_owned(), json!(state));
        }
        obj.insert("deduplicated".to_owned(), json!(true));
        return Ok(Some(receipt));
    }
    Err(ContractError::new(
        rejection_code.unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
        "request previously rejected definitively; retry with a new idempotencyKey",
        RetrySpec::never(),
        EffectStatus::NotApplied,
    ))
}

/// 受理记录（`requests.lookup` 同 methodId/key 返回原资源 ID）。并发同
/// key 的第二份受理在此撞主键而显式失败（其 operation 行被调用方终态
/// 化），而不是静默丢弃 acceptance。
fn record_acceptance(
    store: &V1Store,
    key: &str,
    input: &Value,
    operation_id: &str,
    receipt: &Value,
) -> Result<(), ContractError> {
    let digest = compute_input_digest(METHOD_ID, input);
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "INSERT INTO idempotency_records
            (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
             receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')",
        params![
            key,
            METHOD_ID,
            digest,
            serde_json::to_string(&json!({"kind": "operation", "operationId": operation_id}))
                .unwrap_or_default(),
            serde_json::to_string(receipt).unwrap_or_default(),
            now_rfc3339(),
            (chrono::Utc::now() + chrono::Duration::seconds(RETENTION_SECS)).to_rfc3339(),
        ],
    )
    .map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("sqlite storage error: {e}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;
    Ok(())
}

/// 预检拒绝记录（同 key 重试得到同一拒绝，不重新执行）。
fn record_rejection(store: &V1Store, key: &str, input: &Value, code: &str) {
    let digest = compute_input_digest(METHOD_ID, input);
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT OR IGNORE INTO idempotency_records
            (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
             receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6, 'correctRejectedInputWithNewKey')",
        params![
            key,
            METHOD_ID,
            digest,
            code,
            now_rfc3339(),
            (chrono::Utc::now() + chrono::Duration::seconds(RETENTION_SECS)).to_rfc3339(),
        ],
    );
}

/// 后台终态推进后刷新重放 receipt 的 state（同 key 重试观察到终态）。
fn update_receipt_terminal_state(store: &V1Store, key: Option<&str>, state: &str) {
    let Some(key) = key else {
        return;
    };
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let Ok(raw) = conn
        .query_row(
            "SELECT receipt_json FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, METHOD_ID],
            |r| r.get::<_, String>(0),
        )
    else {
        return;
    };
    if let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
        && let Some(obj) = receipt.as_object_mut()
    {
        obj.insert("state".to_owned(), json!(state));
        let _ = conn.execute(
            "UPDATE idempotency_records SET receipt_json = ?1
             WHERE idempotency_key = ?2 AND method_id = ?3",
            params![
                serde_json::to_string(&receipt).unwrap_or(raw),
                key,
                METHOD_ID
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// 后台执行。
// ---------------------------------------------------------------------------

struct PrepareJob {
    operation_id: String,
    mode: String,
    root_id: String,
    /// 授权根的 canonical 绝对路径。
    root: PathBuf,
    /// 目标相对路径（已过收容校验）。
    relative: PathBuf,
    /// 目标相对路径的 UTF-8 形态（登记服务的 root.relativePath）。
    relative_str: String,
    repository_url: Option<String>,
    branch: Option<String>,
    name: String,
    idempotency_key: Option<String>,
}

/// 执行结果：成功为 completionResult；失败区分取消（已按清单回收）与
/// 失败（清理策略已在 `run_phases` 内按分支执行，错误消息携带残留报
/// 告）。
enum PrepareFailure {
    Cancelled,
    Failed(ContractError),
}

async fn execute_prepare(store: Arc<V1Store>, job: PrepareJob) {
    let _ = store.mark_operation_running(&job.operation_id);
    match run_phases(&store, &job).await {
        Ok(completion) => {
            let key = job.idempotency_key.clone();
            let operation_id = job.operation_id.clone();
            let mode = job.mode.clone();
            let _ = store.finish_long_operation(
                &operation_id,
                LongOperationOutcome::Succeeded {
                    result: completion,
                    result_schema_id: Some(COMPLETION_SCHEMA_ID.to_owned()),
                },
            );
            // 合同事件：projects.changed（登记成功）；git.changed 仅 clone。
            let mut event_types = vec!["projects.changed"];
            if mode == "clone" {
                event_types.push("git.changed");
            }
            emit_workspace_events(&store, &operation_id, &event_types);
            update_receipt_terminal_state(&store, key.as_deref(), "succeeded");
        }
        Err(PrepareFailure::Cancelled) => {
            let _ = store.finish_long_operation(&job.operation_id, LongOperationOutcome::Cancelled);
            update_receipt_terminal_state(&store, job.idempotency_key.as_deref(), "cancelled");
        }
        Err(PrepareFailure::Failed(err)) => {
            let _ = store.finish_long_operation(
                &job.operation_id,
                LongOperationOutcome::Failed {
                    code: err.code.clone(),
                    message: err.message.clone(),
                },
            );
            update_receipt_terminal_state(&store, job.idempotency_key.as_deref(), "failed");
        }
    }
}

fn cancel_requested(store: &V1Store, operation_id: &str) -> bool {
    matches!(store.operation_cancel_requested(operation_id), Ok(true))
}

fn phase_progress(store: &V1Store, operation_id: &str, stage: &str) {
    let _ = store.record_operation_progress(operation_id, "phase", &json!({"stage": stage}));
}

async fn run_phases(store: &Arc<V1Store>, job: &PrepareJob) -> Result<Value, PrepareFailure> {
    let destination = job.root.join(&job.relative);
    // 本次创建/占位路径清单（浅→深 push；回收按倒序）。t013 recovery
    // 先例：清单只含本次操作拥有的路径，回收边界即清单边界。
    let mut manifest: Vec<PathBuf> = Vec::new();

    match job.mode.as_str() {
        "existing" => {
            phase_progress(store, &job.operation_id, "registering");
            register_and_complete(store, job, Vec::new(), None)
                .await
                .map_err(PrepareFailure::Failed)
        }
        "new" => {
            phase_progress(store, &job.operation_id, "creating");
            if let Err(err) = create_missing_chain(&destination, &mut manifest) {
                let residual = reclaim_manifest_paths(&manifest);
                return Err(PrepareFailure::Failed(attach_residual(err, &residual)));
            }
            // 创建后取消检查：本次新建的链按清单回收，既有内容零触碰。
            if cancel_requested(store, &job.operation_id) {
                let residual = reclaim_manifest_paths(&manifest);
                if !residual.is_empty() {
                    // 回收不完整：报告失败而非假装已取消干净。
                    return Err(PrepareFailure::Failed(attach_residual(
                        ContractError::new(
                            CODE_PARTIAL_FAILURE,
                            "cancel requested but reclaiming the created directories failed",
                            RetrySpec::never(),
                            EffectStatus::Unknown,
                        ),
                        &residual,
                    )));
                }
                return Err(PrepareFailure::Cancelled);
            }
            phase_progress(store, &job.operation_id, "registering");
            let created = manifest_relative(&manifest, &job.root_id, &job.root);
            register_and_complete(store, job, created, None)
                .await
                .map_err(PrepareFailure::Failed)
        }
        "clone" => run_clone_phases(store, job, &destination, &mut manifest).await,
        other => Err(PrepareFailure::Failed(invalid(format!(
            "unknown mode `{other}`"
        )))),
    }
}

async fn run_clone_phases(
    store: &Arc<V1Store>,
    job: &PrepareJob,
    destination: &Path,
    manifest: &mut Vec<PathBuf>,
) -> Result<Value, PrepareFailure> {
    let url = job.repository_url.clone().unwrap_or_default();
    // 本次拥有的 staging（根内隐藏目录，同文件系统保证 rename 原子）。
    let staging = job
        .root
        .join(format!("{STAGING_PREFIX}{}", job.operation_id));
    let payload = staging.join("payload");
    if let Err(err) = std::fs::create_dir_all(&staging) {
        return Err(PrepareFailure::Failed(unavailable(format!(
            "cannot create clone staging directory `{}`: {err}",
            staging.display()
        ))));
    }
    manifest.push(staging.clone());

    phase_progress(store, &job.operation_id, "cloning");
    match spawn_git_clone(
        store,
        &job.operation_id,
        &url,
        job.branch.as_deref(),
        &payload,
    )
    .await
    {
        CloneOutcome::Output(_) => {}
        CloneOutcome::Cancelled => {
            let residual = reclaim_manifest_paths(manifest);
            if !residual.is_empty() {
                return Err(PrepareFailure::Failed(attach_residual(
                    ContractError::new(
                        CODE_PARTIAL_FAILURE,
                        "cancel requested but reclaiming the staged clone failed",
                        RetrySpec::never(),
                        EffectStatus::Unknown,
                    ),
                    &residual,
                )));
            }
            return Err(PrepareFailure::Cancelled);
        }
        CloneOutcome::Failure(err) => {
            let residual = reclaim_manifest_paths(manifest);
            // 「认证失败只清理本次创建且确认归属的目录」：staging 归本次
            // 操作所有，回收；destination 未被触碰（clone 在 staging 内）。
            return Err(PrepareFailure::Failed(attach_residual(err, &residual)));
        }
    }

    // 移动前取消检查与目标复验（并发窗口内目标可能被他人占用）。
    if cancel_requested(store, &job.operation_id) {
        let residual = reclaim_manifest_paths(manifest);
        if !residual.is_empty() {
            return Err(PrepareFailure::Failed(attach_residual(
                ContractError::new(
                    CODE_PARTIAL_FAILURE,
                    "cancel requested but reclaiming the staged clone failed",
                    RetrySpec::never(),
                    EffectStatus::Unknown,
                ),
                &residual,
            )));
        }
        return Err(PrepareFailure::Cancelled);
    }
    phase_progress(store, &job.operation_id, "moving");
    if let Err(err) = move_payload(&payload, destination, manifest) {
        let residual = reclaim_manifest_paths(manifest);
        return Err(PrepareFailure::Failed(attach_residual(err, &residual)));
    }

    // 登记前取消检查：移入的内容按清单回收（destination 本身是本次创
    // 建的交付物之一，也在清单内）。
    if cancel_requested(store, &job.operation_id) {
        let residual = reclaim_manifest_paths(manifest);
        if !residual.is_empty() {
            return Err(PrepareFailure::Failed(attach_residual(
                ContractError::new(
                    CODE_PARTIAL_FAILURE,
                    "cancel requested but reclaiming the cloned workspace failed",
                    RetrySpec::never(),
                    EffectStatus::Unknown,
                ),
                &residual,
            )));
        }
        return Err(PrepareFailure::Cancelled);
    }
    phase_progress(store, &job.operation_id, "registering");

    // 成功路径的清单定型：发布 recovery manifest（t013 先例的审计面，
    // 含 staging 的完整过程记录）；completionResult.createdPaths 只报告
    // 交付物（staging 是过程产物，成功路径已回收，不属于「本次新建」的
    // 结果面）。
    let created = {
        let deliverables: Vec<PathBuf> = manifest
            .iter()
            .filter(|path| !path.starts_with(&staging))
            .cloned()
            .collect();
        manifest_relative(&deliverables, &job.root_id, &job.root)
    };
    let manifest_artifact = publish_manifest_artifact(store, job, manifest);

    match register_and_complete(store, job, created, Some(destination)).await {
        Ok(completion) => {
            // staging 空壳回收（payload 已移走）；成功路径不回收清单内
            // 的交付物。清不掉只影响磁盘，不影响交付。
            let _ = std::fs::remove_dir(&staging);
            if let Some((artifact_id, _)) = manifest_artifact.as_ref() {
                let _ = store.record_operation_progress(
                    &job.operation_id,
                    "recoveryArtifactId",
                    &json!(artifact_id),
                );
            }
            Ok(completion)
        }
        Err(err) => {
            // 「克隆完登记失败要报告残留路径，不能再克隆覆盖」：删除完
            // 整克隆结果会丢数据——保留 destination，错误消息报告残留；
            // 目标非空使同 destination 的重试被「不覆盖非空目录」拒绝。
            Err(PrepareFailure::Failed(ContractError::new(
                err.code.clone(),
                format!(
                    "clone completed but project registration failed: {}; residual cloned \
                     workspace left at `{}` (not reclaimed; a retry against the same \
                     destination will be refused while it is non-empty)",
                    err.message,
                    destination.display()
                ),
                err.retry.clone(),
                EffectStatus::NotApplied,
            )))
        }
    }
}

/// 登记并构造 completionResult（existing/new/clone 共用同一登记服务
/// `V1Store::register_project`——「作业成功包含唯一 Project 结果」）。
/// clone 模式额外回填 repositoryId（canonical git toplevel，git 域同
/// 形态）。
async fn register_and_complete(
    store: &Arc<V1Store>,
    job: &PrepareJob,
    created: Vec<Value>,
    cloned_destination: Option<&Path>,
) -> Result<Value, ContractError> {
    let project_id = store.next_id();
    let register_input = json!({
        "instanceId": store.meta().instance_id,
        "projectId": project_id,
        "name": job.name,
        "root": {
            "instanceId": store.meta().instance_id,
            "rootId": job.root_id,
            "relativePath": job.relative_str,
        },
        "expectedAbsent": true,
        "environmentId": "local",
    });
    let registered = store.register_project(&register_input)?;
    let Some(project_id) = registered["project"]["projectId"].as_str() else {
        return Err(unavailable(
            "the registration service returned no projectId",
        ));
    };
    let mut completion = json!({
        "projectId": project_id,
        "serverPath": server_path_json(&job.root_id, &job.relative),
        "mode": job.mode,
        "createdPaths": created,
    });
    if let Some(destination) = cloned_destination
        && let Some(repository_id) = repository_toplevel(destination).await
    {
        completion["repositoryId"] = json!(repository_id);
    }
    Ok(completion)
}

/// 目标仓库的 canonical git toplevel（`repositoryId` 的 opaque 形态，
/// git 域 `resolve_repo` 同语义）。
async fn repository_toplevel(destination: &Path) -> Option<String> {
    let out =
        crate::public_v1::domains::git::git_run_str(destination, &["rev-parse", "--show-toplevel"])
            .await
            .ok()?;
    let text = String::from_utf8_lossy(&out).trim_end().to_owned();
    let path = PathBuf::from(&text);
    let canonical = path.canonicalize().unwrap_or(path);
    Some(canonical.display().to_string())
}

// ---------------------------------------------------------------------------
// 文件系统阶段（创建链 / 移动 / 清单回收）。
// ---------------------------------------------------------------------------

/// 从最浅缺失祖先到 `target` 逐级创建目录（不用 create_dir_all：需要精
/// 确知道哪些级是本次新建的，进 manifest）。
fn create_missing_chain(target: &Path, manifest: &mut Vec<PathBuf>) -> Result<(), ContractError> {
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(target);
    while let Some(current) = cursor {
        if current.as_os_str().is_empty() {
            break;
        }
        match std::fs::symlink_metadata(current) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
            }
            Err(e) => {
                return Err(unavailable(format!(
                    "cannot stat `{}`: {e}",
                    current.display()
                )));
            }
        }
        cursor = current.parent();
    }
    for path in missing.iter().rev() {
        match std::fs::create_dir(path) {
            Ok(()) => manifest.push(path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 并发窗口：他人先建了同名目录——本次零创建，目标已存在
                // 时由调用方的复验裁决。
            }
            Err(e) => {
                return Err(unavailable(format!(
                    "cannot create directory `{}`: {e}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

/// 把 staging payload 移入 destination：目标不存在时整体 rename（原
/// 子）；目标为已存在的空目录时逐条目移入（目录本身保留——它不是本次
/// 创建的）。移动产生的路径进 manifest（取消/失败时回收边界）。
fn move_payload(
    payload: &Path,
    destination: &Path,
    manifest: &mut Vec<PathBuf>,
) -> Result<(), ContractError> {
    match std::fs::symlink_metadata(destination) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = destination.parent() {
                create_missing_chain(parent, manifest)?;
            }
            std::fs::rename(payload, destination).map_err(|e| {
                unavailable(format!(
                    "cannot move the cloned workspace into `{}`: {e}",
                    destination.display()
                ))
            })?;
            manifest.push(destination.to_path_buf());
            Ok(())
        }
        Err(e) => Err(unavailable(format!(
            "cannot stat destination `{}`: {e}",
            destination.display()
        ))),
        Ok(_) => {
            // 已存在（受理时为空）：复验仍为空——并发窗口内被占用则拒绝
            // （不覆盖非空目录）。
            let is_dir = std::fs::metadata(destination)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if !is_dir {
                return Err(ContractError::new(
                    CODE_ALREADY_EXISTS,
                    format!(
                        "destination `{}` was replaced by a non-directory during the clone",
                        destination.display()
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let is_empty = std::fs::read_dir(destination)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if !is_empty {
                return Err(ContractError::new(
                    CODE_ALREADY_EXISTS,
                    format!(
                        "destination `{}` became non-empty during the clone; refusing to overwrite",
                        destination.display()
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let entries: Vec<std::ffi::OsString> = std::fs::read_dir(payload)
                .map_err(|e| unavailable(format!("cannot read the staged clone: {e}")))?
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
            for name in entries {
                let source = payload.join(&name);
                let target = destination.join(&name);
                std::fs::rename(&source, &target).map_err(|e| {
                    unavailable(format!(
                        "cannot move the cloned entry into `{}`: {e}",
                        target.display()
                    ))
                })?;
                manifest.push(target);
            }
            Ok(())
        }
    }
}

/// 按清单倒序回收（深→浅），只删清单内路径。返回未能回收的残留报告
/// （空 = 全部回收）。
fn reclaim_manifest_paths(manifest: &[PathBuf]) -> Vec<String> {
    let mut residual: Vec<String> = Vec::new();
    for path in manifest.iter().rev() {
        match std::fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => residual.push(format!("{}: {e}", path.display())),
        }
    }
    residual
}

/// 把回收残留并进错误消息（「要报告残留路径」）。operation 记录的
/// error 无长度限制，但 dispatch 面的 error.message schema 上限 256——
/// 截断到 char 边界而不是超限。
fn attach_residual(mut err: ContractError, residual: &[String]) -> ContractError {
    if !residual.is_empty() {
        err.message.push_str("; residual paths left behind: ");
        err.message.push_str(&residual.join("; "));
        if err.message.len() > 256 {
            let mut cut = 256;
            while cut > 0 && !err.message.is_char_boundary(cut) {
                cut -= 1;
            }
            err.message.truncate(cut);
        }
    }
    err
}

/// 清单 → completionResult 的 createdPaths（root 相对 ServerPath）。
fn manifest_relative(manifest: &[PathBuf], root_id: &str, root: &Path) -> Vec<Value> {
    manifest
        .iter()
        .filter_map(|path| {
            let relative = path.strip_prefix(root).ok()?;
            if relative.as_os_str().is_empty() {
                return None;
            }
            Some(server_path_json(root_id, relative))
        })
        .collect()
}

/// root 相对路径 → 合同 `workspace__ServerPath`（UTF-8 直写，非 UTF-8 用
/// base64 原始字节——与 t012 编码面同语义）。
fn server_path_json(root_id: &str, relative: &Path) -> Value {
    json!({
        "authorizedRootId": root_id,
        "relativePath": encode_byte_path(relative),
    })
}

fn encode_byte_path(relative: &Path) -> Value {
    match relative.to_str() {
        Some(text) => json!({"encoding": "utf8", "value": text}),
        None => {
            #[cfg(unix)]
            {
                use base64::Engine;
                use std::os::unix::ffi::OsStrExt;
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(relative.as_os_str().as_bytes());
                json!({"encoding": "base64", "value": encoded})
            }
            #[cfg(not(unix))]
            {
                json!({"encoding": "utf8", "value": relative.to_string_lossy()})
            }
        }
    }
}

// ---------------------------------------------------------------------------
// git clone（argv spawn，无 shell；取消轮询 + kill；stderr 只用于分类，
// 不透传）。
// ---------------------------------------------------------------------------

enum CloneOutcome {
    Output(#[allow(dead_code)] Vec<u8>),
    Cancelled,
    Failure(ContractError),
}

/// `git clone [--branch <branch>] <url> <payload>`：
/// * argv 直接传递（「不把URL作为shell命令」）；
/// * `GIT_TERMINAL_PROMPT=0` 防凭据提示挂起（TS 基线 clone-progress:613
///   同款）；
/// * 取消轮询（150 ms）：`operations.cancel` 置位后 kill 子进程，本次
///   staging 由调用方回收；
/// * 300 s 网络时限 → RESULT_UNKNOWN（after_reconcile）。
async fn spawn_git_clone(
    store: &Arc<V1Store>,
    operation_id: &str,
    url: &str,
    branch: Option<&str>,
    payload: &Path,
) -> CloneOutcome {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone");
    if let Some(branch) = branch {
        cmd.arg("--branch").arg(branch);
    }
    cmd.arg(url)
        .arg(payload)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return CloneOutcome::Failure(unavailable(format!(
                "cannot spawn git for the clone: {e}"
            )));
        }
    };
    let mut stderr_pipe = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            let _ = child.start_kill();
            return CloneOutcome::Failure(unavailable("git clone stderr could not be captured"));
        }
    };
    // stderr 收集（后台任务：克隆可能长时间无输出，不能阻塞读）。
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buffer = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buffer).await;
        buffer
    });

    // 轮询结构（`try_wait` 短借用，避免与 `start_kill`/`wait` 的可变借用
    // 冲突）：每 150 ms 检查取消意图；网络时限 300 s。kill 后不再裁决
    // 限时——等待子进程随 SIGKILL 退出。
    let deadline = tokio::time::Instant::now() + CLONE_TIMEOUT;
    let mut kill_signalled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if kill_signalled {
            break child.wait().await;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stderr_task.await;
            return CloneOutcome::Failure(ContractError::new(
                CODE_RESULT_UNKNOWN,
                "git clone exceeded its time limit",
                RetrySpec::after_reconcile(),
                EffectStatus::Unknown,
            ));
        }
        tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
        if !kill_signalled && cancel_requested(store, operation_id) {
            kill_signalled = true;
            let _ = child.start_kill();
        }
    };
    let stderr = stderr_task.await.unwrap_or_default();
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            return CloneOutcome::Failure(unavailable(format!(
                "git clone could not be awaited: {e}"
            )));
        }
    };
    if kill_signalled {
        return CloneOutcome::Cancelled;
    }
    if status.success() {
        return CloneOutcome::Output(Vec::new());
    }
    CloneOutcome::Failure(classify_clone_failure(&stderr))
}

/// clone 失败分类（TS 基线 git.js:343-348,1205-1219,1273-1290 的字符串
/// 匹配语义；认证优先于不可达）。消息为自构造短语——stderr 不透传
/// （脱敏）。
fn classify_clone_failure(stderr: &[u8]) -> ContractError {
    let text = format!("{} ", String::from_utf8_lossy(stderr)).to_lowercase();
    let is_auth = text.contains("permission denied")
        || text.contains("authentication failed")
        || text.contains("could not read username")
        || text.contains("terminal prompts disabled");
    if is_auth {
        return ContractError::new(
            CODE_AUTHENTICATION_FAILED,
            "clone: authentication failed",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
    }
    let is_unreachable = text.contains("could not resolve hostname")
        || text.contains("network is unreachable")
        || text.contains("connection refused")
        || text.contains("failed to connect")
        || text.contains("does not appear to be a git repository")
        || text.contains("could not find remote repository")
        || (text.contains("repository") && text.contains("not found"))
        || (text.contains("repository") && text.contains("does not exist"));
    if is_unreachable {
        return ContractError::new(
            CODE_REMOTE_UNREACHABLE,
            "clone: unable to reach the remote repository",
            RetrySpec::backoff_read(Some(1000)),
            EffectStatus::NotApplied,
        );
    }
    ContractError::new(
        CODE_UNAVAILABLE,
        "git clone failed",
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// 事件与 recovery manifest artifact。
// ---------------------------------------------------------------------------

/// 合同事件（projects.changed / git.changed）：写入共享 events 表，
/// resource_id = operationId（`operations.events` 的消费键）。
fn emit_workspace_events(store: &V1Store, operation_id: &str, event_types: &[&str]) {
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    for event_type in event_types {
        let payload = json!({"operationId": operation_id, "type": event_type});
        let _ = append_domain_event_in_tx(
            &conn,
            &store.meta().instance_id,
            operation_id,
            "",
            event_type,
            &payload,
            &now_rfc3339(),
        );
    }
}

/// 成功路径把本次新建路径清单发布为 recovery artifact（t013 先例：审计
/// /故障恢复面）。归属记录为 operationId（登记完成前尚无 projectId）。
fn publish_manifest_artifact(
    store: &V1Store,
    job: &PrepareJob,
    manifest: &[PathBuf],
) -> Option<(String, String)> {
    let created: Vec<String> = manifest.iter().map(|p| p.display().to_string()).collect();
    let body = json!({
        "kind": "workspace-prepare-manifest",
        "operationId": job.operation_id,
        "mode": job.mode,
        "createdPaths": created,
    });
    let bytes = serde_json::to_vec(&body).ok()?;
    let artifact = crate::public_v1::domains::artifacts::publish_artifact(
        store,
        &job.operation_id,
        "application/json",
        Some(&format!("recovery-manifest:{}", job.operation_id)),
        &bytes,
        "recovery",
    )
    .ok()?;
    let artifact_id = artifact["artifactId"].as_str()?.to_owned();
    Some((artifact_id, MANIFEST_SCHEMA_ID.to_owned()))
}

// ---------------------------------------------------------------------------
// Tests（合同正反例；dispatch 端到端 + 后台执行的确定性子路径）。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use std::time::Instant;

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
        let store = V1Store::open_and_recover(root_dir, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    fn fresh_home(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, String) {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&home);
        // default 授权根（home/workspaces）在首次 decode 时惰性注册。
        (tmp, rt, iid)
    }

    fn server_path(root_id: &str, relative: &str) -> Value {
        json!({
            "authorizedRootId": root_id,
            "relativePath": {"encoding": "utf8", "value": relative},
        })
    }

    fn contract_code(error: &crate::public_v1::runtime::DispatchError) -> String {
        match error {
            crate::public_v1::runtime::DispatchError::Application(e) => e.code.clone(),
            crate::public_v1::runtime::DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    fn application_error(error: &crate::public_v1::runtime::DispatchError) -> ContractError {
        match error {
            crate::public_v1::runtime::DispatchError::Application(e) => e.clone(),
            other => panic!("expected an application error, got {other:?}"),
        }
    }

    /// 轮询 operations.get 到终态（后台任务驱动）。
    async fn await_terminal_operation(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        operation_id: &str,
    ) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let Ok((_, value)) = rt
                .dispatch(
                    "operations.get",
                    json!({"instanceId": iid, "operationId": operation_id}),
                    None,
                    None,
                )
                .await
            else {
                panic!("operations.get failed for `{operation_id}`");
            };
            let terminal = value["operation"]["state"].as_str().is_some_and(|state| {
                matches!(
                    state,
                    "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
                )
            });
            if terminal {
                return value["operation"].clone();
            }
            if Instant::now() > deadline {
                panic!("operation `{operation_id}` never reached a terminal state");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 测试侧系统 git（与 gateway spawn 同样的隔离配置）。
    fn git_sync(cwd: &Path, args: &[&str]) {
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

    fn git_present() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// 造一个可克隆的本地 bare 仓库（file:// 源，确定性无网络）。
    fn seed_bare_repo(parent: &Path, name: &str) -> std::path::PathBuf {
        let work = parent.join(format!("{name}-work"));
        std::fs::create_dir_all(&work).unwrap();
        git_sync(&work, &["init", "-q", "-b", "main"]);
        git_sync(&work, &["config", "commit.gpgsign", "false"]);
        git_sync(&work, &["config", "user.email", "seed@example.invalid"]);
        git_sync(&work, &["config", "user.name", "Seed"]);
        std::fs::write(work.join("README.md"), "seeded\n").unwrap();
        git_sync(&work, &["add", "."]);
        git_sync(&work, &["commit", "-q", "-m", "seed"]);
        let bare = parent.join(format!("{name}.git"));
        git_sync(
            &work,
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );
        bare
    }

    fn workspaces_root(home: &Path) -> PathBuf {
        home.join("workspaces")
    }

    #[tokio::test]
    async fn existing_registers_without_creating_or_cloning() {
        let (tmp, rt, iid) = fresh_home("ws-existing");
        let home = tmp.0.join("pd-home");
        let target = workspaces_root(&home).join("already-there");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("marker.txt"), b"existing content").unwrap();

        let (outcome, receipt) = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "existing",
                    "destination": server_path("default", "already-there"),
                    "displayName": "Existing WS",
                }),
                Some("ws-key-existing-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result(METHOD_ID, &receipt)
            .unwrap();
        assert_eq!(receipt["kind"], "operation");
        assert_eq!(receipt["deduplicated"], false);
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        assert_eq!(completion["mode"], "existing");
        assert_eq!(
            completion["createdPaths"],
            json!([]),
            "existing 模式零克隆零创建"
        );
        let project_id = completion["projectId"].as_str().unwrap().to_owned();
        assert_eq!(
            completion["serverPath"]["relativePath"]["value"],
            "already-there"
        );

        // 登记可见；既有文件原样。
        let (_, project) = rt
            .dispatch(
                "project.get",
                json!({"instanceId": iid, "projectId": project_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(project["project"]["name"], "Existing WS");
        assert_eq!(
            project["project"]["root"]["rootId"], "default",
            "登记走 rootId=authorizedRootId"
        );
        assert_eq!(
            std::fs::read(target.join("marker.txt")).unwrap(),
            b"existing content"
        );

        // events：operation 生命周期事件 + projects.changed（无 git.changed）。
        let (_, events) = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": operation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let types: Vec<&str> = events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        assert!(types.contains(&"operation.queued"));
        assert!(types.contains(&"projects.changed"));
        assert!(!types.contains(&"git.changed"));
    }

    #[tokio::test]
    async fn new_creates_the_chain_and_deduplicates_by_key() {
        let (tmp, rt, iid) = fresh_home("ws-new");
        let home = tmp.0.join("pd-home");
        let input = json!({
            "instanceId": iid,
            "mode": "new",
            "destination": server_path("default", "team/deep/workspace-a"),
        });

        let (outcome, receipt) = rt
            .dispatch(METHOD_ID, input.clone(), Some("ws-key-new-1"), None)
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result(METHOD_ID, &receipt)
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        let created = completion["createdPaths"].as_array().unwrap().clone();
        assert_eq!(
            created.len(),
            3,
            "team/deep/workspace-a 三级都新建: {created:?}"
        );
        assert_eq!(created[0]["relativePath"]["value"], "team");
        assert_eq!(created[2]["relativePath"]["value"], "team/deep/workspace-a");
        assert!(
            workspaces_root(&home)
                .join("team/deep/workspace-a")
                .is_dir()
        );

        // 同 key 同输入：重放，不重复创建（deduplicated=true，state 实况）。
        let (_, replay) = rt
            .dispatch(METHOD_ID, input.clone(), Some("ws-key-new-1"), None)
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"], operation_id);
        assert_eq!(replay["state"], "succeeded");

        // 同 key 不同输入：冲突。
        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "new",
                    "destination": server_path("default", "other"),
                }),
                Some("ws-key-new-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "IDEMPOTENCY_CONFLICT");

        // requests.lookup 恢复受理（回包丢失场景）。
        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({
                    "instanceId": iid,
                    "methodId": METHOD_ID,
                    "idempotencyKey": "ws-key-new-1",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup["requestState"], "accepted");
        assert_eq!(lookup["resource"]["operationId"], operation_id);
    }

    #[tokio::test]
    async fn new_refuses_nonempty_or_file_destinations() {
        let (tmp, rt, iid) = fresh_home("ws-new-refuse");
        let home = tmp.0.join("pd-home");
        let root = workspaces_root(&home);
        std::fs::create_dir_all(root.join("occupied")).unwrap();
        std::fs::write(root.join("occupied").join("data.txt"), b"keep me").unwrap();
        std::fs::write(root.join("plain-file"), b"file").unwrap();

        for relative in ["occupied", "plain-file"] {
            let error = rt
                .dispatch(
                    METHOD_ID,
                    json!({
                        "instanceId": iid,
                        "mode": "new",
                        "destination": server_path("default", relative),
                    }),
                    Some(format!("ws-key-refuse-{relative}").leak()),
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(
                contract_code(&error),
                "ALREADY_EXISTS",
                "target `{relative}` must be refused"
            );
        }
        // 既有内容零触碰。
        assert_eq!(
            std::fs::read(root.join("occupied").join("data.txt")).unwrap(),
            b"keep me"
        );
        // 预检拒绝记录为终局：同 key 重试得到同一拒绝。
        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "new",
                    "destination": server_path("default", "occupied"),
                }),
                Some("ws-key-refuse-occupied"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ALREADY_EXISTS");
    }

    #[tokio::test]
    async fn existing_requires_a_real_directory() {
        let (tmp, rt, iid) = fresh_home("ws-existing-refuse");
        let home = tmp.0.join("pd-home");
        let root = workspaces_root(&home);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a-file"), b"x").unwrap();

        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "existing",
                    "destination": server_path("default", "missing-dir"),
                }),
                Some("ws-key-missing"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");

        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "existing",
                    "destination": server_path("default", "a-file"),
                }),
                Some("ws-key-file"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_A_DIRECTORY");

        // root 本身 / 越界路径 / 未知 root。
        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "existing",
                    "destination": server_path("default", ""),
                }),
                Some("ws-key-root"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "INVALID_ARGUMENT");

        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "new",
                    "destination": server_path("default", "../escape"),
                }),
                Some("ws-key-escape"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "PATH_ESCAPE");

        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "new",
                    "destination": server_path("nope", "x"),
                }),
                Some("ws-key-root403"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "FORBIDDEN");
    }

    #[tokio::test]
    async fn clone_from_a_local_repository_registers_and_emits_git_changed() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (tmp, rt, iid) = fresh_home("ws-clone-ok");
        let home = tmp.0.join("pd-home");
        let bare = seed_bare_repo(&tmp.0, "seed");
        let url = format!("file://{}", bare.display());

        let (outcome, receipt) = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "cloned/project-x"),
                    "repositoryUrl": url,
                    "branch": "main",
                }),
                Some("ws-key-clone-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result(METHOD_ID, &receipt)
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = &operation["result"];
        assert_eq!(completion["mode"], "clone");
        // createdPaths 浅→深：父目录 `cloned` 在前，交付物
        // `cloned/project-x` 在后（与 new 模式三级链的 [0]=="team" 同序）。
        assert_eq!(
            completion["createdPaths"][0]["relativePath"]["value"],
            "cloned"
        );
        assert_eq!(
            completion["createdPaths"][1]["relativePath"]["value"],
            "cloned/project-x"
        );
        assert!(
            completion["repositoryId"]
                .as_str()
                .is_some_and(|id| id.contains("project-x")),
            "repositoryId 是 canonical toplevel: {completion}"
        );
        // 克隆内容就位；staging 清理。
        let destination = workspaces_root(&home).join("cloned/project-x");
        assert!(destination.join("README.md").is_file());
        let staging: Vec<_> = std::fs::read_dir(workspaces_root(&home))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".pd-staging-"))
            .collect();
        assert!(staging.is_empty(), "staging 应已回收: {staging:?}");

        // git.changed + projects.changed。
        let (_, events) = rt
            .dispatch(
                "operations.events",
                json!({"instanceId": iid, "operationId": operation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let types: Vec<&str> = events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        assert!(types.contains(&"projects.changed"));
        assert!(types.contains(&"git.changed"));

        // 重复目录（非空目标）→ 拒绝；既有克隆零触碰。
        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "cloned/project-x"),
                    "repositoryUrl": url,
                }),
                Some("ws-key-clone-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ALREADY_EXISTS");
        assert!(destination.join("README.md").is_file());
    }

    #[tokio::test]
    async fn clone_into_an_existing_empty_directory_moves_entries() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (tmp, rt, iid) = fresh_home("ws-clone-empty");
        let home = tmp.0.join("pd-home");
        let bare = seed_bare_repo(&tmp.0, "seed2");
        let destination = workspaces_root(&home).join("holder");
        std::fs::create_dir_all(&destination).unwrap();

        let (_, receipt) = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "holder"),
                    "repositoryUrl": format!("file://{}", bare.display()),
                }),
                Some("ws-key-clone-empty-1"),
                None,
            )
            .await
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        // 空目录本身保留（非本次创建），条目移入。
        assert!(destination.is_dir());
        assert!(destination.join("README.md").is_file());
        let completion = &operation["result"];
        let created = completion["createdPaths"].as_array().unwrap();
        assert!(
            created
                .iter()
                .all(|p| p["relativePath"]["value"].as_str() != Some("holder")),
            "既有空目录不进 createdPaths: {created:?}"
        );
    }

    #[tokio::test]
    async fn clone_refuses_bad_urls_embedded_credentials_and_credential_refs() {
        let (_tmp, rt, iid) = fresh_home("ws-clone-url");

        let cases: Vec<(&str, String, &str)> = vec![
            (
                "no scheme",
                "github.com/org/repo".to_owned(),
                "INVALID_REPOSITORY_URL",
            ),
            (
                "scp-like",
                "git@github.com:org/repo.git".to_owned(),
                "INVALID_REPOSITORY_URL",
            ),
            (
                "plaintext http",
                "http://example.invalid/repo".to_owned(),
                "INVALID_REPOSITORY_URL",
            ),
            (
                "unknown scheme",
                "svn://example.invalid/repo".to_owned(),
                "INVALID_REPOSITORY_URL",
            ),
            (
                "embedded userinfo",
                "https://user:secret@example.invalid/repo".to_owned(),
                "INVALID_REPOSITORY_URL",
            ),
            (
                "no host",
                "https:///repo".to_owned(),
                "INVALID_REPOSITORY_URL",
            ),
            (
                "relative file",
                "file://relative/path".to_owned(),
                "INVALID_REPOSITORY_URL",
            ),
        ];
        for (idx, (label, url, expected)) in cases.iter().enumerate() {
            let error = rt
                .dispatch(
                    METHOD_ID,
                    json!({
                        "instanceId": iid,
                        "mode": "clone",
                        "destination": server_path("default", &format!("u{idx}")),
                        "repositoryUrl": url,
                    }),
                    Some(format!("ws-key-url-{idx}").leak()),
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code(&error), *expected, "case `{label}` ({url})");
            // 错误消息脱敏：不回显 URL 文本（含机密的形态在源头已拒）。
            let message = application_error(&error).message;
            assert!(
                !message.contains("secret") && !message.contains("example.invalid"),
                "message must not echo the URL: {message}"
            );
        }

        // credentialRef：引用形态，注入未接线 → 明确拒绝（与 git 域 R3 一致）。
        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "credref"),
                    "repositoryUrl": "https://example.invalid/org/repo".to_owned(),
                    "credentialRef": "cred-1",
                }),
                Some("ws-key-credref"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "UNAVAILABLE");

        // 非法 branch。
        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "badbranch"),
                    "repositoryUrl": "https://example.invalid/org/repo".to_owned(),
                    "branch": "- injected",
                }),
                Some("ws-key-branch"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn clone_failure_cleans_up_and_registers_nothing() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (tmp, rt, iid) = fresh_home("ws-clone-fail");
        let home = tmp.0.join("pd-home");
        let root = workspaces_root(&home);
        // 既有文件（取消/失败不得触碰）。
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine").join("keep.txt"), b"keep").unwrap();

        let (_, receipt) = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "doomed"),
                    "repositoryUrl": "file:///nonexistent/source.git".to_owned(),
                }),
                Some("ws-key-clone-fail-1"),
                None,
            )
            .await
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "failed");
        assert_eq!(operation["error"]["code"], "REMOTE_UNREACHABLE");

        // 失败不登记半项目；destination 未创建；既有文件原样。
        assert!(!root.join("doomed").exists(), "目标不应残留");
        let staging: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".pd-staging-"))
            .collect();
        assert!(staging.is_empty(), "staging 应回收: {staging:?}");
        assert_eq!(
            std::fs::read(root.join("mine").join("keep.txt")).unwrap(),
            b"keep"
        );
        let (_, projects) = rt
            .dispatch("project.list", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        assert_eq!(
            projects["items"].as_array().map(Vec::len),
            Some(0),
            "失败不登记半项目: {projects}"
        );
    }

    #[tokio::test]
    async fn registration_failure_keeps_the_clone_and_reports_residual() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (tmp, rt, iid) = fresh_home("ws-residual");
        let home = tmp.0.join("pd-home");
        let bare = seed_bare_repo(&tmp.0, "seed3");
        let destination = workspaces_root(&home).join("residual-x");

        // 故障注入：让登记服务 INSERT 失败（确定性构造「克隆完登记失败」）。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute_batch(
                "CREATE TRIGGER fail_project_insert BEFORE INSERT ON projects
                 BEGIN SELECT RAISE(ABORT, 'injected registration failure'); END;",
            )
            .unwrap();
        }

        let (_, receipt) = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "residual-x"),
                    "repositoryUrl": format!("file://{}", bare.display()),
                }),
                Some("ws-key-residual-1"),
                None,
            )
            .await
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "failed");
        let message = operation["error"]["message"].as_str().unwrap().to_owned();
        assert!(
            message.contains("residual"),
            "错误消息要报告残留: {message}"
        );
        // 克隆结果保留（不清理），内容完整。
        assert!(destination.join("README.md").is_file(), "克隆结果应保留");
        // 不登记半项目。
        let (_, projects) = rt
            .dispatch("project.list", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        assert_eq!(projects["items"].as_array().map(Vec::len), Some(0));

        // 同 destination 再克隆：非空 → 拒绝（不能覆盖残留）。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute_batch("DROP TRIGGER fail_project_insert")
                .unwrap();
        }
        let error = rt
            .dispatch(
                METHOD_ID,
                json!({
                    "instanceId": iid,
                    "mode": "clone",
                    "destination": server_path("default", "residual-x"),
                    "repositoryUrl": format!("file://{}", bare.display()),
                }),
                Some("ws-key-residual-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ALREADY_EXISTS");
    }

    #[tokio::test]
    async fn cancelled_clone_reclaims_only_its_own_paths() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (tmp, rt, iid) = fresh_home("ws-cancel");
        let home = tmp.0.join("pd-home");
        let root = workspaces_root(&home);
        let bare = seed_bare_repo(&tmp.0, "seed4");
        // 既有内容（取消不得触碰）。
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine").join("keep.txt"), b"keep").unwrap();

        // 确定性子路径：受理登记 + 显式取消（真 seam）后直接驱动执行函数
        // ——绕开 spawn 的时序，验证「取消按清单回收、边界外零触碰」。
        let rooted =
            decode_server_path(rt.store(), &server_path("default", "cancelled-x"), false).unwrap();
        let (operation_id, _rev) = rt
            .store()
            .begin_long_operation(METHOD_ID, &json!({"kind": "instance"}), true)
            .unwrap();
        rt.dispatch(
            "operations.cancel",
            json!({"instanceId": iid, "operationId": operation_id}),
            Some("ws-cancel-key-1"),
            None,
        )
        .await
        .unwrap();

        let job = PrepareJob {
            operation_id: operation_id.clone(),
            mode: "clone".to_owned(),
            root_id: rooted.root_id.clone(),
            root: rooted.root.clone(),
            relative: rooted.relative.clone(),
            relative_str: "cancelled-x".to_owned(),
            repository_url: Some(format!("file://{}", bare.display())),
            branch: None,
            name: "Cancelled".to_owned(),
            idempotency_key: None,
        };
        execute_prepare(rt.store().clone(), job).await;

        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "cancelled");
        // 本次目标与 staging 全部回收；既有内容原样。
        assert!(!root.join("cancelled-x").exists(), "目标应回收");
        let staging: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".pd-staging-"))
            .collect();
        assert!(staging.is_empty(), "staging 应回收: {staging:?}");
        assert_eq!(
            std::fs::read(root.join("mine").join("keep.txt")).unwrap(),
            b"keep"
        );
    }

    #[tokio::test]
    async fn cancelled_new_reclaims_only_the_created_chain() {
        let (tmp, rt, iid) = fresh_home("ws-cancel-new");
        let home = tmp.0.join("pd-home");
        let root = workspaces_root(&home);
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine").join("keep.txt"), b"keep").unwrap();

        let rooted = decode_server_path(
            rt.store(),
            &server_path("default", "chain/deep/cancelled-new"),
            false,
        )
        .unwrap();
        let (operation_id, _rev) = rt
            .store()
            .begin_long_operation(METHOD_ID, &json!({"kind": "instance"}), true)
            .unwrap();
        rt.dispatch(
            "operations.cancel",
            json!({"instanceId": iid, "operationId": operation_id}),
            Some("ws-cancel-new-key-1"),
            None,
        )
        .await
        .unwrap();

        let job = PrepareJob {
            operation_id: operation_id.clone(),
            mode: "new".to_owned(),
            root_id: rooted.root_id.clone(),
            root: rooted.root.clone(),
            relative: rooted.relative.clone(),
            relative_str: "chain/deep/cancelled-new".to_owned(),
            repository_url: None,
            branch: None,
            name: "CancelledNew".to_owned(),
            idempotency_key: None,
        };
        execute_prepare(rt.store().clone(), job).await;

        let operation = await_terminal_operation(&rt, &iid, &operation_id).await;
        assert_eq!(operation["state"], "cancelled");
        assert!(!root.join("chain").exists(), "创建链应整体回收");
        assert_eq!(
            std::fs::read(root.join("mine").join("keep.txt")).unwrap(),
            b"keep",
            "清单外的既有内容零触碰"
        );
    }

    #[test]
    fn url_validation_is_strict_but_silent() {
        for good in [
            "https://github.com/org/repo.git",
            "git://example.invalid/org/repo",
            "ssh://git@example.invalid/org/repo.git",
            "file:///srv/git/repo.git",
        ] {
            validate_repository_url(good)
                .unwrap_or_else(|e| panic!("`{good}` must be accepted: {e:?}"));
        }
        // 错误消息不回显 URL（脱敏：URL 文本只可能是攻击者可控输入）。
        let err = validate_repository_url("https://user:pass@example.invalid/repo").unwrap_err();
        assert!(!err.message.contains("example.invalid"), "{}", err.message);
        assert!(!err.message.contains("pass"), "{}", err.message);
    }
}
