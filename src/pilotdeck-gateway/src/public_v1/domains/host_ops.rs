//! `host.*` — the generic module-host capability surface
//! (`cli.task.runtime`, S3; capability `cli.capability.public-host-capabilities`).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `host.describe` /
//! `host.prepare_model` / `host.stream_model` 条目。TS 基线（cc7c4e6）的语义
//! 参考是 `src/composition/nativeHostCapabilityProvider.ts`（56-64, 122-134,
//! 227-266）与 `src/composition/publicHostHttpTransport.ts`（9-56）——v1 把
//! 该 HTTP 面重设计为 `/v1/ws` 公开操作，不搬 StaffDeck HTTP 桥（任务简报明
//! 确「不复用StaffDeck HTTP桥接」）。
//!
//! 能力面（`host.describe`）：`cli.capability.public-host-capabilities` 的 8
//! 个操作——model.list / tools.list / tools.test / skills.list /
//! skills.apply_import / host.describe / host.prepare_model /
//! host.stream_model。可用性逐项来自本构建的真实可调用性：
//! * model.list（models 域）、tools.list / tools.test（tools 域）、
//!   host.describe（本域）：已注册 → available。
//! * skills.list / skills.apply_import：归 extensions 任务，本构建未装配
//!   → `available=false` + `CAPABILITY_UNAVAILABLE`，并进 `declaredGaps`
//!   （`OWNER_NOT_ASSEMBLED`）——「GAPS无owner必须明确Unavailable，不能被广
//!   告为ready」。
//! * host.prepare_model / host.stream_model：已实现但**受理门关闭**。合同
//!   （permission/preconditions）：仅「已实际装配且被授权的
//!   runtimeTool/runtimeExtension 内部消费」，须有「后端签发且未过期的
//!   消费者授权（grantRef）」，且「未具备该授权机制时不广告此能力」。v1
//!   的 grant 签发机制尚无任何公开路径（签发属运行时消费者装配，后续任
//!   务），因此本域报告 `available=false` +
//!   `reasonCode=CAPABILITY_GRANT_REQUIRED`，与真实调用（无有效 grant 一律
//!   `CAPABILITY_GRANT_REQUIRED`）逐项一致。
//!
//! `host.prepare_model`（sync，`retry=idempotentSubmit`）：验证 grant 绑定
//! （consumerKind / parentRunId / parentInvocationId / scope / 模型 / 有效
//! 期）→ 父 run 未终态 → 模型真实存在且启用（「校验真实model owner」）→
//! 请求按固定版本 schema（`agentcpm-pd.model.request/1`）校验 → 预算在
//! grant 剩余额度内**原子预留**（「不得通过多个prepare/stream重复消费同一剩
//! 余额度」「预算超限在provider调用前拒绝」）→ 落库 prepared request。
//! 「准备不发起模型生成」：本操作零模型调用、零网络。
//!
//! `host.stream_model`（stream）：`agentcpm-pd.model.stream/1` 事件日志（共
//! 享 `events` 表，resource_id = invocation 的 operationId）+ 通用订阅泵的
//! `StreamSource`（terminals_stream.rs 同款）。每个 prepared request 恰一
//! 个 invocation：同 envelope.idempotencyKey 重连只重放有序事件（受理去重
//! + cursor 续读），不同 key 再流也挂到同一 invocation——绝不产生第二次模
//!   型付费请求（「不得通过多个prepare/stream重复消费」）。执行桥：v1 未装
//!   配 host 侧模型调用桥（D20 的 TurnSubmitter 是 run 路径，不经本面），驱动
//!   如实终态化为结构化 `model.failed`（`MODEL_UNAVAILABLE`），不以空成功伪
//!   装——与 git.rs 的 `invoke_suggestion_model` 同一诚实口径。取消走
//!   `operations.cancel`（invocation 即长操作记录）。
//!
//! 越权锚（「普通CLI主体即使知道operationId也不能绕过父run权限/审批」）：
//! grant 表服务端独占写（无公开签发路径），伪造 grantRef / 跨 run 复用 /
//! scope 不匹配 / 过期 → `CAPABILITY_GRANT_REQUIRED`、`PERMISSION_DENIED`
//! 或 `CAPABILITY_GRANT_EXPIRED`，全部在受理面拒绝、零副作用。

use std::sync::Arc;
use std::time::Duration;

