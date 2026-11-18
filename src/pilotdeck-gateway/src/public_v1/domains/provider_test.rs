//! `provider_test.*` — async provider connection tests (`cli.task.model-test`).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `provider_test.start` /
//! `.retry` / `.acknowledge` / `.confirm_capabilities` 条目。TS 基线（cc7c4e6）
//! 的 `ui/server/routes/onboarding.js`（`prepareConnectionTest` /
//! `applyImageCapabilities`）与 `ui/server/services/modelConnectionProbe.js`
//! 是语义参考：
//!
//! * **受控探测**：`probeMode='network'` 才出网（「network显式且受预算」），
//!   `validateOnly` 零网络（受理面把 provider 存在性 / `configRevision`
//!   一致性 / 凭据引用可解析性验完，探测只回填逐项结果）。
//! * **请求形态**：按 provider 的 `protocol`（openai / openai-responses /
//!   anthropic / google，t016 已归一）构造最小 chat 请求；endpoint 候选与
//!   400/404/405 回退照抄基线 `buildEndpointCandidates`
//!   （`src/model/providerEndpoint.ts:52-111`）。v1 合同没有 retryPolicy 输入，
//!   每次候选尝试消耗一单位 `requestBudget`（默认 1 = 只打首选候选）。
//! * **证据纪律**（验收锚「图像能力不由 text-only 推定」「无证据即 unknown
//!   不猜」）：每个 testKind 独立探测；只有拿到「实际响应证据」才落
//!   `v1_test_evidence`（2xx 且响应形态正确且有可用文本 → supported；明确
//!   的服务端不支持声明（基线 `looksLikeImageUnsupported`）→ unsupported＝
//!   确定性否定证据）；鉴权失败 / 超时 / 不可达 / 图像应答未描述测试图 →
//!   无证据，仅记 issue。image 探测发送基线同款纯色 PNG 并要求应答包含
//!   颜色词——文本模型收图照样答的假阳性由此拦截。text 成功不产生任何
//!   image 证据，confirm 阶段 image 只认 image 证据。
//! * **配置绑定**（验收锚「旧配置证据不背书新配置」）：`configDigest` =
//!   sha256(protocol + endpoint + 有效 credentialRef + modelId)。探测使用
//!   受理时捕获的 protocol/endpoint（不是探测时的最新行），证据因此严格
//!   绑定在受测配置上；`confirm_capabilities` 重算当前 provider 配置的摘
//!   要，证据摘要不一致 → `EVIDENCE_MISMATCH`（对应基线 source-flow 98c2
//!   「变更endpoint/model/凭据后须重新确认」）。
//! * **确认语义**：`confirm_capabilities` 是唯一能写 `v1_models.
//!   capability_evidence_json` 的入口（t016 的 `model.write` 永不合成
//!   evidence，见 models.rs 模块注释）；确认某能力必须有同 kind 的证据
//!   （`INSUFFICIENT_EVIDENCE`），证据与确认值矛盾 → `EVIDENCE_MISMATCH`。
//! * **acknowledge**：只标记「人已查阅」，绝不改测试结果（「不把失败改成功」），
//!   非终态操作 → `OPERATION_NOT_READY`（「运行中不得ack成终态」）。
//! * **retry**：新 operation（completionResult 保留 `sourceOperationId`，
//!   target 行带来源），原记录零改动；源 operation 必须已终态
//!   （`SOURCE_NOT_TERMINAL`）且被重试 target 必须是显式非成功终态；
//!   provider 当前 revision 必须仍等于源测试时的 `configRevision`
//!   （「旧revision不能偷偷测试新配置」→ 否则 `CONFIG_REVISION_CONFLICT`，
//!   调用方需用新 revision 重新 start）。
//!
//! 操作记录走 collections 的长操作缝（`workspaces.rs` 同款受理形态）：
//! 预检 → `begin_long_operation`（operations 表 + `operation.queued` 事件）
//! → 逐 target 行 → `idempotency_records` 防重记录；驱动侧用缝上的
//! `mark_operation_running` / `record_operation_progress` /
//! `operation_cancel_requested` / `finish_long_operation`，并像 workspaces
//! 一样在终态后回写重放 receipt 的 state。
//!
//! 合同未明确处的选择（详见任务报告）：批量逐项失败映射为「operation
//! succeeded + TestOutcome.state 逐项 + partial=true」（缝的 Failed 终态不存
//! completionResult，会把逐项结果吞掉；「结果缺失被当成功」由 partial 与
//! 逐项 state 显式表达）；未知的 testKind 显式记 `UNSUPPORTED_TEST_KIND`
//! issue 而不是静默通过；`timeoutMs` 按单次 HTTP 尝试计（基线
//! `TIMEOUT_MS` 同为单探测粒度）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE,
    ContractError, EffectStatus, RetrySpec,
};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::operations_ops::LongOperationOutcome;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, format_cursor, now_rfc3339};

/// Contract error codes these operations declare that no shared constant
/// covers yet (all appear verbatim in the contract `errors` lists).
const CODE_CONFIG_REVISION_CONFLICT: &str = "CONFIG_REVISION_CONFLICT";
const CODE_BUDGET_EXCEEDED: &str = "BUDGET_EXCEEDED";
const CODE_SOURCE_NOT_TERMINAL: &str = "SOURCE_NOT_TERMINAL";
const CODE_OPERATION_NOT_READY: &str = "OPERATION_NOT_READY";
const CODE_EVIDENCE_MISMATCH: &str = "EVIDENCE_MISMATCH";
const CODE_INSUFFICIENT_EVIDENCE: &str = "INSUFFICIENT_EVIDENCE";

/// Probe-side issue codes (baseline `classifyProbeError` vocabulary plus the
/// explicit unknown-kind marker). These are `Issue.code` values inside
/// `TestOutcome.issues`, not operation errors.
const CODE_AUTHENTICATION_FAILED: &str = "AUTHENTICATION_FAILED";
const CODE_MODEL_NOT_FOUND: &str = "MODEL_NOT_FOUND";
const CODE_ENDPOINT_UNREACHABLE: &str = "ENDPOINT_UNREACHABLE";
const CODE_IMAGE_CAPABILITY_UNKNOWN: &str = "IMAGE_CAPABILITY_UNKNOWN";
const CODE_UNSUPPORTED_TEST_KIND: &str = "UNSUPPORTED_TEST_KIND";

const OPERATION_EVENT_SCHEMA: &str = "agentcpm-pd.operation.event/1";
/// The credential kind a model provider consumes (providers.rs parity).
const PROVIDER_CREDENTIAL_KIND: &str = "api_key";
/// Pre-accept cap over the whole operation (「受预算」的硬顶；
/// `BUDGET_EXCEEDED` 在任何资源创建前返回).
const MAX_TOTAL_REQUESTS_PER_OPERATION: u64 = 128;
/// Probe prompt / image payload (baseline `modelConnectionProbe.js`).
const TEXT_PROBE_PROMPT: &str = "Reply exactly: 1";
const IMAGE_PROBE_PROMPT: &str =
    "What color is the shape in this image? Reply with one English color word.";
const IMAGE_COLORS: [(&str, [u8; 3]); 4] = [
    ("red", [220, 48, 48]),
    ("blue", [40, 96, 220]),
    ("green", [36, 168, 72]),
    ("yellow", [232, 196, 40]),
];
const PROBE_MAX_TOKENS: u64 = 4096;