use agentcpm_pd_protocol::contract::{EventEnvelope, ResponseOutcome};
use agentcpm_pd_protocol::errors::{
    CODE_FORBIDDEN, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::schema_validator::validate_against_schema;
use agentcpm_pd_protocol::wire::{
    JSONRPC_VERSION, JsonRpcNotification, SubEventNotification, WIRE_SUB_EVENT,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::public_v1::PublicV1State;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::operations_ops::LongOperationOutcome;
use crate::public_v1::domains::runtimes::{
    RuntimeScope, parse_runtime_scope, record_acceptance, record_rejection, replay_accepted_receipt,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, format_cursor, now_rfc3339, parse_cursor};
use crate::public_v1::ws::{StreamFetch, StreamFrame, StreamSource};

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for yet.
const CODE_SCHEMA_MISMATCH: &str = "SCHEMA_MISMATCH";
const CODE_CAPABILITY_GRANT_REQUIRED: &str = "CAPABILITY_GRANT_REQUIRED";
const CODE_CAPABILITY_GRANT_EXPIRED: &str = "CAPABILITY_GRANT_EXPIRED";
const CODE_BUDGET_EXCEEDED: &str = "BUDGET_EXCEEDED";
const CODE_PARENT_RUN_TERMINAL: &str = "PARENT_RUN_TERMINAL";
const CODE_PREPARED_REQUEST_EXPIRED: &str = "PREPARED_REQUEST_EXPIRED";
const CODE_INTEGRITY_ERROR: &str = "INTEGRITY_ERROR";
const CODE_MODEL_UNAVAILABLE: &str = "MODEL_UNAVAILABLE";
const CODE_DEADLINE_EXCEEDED: &str = "DEADLINE_EXCEEDED";

/// The uniform fixed-version request schema a host model call accepts.
const MODEL_REQUEST_SCHEMA_ID: &str = "agentcpm-pd.model.request/1";
/// The uniform fixed-version schema of every stream event's `data`.
const MODEL_STREAM_SCHEMA_ID: &str = "agentcpm-pd.model.stream/1";
const MODEL_REQUEST_SCHEMA: &str = r#"{"type":"object","required":["messages"],"additionalProperties":false,"properties":{"messages":{"type":"array","minItems":1,"items":{"type":"object","required":["role","content"],"additionalProperties":false,"properties":{"role":{"type":"string","enum":["system","user","assistant","tool"]},"content":{"type":"string"}}}}}}"#;
/// How long a prepared request stays usable past its own budget timeout
/// (the stream may start slightly after preparation completed).
const PREPARE_EXPIRY_GRACE_SECS: i64 = 30;
/// The bounded wait of the unary (non-follow) stream_model read for the next
/// event after `afterCursor`.
const UNARY_EVENT_WAIT: Duration = Duration::from_secs(10);
/// The event types the invocation journal terminalizes with.
const MODEL_TERMINAL_EVENT_TYPES: [&str; 3] =
    ["model.completed", "model.failed", "model.cancelled"];

/// The public-host-capabilities operation set with its designated owners.
/// Cross-task merged exposure (task brief): model.list lives in the models
/// domain, tools.list/tools.test in the tools domain, skills.* belong to the
/// extensions task — this projection reports their true callability per
/// build, it does not re-implement them.
const HOST_OPERATIONS: [(&str, &str); 8] = [
    ("model.list", "models"),
    ("tools.list", "tools"),
    ("tools.test", "tools"),
    ("skills.list", "extensions"),
    ("skills.apply_import", "extensions"),
    ("host.describe", "host"),
    ("host.prepare_model", "host"),
    ("host.stream_model", "host"),
];

const DDL_V1_HOST_GRANTS: &str = "CREATE TABLE IF NOT EXISTS v1_host_grants (
    grant_ref TEXT PRIMARY KEY,
    consumer_kind TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    parent_run_id TEXT NOT NULL,
    parent_invocation_id TEXT NOT NULL,
    model_id TEXT,
    max_input_tokens INTEGER NOT NULL,
    max_output_tokens INTEGER NOT NULL,
    timeout_ms INTEGER NOT NULL,
    consumed_input_tokens INTEGER NOT NULL DEFAULT 0,
    consumed_output_tokens INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL
)";
const DDL_V1_HOST_PREPARED_REQUESTS: &str = "CREATE TABLE IF NOT EXISTS v1_host_prepared_requests (
    prepared_request_id TEXT PRIMARY KEY,
    model_id TEXT NOT NULL,
    schema_id TEXT NOT NULL,
    request_json TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    budget_json TEXT NOT NULL,
    grant_ref TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    session_id TEXT NOT NULL DEFAULT '',
    expires_at TEXT NOT NULL,
    invocation_id TEXT,
    created_at TEXT NOT NULL
)";
const DDL_V1_HOST_MODEL_INVOCATIONS: &str = "CREATE TABLE IF NOT EXISTS v1_host_model_invocations (
    invocation_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    prepared_request_id TEXT NOT NULL UNIQUE,
    model_id TEXT NOT NULL,
    grant_ref TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    session_id TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    terminal INTEGER NOT NULL DEFAULT 0,
    completion_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_host_grants", DDL_V1_HOST_GRANTS);
    reg.schema("v1_host_prepared_requests", DDL_V1_HOST_PREPARED_REQUESTS);
    reg.schema("v1_host_model_invocations", DDL_V1_HOST_MODEL_INVOCATIONS);
    reg.handler("host.describe", |rt, input, _key, _rev| {
        Box::pin(host_describe(rt, input))
    });
    reg.handler("host.prepare_model", |rt, input, key, _rev| {
        Box::pin(host_prepare_model(rt, input, key))
    });
    // The dispatch handler serves the unary (non-follow) read and the schema
    // path; the follow stream is the `StreamSource` registered right after.
    reg.handler("host.stream_model", |rt, input, _key, _rev| {
        Box::pin(host_stream_model_unary(rt, input))
    });
    crate::public_v1::ws::register_stream_source("host.stream_model", host_stream_model_source);
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn host_invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn grant_required(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_CAPABILITY_GRANT_REQUIRED,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn grant_expired(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_CAPABILITY_GRANT_EXPIRED,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

// ---------------------------------------------------------------------------
// host.describe
// ---------------------------------------------------------------------------

/// A digest-pinned, version-fixed schema id: the registry's own digest of the
/// operation's embedded input/result schema (the same source `schema.get`
/// serves), so the advertised id is verifiable against the real schema.
fn schema_id_for(op_id: &str, input: bool) -> String {
    let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();
    let digest = reg
        .get(op_id)
        .map(|spec| {
            if input {
                spec.input_schema_digest.as_str()
            } else {
                spec.result_schema_digest.as_str()
            }
        })
        .unwrap_or("unknown");
    let side = if input { "input" } else { "result" };
    let prefix = &digest[..12.min(digest.len())];
    format!("agentcpm-pd.operation.{op_id}/{side}@{prefix}")
}

async fn host_describe(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    parse_runtime_scope(&rt, &input)?;
    let domains = DomainRegistry::global();

    let mut operations = Vec::with_capacity(HOST_OPERATIONS.len());
    let mut declared_gaps = Vec::new();
    for (op_id, owner) in HOST_OPERATIONS {
        let registered = domains.is_registered(op_id);
        // The two grant-gated entry points: implemented, but the grant
        // issuance mechanism is not assembled in v1, so they are closed for
        // every public caller — advertised exactly as unreachable.
        let grant_gated = op_id == "host.prepare_model" || op_id == "host.stream_model";
        let (available, reason_code) = if grant_gated {
            (false, json!(CODE_CAPABILITY_GRANT_REQUIRED))
        } else if registered {
            (true, Value::Null)
        } else {
            (false, json!("CAPABILITY_UNAVAILABLE"))
        };
        if !registered {
            declared_gaps.push(json!({
                "operationId": op_id,
                "reasonCode": "OWNER_NOT_ASSEMBLED",
            }));
        }
        operations.push(json!({
            "operationId": op_id,
            "ownerId": owner,
            "inputSchemaId": schema_id_for(op_id, true),
            "resultSchemaId": schema_id_for(op_id, false),
            "available": available,
            "reasonCode": reason_code,
        }));
    }

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "operations": operations,
            "declaredGaps": declared_gaps,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Grant verification
// ---------------------------------------------------------------------------

struct VerifiedGrant {
    grant_ref: String,
    scope_key: String,
    parent_run_id: String,
}

/// Verifies the execution context's grant binding against the server-side
/// grant table: existence, expiry, consumer kind, parent run/invocation,
/// scope, and (when the grant binds a model) the requested model. Every
/// mismatch is a definitive pre-side-effect rejection. Caller must not hold
/// the store mutex (this helper takes no other store locks).
fn verify_grant(
    conn: &Connection,
    input: &Value,
    scope: &RuntimeScope,
) -> Result<VerifiedGrant, ContractError> {
    let ctx = input
        .get("executionContext")
        .and_then(Value::as_object)
        .ok_or_else(|| host_invalid("executionContext is required"))?;
    let grant_ref = ctx
        .get("grantRef")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if grant_ref.is_empty() {
        return Err(grant_required("executionContext.grantRef is required"));
    }
    let row: Option<(String, String, String, String, Option<String>, String)> = conn
        .query_row(
            "SELECT consumer_kind, scope_key, parent_run_id, parent_invocation_id, model_id,
                expires_at
             FROM v1_host_grants WHERE grant_ref = ?1",
            params![grant_ref],
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
    let Some((
        consumer_kind,
        grant_scope_key,
        parent_run_id,
        parent_invocation_id,
        grant_model_id,
        expires_at,
    )) = row
    else {
        return Err(grant_required(format!(
            "grantRef `{grant_ref}` is not a grant this backend issued"
        )));
    };
    if expires_at <= now_rfc3339() {
        return Err(grant_expired(format!(
            "grantRef `{grant_ref}` expired at {expires_at}"
        )));
    }
    let input_consumer = ctx
        .get("consumerKind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if consumer_kind != input_consumer {
        return Err(grant_required(format!(
            "grant `{grant_ref}` is bound to consumerKind `{consumer_kind}`, not `{input_consumer}`"
        )));
    }
    let input_run = ctx
        .get("parentRunId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if parent_run_id != input_run {
        return Err(grant_required(format!(
            "grant `{grant_ref}` is bound to parent run `{parent_run_id}`, not `{input_run}` (cross-run grant reuse is rejected)"
        )));
    }
    let input_invocation = ctx
        .get("parentInvocationId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if parent_invocation_id != input_invocation {
        return Err(grant_required(format!(
            "grant `{grant_ref}` is bound to parent invocation `{parent_invocation_id}`, not `{input_invocation}`"
        )));
    }
    let scope_key = scope.key();
    if grant_scope_key != scope_key {
        return Err(ContractError::new(
            CODE_FORBIDDEN,
            format!(
                "grant `{grant_ref}` is not valid for the requested scope (grant scope `{grant_scope_key}`)"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if let Some(grant_model) = grant_model_id
        && grant_model != input["modelId"].as_str().unwrap_or_default()
    {
        return Err(grant_required(format!(
            "grant `{grant_ref}` is bound to model `{grant_model}`"
        )));
    }
    Ok(VerifiedGrant {
        grant_ref: grant_ref.to_owned(),
        scope_key,
        parent_run_id,
    })
}

/// The parent run must exist and be live: a terminal or cancelling parent
/// refuses new preparations (「父run取消后发起均拒绝」).
fn check_parent_run_live(
    rt: &PublicV1Runtime,
    parent_run_id: &str,
) -> Result<String, ContractError> {
    let run = rt
        .store()
        .get_run(parent_run_id)
        .map_err(|_| host_invalid(format!("parent run `{parent_run_id}` not found")))?;
    let state = format!("{:?}", run.state).to_lowercase();
    if run.terminal || state == "cancelling" {
        return Err(ContractError::new(
            CODE_PARENT_RUN_TERMINAL,
            format!(
                "parent run `{parent_run_id}` is `{state}`; host model calls must start while it is live"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(run.session_id.clone())
}

// ---------------------------------------------------------------------------
// host.prepare_model
// ---------------------------------------------------------------------------

async fn host_prepare_model(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let method_id = "host.prepare_model";

    // Idempotent intake: same key + same input replays the stored success
    // result verbatim (preparation is idempotent by its own request hash).
    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = replay_accepted_receipt(&store, method_id, &input, key)?
    {
        return Ok((ResponseOutcome::Succeeded, replay));
    }

    match prepare_model_inner(&rt, &input, &store) {
        Ok(result) => {
            if let Some(key) = idempotency_key.as_deref() {
                record_acceptance(&store, key, method_id, &input, &result)?;
            }
            Ok((ResponseOutcome::Succeeded, result))
        }
        Err(err) => {
            if let Some(key) = idempotency_key.as_deref() {
                record_rejection(&store, key, method_id, &input, &err.code);
            }
            Err(err.into())
        }
    }
}

fn prepare_model_inner(
    rt: &Arc<PublicV1Runtime>,
    input: &Value,
    store: &Arc<V1Store>,
) -> Result<Value, ContractError> {
    let scope = parse_runtime_scope(rt, input)?;
    let model_id = input["modelId"].as_str().unwrap_or_default().to_owned();
    let schema_id = input["schemaId"].as_str().unwrap_or_default();
    let request = input.get("request").cloned().unwrap_or(Value::Null);

    if model_id.is_empty() {
        return Err(host_invalid("modelId is required"));
    }
    if schema_id != MODEL_REQUEST_SCHEMA_ID {
        return Err(ContractError::new(
            CODE_SCHEMA_MISMATCH,
            format!(
                "schemaId `{schema_id}` is not a registered model request schema (this build accepts `{MODEL_REQUEST_SCHEMA_ID}`)"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let parsed: Value = serde_json::from_str(MODEL_REQUEST_SCHEMA).expect("fixed schema");
    if let Err(errors) = validate_against_schema(&request, &parsed) {
        let summary = errors
            .iter()
            .map(|e| format!("{}: {}", e.path, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ContractError::new(
            CODE_SCHEMA_MISMATCH,
            format!("request does not satisfy `{MODEL_REQUEST_SCHEMA_ID}`: {summary}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let budget = input.get("budget").cloned().unwrap_or(Value::Null);
    let max_input = budget["maxInputTokens"].as_u64().unwrap_or(0) as i64;
    let max_output = budget["maxOutputTokens"].as_u64().unwrap_or(0) as i64;
    let timeout_ms = budget["timeoutMs"].as_u64().unwrap_or(0);
    let timeout_ms_sql = timeout_ms as i64;

    // Phase 1 (own lock): the grant binding.
    let grant = {
        let conn = store.conn.lock().expect("sqlite mutex");
        verify_grant(&conn, input, &scope)?
    };

    // Phase 2 (store-internal lock): the parent run must exist and be live.
    let session_id = check_parent_run_live(rt, &grant.parent_run_id)?;

    let conn = store.conn.lock().expect("sqlite mutex");

    // Phase 3 (same lock as phase 1, released above): the model must be a
    // real, enabled catalog entry. `modelId` is the `<providerId>/<modelId>`
    // pair (documented v1 convention).
    let (provider_id, catalog_model_id) = model_id.split_once('/').ok_or_else(|| {
        host_invalid(format!(
            "modelId `{model_id}` must be `<providerId>/<modelId>`"
        ))
    })?;
    let model_row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT m.enabled, p.enabled FROM v1_models m
             JOIN v1_providers p ON p.provider_id = m.provider_id
             WHERE m.provider_id = ?1 AND m.model_id = ?2",
            params![provider_id, catalog_model_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    match model_row {
        None => {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("model `{model_id}` is not in the catalog"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Some((model_enabled, provider_enabled)) if model_enabled == 0 || provider_enabled == 0 => {
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!("model `{model_id}` or its provider is disabled"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        Some(_) => {}
    }

    // Atomic budget reservation against the grant's remaining allowance
    // (「预算超限在provider调用前拒绝」「不得通过多个prepare/stream重复消
    // 费同一剩余额度」): the conditional UPDATE fails closed on races.
    let reserved = conn
        .execute(
            "UPDATE v1_host_grants
             SET consumed_input_tokens = consumed_input_tokens + ?1,
                 consumed_output_tokens = consumed_output_tokens + ?2
             WHERE grant_ref = ?3
               AND consumed_input_tokens + ?1 <= max_input_tokens
               AND consumed_output_tokens + ?2 <= max_output_tokens
               AND ?4 <= timeout_ms",
            params![max_input, max_output, grant.grant_ref, timeout_ms_sql],
        )
        .map_err(sql_err)?;
    if reserved == 0 {
        return Err(ContractError::new(
            CODE_BUDGET_EXCEEDED,
            format!(
                "the requested budget exceeds grant `{}` remaining allowance or per-call timeout",
                grant.grant_ref
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let prepared_request_id = format!("hpr-{}", store.next_id());
    let request_hash = request_hash(&model_id, MODEL_REQUEST_SCHEMA_ID, &request, &budget);
    let expires_at = (chrono::Utc::now()
        + chrono::Duration::milliseconds(timeout_ms as i64)
        + chrono::Duration::seconds(PREPARE_EXPIRY_GRACE_SECS))
    .to_rfc3339();
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO v1_host_prepared_requests (prepared_request_id, model_id, schema_id,
            request_json, request_hash, budget_json, grant_ref, scope_key, session_id,
            expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            prepared_request_id,
            model_id,
            schema_id,
            serde_json::to_string(&request).unwrap_or_default(),
            request_hash,
            serde_json::to_string(&budget).unwrap_or_default(),
            grant.grant_ref,
            grant.scope_key,
            session_id,
            expires_at,
            now
        ],
    )
    .map_err(sql_err)?;
    drop(conn);
    store.event_notifier().notify_waiters();

    Ok(json!({
        "preparedRequestId": prepared_request_id,
        "requestHash": request_hash,
        "modelId": model_id,
        "expiresAt": expires_at,
        "streamSchemaId": MODEL_STREAM_SCHEMA_ID,
        "approvedBudget": {
            "maxInputTokens": max_input,
            "maxOutputTokens": max_output,
            "timeoutMs": timeout_ms,
        },
    }))
}

/// The integrity hash binding the prepared request: model, schema, request
/// and budget (a stream presenting a different hash is refused).
fn request_hash(model_id: &str, schema_id: &str, request: &Value, budget: &Value) -> String {
    let canonical = json!({
        "modelId": model_id,
        "schemaId": schema_id,
        "request": request,
        "budget": budget,
    });
    let text = serde_json::to_string(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(b"agentcpm-pd.host.request/1");
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// host.stream_model — shared invocation machinery
// ---------------------------------------------------------------------------

struct InvocationInfo {
    invocation_id: String,
    operation_id: String,
    #[allow(dead_code)]
    terminal: bool,
    state: String,
}

/// Validates the stream request (grant, prepared request, hash, scope,
/// expiry) and finds — or atomically creates — the prepared request's single
/// invocation, spawning its driver. Every failure is a pre-side-effect
/// rejection (「输入所属scope不匹配、准备过期或hash变化拒绝」).
fn ensure_invocation(
    rt: &Arc<PublicV1Runtime>,
    store: &Arc<V1Store>,
    input: &Value,
) -> Result<InvocationInfo, ContractError> {
    let scope = parse_runtime_scope(rt, input)?;
    let prepared_request_id = input["preparedRequestId"].as_str().unwrap_or_default();
    let request_hash_value = input["requestHash"].as_str().unwrap_or_default();

    // Phase 1: grant + prepared request validation.
    let (grant_ref, stored_hash, stored_scope, expires_at, model_id, session_id) = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let grant = verify_grant(&conn, input, &scope)?;
        let row: Option<(String, String, String, String, String)> = conn
            .query_row(
                "SELECT request_hash, scope_key, expires_at, model_id, session_id
                 FROM v1_host_prepared_requests WHERE prepared_request_id = ?1",
                params![prepared_request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((stored_hash, stored_scope, expires_at, model_id, session_id)) = row else {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("prepared request `{prepared_request_id}` not found"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        (
            grant.grant_ref,
            stored_hash,
            stored_scope,
            expires_at,
            model_id,
            session_id,
        )
    };
    if expires_at <= now_rfc3339() {
        return Err(ContractError::new(
            CODE_PREPARED_REQUEST_EXPIRED,
            format!("prepared request `{prepared_request_id}` expired at {expires_at}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if stored_hash != request_hash_value {
        return Err(ContractError::new(
            CODE_INTEGRITY_ERROR,
            format!(
                "requestHash `{request_hash_value}` does not match prepared request `{prepared_request_id}`"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if stored_scope != scope.key() {
        return Err(ContractError::new(
            CODE_FORBIDDEN,
            format!("prepared request `{prepared_request_id}` belongs to a different scope"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    // Phase 2: find the prepared request's single invocation, if any.
    let existing = {
        let conn = store.conn.lock().expect("sqlite mutex");
        // The grant that prepared the request is the grant that may stream it.
        let prepared_grant: Option<String> = conn
            .query_row(
                "SELECT grant_ref FROM v1_host_prepared_requests WHERE prepared_request_id = ?1",
                params![prepared_request_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match prepared_grant.as_deref() {
            None => {
                return Err(ContractError::new(
                    CODE_NOT_FOUND,
                    format!("prepared request `{prepared_request_id}` not found"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            Some(prepared) if prepared != grant_ref => {
                return Err(grant_required(format!(
                    "prepared request `{prepared_request_id}` was prepared under a different grant"
                )));
            }
            _ => {}
        }
        conn.query_row(
            "SELECT invocation_id, operation_id, terminal, state
             FROM v1_host_model_invocations WHERE prepared_request_id = ?1",
            params![prepared_request_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?
    };
    if let Some((invocation_id, operation_id, terminal, state)) = existing {
        return Ok(InvocationInfo {
            invocation_id,
            operation_id,
            terminal,
            state,
        });
    }

    // Phase 3: create the invocation (the long-operation seam takes its own
    // lock, so this runs outside any held mutex).
    let (operation_id, _accepted_revision) = store.begin_long_operation(
        "host.stream_model",
        &json!({"kind": scope.kind(), "id": scope.id()}),
        true,
    )?;
    let invocation_id = format!("hinv-{}", store.next_id());
    let now = now_rfc3339();
    let inserted = {
        let conn = store.conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO v1_host_model_invocations (invocation_id, operation_id,
                prepared_request_id, model_id, grant_ref, scope_key, session_id, state,
                terminal, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'starting', 0, ?8, ?8)",
            params![
                invocation_id,
                operation_id,
                prepared_request_id,
                model_id,
                grant_ref,
                scope.key(),
                session_id,
                now
            ],
        )
        .map_err(sql_err)?
    };
    if inserted == 1 {
        store.event_notifier().notify_waiters();
        tokio::spawn(invocation_driver(
            store.clone(),
            invocation_id.clone(),
            operation_id.clone(),
            prepared_request_id.to_owned(),
        ));
    } else {
        // Lost the creation race: report the winner's invocation.
        let conn = store.conn.lock().expect("sqlite mutex");
        let winner: Option<(String, String, bool, String)> = conn
            .query_row(
                "SELECT invocation_id, operation_id, terminal, state
                 FROM v1_host_model_invocations WHERE prepared_request_id = ?1",
                params![prepared_request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((invocation_id, operation_id, terminal, state)) = winner {
            return Ok(InvocationInfo {
                invocation_id,
                operation_id,
                terminal,
                state,
            });
        }
    }
    Ok(InvocationInfo {
        invocation_id,
        operation_id,
        terminal: false,
        state: "starting".to_owned(),
    })
}

/// Appends one `model.*` event into the shared `events` table (resource_id =
/// the invocation's operation id, the `operations.events` cursor discipline).
fn append_model_event(
    conn: &Connection,
    operation_id: &str,
    session_id: &str,
    event_type: &str,
    data: &Value,
) -> Result<String, ContractError> {
    let placeholder = format!("tmp-hinv-{}", now_rfc3339());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id,
            payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
        params![
            placeholder,
            operation_id,
            session_id,
            event_type,
            MODEL_STREAM_SCHEMA_ID,
            serde_json::to_string(data).unwrap_or_else(|_| "{}".into()),
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

/// The invocation driver: the durable journal of one host model call. The
/// v1 build has no host-side model bridge mounted (the D20 turn submitter is
/// the run path, not this surface), so the invocation terminalizes as the
/// structured `model.failed` (`MODEL_UNAVAILABLE`) — never an empty fake
/// success. Cancellation through `operations.cancel` is honored.
async fn invocation_driver(
    store: Arc<V1Store>,
    invocation_id: String,
    operation_id: String,
    prepared_request_id: String,
) {
    let _ = store.mark_operation_running(&operation_id);
    let session_id: String = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let _ = conn.execute(
            "UPDATE v1_host_model_invocations SET state = 'running', updated_at = ?1
             WHERE invocation_id = ?2",
            params![now_rfc3339(), invocation_id],
        );
        let session_id = conn
            .query_row(
                "SELECT COALESCE(session_id, '') FROM v1_host_model_invocations
                 WHERE invocation_id = ?1",
                params![invocation_id],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default();
        let _ = append_model_event(
            &conn,
            &operation_id,
            &session_id,
            "model.started",
            &json!({
                "invocationId": invocation_id,
                "preparedRequestId": prepared_request_id,
            }),
        );
        session_id
    };
    store.event_notifier().notify_waiters();

    // No model bridge: the honest structured failure. A racing cancel wins
    // if its intent lands first (「明确operations.cancel才请求取消对应模
    // 型调用」).
    let mut cancel_poll = Box::pin(async {
        loop {
            if store
                .operation_cancel_requested(&operation_id)
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let cancelled = tokio::select! {
        _ = &mut cancel_poll => true,
        _ = tokio::time::sleep(Duration::from_millis(50)) => false,
    };

    let outcome = if cancelled {
        let conn = store.conn.lock().expect("sqlite mutex");
        let _ = append_model_event(
            &conn,
            &operation_id,
            &session_id,
            "model.cancelled",
            &json!({"invocationId": invocation_id}),
        );
        let _ = conn.execute(
            "UPDATE v1_host_model_invocations SET state = 'cancelled', terminal = 1,
                updated_at = ?1 WHERE invocation_id = ?2",
            params![now_rfc3339(), invocation_id],
        );
        drop(conn);
        LongOperationOutcome::Cancelled
    } else {
        let conn = store.conn.lock().expect("sqlite mutex");
        let _ = append_model_event(
            &conn,
            &operation_id,
            &session_id,
            "model.failed",
            &json!({
                "invocationId": invocation_id,
                "code": CODE_MODEL_UNAVAILABLE,
                "message": "no host model invocation bridge is mounted in this build",
            }),
        );
        let _ = conn.execute(
            "UPDATE v1_host_model_invocations SET state = 'failed', terminal = 1,
                updated_at = ?1 WHERE invocation_id = ?2",
            params![now_rfc3339(), invocation_id],
        );
        drop(conn);
        LongOperationOutcome::Failed {
            code: CODE_MODEL_UNAVAILABLE.to_owned(),
            message: "no host model invocation bridge is mounted in this build".to_owned(),
        }
    };
    let _ = store.finish_long_operation(&operation_id, outcome);
    store.event_notifier().notify_waiters();
}

/// One journal event row projected into the contract event shape.
struct JournalEvent {
    seq: i64,
    cursor: String,
    event_type: String,
    data: Value,
}

fn fetch_events_after(
    conn: &Connection,
    operation_id: &str,
    after_seq: i64,
    limit: usize,
) -> Result<Vec<JournalEvent>, ContractError> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, cursor, event_type, data_json FROM events
             WHERE resource_id = ?1 AND event_type LIKE 'model.%' AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![operation_id, after_seq, limit as i64], |r| {
            Ok(JournalEvent {
                seq: r.get(0)?,
                cursor: r.get(1)?,
                event_type: r.get(2)?,
                data: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(json!({})),
            })
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

async fn host_stream_model_unary(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let info = ensure_invocation(&rt, &store, &input)?;

    let after_seq = input
        .get("afterCursor")
        .and_then(Value::as_str)
        .and_then(parse_cursor)
        .unwrap_or(0);

    // Bounded wait for the first event after the cursor; once terminal with
    // nothing new, the read is idempotent: it re-reports the last terminal
    // event (the durable end state) instead of inventing an empty one.
    let deadline = tokio::time::Instant::now() + UNARY_EVENT_WAIT;
    let (event, terminal) = loop {
        let events = {
            let conn = store.conn.lock().expect("sqlite mutex");
            fetch_events_after(&conn, &info.operation_id, after_seq, 2)?
        };
        if let Some(event) = events.into_iter().next() {
            let more_after = {
                let conn = store.conn.lock().expect("sqlite mutex");
                !fetch_events_after(&conn, &info.operation_id, event.seq, 1)?.is_empty()
            };
            let terminal_event = MODEL_TERMINAL_EVENT_TYPES.contains(&event.event_type.as_str());
            break (event, terminal_event && !more_after);
        }
        let terminal_now: bool = {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.query_row(
                "SELECT terminal FROM v1_host_model_invocations WHERE invocation_id = ?1",
                params![info.invocation_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .unwrap_or(true)
        };
        if terminal_now {
            // Replay the durable terminal tail.
            let conn = store.conn.lock().expect("sqlite mutex");
            let last = fetch_events_after(&conn, &info.operation_id, 0, 500)?
                .into_iter()
                .last();
            if let Some(last) = last {
                break (last, true);
            }
            // No events at all and already terminal (a driver crash between
            // the two writes): surface the structured state.
            break (
                JournalEvent {
                    seq: 0,
                    cursor: format_cursor(0),
                    event_type: "model.failed".to_owned(),
                    data: json!({
                        "invocationId": info.invocation_id,
                        "code": "INVOCATION_JOURNAL_EMPTY",
                        "message": "the invocation terminalized without a journaled event",
                    }),
                },
                true,
            );
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ContractError::new(
                CODE_DEADLINE_EXCEEDED,
                format!(
                    "no stream event after cursor within {} ms and the invocation is still `{}`",
                    UNARY_EVENT_WAIT.as_millis(),
                    info.state
                ),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::NotApplied,
            )
            .into());
        }
        // Register the waiter before sleeping so a driver write between the
        // read above and this registration cannot be lost.
        let notifier = store.event_notifier();
        let notified = notifier.notified();
        let tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::select! {
            _ = notified => {}
            _ = tick => {}
        }
    };

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "operationId": info.operation_id,
            "invocationId": info.invocation_id,
            "event": {
                "cursor": event.cursor,
                "type": event.event_type,
                "schemaId": MODEL_STREAM_SCHEMA_ID,
                "data": event.data,
            },
            "terminal": terminal,
        }),
    ))
}

// ---------------------------------------------------------------------------
// host.stream_model — the StreamSource (sub.open follow mode)
// ---------------------------------------------------------------------------

struct HostModelStreamSource {
    store: Arc<V1Store>,
    instance_id: String,
    sub_id: String,
    operation_id: String,
    invocation_id: String,
    last_fetched_seq: i64,
}

/// The `host.stream_model` factory registered with the pump: validates the
/// request (grant, prepared request, hash, scope, expiry), attaches to (or
/// creates) the prepared request's single invocation, then follows its
/// journal from `afterCursor` until terminal.
fn host_stream_model_source(
    state: &Arc<PublicV1State>,
    input: &Value,
    sub_id: &str,
) -> Result<Box<dyn StreamSource>, ContractError> {
    let store = state.runtime.store();
    let info = ensure_invocation(&state.runtime, store, input)?;
    let after_seq = input
        .get("afterCursor")
        .and_then(Value::as_str)
        .and_then(parse_cursor)
        .unwrap_or(0);
    Ok(Box::new(HostModelStreamSource {
        store: store.clone(),
        instance_id: store.meta().instance_id.clone(),
        sub_id: sub_id.to_owned(),
        operation_id: info.operation_id,
        invocation_id: info.invocation_id,
        last_fetched_seq: after_seq,
    }))
}

impl StreamSource for HostModelStreamSource {
    fn fetch_next(&mut self, batch: usize) -> Result<StreamFetch, ContractError> {
        let conn = self.store.conn.lock().expect("sqlite mutex");
        let events = fetch_events_after(&conn, &self.operation_id, self.last_fetched_seq, batch)?;
        let terminal: bool = conn
            .query_row(
                "SELECT terminal FROM v1_host_model_invocations WHERE invocation_id = ?1",
                params![self.invocation_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .unwrap_or(true);
        drop(conn);

        let mut frames = Vec::with_capacity(events.len());
        for event in &events {
            self.last_fetched_seq = event.seq;
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
                        cursor: event.cursor.clone(),
                        event_type: event.event_type.clone(),
                        data: event.data.as_object().cloned().unwrap_or_default(),
                    },
                })
                .unwrap_or(Value::Null),
            };
            let text = serde_json::to_string(&notif).unwrap_or_default();
            let estimated_bytes = text.len() + 128;
            frames.push(StreamFrame {
                text,
                cursor: event.cursor.clone(),
                estimated_bytes,
            });
        }
        Ok(StreamFetch {
            frames,
            stream_ended: terminal && events.len() < batch,
        })
    }

    fn wake(&self) -> Arc<Notify> {
        self.store.event_notifier()
    }

    fn oversized_recovery_hint(&self) -> &'static str {
        "re-open the host.stream_model subscription from the last delivered cursor"
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;

    fn test_runtime() -> (Arc<PublicV1Runtime>, String, TempDir) {
        let tmp = TempDir::new("hostops");
        let root = tmp.0.join("home");
        initialize_instance(&root, None, Some("local"), Some(true)).unwrap();
        let file = crate::public_v1::bootstrap::load_local_instance_and_token(&root, None)
            .unwrap()
            .0;
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id,
            product: file.product,
            protocol_version: file.protocol_version,
            node_id: file.node_id,
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision,
        };
        let store = V1Store::open_and_recover(&root, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        let iid = rt.store().meta().instance_id.clone();
        (rt, iid, tmp)
    }

    fn global_scope() -> Value {
        json!({"kind": "global"})
    }

    /// Grants are server-issued credentials; tests seed the table directly
    /// (the only writer in v1 — there is deliberately no public issuance path).
    fn seed_grant(rt: &Arc<PublicV1Runtime>, parent_run_id: &str, grant_ref: &str) {
        let expires = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO v1_host_grants (grant_ref, consumer_kind, scope_key, parent_run_id,
                parent_invocation_id, model_id, max_input_tokens, max_output_tokens, timeout_ms,
                expires_at, created_at)
             VALUES (?1, 'runtimeTool', '{\"kind\":\"global\"}', ?2, 'inv-1', NULL,
                10000, 10000, 30000, ?3, ?4)",
            params![grant_ref, parent_run_id, expires, now_rfc3339()],
        )
        .unwrap();
    }

    /// Seeds a live run and returns the store-assigned run id (submit_run
    /// assigns ids itself; callers never choose them).
    fn seed_live_run(rt: &Arc<PublicV1Runtime>, iid: &str) -> String {
        let store = rt.store();
        store
            .register_project(&json!({
                "instanceId": iid,
                "projectId": "proj-h",
                "name": "H",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "r1", "relativePath": "w"}
            }))
            .unwrap();
        store
            .create_session(&json!({
                "instanceId": iid, "sessionId": "sess-h", "projectId": "proj-h"
            }))
            .unwrap();
        let disposition = store
            .submit_run(
                &json!({
                    "instanceId": iid,
                    "sessionId": "sess-h",
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=60000]"}],
                }),
                None,
            )
            .unwrap();
        let run_id = match disposition {
            crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("expected a new seeded run, got {other:?}"),
        };
        rt.start_session_worker(run_id.clone());
        run_id
    }

    fn seed_catalog_model(rt: &Arc<PublicV1Runtime>) {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO v1_providers (provider_id, protocol, endpoint, credential_ref,
                enabled, revision, created_at, updated_at)
             VALUES ('prov-h', 'openai', 'https://example.invalid', NULL, 1, 'rev-1', ?1, ?1)",
            params![now_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO v1_models (provider_id, model_id, name, enabled, revision,
                created_at, updated_at)
             VALUES ('prov-h', 'm-h', 'Host Model', 1, 'rev-1', ?1, ?1)",
            params![now_rfc3339()],
        )
        .unwrap();
    }

    fn execution_context(run_id: &str, grant_ref: &str) -> Value {
        json!({
            "parentRunId": run_id,
            "parentInvocationId": "inv-1",
            "consumerKind": "runtimeTool",
            "grantRef": grant_ref,
        })
    }

    fn prepare_input(run_id: &str, grant_ref: &str, budget: Value) -> Value {
        json!({
            "scope": global_scope(),
            "modelId": "prov-h/m-h",
            "schemaId": MODEL_REQUEST_SCHEMA_ID,
            "request": {"messages": [{"role": "user", "content": "hi"}]},
            "executionContext": execution_context(run_id, grant_ref),
            "budget": budget,
        })
    }

    fn small_budget() -> Value {
        json!({
            "maxInputTokens": 1000,
            "maxOutputTokens": 500,
            "timeoutMs": 5000
        })
    }

    #[tokio::test]
    async fn describe_reports_true_callability_per_operation() {
        let (rt, _iid, _home) = test_runtime();
        let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();
        let (_, described) = rt
            .dispatch(
                "host.describe",
                json!({"scope": global_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("host.describe", &described).unwrap();

        let ops: std::collections::HashMap<String, Value> = described["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["operationId"].as_str().unwrap().to_owned(),
                    entry.clone(),
                )
            })
            .collect();
        assert_eq!(ops.len(), 8);
        // Registered, non-gated operations are available with no reason.
        for op in ["model.list", "tools.list", "tools.test", "host.describe"] {
            assert_eq!(ops[op]["available"], true, "{op}");
            assert_eq!(ops[op]["reasonCode"], Value::Null, "{op}");
            assert!(
                ops[op]["inputSchemaId"]
                    .as_str()
                    .unwrap()
                    .starts_with("agentcpm-pd.operation.")
            );
        }
        // The grant-gated pair is implemented but closed.
        for op in ["host.prepare_model", "host.stream_model"] {
            assert_eq!(ops[op]["available"], false, "{op}");
            assert_eq!(ops[op]["reasonCode"], "CAPABILITY_GRANT_REQUIRED", "{op}");
        }
        // The skills pair belongs to the extensions task: not assembled.
        for op in ["skills.list", "skills.apply_import"] {
            assert_eq!(ops[op]["available"], false, "{op}");
            assert_eq!(ops[op]["reasonCode"], "CAPABILITY_UNAVAILABLE", "{op}");
        }
        let gaps: Vec<String> = described["declaredGaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["operationId"].as_str().unwrap().to_owned())
            .collect();
        assert!(gaps.contains(&"skills.list".to_owned()));
        assert!(gaps.contains(&"skills.apply_import".to_owned()));
    }

    #[tokio::test]
    async fn prepare_model_enforces_grant_binding_and_budget() {
        let (rt, _iid, _home) = test_runtime();
        let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();
        seed_catalog_model(&rt);
        let run_live_1 = seed_live_run(&rt, &rt.store().meta().instance_id);
        seed_grant(&rt, &run_live_1, "grant-ok");

        // Forged grant.
        let err = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(&run_live_1, "grant-forged", small_budget()),
                Some("key-h-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CAPABILITY_GRANT_REQUIRED");

        // Cross-run reuse.
        let run_other_2 = seed_live_run(&rt, &rt.store().meta().instance_id);
        let err = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(&run_other_2, "grant-ok", small_budget()),
                Some("key-h-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CAPABILITY_GRANT_REQUIRED");

        // Unknown schema id.
        let mut bad_schema = prepare_input(&run_live_1, "grant-ok", small_budget());
        bad_schema["schemaId"] = json!("vendor/schema-9");
        let err = rt
            .dispatch("host.prepare_model", bad_schema, Some("key-h-3"), None)
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "SCHEMA_MISMATCH");

        // Request failing the fixed schema.
        let mut bad_request = prepare_input(&run_live_1, "grant-ok", small_budget());
        bad_request["request"] = json!({"messages": []});
        let err = rt
            .dispatch("host.prepare_model", bad_request, Some("key-h-4"), None)
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "SCHEMA_MISMATCH");

        // Unknown model.
        let mut missing_model = prepare_input(&run_live_1, "grant-ok", small_budget());
        missing_model["modelId"] = json!("prov-h/m-missing");
        let err = rt
            .dispatch("host.prepare_model", missing_model, Some("key-h-5"), None)
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "NOT_FOUND");

        // Budget beyond the grant.
        let err = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(
                    &run_live_1,
                    "grant-ok",
                    json!({
                        "maxInputTokens": 999999,
                        "maxOutputTokens": 500,
                        "timeoutMs": 5000
                    }),
                ),
                Some("key-h-6"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "BUDGET_EXCEEDED");

        // The happy path: preparation does not invoke any model.
        let (_, prepared) = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(&run_live_1, "grant-ok", small_budget()),
                Some("key-h-7"),
                None,
            )
            .await
            .unwrap();
        reg.validate_result("host.prepare_model", &prepared)
            .unwrap();
        assert_eq!(prepared["modelId"], "prov-h/m-h");
        assert_eq!(prepared["streamSchemaId"], MODEL_STREAM_SCHEMA_ID);
        assert_eq!(prepared["approvedBudget"]["maxInputTokens"], 1000);

        // Same key replays the identical success result.
        let (_, replay) = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(&run_live_1, "grant-ok", small_budget()),
                Some("key-h-7"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["preparedRequestId"], prepared["preparedRequestId"]);

        // The reservation is consumed: a second prepare under a new key
        // exceeding the remainder is refused (10000 - 1000 < 9500).
        let err = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(
                    &run_live_1,
                    "grant-ok",
                    json!({
                        "maxInputTokens": 9500,
                        "maxOutputTokens": 500,
                        "timeoutMs": 5000
                    }),
                ),
                Some("key-h-8"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "BUDGET_EXCEEDED");
    }

    #[tokio::test]
    async fn stream_model_journals_structured_failure_and_replays() {
        let (rt, iid, _home) = test_runtime();
        let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();
        seed_catalog_model(&rt);
        let run_live_2 = seed_live_run(&rt, &iid);
        seed_grant(&rt, &run_live_2, "grant-s");

        let (_, prepared) = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(&run_live_2, "grant-s", small_budget()),
                Some("key-s-1"),
                None,
            )
            .await
            .unwrap();
        let prepared_id = prepared["preparedRequestId"].as_str().unwrap().to_owned();
        let hash = prepared["requestHash"].as_str().unwrap().to_owned();

        // Hash tampering is refused.
        let err = rt
            .dispatch(
                "host.stream_model",
                json!({
                    "scope": global_scope(),
                    "preparedRequestId": prepared_id,
                    "requestHash": "deadbeef",
                    "executionContext": execution_context(&run_live_2, "grant-s"),
                }),
                Some("key-s-tamper"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "INTEGRITY_ERROR");

        // Unknown prepared request.
        let err = rt
            .dispatch(
                "host.stream_model",
                json!({
                    "scope": global_scope(),
                    "preparedRequestId": "hpr-none",
                    "requestHash": hash,
                    "executionContext": execution_context(&run_live_2, "grant-s"),
                }),
                Some("key-s-missing"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "NOT_FOUND");

        // The unary read walks started -> failed, terminal.
        let (_, first) = rt
            .dispatch(
                "host.stream_model",
                json!({
                    "scope": global_scope(),
                    "preparedRequestId": prepared_id,
                    "requestHash": hash,
                    "executionContext": execution_context(&run_live_2, "grant-s"),
                }),
                Some("key-s-2"),
                None,
            )
            .await
            .unwrap();
        reg.validate_result("host.stream_model", &first).unwrap();
        let op_id = first["operationId"].as_str().unwrap().to_owned();
        let invocation_id = first["invocationId"].as_str().unwrap().to_owned();
        if first["event"]["type"] == "model.started" {
            let (_, second) = rt
                .dispatch(
                    "host.stream_model",
                    json!({
                        "scope": global_scope(),
                        "preparedRequestId": prepared_id,
                        "requestHash": hash,
                        "afterCursor": first["event"]["cursor"],
                        "executionContext": execution_context(&run_live_2, "grant-s"),
                    }),
                    Some("key-s-3"),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(second["event"]["type"], "model.failed");
            assert_eq!(second["event"]["data"]["code"], "MODEL_UNAVAILABLE");
            assert_eq!(second["terminal"], true);
        } else {
            assert_eq!(first["event"]["type"], "model.failed");
            assert_eq!(first["terminal"], true);
        }

        // Re-streaming under a different key attaches to the same invocation
        // (no second paid request): the terminal tail replays.
        let (_, replay) = rt
            .dispatch(
                "host.stream_model",
                json!({
                    "scope": global_scope(),
                    "preparedRequestId": prepared_id,
                    "requestHash": hash,
                    "executionContext": execution_context(&run_live_2, "grant-s"),
                }),
                Some("key-s-4"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["invocationId"], json!(invocation_id));
        assert_eq!(replay["terminal"], true);

        // The invocation is a cancellable long operation; it is terminal now
        // so a cancel replays alreadyTerminal.
        let (_, cancel_res) = rt
            .dispatch(
                "operations.cancel",
                json!({"instanceId": iid, "operationId": op_id}),
                Some("key-s-cancel"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancel_res["receipt"]["disposition"], "alreadyTerminal");
    }

    #[tokio::test]
    async fn expired_grant_and_terminal_parent_are_refused() {
        let (rt, iid, _home) = test_runtime();
        seed_catalog_model(&rt);
        let run_live_3 = seed_live_run(&rt, &iid);
        // An already-expired grant.
        let expired_at = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO v1_host_grants (grant_ref, consumer_kind, scope_key, parent_run_id,
                    parent_invocation_id, model_id, max_input_tokens, max_output_tokens,
                    timeout_ms, expires_at, created_at)
                 VALUES ('grant-old', 'runtimeTool', '{\"kind\":\"global\"}', ?2,
                    'inv-1', NULL, 10000, 10000, 30000, ?1, ?1)",
                params![expired_at, run_live_3],
            )
            .unwrap();
        }

        let err = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(&run_live_3, "grant-old", small_budget()),
                Some("key-old-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CAPABILITY_GRANT_EXPIRED");

        // A terminal parent run refuses new preparations.
        let run_done_9 = seed_live_run(&rt, &iid);
        let store = rt.store();
        store
            .transition_run_state(
                &run_done_9,
                crate::public_v1::store::RunTransition::new(
                    agentcpm_pd_protocol::contract::RunState::Succeeded,
                    "run.completed",
                    &json!({"runId": run_done_9}),
                )
                .outcome_code("SUCCEEDED"),
            )
            .unwrap();
        seed_grant(&rt, &run_done_9, "grant-done");
        let err = rt
            .dispatch(
                "host.prepare_model",
                prepare_input(&run_done_9, "grant-done", small_budget()),
                Some("key-done-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "PARENT_RUN_TERMINAL");
    }

    fn err_code(err: &DispatchError) -> String {
        match err {
            DispatchError::Application(contract) => contract.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}