const DDL_PROVIDER_TESTS: &str = "CREATE TABLE IF NOT EXISTS v1_provider_tests (
    test_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    source_operation_id TEXT,
    target_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model_id TEXT,
    protocol TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    config_revision TEXT NOT NULL,
    config_digest TEXT NOT NULL,
    credential_ref TEXT,
    test_kinds_json TEXT NOT NULL,
    state TEXT NOT NULL,
    elapsed_ms INTEGER,
    issues_json TEXT NOT NULL DEFAULT '[]',
    evidence_ids_json TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const DDL_TEST_EVIDENCE: &str = "CREATE TABLE IF NOT EXISTS v1_test_evidence (
    evidence_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model_id TEXT,
    config_digest TEXT NOT NULL,
    kind TEXT NOT NULL,
    supported INTEGER NOT NULL,
    created_at TEXT NOT NULL
)";
const DDL_TEST_STATE: &str = "CREATE TABLE IF NOT EXISTS v1_provider_test_state (
    operation_id TEXT PRIMARY KEY,
    acknowledged INTEGER NOT NULL DEFAULT 0,
    acknowledged_at TEXT
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_provider_tests", DDL_PROVIDER_TESTS);
    reg.schema("v1_test_evidence", DDL_TEST_EVIDENCE);
    reg.schema("v1_provider_test_state", DDL_TEST_STATE);
    reg.handler("provider_test.start", |rt, input, key, _rev| {
        Box::pin(provider_test_start(rt, input, key))
    });
    reg.handler("provider_test.retry", |rt, input, key, _rev| {
        Box::pin(provider_test_retry(rt, input, key))
    });
    reg.handler("provider_test.acknowledge", |rt, input, _key, _rev| {
        Box::pin(provider_test_acknowledge(rt, input))
    });
    reg.handler(
        "provider_test.confirm_capabilities",
        |rt, input, _key, _rev| Box::pin(provider_test_confirm_capabilities(rt, input)),
    );
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn invalid(msg: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        msg.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// `CONFIG_REVISION_CONFLICT` — the tested configuration is not current
/// (missing provider, revision drift on start, or drift since the source
/// test on retry).
fn config_revision_conflict(msg: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_CONFIG_REVISION_CONFLICT,
        msg.into(),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

/// One provider row as the test sees it (local read-only view of the t016
/// table; the providers domain owns the writes).
struct TestedProvider {
    protocol: String,
    endpoint: String,
    credential_ref: Option<String>,
    revision: String,
}

fn load_tested_provider(
    conn: &Connection,
    provider_id: &str,
) -> Result<Option<TestedProvider>, ContractError> {
    conn.query_row(
        "SELECT protocol, endpoint, credential_ref, revision FROM v1_providers WHERE provider_id = ?1",
        params![provider_id],
        |r| {
            Ok(TestedProvider {
                protocol: r.get(0)?,
                endpoint: r.get(1)?,
                credential_ref: r.get(2)?,
                revision: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(sql_err)
}

/// The config digest a test outcome and its evidence bind to: the exact
/// (protocol, endpoint, credential reference, model) tuple that was probed
/// （「结果绑定配置hash/credential引用/模态证据」）.
fn provider_config_digest(
    protocol: &str,
    endpoint: &str,
    credential_ref: Option<&str>,
    model_id: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentcpm-pd.provider-test.config/1");
    hasher.update([0u8]);
    hasher.update(protocol.as_bytes());
    hasher.update([0u8]);
    hasher.update(endpoint.as_bytes());
    hasher.update([0u8]);
    hasher.update(credential_ref.unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(model_id.unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Accept-time credential check: the reference must exist with the provider
/// credential kind. The enabled state is deliberately not required here —
/// a disabled credential is a runtime condition the probe reports
/// (AUTHENTICATION_FAILED, fail-closed through `resolve_credential_secret`),
/// not an input error.
fn check_credential_reference(
    conn: &Connection,
    credential_ref: &str,
) -> Result<(), ContractError> {
    let kind: Option<String> = conn
        .query_row(
            "SELECT kind FROM v1_credentials WHERE credential_id = ?1",
            params![credential_ref],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    match kind.as_deref() {
        Some(PROVIDER_CREDENTIAL_KIND) => Ok(()),
        Some(other) => Err(invalid(format!(
            "credential `{credential_ref}` has kind `{other}`; provider tests consume `{PROVIDER_CREDENTIAL_KIND}` credentials"
        ))),
        None => Err(invalid(format!(
            "credential `{credential_ref}` does not exist"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Idempotency (accepted-receipt replay, same table/shape as run.submit,
// uploads.complete and workspaces.prepare)
// ---------------------------------------------------------------------------

fn replay_accepted_receipt(
    store: &V1Store,
    method_id: &str,
    input: &Value,
    key: &str,
) -> Result<Option<Value>, ContractError> {
    let digest = compute_input_digest(method_id, input);
    let conn = store.conn.lock().expect("sqlite mutex");
    let existing: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT input_digest, receipt_json, rejection_code
             FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, method_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    match existing {
        None => Ok(None),
        Some((stored_digest, receipt_json, rejection)) => {
            if stored_digest != digest {
                return Err(ContractError::new(
                    CODE_IDEMPOTENCY_CONFLICT,
                    format!(
                        "idempotencyKey `{key}` already used for {method_id} with different input"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if let Some(code) = rejection {
                return Err(ContractError::new(
                    code,
                    "request previously rejected definitively",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            match receipt_json.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) {
                Some(mut receipt) => {
                    if let Some(obj) = receipt.as_object_mut() {
                        obj.insert("deduplicated".into(), json!(true));
                    }
                    Ok(Some(receipt))
                }
                None => Ok(None),
            }
        }
    }
}

fn record_acceptance(
    store: &V1Store,
    key: &str,
    method_id: &str,
    input: &Value,
    resource: &Value,
    receipt: &Value,
) -> Result<(), ContractError> {
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    let retained = (Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')",
        params![
            key,
            method_id,
            digest,
            serde_json::to_string(resource).unwrap_or_default(),
            serde_json::to_string(receipt).unwrap_or_default(),
            now,
            retained,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn record_rejection(store: &V1Store, key: &str, method_id: &str, input: &Value, code: &str) {
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    let retained = (Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6, 'correctRejectedInputWithNewKey')",
        params![key, method_id, digest, code, now, retained],
    );
}

/// 终态推进后刷新重放 receipt 的 state（同 key 重试观察到终态；
/// workspaces.rs 同款）。
fn update_receipt_terminal_state(store: &V1Store, key: Option<&str>, method_id: &str, state: &str) {
    let Some(key) = key else {
        return;
    };
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let Ok(raw) = conn.query_row(
        "SELECT receipt_json FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
        params![key, method_id],
        |r| r.get::<_, String>(0),
    ) else {
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
                method_id
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// Accepted target rows
// ---------------------------------------------------------------------------

/// The accepted form of one test target, captured at accept time. The
/// protocol/endpoint are the values the driver will actually probe — the
/// evidence stays bound to the accepted configuration even if the provider
/// row changes afterwards.
struct AcceptedTarget {
    target_id: String,
    provider_id: String,
    model_id: Option<String>,
    protocol: String,
    endpoint: String,
    config_revision: String,
    config_digest: String,
    credential_ref: Option<String>,
    test_kinds: Vec<String>,
}

fn insert_target_rows(
    store: &V1Store,
    operation_id: &str,
    source_operation_id: Option<&str>,
    targets: &[AcceptedTarget],
) -> Result<(), ContractError> {
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;
    let now = now_rfc3339();
    for target in targets {
        tx.execute(
            "INSERT INTO v1_provider_tests (test_id, operation_id, source_operation_id, target_id,
                provider_id, model_id, protocol, endpoint, config_revision, config_digest,
                credential_ref, test_kinds_json, state, elapsed_ms, issues_json, evidence_ids_json,
                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'queued', NULL, '[]', '[]', ?13, ?13)",
            params![
                store.next_id(),
                operation_id,
                source_operation_id,
                target.target_id,
                target.provider_id,
                target.model_id,
                target.protocol,
                target.endpoint,
                target.config_revision,
                target.config_digest,
                target.credential_ref,
                serde_json::to_string(&target.test_kinds).unwrap_or_else(|_| "[]".into()),
                now,
            ],
        )
        .map_err(sql_err)?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(())
}

fn operation_receipt(operation_id: &str, accepted_revision: &str, deduplicated: bool) -> Value {
    json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": accepted_revision,
        "deduplicated": deduplicated,
        "state": "queued",
    })
}

/// The post-accept tail shared by start and retry: begin the operation,
/// persist the target rows and the idempotency record, spawn the driver.
/// Any persistence failure after `begin_long_operation` terminalizes the
/// fresh row as failed instead of leaving an undriven queued row.
#[allow(clippy::too_many_arguments)]
fn accept_provider_test_operation(
    store: &Arc<V1Store>,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    source_operation_id: Option<&str>,
    targets: &[AcceptedTarget],
    probe_mode: &str,
    request_budget: u64,
    timeout_ms: u64,
) -> Result<(String, Value), ContractError> {
    let (operation_id, accepted_revision) =
        store.begin_long_operation(method_id, &json!({"kind": "instance"}), true)?;
    let receipt = operation_receipt(&operation_id, &accepted_revision, false);
    let persist = (|| -> Result<(), ContractError> {
        insert_target_rows(store, &operation_id, source_operation_id, targets)?;
        if let Some(key) = idempotency_key {
            record_acceptance(
                store,
                key,
                method_id,
                input,
                &json!({"operationId": operation_id}),
                &receipt,
            )?;
        }
        Ok(())
    })();
    if let Err(err) = persist {
        let _ = store.finish_long_operation(
            &operation_id,
            LongOperationOutcome::Failed {
                code: err.code.clone(),
                message: err.message.clone(),
            },
        );
        return Err(err);
    }
    store.event_notifier().notify_waiters();
    spawn_probe_driver(
        store.clone(),
        operation_id.clone(),
        method_id.to_owned(),
        source_operation_id.map(str::to_owned),
        probe_mode.to_owned(),
        request_budget,
        timeout_ms,
        idempotency_key.map(str::to_owned),
    );
    Ok((operation_id, receipt))
}

// ---------------------------------------------------------------------------
// provider_test.start — accept path
// ---------------------------------------------------------------------------

/// Pre-accept validation over every target (the baseline validates before
/// accepting a task: "先校验schema、作用域与实际能力"). Zero side effects:
/// on failure nothing has been created.
fn precheck_start_targets(
    store: &V1Store,
    input: &Value,
    probe_mode: &str,
    request_budget: u64,
) -> Result<Vec<AcceptedTarget>, ContractError> {
    let raw_targets = input["targets"].as_array().cloned().unwrap_or_default();
    let mut targets = Vec::with_capacity(raw_targets.len());
    let mut budget_total: u64 = 0;
    let conn = store.conn.lock().expect("sqlite mutex");
    for target in &raw_targets {
        let target_id = target["targetId"].as_str().unwrap_or_default().to_owned();
        let provider_id = target["providerId"].as_str().unwrap_or_default().to_owned();
        let model_id = target["modelId"].as_str().map(str::to_owned);
        let config_revision = target["configRevision"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let explicit_credential = target["credentialRef"].as_str().map(str::to_owned);
        let test_kinds: Vec<String> = target["testKinds"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        let Some(provider) = load_tested_provider(&conn, &provider_id)? else {
            return Err(config_revision_conflict(format!(
                "provider `{provider_id}` does not exist; the tested configuration is not current"
            )));
        };
        if provider.revision != config_revision {
            return Err(config_revision_conflict(format!(
                "provider `{provider_id}` is at revision `{}`; the target names `{config_revision}`",
                provider.revision
            )));
        }
        // Effective credential: explicit target override, else the
        // provider's stored reference. A credentialless provider is
        // legitimate (local servers, the baseline's ollama case); a present
        // reference must at least resolve to the right kind.
        let credential_ref = match &explicit_credential {
            Some(cref) => {
                check_credential_reference(&conn, cref)?;
                Some(cref.clone())
            }
            None => match provider.credential_ref.as_deref() {
                Some(cref) => {
                    check_credential_reference(&conn, cref)?;
                    Some(cref.to_owned())
                }
                None => None,
            },
        };
        if probe_mode == "network" && model_id.is_none() {
            return Err(invalid(format!(
                "target `{target_id}`: a network probe needs a modelId (every provider protocol addresses a concrete model)"
            )));
        }
        let kinds = if test_kinds.is_empty() {
            1
        } else {
            test_kinds.len() as u64
        };
        budget_total += kinds.saturating_mul(request_budget.max(1));
        if budget_total > MAX_TOTAL_REQUESTS_PER_OPERATION {
            return Err(ContractError::new(
                CODE_BUDGET_EXCEEDED,
                format!(
                    "targets × testKinds × requestBudget would exceed the per-operation request budget of {MAX_TOTAL_REQUESTS_PER_OPERATION}"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        targets.push(AcceptedTarget {
            config_digest: provider_config_digest(
                &provider.protocol,
                &provider.endpoint,
                credential_ref.as_deref(),
                model_id.as_deref(),
            ),
            target_id,
            provider_id,
            model_id,
            protocol: provider.protocol,
            endpoint: provider.endpoint,
            config_revision,
            credential_ref,
            test_kinds,
        });
    }
    Ok(targets)
}

async fn provider_test_start(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let method_id = "provider_test.start";
    let probe_mode = input
        .get("probeMode")
        .and_then(Value::as_str)
        .unwrap_or("validateOnly")
        .to_owned();
    let request_budget = input
        .get("requestBudget")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let timeout_ms = input
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);

    // 幂等 intake：同 key 同输入重放受理结果（「再次相同提交不重复创
    // 建」）；同 key 不同输入 → IDEMPOTENCY_CONFLICT。
    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = replay_accepted_receipt(&store, method_id, &input, key)?
    {
        return Ok((ResponseOutcome::Accepted, replay));
    }

    // 预检（零副作用）。拒绝是终局性的：登记后同 key 重试得到同一拒绝。
    let targets = match precheck_start_targets(&store, &input, &probe_mode, request_budget) {
        Ok(targets) => targets,
        Err(err) => {
            if let Some(key) = idempotency_key.as_deref() {
                record_rejection(&store, key, method_id, &input, &err.code);
            }
            return Err(err.into());
        }
    };

    let (_operation_id, receipt) = accept_provider_test_operation(
        &store,
        method_id,
        &input,
        idempotency_key.as_deref(),
        None,
        &targets,
        &probe_mode,
        request_budget,
        timeout_ms,
    )?;
    Ok((ResponseOutcome::Accepted, receipt))
}

// ---------------------------------------------------------------------------
// provider_test.retry — accept path
// ---------------------------------------------------------------------------

/// Pre-accept validation for a retry: the source operation is one of ours
/// and terminal, every named target belongs to it and did not succeed, and
/// the provider is still at the revision the source test targeted
/// （「旧revision不能偷偷测试新配置」）.
fn precheck_retry_targets(
    store: &V1Store,
    input: &Value,
    probe_mode: &str,
    request_budget: u64,
) -> Result<(String, Vec<AcceptedTarget>), ContractError> {
    let source_operation_id = input["sourceOperationId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let target_ids: Vec<String> = input["targetIds"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let mut targets = Vec::with_capacity(target_ids.len());
    let mut budget_total: u64 = 0;
    let conn = store.conn.lock().expect("sqlite mutex");

    // The source operation must be one of ours and terminal.
    let source: Option<(String, i64)> = conn
        .query_row(
            "SELECT operation_type, terminal FROM operations WHERE operation_id = ?1",
            params![source_operation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    match source {
        None => {
            return Err(invalid(format!(
                "source operation `{source_operation_id}` not found"
            )));
        }
        Some((op_type, _))
            if op_type != "provider_test.start" && op_type != "provider_test.retry" =>
        {
            return Err(invalid(format!(
                "operation `{source_operation_id}` is not a provider test operation"
            )));
        }
        Some((_, 0)) => {
            return Err(ContractError::new(
                CODE_SOURCE_NOT_TERMINAL,
                format!(
                    "source operation `{source_operation_id}` has not reached a terminal state"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        Some((_, _)) => {}
    }

    for target_id in &target_ids {
        #[allow(clippy::type_complexity)]
        let row: Option<(String, Option<String>, String, Option<String>, String, String)> = conn
            .query_row(
                "SELECT provider_id, model_id, config_revision, credential_ref, state, test_kinds_json
                 FROM v1_provider_tests WHERE operation_id = ?1 AND target_id = ?2",
                params![source_operation_id, target_id],
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
        let Some((provider_id, model_id, config_revision, source_credential, state, kinds_json)) =
            row
        else {
            return Err(invalid(format!(
                "target `{target_id}` not found in source operation `{source_operation_id}`"
            )));
        };
        // 「重试显式失败测试项」: only explicitly non-succeeded outcomes are
        // retryable (a succeeded target is a fresh `start`, not a retry).
        if state == "succeeded" {
            return Err(invalid(format!(
                "target `{target_id}` did not fail in the source operation; nothing to retry"
            )));
        }
        let Some(provider) = load_tested_provider(&conn, &provider_id)? else {
            return Err(config_revision_conflict(format!(
                "provider `{provider_id}` no longer exists; the source test configuration is not current"
            )));
        };
        if provider.revision != config_revision {
            return Err(config_revision_conflict(format!(
                "provider `{provider_id}` moved from revision `{config_revision}` to `{}`; re-issue provider_test.start with the current revision",
                provider.revision
            )));
        }
        // The retry keeps the source target's explicit credential override;
        // without one it follows the provider's current reference.
        let credential_ref = match source_credential {
            Some(cref) => {
                check_credential_reference(&conn, &cref)?;
                Some(cref)
            }
            None => match provider.credential_ref.as_deref() {
                Some(cref) => {
                    check_credential_reference(&conn, cref)?;
                    Some(cref.to_owned())
                }
                None => None,
            },
        };
        if probe_mode == "network" && model_id.is_none() {
            return Err(invalid(format!(
                "target `{target_id}`: a network probe needs a modelId"
            )));
        }
        let test_kinds: Vec<String> = serde_json::from_str(&kinds_json).unwrap_or_default();
        let kinds = if test_kinds.is_empty() {
            1
        } else {
            test_kinds.len() as u64
        };
        budget_total += kinds.saturating_mul(request_budget.max(1));
        if budget_total > MAX_TOTAL_REQUESTS_PER_OPERATION {
            return Err(ContractError::new(
                CODE_BUDGET_EXCEEDED,
                format!(
                    "targets × testKinds × requestBudget would exceed the per-operation request budget of {MAX_TOTAL_REQUESTS_PER_OPERATION}"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        targets.push(AcceptedTarget {
            config_digest: provider_config_digest(
                &provider.protocol,
                &provider.endpoint,
                credential_ref.as_deref(),
                model_id.as_deref(),
            ),
            target_id: target_id.clone(),
            provider_id,
            model_id,
            protocol: provider.protocol,
            endpoint: provider.endpoint,
            config_revision,
            credential_ref,
            test_kinds,
        });
    }
    Ok((source_operation_id, targets))
}

async fn provider_test_retry(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let method_id = "provider_test.retry";
    let probe_mode = input
        .get("probeMode")
        .and_then(Value::as_str)
        .unwrap_or("validateOnly")
        .to_owned();
    let request_budget = input
        .get("requestBudget")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    // The retry input schema has no timeoutMs; the start-side default
    // (30 s per HTTP attempt) applies.
    let timeout_ms = 30_000u64;

    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = replay_accepted_receipt(&store, method_id, &input, key)?
    {
        return Ok((ResponseOutcome::Accepted, replay));
    }

    let (source_operation_id, targets) =
        match precheck_retry_targets(&store, &input, &probe_mode, request_budget) {
            Ok(found) => found,
            Err(err) => {
                if let Some(key) = idempotency_key.as_deref() {
                    record_rejection(&store, key, method_id, &input, &err.code);
                }
                return Err(err.into());
            }
        };

    let (_operation_id, receipt) = accept_provider_test_operation(
        &store,
        method_id,
        &input,
        idempotency_key.as_deref(),
        Some(&source_operation_id),
        &targets,
        &probe_mode,
        request_budget,
        timeout_ms,
    )?;
    Ok((ResponseOutcome::Accepted, receipt))
}

// ---------------------------------------------------------------------------
// Probe engine
// ---------------------------------------------------------------------------

/// One probed (target, kind) result.
enum KindProbeResult {
    /// Definitive evidence the capability is supported.
    Supported,
    /// Definitive evidence the capability is unsupported (an explicit
    /// server-side "not supported" statement).
    Unsupported,
    /// No evidence; the kind stays unknown. Carries the issue (code,
    /// message) for `TestOutcome.issues`.
    Undetermined(String, String),
}

/// Baseline `looksLikeImageUnsupported` (modelConnectionProbe.js).
fn looks_like_image_unsupported(detail: &str) -> bool {
    let text = detail.to_ascii_lowercase();
    let has = |needle: &str| text.contains(needle);
    let modality = has("image") || has("vision") || has("multimodal");
    let unsupported = has("not supported")
        || has("unsupported")
        || has("not enabled")
        || has("not available")
        || has("does not support");
    if modality && unsupported {
        return true;
    }
    // "...content.type...allowed values: ['text']" — an explicit
    // content-type allowlist without image.
    (has("content.type") || has("content type")) && has("allowed values") && has("text")
}

/// Baseline `classifyProbeError` — the issue code for a failed probe.
fn classify_probe_error(detail: &str, status: u16) -> &'static str {
    let text = detail.to_ascii_lowercase();
    if status == 401
        || status == 403
        || text.contains("api key")
        || text.contains("apikey")
        || text.contains("authentication")
        || text.contains("unauthorized")
        || text.contains("unauthorised")
    {
        return CODE_AUTHENTICATION_FAILED;
    }
    if status == 404
        || text.contains("model not found")
        || text.contains("unknown model")
        || text.contains("does not exist")
    {
        return CODE_MODEL_NOT_FOUND;
    }
    CODE_ENDPOINT_UNREACHABLE
}

/// The exhausted-candidates code when the endpoint answered 2xx but never
/// produced a usable completion (the baseline's invalid-response /
/// no-chat-text continuation paths).
const CODE_PROBE_INVALID_RESPONSE: &str = "PROBE_INVALID_RESPONSE";

/// A bounded response detail for issue messages (the body text is the
/// provider's own response, never our request material; 200 chars keeps
/// the stored record small).
fn response_detail(body: &[u8], status: u16) -> String {
    let text = String::from_utf8_lossy(body);
    if let Ok(parsed) = serde_json::from_str::<Value>(text.trim()) {
        for pointer in ["/error/message", "/error/type", "/message"] {
            if let Some(found) = parsed.pointer(pointer).and_then(Value::as_str) {
                return truncate_detail(found);
            }
        }
    }
    if text.trim().is_empty() {
        return format!("{status}");
    }
    truncate_detail(text.trim())
}

fn truncate_detail(text: &str) -> String {
    let mut out: String = text.chars().take(200).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

/// Baseline `buildEndpointCandidates` (providerEndpoint.ts:52-111): a base
/// that already ends with the endpoint path is used as-is; a versioned base
/// gets the plain path; otherwise the versioned candidate comes first and
/// the unversioned one second (tried on 400/404/405, budget permitting).
fn endpoint_candidates(protocol: &str, base: &str, model_id: &str) -> Vec<String> {
    let (default_version, path) = match protocol {
        "anthropic" => ("v1", "messages".to_owned()),
        "openai-responses" => ("v1", "responses".to_owned()),
        "google" => (
            "v1beta",
            format!("models/{}:generateContent", url_encode_path(model_id)),
        ),
        // openai (t016 normalized protocols to the four; anything else
        // never passes provider.write).
        _ => ("v1", "chat/completions".to_owned()),
    };
    let normalized = base.trim().trim_end_matches('/');
    if normalized.is_empty() {
        return Vec::new();
    }
    // Path segments of the base URL (after the authority).
    let after_scheme = normalized.split("://").nth(1).unwrap_or(normalized);
    let segments: Vec<&str> = after_scheme
        .split('/')
        .filter(|s| !s.is_empty())
        .skip(1)
        .collect();
    let path_segments: Vec<String> = path.split('/').map(str::to_owned).collect();
    let lower: Vec<String> = segments.iter().map(|s| s.to_ascii_lowercase()).collect();
    let path_lower: Vec<String> = path_segments
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let ends_with = lower.len() >= path_lower.len()
        && lower[lower.len() - path_lower.len()..] == path_lower[..];
    if ends_with {
        return vec![normalized.to_owned()];
    }
    let is_version = |segment: &str| -> bool {
        let bytes = segment.as_bytes();
        bytes.len() >= 2 && bytes[0] == b'v' && bytes[1..].iter().all(|b| b.is_ascii_digit())
    };
    let unversioned = format!("{normalized}/{}", path.trim_start_matches('/'));
    if segments.iter().any(|s| is_version(s)) {
        return vec![unversioned];
    }
    let versioned = format!(
        "{normalized}/{default_version}/{}",
        path.trim_start_matches('/')
    );
    if versioned == unversioned {
        vec![unversioned]
    } else {
        vec![versioned, unversioned]
    }
}

/// Minimal percent-encoding for a path component (the baseline's
/// `encodeURIComponent` keeps the unreserved set plus `!*'()-`).
fn url_encode_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'~' | b'(' | b')' | b'!' | b'*' | b'\''
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The probe request (headers + JSON body) per protocol — the baseline's
/// `requestFor` (modelConnectionProbe.js) for the four v1 protocols. For an
/// image probe it also returns the color the answer must name.
fn probe_request(
    protocol: &str,
    secret: Option<&str>,
    model_id: &str,
    kind: &str,
) -> (Vec<(&'static str, String)>, Value, Option<&'static str>) {
    let image = kind == "image";
    let mut expected_color: Option<&'static str> = None;
    let image_b64 = if image {
        let (color, rgb) =
            IMAGE_COLORS[Utc::now().timestamp_subsec_nanos() as usize % IMAGE_COLORS.len()];
        expected_color = Some(color);
        solid_png_base64(rgb)
    } else {
        String::new()
    };
    let text = if image {
        IMAGE_PROBE_PROMPT
    } else {
        TEXT_PROBE_PROMPT
    };
    let mut headers: Vec<(&'static str, String)> = Vec::new();
    let body = match protocol {
        "google" => {
            headers.push(("x-goog-api-key", secret.unwrap_or("").to_owned()));
            let mut parts = vec![json!({"text": text})];
            if image {
                parts.push(json!({"inlineData": {"mimeType": "image/png", "data": image_b64}}));
            }
            json!({
                "contents": [{"role": "user", "parts": parts}],
                "generationConfig": {"maxOutputTokens": PROBE_MAX_TOKENS},
            })
        }
        "anthropic" => {
            headers.push(("x-api-key", secret.unwrap_or("").to_owned()));
            headers.push(("anthropic-version", "2023-06-01".to_owned()));
            let content = if image {
                json!([
                    {"type": "text", "text": text},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": image_b64}},
                ])
            } else {
                json!(text)
            };
            json!({
                "model": model_id,
                "max_tokens": PROBE_MAX_TOKENS,
                "messages": [{"role": "user", "content": content}],
            })
        }
        "openai-responses" => {
            if let Some(secret) = secret {
                headers.push(("authorization", format!("Bearer {secret}")));
            }
            let input = if image {
                json!([{"role": "user", "content": [
                    {"type": "input_text", "text": text},
                    {"type": "input_image", "image_url": format!("data:image/png;base64,{image_b64}")},
                ]}])
            } else {
                json!(text)
            };
            json!({
                "model": model_id,
                "max_output_tokens": PROBE_MAX_TOKENS,
                "store": false,
                "input": input,
            })
        }
        // openai
        _ => {
            if let Some(secret) = secret {
                headers.push(("authorization", format!("Bearer {secret}")));
            }
            let content = if image {
                json!([
                    {"type": "text", "text": text},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{image_b64}")}},
                ])
            } else {
                json!(text)
            };
            json!({
                "model": model_id,
                "max_tokens": PROBE_MAX_TOKENS,
                "messages": [{"role": "user", "content": content}],
            })
        }
    };
    headers.push(("content-type", "application/json".to_owned()));
    (headers, body, expected_color)
}

/// The baseline's solid-color 32×32 RGB PNG (`encodeSolidPng`), through the
/// already-locked png crate.
fn solid_png_base64(rgb: [u8; 3]) -> String {
    const SIZE: u32 = 32;
    let mut raw = Vec::with_capacity((SIZE * SIZE * 3) as usize);
    for _ in 0..SIZE {
        for _ in 0..SIZE {
            raw.extend_from_slice(&rgb);
        }
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, SIZE, SIZE);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(&raw);
        }
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Baseline `isExpectedProviderResponseShape`.
fn is_expected_shape(protocol: &str, body: &Value) -> bool {
    match protocol {
        "anthropic" => body["content"].is_array() || body["type"] == "message",
        "google" => body["candidates"].is_array(),
        "openai-responses" => {
            body["object"] == "response"
                || body["output"].is_array()
                || body["output_text"].is_string()
        }
        _ => body["choices"].is_array(),
    }
}

/// Baseline `extractProbeText`: usable model output per protocol.
fn extract_probe_text(protocol: &str, body: &Value) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let push = |chunks: &mut Vec<String>, value: &Value| {
        if let Some(text) = value.as_str()
            && !text.trim().is_empty()
        {
            chunks.push(text.to_owned());
        }
    };
    match protocol {
        "anthropic" => {
            if let Some(content) = body["content"].as_array() {
                for part in content {
                    if part["type"] == "text" {
                        push(&mut chunks, &part["text"]);
                    }
                }
            }
        }
        "google" => {
            if let Some(candidates) = body["candidates"].as_array() {
                for candidate in candidates {
                    if let Some(parts) = candidate["content"]["parts"].as_array() {
                        for part in parts {
                            if part.get("thought").is_none() {
                                push(&mut chunks, &part["text"]);
                            }
                        }
                    }
                }
            }
        }
        "openai-responses" => {
            if let Some(text) = body["output_text"].as_str() {
                chunks.push(text.to_owned());
            } else if let Some(output) = body["output"].as_array() {
                for item in output {
                    if item["type"] == "reasoning" {
                        continue;
                    }
                    if let Some(content) = item["content"].as_array() {
                        for part in content {
                            push(&mut chunks, &part["text"]);
                            push(&mut chunks, &part["output_text"]);
                        }
                    }
                }
            }
        }
        _ => {
            if let Some(choices) = body["choices"].as_array() {
                for choice in choices {
                    let content = &choice["message"]["content"];
                    if content.as_str().is_some() {
                        push(&mut chunks, content);
                    } else if let Some(parts) = content.as_array() {
                        for part in parts {
                            push(&mut chunks, &part["text"]);
                        }
                    }
                    push(&mut chunks, &choice["text"]);
                }
            }
        }
    }
    strip_thinking(&chunks.join(" ")).trim().to_owned()
}

/// Baseline `stripThinking`: drop complete or unterminated think/thinking
/// blocks from the probe answer.
fn strip_thinking(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = find_case_insensitive(rest, "<think") else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        // The longer close tag first so `</thinking>` is not half-consumed
        // by its `</think` prefix.
        let end = find_case_insensitive(after, "</thinking")
            .map(|p| p + "</thinking".len())
            .or_else(|| find_case_insensitive(after, "</think").map(|p| p + "</think".len()));
        match end {
            Some(end) => rest = &after[end..],
            None => return out, // unterminated thinking: drop the tail
        }
    }
}

/// ASCII-case-insensitive substring search over raw bytes (the probe tags
/// are ASCII, so no boundary hazard on the returned offset).
fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// One full HTTP attempt against one candidate URL. Failures are transport
/// errors (connect / timeout / read), reported exactly like the baseline's
/// `TypeError: fetch failed` surface minus the secret material.
async fn send_probe_request(
    url: &str,
    headers: &[(&'static str, String)],
    body: &Value,
    timeout_ms: u64,
) -> Result<(u16, Bytes), String> {
    let config = crate::runtime::model::http::webpki_client_config();
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .build(crate::runtime::model::http::https_connector(config));
    let uri: hyper::Uri = url
        .parse()
        .map_err(|error| format!("invalid probe url: {error}"))?;
    let mut builder = Request::builder().method("POST").uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, value.as_str());
    }
    let request = builder
        .body(Full::new(Bytes::from(body.to_string())))
        .map_err(|error| format!("probe request build failed: {error}"))?;
    let exchange = async {
        let response = client
            .request(request)
            .await
            .map_err(|error| format!("fetch failed: {error}"))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("fetch failed: {error}"))?
            .to_bytes();
        Ok::<(u16, Bytes), String>((status.as_u16(), body))
    };
    match tokio::time::timeout(Duration::from_millis(timeout_ms), exchange).await {
        Ok(result) => result,
        Err(_) => Err("Connection timed out.".to_owned()),
    }
}

/// Baseline `isValidImageColorAnswer`: the answer names the exact color of
/// the probe's solid square.
fn image_answer_names_color(answer: &str, color: &str) -> bool {
    answer.to_ascii_lowercase().contains(color)
}

/// Probes one (target, kind) across the endpoint candidates, budget-bound.
async fn run_kind_probe(
    protocol: &str,
    endpoint: &str,
    secret: Option<&str>,
    model_id: &str,
    kind: &str,
    request_budget: u64,
    timeout_ms: u64,
) -> KindProbeResult {
    if kind != "text" && kind != "image" {
        return KindProbeResult::Undetermined(
            CODE_UNSUPPORTED_TEST_KIND.to_owned(),
            format!("test kind `{kind}` has no implemented probe; supported kinds: text, image"),
        );
    }
    let candidates = endpoint_candidates(protocol, endpoint, model_id);
    if candidates.is_empty() {
        return KindProbeResult::Undetermined(
            CODE_ENDPOINT_UNREACHABLE.to_owned(),
            "provider endpoint is empty".to_owned(),
        );
    }
    let (headers, body, expected_color) = probe_request(protocol, secret, model_id, kind);
    let mut remaining = request_budget.max(1);
    let mut last_detail = String::new();
    let mut last_status: Option<u16> = None;
    let mut last_was_2xx = false;
    for url in &candidates {
        if remaining == 0 {
            break;
        }
        remaining -= 1;
        match send_probe_request(url, &headers, &body, timeout_ms).await {
            // Transport failure (connect / timeout): no evidence, no
            // candidate fallback (the baseline's retry ladder lives inside
            // networkFetch; v1 has no retryPolicy input).
            Err(message) => {
                return KindProbeResult::Undetermined(
                    CODE_ENDPOINT_UNREACHABLE.to_owned(),
                    message,
                );
            }
            Ok((status, response_body)) => {
                last_status = Some(status);
                last_detail = response_detail(&response_body, status);
                if (200..300).contains(&status) {
                    last_was_2xx = true;
                    let parsed: Value =
                        serde_json::from_slice(&response_body).unwrap_or(Value::Null);
                    if !is_expected_shape(protocol, &parsed) {
                        // Baseline: shape mismatch falls through to the next
                        // candidate; a budget of 1 leaves it undetermined.
                        continue;
                    }
                    let text = extract_probe_text(protocol, &parsed);
                    if text.is_empty() {
                        // Valid completion without usable model output.
                        continue;
                    }
                    if kind == "image"
                        && !image_answer_names_color(&text, expected_color.unwrap_or_default())
                    {
                        return KindProbeResult::Undetermined(
                            CODE_IMAGE_CAPABILITY_UNKNOWN.to_owned(),
                            "The model replied without describing the test image.".to_owned(),
                        );
                    }
                    return KindProbeResult::Supported;
                }
                // Non-2xx: an explicit server-side "modality unsupported"
                // statement is definitive negative evidence.
                if kind == "image" && looks_like_image_unsupported(&last_detail) {
                    return KindProbeResult::Unsupported;
                }
                // Baseline fallback statuses try the next candidate.
                if matches!(status, 400 | 404 | 405) && remaining > 0 {
                    continue;
                }
                let code = classify_probe_error(&last_detail, status);
                return KindProbeResult::Undetermined(code.to_owned(), last_detail);
            }
        }
    }
    // Candidates exhausted (budget or shape): undetermined.
    if last_was_2xx {
        return KindProbeResult::Undetermined(
            CODE_PROBE_INVALID_RESPONSE.to_owned(),
            "The endpoint did not return a usable completion response.".to_owned(),
        );
    }
    let status = last_status.unwrap_or(0);
    let code = classify_probe_error(&last_detail, status);
    KindProbeResult::Undetermined(code.to_owned(), last_detail)
}

// ---------------------------------------------------------------------------
// Probe driver
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn spawn_probe_driver(
    store: Arc<V1Store>,
    operation_id: String,
    method_id: String,
    source_operation_id: Option<String>,
    probe_mode: String,
    request_budget: u64,
    timeout_ms: u64,
    idempotency_key: Option<String>,
) {
    tokio::spawn(async move {
        let _ = store.mark_operation_running(&operation_id);
        let outcomes = drive_provider_test(
            &store,
            &operation_id,
            &probe_mode,
            request_budget,
            timeout_ms,
        )
        .await;
        let cancelled = outcomes.iter().any(|(_, state)| state == "cancelled");
        let partial = outcomes
            .iter()
            .any(|(_, state)| state != "succeeded" && state != "cancelled");
        let mut completion = serde_json::Map::new();
        if method_id == "provider_test.retry"
            && let Some(source) = &source_operation_id
        {
            completion.insert("sourceOperationId".into(), json!(source));
        }
        let outcomes_json: Vec<Value> = outcomes
            .iter()
            .map(|(outcome, _)| outcome.clone())
            .collect();
        completion.insert("targets".into(), Value::Array(outcomes_json));
        completion.insert("partial".into(), json!(partial));
        if cancelled {
            let _ = store.finish_long_operation(&operation_id, LongOperationOutcome::Cancelled);
            update_receipt_terminal_state(
                &store,
                idempotency_key.as_deref(),
                &method_id,
                "cancelled",
            );
            return;
        }
        let _ = store.finish_long_operation(
            &operation_id,
            LongOperationOutcome::Succeeded {
                result: Value::Object(completion),
                result_schema_id: Some(format!("agentcpm-pd.{method_id}.completion/1")),
            },
        );
        update_receipt_terminal_state(&store, idempotency_key.as_deref(), &method_id, "succeeded");
    });
}

/// One loaded target row for the driver.
struct PendingTarget {
    target_id: String,
    provider_id: String,
    model_id: Option<String>,
    protocol: String,
    endpoint: String,
    config_digest: String,
    credential_ref: Option<String>,
    test_kinds: Vec<String>,
}

/// Loads the operation's target rows and probes them in order. Returns one
/// (TestOutcome JSON, state) pair per target. Cancellation between targets
/// marks the remaining targets `cancelled`.
async fn drive_provider_test(
    store: &Arc<V1Store>,
    operation_id: &str,
    probe_mode: &str,
    request_budget: u64,
    timeout_ms: u64,
) -> Vec<(Value, String)> {
    let pending: Vec<PendingTarget> = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let Ok(mut stmt) = conn.prepare(
            "SELECT target_id, provider_id, model_id, protocol, endpoint, config_digest,
                    credential_ref, test_kinds_json
             FROM v1_provider_tests WHERE operation_id = ?1 ORDER BY rowid ASC",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = stmt
            .query_map(params![operation_id], |r| {
                Ok(PendingTarget {
                    target_id: r.get(0)?,
                    provider_id: r.get(1)?,
                    model_id: r.get(2)?,
                    protocol: r.get(3)?,
                    endpoint: r.get(4)?,
                    config_digest: r.get(5)?,
                    credential_ref: r.get(6)?,
                    test_kinds: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                })
            })
            .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
        else {
            return Vec::new();
        };
        rows
    };

    let mut outcomes = Vec::with_capacity(pending.len());
    let mut cancelled_seen = false;
    for target in pending {
        if cancelled_seen || matches!(store.operation_cancel_requested(operation_id), Ok(true)) {
            cancelled_seen = true;
            let _ = update_target_outcome(
                store,
                operation_id,
                &target.target_id,
                "cancelled",
                0,
                &[],
                &[],
            );
            outcomes.push((
                target_outcome(
                    &target.target_id,
                    "cancelled",
                    &target.config_digest,
                    0,
                    &[],
                    &[],
                ),
                "cancelled".to_owned(),
            ));
            continue;
        }
        let started = Instant::now();
        let mut issues: Vec<Value> = Vec::new();
        let mut evidence_ids: Vec<String> = Vec::new();
        let mut undetermined = false;
        for kind in &target.test_kinds {
            let result = if probe_mode == "network" {
                // Fail-closed secret resolution: missing/disabled credential
                // is a runtime condition, not a guess.
                let secret = match target.credential_ref.as_deref() {
                    Some(cref) => store.resolve_credential_secret(cref).unwrap_or(None),
                    None => None,
                };
                if target.credential_ref.is_some() && secret.is_none() {
                    KindProbeResult::Undetermined(
                        CODE_AUTHENTICATION_FAILED.to_owned(),
                        format!(
                            "credential `{}` is missing or disabled",
                            target.credential_ref.as_deref().unwrap_or_default()
                        ),
                    )
                } else {
                    run_kind_probe(
                        &target.protocol,
                        &target.endpoint,
                        secret.as_deref(),
                        target.model_id.as_deref().unwrap_or_default(),
                        kind,
                        request_budget,
                        timeout_ms,
                    )
                    .await
                }
            } else {
                // validateOnly: zero network — nothing to determine and no
                // evidence is ever produced by a validation-only pass.
                KindProbeResult::Supported
            };
            match result {
                KindProbeResult::Supported if probe_mode == "network" => {
                    if let Some(evidence_id) = record_evidence(
                        store,
                        operation_id,
                        &target.target_id,
                        &target.provider_id,
                        target.model_id.as_deref(),
                        &target.config_digest,
                        kind,
                        true,
                    ) {
                        evidence_ids.push(evidence_id);
                    }
                }
                KindProbeResult::Unsupported => {
                    if let Some(evidence_id) = record_evidence(
                        store,
                        operation_id,
                        &target.target_id,
                        &target.provider_id,
                        target.model_id.as_deref(),
                        &target.config_digest,
                        kind,
                        false,
                    ) {
                        evidence_ids.push(evidence_id);
                    }
                }
                KindProbeResult::Undetermined(code, message) => {
                    undetermined = true;
                    issues.push(json!({"code": code, "field": kind, "message": message}));
                }
                // validateOnly's vacuous success: no evidence, no issue.
                KindProbeResult::Supported => {}
            }
        }
        let state = if undetermined { "failed" } else { "succeeded" };
        let elapsed_ms = started.elapsed().as_millis() as i64;
        let outcome = target_outcome(
            &target.target_id,
            state,
            &target.config_digest,
            elapsed_ms,
            &evidence_ids,
            &issues,
        );
        // Persist the per-target outcome — `provider_test.retry`'s
        // explicit-failure gate reads this state.
        let _ = update_target_outcome(
            store,
            operation_id,
            &target.target_id,
            state,
            elapsed_ms,
            &evidence_ids,
            &issues,
        );
        let _ = store.record_operation_progress(
            operation_id,
            &format!("target:{}", target.target_id),
            &json!({"state": state}),
        );
        outcomes.push((outcome, state.to_owned()));
    }
    outcomes
}

fn update_target_outcome(
    store: &Arc<V1Store>,
    operation_id: &str,
    target_id: &str,
    state: &str,
    elapsed_ms: i64,
    evidence_ids: &[String],
    issues: &[Value],
) -> Result<(), ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "UPDATE v1_provider_tests
         SET state = ?1, elapsed_ms = ?2, evidence_ids_json = ?3, issues_json = ?4, updated_at = ?5
         WHERE operation_id = ?6 AND target_id = ?7",
        params![
            state,
            elapsed_ms,
            serde_json::to_string(evidence_ids).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(issues).unwrap_or_else(|_| "[]".into()),
            now_rfc3339(),
            operation_id,
            target_id,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn target_outcome(
    target_id: &str,
    state: &str,
    config_digest: &str,
    elapsed_ms: i64,
    evidence_ids: &[String],
    issues: &[Value],
) -> Value {
    json!({
        "targetId": target_id,
        "state": state,
        "configDigest": config_digest,
        "elapsedMs": elapsed_ms,
        "evidenceIds": evidence_ids,
        "issues": issues,
    })
}

/// Persists one evidence row and returns its id.
#[allow(clippy::too_many_arguments)]
fn record_evidence(
    store: &Arc<V1Store>,
    operation_id: &str,
    target_id: &str,
    provider_id: &str,
    model_id: Option<&str>,
    config_digest: &str,
    kind: &str,
    supported: bool,
) -> Option<String> {
    let evidence_id = store.next_id();
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "INSERT INTO v1_test_evidence (evidence_id, operation_id, target_id, provider_id, model_id, config_digest, kind, supported, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            evidence_id,
            operation_id,
            target_id,
            provider_id,
            model_id,
            config_digest,
            kind,
            i64::from(supported),
            now_rfc3339(),
        ],
    )
    .ok()?;
    Some(evidence_id)
}

// ---------------------------------------------------------------------------
// provider_test.acknowledge
// ---------------------------------------------------------------------------

async fn provider_test_acknowledge(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let operation_id = input["operationId"].as_str().unwrap_or_default().to_owned();
    let expected_revision = input["expectedRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;
    let row: Option<(String, i64, String)> = tx
        .query_row(
            "SELECT operation_type, terminal, revision FROM operations WHERE operation_id = ?1",
            params![operation_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((operation_type, terminal, revision)) = row else {
        return Err(invalid(format!("operation `{operation_id}` not found")).into());
    };
    if operation_type != "provider_test.start" && operation_type != "provider_test.retry" {
        return Err(invalid(format!(
            "operation `{operation_id}` is not a provider test operation"
        ))
        .into());
    }
    // 「运行中不得ack成终态」: acknowledging a non-terminal operation must
    // not fake a terminal record.
    if terminal == 0 {
        return Err(ContractError::new(
            CODE_OPERATION_NOT_READY,
            format!("operation `{operation_id}` has not reached a terminal state"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    if revision != expected_revision {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!("expectedRevision `{expected_revision}` does not match `{revision}`"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    // The CAS window is closed inside this transaction: nothing else can
    // move the revision between the check above and the writes below.
    let new_rev = next_rev_in_tx(&tx)?;
    let now = now_rfc3339();
    tx.execute(
        "INSERT INTO v1_provider_test_state (operation_id, acknowledged, acknowledged_at)
         VALUES (?1, 1, ?2)
         ON CONFLICT(operation_id) DO UPDATE SET acknowledged = 1, acknowledged_at = excluded.acknowledged_at",
        params![operation_id, now],
    )
    .map_err(sql_err)?;
    tx.execute(
        "UPDATE operations SET revision = ?1, updated_at = ?2 WHERE operation_id = ?3",
        params![new_rev, now, operation_id],
    )
    .map_err(sql_err)?;
    append_acknowledge_event_in_tx(&tx, &operation_id)?;
    tx.commit().map_err(sql_err)?;
    drop(conn);
    store.event_notifier().notify_waiters();

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "operationId": operation_id,
            "acknowledged": true,
            "revision": new_rev,
        }),
    ))
}

fn next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let next: i64 = conn
        .query_row(
            "SELECT COALESCE(CAST(value AS INTEGER), 0) + 1 FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("rev-{next:08}"))
}

/// Appends the `operation.progress` event carrying the acknowledgment (the
/// same events-table shape the collections seam's own progress writes use,
/// so `operations.events` shows the human confirmation).
fn append_acknowledge_event_in_tx(
    conn: &Connection,
    operation_id: &str,
) -> Result<(), ContractError> {
    let placeholder = format!("tmp-pt-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let payload = serde_json::to_string(&json!({
        "operationId": operation_id,
        "progress": {"key": "acknowledged", "value": true},
    }))
    .unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', 'operation.progress', ?3, ?4, ?4, ?5)",
        params![
            placeholder,
            operation_id,
            OPERATION_EVENT_SCHEMA,
            payload,
            now_rfc3339(),
        ],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![format_cursor(seq), seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// provider_test.confirm_capabilities
// ---------------------------------------------------------------------------

async fn provider_test_confirm_capabilities(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let operation_id = input["operationId"].as_str().unwrap_or_default().to_owned();
    let reference = &input["model"];
    let provider_id = reference["providerId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let model_id = reference["modelId"].as_str().unwrap_or_default().to_owned();
    let expected_model_revision = input["expectedModelRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let evidence_ids: Vec<String> = input["evidenceIds"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let capabilities: Vec<Value> = input["capabilities"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // Pair validation: non-empty unique keys.
    let mut seen_keys = std::collections::BTreeSet::new();
    let mut pairs: Vec<(String, Value)> = Vec::with_capacity(capabilities.len());
    for pair in &capabilities {
        let key = pair["key"].as_str().unwrap_or_default().to_owned();
        if key.is_empty() {
            return Err(invalid("capability key must be a non-empty string").into());
        }
        if !seen_keys.insert(key.clone()) {
            return Err(invalid(format!("duplicate capability key `{key}`")).into());
        }
        pairs.push((key, pair.get("value").cloned().unwrap_or(Value::Null)));
    }

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;

    // The operation must be one of ours (its evidence is what is being
    // confirmed). Terminality is deliberately not required here — the
    // contract's error list for this operation has no not-ready code, and
    // every evidence row carries its own config binding which is checked
    // below against the *current* configuration.
    let op_type: Option<String> = tx
        .query_row(
            "SELECT operation_type FROM operations WHERE operation_id = ?1",
            params![operation_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    match op_type.as_deref() {
        Some("provider_test.start") | Some("provider_test.retry") => {}
        Some(other) => {
            return Err(invalid(format!(
                "operation `{operation_id}` is `{other}`, not a provider test operation"
            ))
            .into());
        }
        None => {
            return Err(invalid(format!("operation `{operation_id}` not found")).into());
        }
    }

    // Model CAS: 「陈旧版本不写」.
    let model_row: Option<(String, String, i64, String, String)> = tx
        .query_row(
            "SELECT name, revision, enabled, declared_capabilities_json, capability_evidence_json
             FROM v1_models WHERE provider_id = ?1 AND model_id = ?2",
            params![provider_id, model_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((name, model_revision, enabled, declared_raw, evidence_raw)) = model_row else {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!("model `{provider_id}/{model_id}` not found"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    };
    if model_revision != expected_model_revision {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "expectedModelRevision `{expected_model_revision}` does not match `{model_revision}`"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    // Current config digest: evidence must have been gathered against the
    // provider's live (protocol, endpoint, credentialRef) and the same
    // model （「证据必须同模型同配置」「变更endpoint/model/凭据后须重新确认」）.
    let Some(provider) = load_tested_provider(&tx, &provider_id)? else {
        return Err(
            config_revision_conflict(format!("provider `{provider_id}` no longer exists")).into(),
        );
    };
    let current_digest = provider_config_digest(
        &provider.protocol,
        &provider.endpoint,
        provider.credential_ref.as_deref(),
        Some(&model_id),
    );

    // Load and validate every cited evidence row.
    let mut evidence: Vec<(String, bool)> = Vec::with_capacity(evidence_ids.len());
    for evidence_id in &evidence_ids {
        let row: Option<(String, Option<String>, String, String, i64)> = tx
            .query_row(
                "SELECT provider_id, model_id, config_digest, kind, supported
                 FROM v1_test_evidence WHERE evidence_id = ?1",
                params![evidence_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((ev_provider, ev_model, ev_digest, kind, supported)) = row else {
            return Err(ContractError::new(
                CODE_EVIDENCE_MISMATCH,
                format!("evidence `{evidence_id}` does not exist"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        };
        if ev_provider != provider_id || ev_model.as_deref() != Some(model_id.as_str()) {
            return Err(ContractError::new(
                CODE_EVIDENCE_MISMATCH,
                format!(
                    "evidence `{evidence_id}` was gathered for `{ev_provider}/{}`; the confirmation targets `{provider_id}/{model_id}`",
                    ev_model.as_deref().unwrap_or_default()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        if ev_digest != current_digest {
            return Err(ContractError::new(
                CODE_EVIDENCE_MISMATCH,
                format!(
                    "evidence `{evidence_id}` was gathered against a different configuration (endpoint/model/credential changed since); re-test before confirming"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        evidence.push((kind, supported != 0));
    }

    // Capability ↔ evidence matching.
    for (key, value) in &pairs {
        let matching: Vec<bool> = evidence
            .iter()
            .filter(|(kind, _)| kind == key)
            .map(|(_, supported)| *supported)
            .collect();
        if matching.is_empty() {
            return Err(ContractError::new(
                CODE_INSUFFICIENT_EVIDENCE,
                format!(
                    "capability `{key}` has no `{key}`-kind evidence; text-only success never establishes image capability"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        // Boolean-shaped confirmations must agree with the evidence; other
        // value shapes only require the evidence to exist (the contract's
        // Pair value is a domain-typed JSON value).
        let wants_supported = match value {
            Value::Bool(want) => Some(*want),
            Value::String(want) if want.as_str() == "supported" => Some(true),
            Value::String(want) if want.as_str() == "unsupported" => Some(false),
            _ => None,
        };
        if let Some(want) = wants_supported
            && !matching.contains(&want)
        {
            return Err(ContractError::new(
                CODE_EVIDENCE_MISMATCH,
                format!("evidence for `{key}` contradicts the confirmed value"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
    }

    // Merge the confirmed pairs into capabilityEvidence (upsert by key) and
    // bump the model revision.
    let mut evidence_pairs: Vec<Value> = serde_json::from_str(&evidence_raw).unwrap_or_default();
    for (key, value) in &pairs {
        if let Some(existing) = evidence_pairs
            .iter_mut()
            .find(|pair| pair["key"].as_str() == Some(key.as_str()))
        {
            if let Some(obj) = existing.as_object_mut() {
                obj.insert("value".into(), value.clone());
            }
        } else {
            evidence_pairs.push(json!({"key": key, "value": value}));
        }
    }
    let new_rev = next_rev_in_tx(&tx)?;
    let now = now_rfc3339();
    tx.execute(
        "UPDATE v1_models
         SET capability_evidence_json = ?1, revision = ?2, updated_at = ?3
         WHERE provider_id = ?4 AND model_id = ?5",
        params![
            serde_json::to_string(&evidence_pairs).unwrap_or_else(|_| "[]".into()),
            new_rev,
            now,
            provider_id,
            model_id,
        ],
    )
    .map_err(sql_err)?;
    tx.commit().map_err(sql_err)?;
    drop(conn);
    store.event_notifier().notify_waiters();

    Ok((
        ResponseOutcome::Succeeded,
        json!({"model": {
            "reference": {"providerId": provider_id, "modelId": model_id},
            "name": name,
            "enabled": enabled != 0,
            "revision": new_rev,
            "capabilities": serde_json::from_str::<Value>(&declared_raw).unwrap_or(json!([])),
            "capabilityEvidence": Value::Array(evidence_pairs),
        }}),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use agentcpm_pd_protocol::registry::OperationRegistry;

    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;

    use super::*;

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
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, instance_id) = open_runtime(&root_dir);
        (tmp, rt, instance_id)
    }

    fn contract_code_of(err: &DispatchError) -> String {
        match err {
            DispatchError::Application(e) => e.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    async fn seed_provider(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        provider_id: &str,
        endpoint: &str,
    ) -> String {
        let (_, result) = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": provider_id,
                    "name": format!("{provider_id} display"),
                    "protocol": "openai",
                    "endpoint": endpoint,
                }),
                None,
                None,
            )
            .await
            .expect("seed provider");
        result["provider"]["revision"].as_str().unwrap().to_owned()
    }

    async fn wait_operation_terminal(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        loop {
            // `instanceId` is injected by dispatch for omitted params.
            let (_, got) = rt
                .dispatch(
                    "operations.get",
                    json!({"operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .expect("operations.get");
            if got["operation"]["state"].as_str().is_some_and(|s| {
                matches!(
                    s,
                    "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
                )
            }) {
                return got["operation"].clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn target(provider_id: &str, revision: &str, model: Option<&str>, kinds: &[&str]) -> Value {
        json!({
            "targetId": format!("t-{provider_id}"),
            "providerId": provider_id,
            "modelId": model,
            "configRevision": revision,
            "testKinds": kinds,
        })
    }

    #[tokio::test]
    async fn validate_only_is_zero_network_and_records_no_evidence() {
        let (_tmp, rt, iid) = fresh_home("pt-validate");
        let rev = seed_provider(&rt, &iid, "prov-a", "https://api.example.com/v1").await;
        let (outcome, receipt) = rt
            .dispatch(
                "provider_test.start",
                json!({
                    "instanceId": iid,
                    "targets": [target("prov-a", &rev, Some("m-1"), &["text", "image"])],
                    "probeMode": "validateOnly",
                }),
                Some("key-validate-1"),
                None,
            )
            .await
            .expect("validateOnly accepts");
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result("provider_test.start", &receipt)
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_operation_terminal(&rt, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        let result = operation["result"].clone();
        assert_eq!(result["partial"], false);
        assert_eq!(result["targets"].as_array().map(Vec::len), Some(1));
        let outcome_row = &result["targets"][0];
        assert_eq!(outcome_row["state"], "succeeded");
        // 「validateOnly零网络」/ evidence discipline: no evidence rows.
        assert_eq!(outcome_row["evidenceIds"].as_array().map(Vec::len), Some(0));
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let evidence: i64 = conn
            .query_row("SELECT COUNT(*) FROM v1_test_evidence", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(evidence, 0);
    }

    #[tokio::test]
    async fn start_preaccept_rejects_stale_and_missing_config() {
        let (_tmp, rt, iid) = fresh_home("pt-preaccept");
        let rev = seed_provider(&rt, &iid, "prov-b", "https://api.example.com/v1").await;
        // Stale revision.
        let err = rt
            .dispatch(
                "provider_test.start",
                json!({
                    "instanceId": iid,
                    "targets": [target("prov-b", "rev-00000001", Some("m-1"), &["text"])],
                }),
                Some("key-stale-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "CONFIG_REVISION_CONFLICT");
        // Missing provider.
        let err = rt
            .dispatch(
                "provider_test.start",
                json!({
                    "instanceId": iid,
                    "targets": [target("prov-missing", &rev, None, &[])],
                }),
                Some("key-missing-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "CONFIG_REVISION_CONFLICT");
        // Same key, same input after a recorded rejection replays the
        // rejection deterministically.
        let err = rt
            .dispatch(
                "provider_test.start",
                json!({
                    "instanceId": iid,
                    "targets": [target("prov-b", "rev-00000001", Some("m-1"), &["text"])],
                }),
                Some("key-stale-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "CONFIG_REVISION_CONFLICT");
        // Zero operations were created by the rejected attempts.
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE operation_type = 'provider_test.start'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn idempotent_replay_and_conflict() {
        let (_tmp, rt, iid) = fresh_home("pt-idem");
        let rev = seed_provider(&rt, &iid, "prov-c", "https://api.example.com/v1").await;
        let input = json!({
            "instanceId": iid,
            "targets": [target("prov-c", &rev, Some("m-1"), &["text"])],
            "probeMode": "validateOnly",
        });
        let (outcome, first) = rt
            .dispatch(
                "provider_test.start",
                input.clone(),
                Some("key-idem-1"),
                None,
            )
            .await
            .expect("accepts");
        assert_eq!(outcome, ResponseOutcome::Accepted);
        assert_eq!(first["deduplicated"], false);
        // Same key + same input: replay with deduplicated=true, same
        // operationId, no second operation.
        let (_, replay) = rt
            .dispatch(
                "provider_test.start",
                input.clone(),
                Some("key-idem-1"),
                None,
            )
            .await
            .expect("replays");
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"], first["operationId"]);
        // requests.lookup surfaces the Gateway resource id.
        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({
                    "instanceId": iid,
                    "methodId": "provider_test.start",
                    "idempotencyKey": "key-idem-1",
                }),
                None,
                None,
            )
            .await
            .expect("lookup");
        assert_eq!(lookup["resource"]["operationId"], first["operationId"]);
        // Same key + different input: conflict.
        let mut other = input.clone();
        other["targets"][0]["modelId"] = json!("m-2");
        let err = rt
            .dispatch("provider_test.start", other, Some("key-idem-1"), None)
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "IDEMPOTENCY_CONFLICT");
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE operation_type = 'provider_test.start'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn acknowledge_semantics() {
        let (_tmp, rt, iid) = fresh_home("pt-ack");
        let rev = seed_provider(&rt, &iid, "prov-d", "https://api.example.com/v1").await;
        let (_, receipt) = rt
            .dispatch(
                "provider_test.start",
                json!({
                    "instanceId": iid,
                    "targets": [target("prov-d", &rev, None, &[])],
                    "probeMode": "validateOnly",
                }),
                Some("key-ack-1"),
                None,
            )
            .await
            .expect("accepts");
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_operation_terminal(&rt, &operation_id).await;
        let rev1 = operation["revision"].as_str().unwrap().to_owned();

        // 「运行中不得ack成终态」: a queued/running provider-test operation
        // refuses with OPERATION_NOT_READY (deterministic through a row the
        // seam inserted but no driver owns).
        let (stuck_id, stuck_rev) = rt
            .store()
            .begin_long_operation("provider_test.start", &json!({"kind": "instance"}), true)
            .unwrap();
        let err = rt
            .dispatch(
                "provider_test.acknowledge",
                json!({
                    "instanceId": iid,
                    "operationId": stuck_id,
                    "expectedRevision": stuck_rev,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "OPERATION_NOT_READY");

        let (_, acked) = rt
            .dispatch(
                "provider_test.acknowledge",
                json!({
                    "instanceId": iid,
                    "operationId": operation_id,
                    "expectedRevision": rev1,
                }),
                None,
                None,
            )
            .await
            .expect("acknowledge");
        OperationRegistry::global()
            .validate_result("provider_test.acknowledge", &acked)
            .unwrap();
        assert_eq!(acked["acknowledged"], true);
        let rev2 = acked["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev1, rev2);

        // Stale revision: conflict; the outcome itself never changed
        // (「不把失败改成功」— acknowledge touches no result fields).
        let err = rt
            .dispatch(
                "provider_test.acknowledge",
                json!({
                    "instanceId": iid,
                    "operationId": operation_id,
                    "expectedRevision": rev1,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");
        let (_, after) = rt
            .dispatch(
                "operations.get",
                json!({"operationId": operation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(after["operation"]["state"], operation["state"]);
        assert_eq!(after["operation"]["result"], operation["result"]);

        // Re-acknowledge with the current revision is idempotent.
        let (_, again) = rt
            .dispatch(
                "provider_test.acknowledge",
                json!({
                    "instanceId": iid,
                    "operationId": operation_id,
                    "expectedRevision": rev2,
                }),
                None,
                None,
            )
            .await
            .expect("re-acknowledge");
        assert_eq!(again["acknowledged"], true);

        // Non-test operations refuse acknowledge.
        let err = rt
            .dispatch(
                "provider_test.acknowledge",
                json!({
                    "instanceId": iid,
                    "operationId": "op-not-a-test",
                    "expectedRevision": "rev-00000001",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn retry_semantics() {
        let (_tmp, rt, iid) = fresh_home("pt-retry");
        // A loopback endpoint on a guaranteed-closed port: the probe fails
        // per item without leaving the machine.
        let dead_endpoint = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            format!("http://127.0.0.1:{port}")
        };
        let rev = seed_provider(&rt, &iid, "prov-e", &dead_endpoint).await;
        let (_, first) = rt
            .dispatch(
                "provider_test.start",
                json!({
                    "instanceId": iid,
                    "targets": [target("prov-e", &rev, Some("m-1"), &["text"])],
                    "probeMode": "network",
                }),
                Some("key-retry-1"),
                None,
            )
            .await
            .expect("accepts");
        let source_id = first["operationId"].as_str().unwrap().to_owned();
        let operation = wait_operation_terminal(&rt, &source_id).await;
        // The probe failed per item but the operation records the per-item
        // outcomes (partial=true).
        assert_eq!(operation["state"], "succeeded");
        assert_eq!(operation["result"]["partial"], true);
        assert_eq!(operation["result"]["targets"][0]["state"], "failed");
        assert_eq!(
            operation["result"]["targets"][0]["issues"][0]["code"],
            "ENDPOINT_UNREACHABLE"
        );
        let source_result = operation["result"].clone();

        // A missing source operation is an input error.
        let err = rt
            .dispatch(
                "provider_test.retry",
                json!({
                    "instanceId": iid,
                    "sourceOperationId": "op-does-not-exist",
                    "targetIds": ["t-prov-e"],
                }),
                Some("key-retry-bad"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");

        // A still-running source operation refuses the retry
        // (SOURCE_NOT_TERMINAL) — deterministic through a seam-inserted row
        // no driver owns.
        let (stuck_id, _stuck_rev) = rt
            .store()
            .begin_long_operation("provider_test.start", &json!({"kind": "instance"}), true)
            .unwrap();
        let err = rt
            .dispatch(
                "provider_test.retry",
                json!({
                    "instanceId": iid,
                    "sourceOperationId": stuck_id,
                    "targetIds": ["t-prov-e"],
                }),
                Some("key-retry-running"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "SOURCE_NOT_TERMINAL");

        // Successful retry of the failed target: the new operation keeps
        // the source; the original record is untouched.
        let (_, second) = rt
            .dispatch(
                "provider_test.retry",
                json!({
                    "instanceId": iid,
                    "sourceOperationId": source_id,
                    "targetIds": ["t-prov-e"],
                    "probeMode": "validateOnly",
                }),
                Some("key-retry-2"),
                None,
            )
            .await
            .expect("retry accepts");
        OperationRegistry::global()
            .validate_result("provider_test.retry", &second)
            .unwrap();
        let retry_id = second["operationId"].as_str().unwrap().to_owned();
        assert_ne!(retry_id, source_id);
        let retry_op = wait_operation_terminal(&rt, &retry_id).await;
        assert_eq!(
            retry_op["result"]["sourceOperationId"],
            json!(source_id),
            "新operation保留来源"
        );
        // 原失败记录不改.
        let (_, source_after) = rt
            .dispatch(
                "operations.get",
                json!({"operationId": source_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(source_after["operation"]["result"], source_result);
        assert_eq!(source_after["operation"]["revision"], operation["revision"]);

        // Retrying a succeeded target is refused.
        let err = rt
            .dispatch(
                "provider_test.retry",
                json!({
                    "instanceId": iid,
                    "sourceOperationId": retry_id,
                    "targetIds": ["t-prov-e"],
                }),
                Some("key-retry-3"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");

        // 旧revision不能偷偷测试新配置: bump the provider and retry the
        // original failed target again.
        let (_, updated) = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-e",
                    "expectedRevision": rev,
                    "name": "prov-e display",
                    "protocol": "openai",
                    "endpoint": format!("{dead_endpoint}/v2"),
                }),
                None,
                None,
            )
            .await
            .expect("provider update");
        assert_ne!(updated["provider"]["revision"], json!(rev));
        let err = rt
            .dispatch(
                "provider_test.retry",
                json!({
                    "instanceId": iid,
                    "sourceOperationId": source_id,
                    "targetIds": ["t-prov-e"],
                }),
                Some("key-retry-4"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "CONFIG_REVISION_CONFLICT");
    }

    #[tokio::test]
    async fn confirm_capabilities_evidence_rules() {
        let (_tmp, rt, iid) = fresh_home("pt-confirm");
        // Seed provider + credential + model.
        rt.dispatch(
            "credential.write",
            json!({
                "instanceId": iid,
                "credentialId": "cred-pt",
                "kind": "api_key",
                "name": "pt credential",
                "secret": "secret-pt-value",
            }),
            None,
            None,
        )
        .await
        .expect("seed credential");
        let (_, provider) = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-f",
                    "name": "prov f",
                    "protocol": "openai",
                    "endpoint": "https://api.example.com/v1",
                    "credentialRef": "cred-pt",
                }),
                None,
                None,
            )
            .await
            .expect("seed provider");
        let provider_rev = provider["provider"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, model) = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-f", "modelId": "m-img"},
                    "name": "image model",
                }),
                None,
                None,
            )
            .await
            .expect("seed model");
        let model_rev = model["model"]["revision"].as_str().unwrap().to_owned();
        assert_eq!(
            model["model"]["capabilityEvidence"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );

        // Inject evidence rows directly for the *current* config: text and
        // image for the target model, plus image evidence for a different
        // model.
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let digest = provider_config_digest(
                "openai",
                "https://api.example.com/v1",
                Some("cred-pt"),
                Some("m-img"),
            );
            let now = now_rfc3339();
            let insert = |evidence_id: &str, model: &str, kind: &str, supported: bool| {
                conn.execute(
                    "INSERT INTO v1_test_evidence (evidence_id, operation_id, target_id, provider_id, model_id, config_digest, kind, supported, created_at)
                     VALUES (?1, 'op-pt-x', 't-1', 'prov-f', ?2, ?3, ?4, ?5, ?6)",
                    params![evidence_id, model, digest, kind, i64::from(supported), now],
                )
                .unwrap();
            };
            insert("ev-text", "m-img", "text", true);
            insert("ev-image", "m-img", "image", true);
            insert("ev-other-model", "m-other", "image", true);
        }

        // text-only cannot confirm image (INSUFFICIENT_EVIDENCE).
        let err = rt
            .dispatch(
                "provider_test.confirm_capabilities",
                json!({
                    "instanceId": iid,
                    "operationId": "op-pt-x",
                    "model": {"providerId": "prov-f", "modelId": "m-img"},
                    "expectedModelRevision": model_rev,
                    "evidenceIds": ["ev-text"],
                    "capabilities": [{"key": "image", "value": true}],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INSUFFICIENT_EVIDENCE");

        // Evidence from a different model: EVIDENCE_MISMATCH.
        let err = rt
            .dispatch(
                "provider_test.confirm_capabilities",
                json!({
                    "instanceId": iid,
                    "operationId": "op-pt-x",
                    "model": {"providerId": "prov-f", "modelId": "m-img"},
                    "expectedModelRevision": model_rev,
                    "evidenceIds": ["ev-other-model"],
                    "capabilities": [{"key": "image", "value": true}],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "EVIDENCE_MISMATCH");

        // Missing evidence id: EVIDENCE_MISMATCH.
        let err = rt
            .dispatch(
                "provider_test.confirm_capabilities",
                json!({
                    "instanceId": iid,
                    "operationId": "op-pt-x",
                    "model": {"providerId": "prov-f", "modelId": "m-img"},
                    "expectedModelRevision": model_rev,
                    "evidenceIds": ["ev-nope"],
                    "capabilities": [{"key": "image", "value": true}],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "EVIDENCE_MISMATCH");

        // Confirmed with matching image evidence: capabilityEvidence gains
        // the pairs, revision bumps.
        let (_, confirmed) = rt
            .dispatch(
                "provider_test.confirm_capabilities",
                json!({
                    "instanceId": iid,
                    "operationId": "op-pt-x",
                    "model": {"providerId": "prov-f", "modelId": "m-img"},
                    "expectedModelRevision": model_rev,
                    "evidenceIds": ["ev-text", "ev-image"],
                    "capabilities": [
                        {"key": "text", "value": true},
                        {"key": "image", "value": true},
                    ],
                }),
                None,
                None,
            )
            .await
            .expect("confirm");
        OperationRegistry::global()
            .validate_result("provider_test.confirm_capabilities", &confirmed)
            .unwrap();
        let new_model_rev = confirmed["model"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(new_model_rev, model_rev);
        let evidence = confirmed["model"]["capabilityEvidence"]
            .as_array()
            .unwrap()
            .clone();
        assert!(
            evidence
                .iter()
                .any(|p| p["key"] == "image" && p["value"] == true)
        );
        assert!(
            evidence
                .iter()
                .any(|p| p["key"] == "text" && p["value"] == true)
        );

        // 陈旧版本不写: the old model revision conflicts now.
        let err = rt
            .dispatch(
                "provider_test.confirm_capabilities",
                json!({
                    "instanceId": iid,
                    "operationId": "op-pt-x",
                    "model": {"providerId": "prov-f", "modelId": "m-img"},
                    "expectedModelRevision": model_rev,
                    "evidenceIds": ["ev-image"],
                    "capabilities": [{"key": "image", "value": true}],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // 变更endpoint后须重新确认: change the provider endpoint (new config
        // digest) and the old evidence no longer matches.
        rt.dispatch(
            "provider.write",
            json!({
                "instanceId": iid,
                "providerId": "prov-f",
                "expectedRevision": provider_rev,
                "name": "prov f",
                "protocol": "openai",
                "endpoint": "https://api.example.com/v2",
                "credentialRef": "cred-pt",
            }),
            None,
            None,
        )
        .await
        .expect("provider update");
        let err = rt
            .dispatch(
                "provider_test.confirm_capabilities",
                json!({
                    "instanceId": iid,
                    "operationId": "op-pt-x",
                    "model": {"providerId": "prov-f", "modelId": "m-img"},
                    "expectedModelRevision": new_model_rev,
                    "evidenceIds": ["ev-image"],
                    "capabilities": [{"key": "image", "value": true}],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "EVIDENCE_MISMATCH");
    }

    #[test]
    fn endpoint_candidates_follow_the_baseline() {
        // Preset-style base without a version segment: versioned first,
        // unversioned second.
        assert_eq!(
            endpoint_candidates("anthropic", "https://api.anthropic.com", "m"),
            vec![
                "https://api.anthropic.com/v1/messages".to_owned(),
                "https://api.anthropic.com/messages".to_owned(),
            ]
        );
        // Versioned base: single candidate.
        assert_eq!(
            endpoint_candidates("openai", "https://api.openai.com/v1", "gpt-x"),
            vec!["https://api.openai.com/v1/chat/completions".to_owned()]
        );
        // Base that already ends with the endpoint path: as-is.
        assert_eq!(
            endpoint_candidates(
                "openai-responses",
                "https://proxy.example.com/api/responses",
                "gpt-x"
            ),
            vec!["https://proxy.example.com/api/responses".to_owned()]
        );
        // Google embeds the (encoded) model.
        assert_eq!(
            endpoint_candidates("google", "https://generativelanguage.googleapis.com", "gemini x"),
            vec![
                "https://generativelanguage.googleapis.com/v1beta/models/gemini%20x:generateContent".to_owned(),
                "https://generativelanguage.googleapis.com/models/gemini%20x:generateContent".to_owned(),
            ]
        );
    }

    #[test]
    fn image_unsupported_detection_and_classification() {
        assert!(looks_like_image_unsupported(
            "Image input is not supported for this model"
        ));
        assert!(looks_like_image_unsupported(
            "this model does not support vision requests"
        ));
        assert!(!looks_like_image_unsupported("rate limited"));
        assert_eq!(
            classify_probe_error("bad api key", 401),
            CODE_AUTHENTICATION_FAILED
        );
        assert_eq!(
            classify_probe_error("model gpt-x not found", 400),
            CODE_MODEL_NOT_FOUND
        );
        assert_eq!(classify_probe_error("boom", 500), CODE_ENDPOINT_UNREACHABLE);
    }

    #[test]
    fn strip_thinking_removes_reasoning_blocks() {
        assert_eq!(strip_thinking("a<think SECRET</thinkokb"), "ab");
        assert_eq!(strip_thinking("a<thinking SECRET</thinking>b"), "ab");
        assert_eq!(strip_thinking("a<think unterminated"), "a");
        assert_eq!(strip_thinking("plain"), "plain");
    }
}
