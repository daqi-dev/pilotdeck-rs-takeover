//! `model_references.*` — model reference inspection, planning and
//! conditional replacement (`cli.task.model-test`).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `model_references.inspect` /
//! `.plan` / `.apply` 条目。TS 基线（cc7c4e6）是语义参考：`ui/server/routes/
//! config.js` 的 `GET /api/config/model-references`（`findModelReferences`）与
//! `POST /api/config/model-removal`（`planModelRemoval`，两段式 preview +
//! `baseRevision` 复验 + 单次原子写 + 悬空防御）。基线的引用面是 pilotdeck.yaml
//! 的 `agent.model` / `router.*` / `memory.model` 槽位；v1 实例的模型引用面是：
//!
//! * `v1_model_selections`（t016）：每个 scope 一条 override —— 替换的目标；
//! * `runs.input_json.model`：run 提交时显式指定的模型 —— 只读的历史引用
//!   （「在途 run 模型不换」，t016 已定），inspect 列出它们供预检，
//!   plan 把在途 run 记为 `ACTIVE_BINDINGS` issue（apply 直接拒绝），终态
//!   run 记 `HISTORICAL_RUN_REFERENCE` issue（保留历史，不被改写）。
//!
//! 引用按稳定 ID 匹配（「中文空格名称按稳定ID处理」）：providerId/modelId
//! 精确字符串匹配，不 trim 不折叠大小写（基线注释「Creation rules do not
//! apply to references」）。
//!
//! `referenceSetRevision` 是整个引用集（全部 selection + 全部带模型的 run）
//! 的内容摘要；任何新引用（或引用消失）都改变它 —— 这就是 plan→apply 的
//! 「新引用使旧计划失效」锚。分页游标 `mref-<digest>-<offset>` 钉住过滤视图
//! 的摘要（collections `operations.list` 同款纪律）：跨页变更回答
//! `SNAPSHOT_CHANGED` 而不是静默假完整。
//!
//! `planDigest` 额外钉住：目标、replacement、removeSource、被改 selection
//! 清单、以及受影响目录行的 revision（provider 行 revision；modelId 定向时
//! 目标模型行 revision；provider 整体时该 provider 的模型清单）。目录并发变
//! 化 → apply 重算摘要不一致 → `REVISION_CONFLICT` 整体拒绝（「任一冲突整体
//! 拒绝」「过期 revision 无部分写入」——apply 的全部写入在单个事务里，复验在
//! 事务内重做）。
//!
//! `removeSource` 语义照基线的「删 provider / 删单个模型」：modelId 定向删
//! 那条 `v1_models` 行；provider 整体（省略 modelId）删该 provider 的全部
//! 模型行和 provider 行本身（基线 `delete next.model.providers[...]` 对位）。
//! 自毁防御：removeSource 会删掉 replacement 指向的目录项 → `MODEL_UNAVAILABLE`
//! （「无效替代不产生可提交计划」）。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_CURSOR_EXPIRED, CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT,
    CODE_SNAPSHOT_CHANGED, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::operations_ops::LongOperationOutcome;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, format_cursor, now_rfc3339};

const METHOD_ID_APPLY: &str = "model_references.apply";

/// Contract error codes these operations declare that no shared constant
/// covers yet.
const CODE_MODEL_UNAVAILABLE: &str = "MODEL_UNAVAILABLE";
const CODE_ACTIVE_BINDINGS: &str = "ACTIVE_BINDINGS";

const MODEL_EVENT_SCHEMA: &str = "agentcpm-pd.model.event/1";
const SELECTION_RESOURCE_TYPE: &str = "model_selection";
const RUN_RESOURCE_TYPE: &str = "run";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("model_references.inspect", |rt, input, _key, _rev| {
        Box::pin(model_references_inspect(rt, input))
    });
    reg.handler("model_references.plan", |rt, input, _key, _rev| {
        Box::pin(model_references_plan(rt, input))
    });
    reg.handler("model_references.apply", |rt, input, key, _rev| {
        Box::pin(model_references_apply(rt, input, key))
    });
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

/// One collected model reference.
struct ReferenceEntry {
    resource_type: &'static str,
    resource_id: String,
    revision: String,
    provider_id: String,
    model_id: String,
    /// Selection rows are replaceable; run references are immutable
    /// history (in-flight or not, apply never rewrites them).
    is_selection: bool,
}

impl ReferenceEntry {
    fn to_contract_json(&self) -> Value {
        json!({
            "resourceType": self.resource_type,
            "resourceId": self.resource_id,
            "revision": self.revision,
            "settingKey": "model",
            "model": {"providerId": self.provider_id, "modelId": self.model_id},
        })
    }

    /// The identity part the reference-set digest folds over: stable under
    /// the reference's own revision changes, new under any new reference
    /// (「新引用使旧计划失效」pins reference identity, not mutable state).
    fn digest_part(&self) -> String {
        let kind = if self.is_selection { "S" } else { "R" };
        format!(
            "{kind}|{}|{}|{}",
            self.resource_id, self.provider_id, self.model_id
        )
    }
}

fn parse_model_ref(value: &Value) -> Option<(String, String)> {
    let provider_id = value["providerId"].as_str()?;
    let model_id = value["modelId"].as_str()?;
    if provider_id.is_empty() || model_id.is_empty() {
        return None;
    }
    Some((provider_id.to_owned(), model_id.to_owned()))
}

/// Collects every model reference in the instance: all selection overrides
/// plus all runs that were submitted with an explicit model.
fn collect_references(conn: &Connection) -> Result<Vec<ReferenceEntry>, ContractError> {
    let mut references = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT scope_key, provider_id, model_id, revision FROM v1_model_selections ORDER BY scope_key ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ReferenceEntry {
                    resource_type: SELECTION_RESOURCE_TYPE,
                    resource_id: r.get(0)?,
                    provider_id: r.get(1)?,
                    model_id: r.get(2)?,
                    revision: r.get(3)?,
                    is_selection: true,
                })
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        references.extend(rows);
    }
    {
        let mut stmt = conn
            .prepare("SELECT run_id, revision, input_json, terminal FROM runs ORDER BY run_id ASC")
            .map_err(sql_err)?;
        let rows: Vec<(String, String, String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        for (run_id, revision, input_json, _terminal) in rows {
            let Ok(input) = serde_json::from_str::<Value>(&input_json) else {
                continue;
            };
            if let Some((provider_id, model_id)) = parse_model_ref(&input["model"]) {
                references.push(ReferenceEntry {
                    resource_type: RUN_RESOURCE_TYPE,
                    resource_id: run_id,
                    revision,
                    provider_id,
                    model_id,
                    is_selection: false,
                });
            }
        }
    }
    Ok(references)
}

/// The whole reference set's revision (content digest). Any reference
/// appearing or disappearing changes it.
fn reference_set_revision(references: &[ReferenceEntry]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentcpm-pd.model-references.set/1");
    for reference in references {
        hasher.update([0u8]);
        hasher.update(reference.digest_part().as_bytes());
    }
    format!("refset-{:x}", hasher.finalize())
}

/// The filtered view's digest (what an inspect page's cursor pins).
fn view_digest(references: &[ReferenceEntry]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentcpm-pd.model-references.view/1");
    for reference in references {
        hasher.update([0u8]);
        hasher.update(reference.digest_part().as_bytes());
    }
    format!("refview-{:x}", hasher.finalize())
}

fn parse_view_cursor(raw: &str) -> Option<(String, usize)> {
    let rest = raw.strip_prefix("mref-")?;
    let (digest, offset) = rest.rsplit_once('-')?;
    if digest.is_empty() {
        return None;
    }
    offset
        .parse::<usize>()
        .ok()
        .map(|off| (digest.to_owned(), off))
}

/// Parses the target filter: (providerId, Option<modelId>).
fn parse_target(input: &Value) -> (String, Option<String>) {
    (
        input["providerId"].as_str().unwrap_or_default().to_owned(),
        input["modelId"].as_str().map(str::to_owned),
    )
}

fn matches_target(reference: &ReferenceEntry, provider_id: &str, model_id: Option<&str>) -> bool {
    reference.provider_id == provider_id && model_id.is_none_or(|model| reference.model_id == model)
}

// ---------------------------------------------------------------------------
// model_references.inspect
// ---------------------------------------------------------------------------

async fn model_references_inspect(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let (provider_id, model_id) = parse_target(&input);
    if provider_id.is_empty() {
        return Err(invalid("providerId is required").into());
    }
    let page = input.get("page");
    let limit = page
        .and_then(Value::as_object)
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;

    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let references = collect_references(&conn)?;
    let set_revision = reference_set_revision(&references);
    let matched: Vec<ReferenceEntry> = references
        .into_iter()
        .filter(|r| matches_target(r, &provider_id, model_id.as_deref()))
        .collect();
    let digest = view_digest(&matched);

    let offset = match page.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
        None => 0usize,
        Some(raw) => match parse_view_cursor(raw) {
            Some((pinned, off)) => {
                if pinned != digest {
                    return Err(ContractError::new(
                        CODE_SNAPSHOT_CHANGED,
                        "the reference listing changed while paging; restart the query from the first page",
                        RetrySpec::after_reconcile(),
                        EffectStatus::NotApplied,
                    )
                    .into());
                }
                off
            }
            None => {
                return Err(ContractError::new(
                    CODE_CURSOR_EXPIRED,
                    format!("invalid pagination cursor `{raw}`"),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                )
                .into());
            }
        },
    };
    if offset > matched.len() {
        return Err(ContractError::new(
            CODE_CURSOR_EXPIRED,
            "pagination cursor beyond the end of the listing",
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    let end = (offset + limit).min(matched.len());
    let complete = end >= matched.len();
    let items: Vec<Value> = matched[offset..end]
        .iter()
        .map(ReferenceEntry::to_contract_json)
        .collect();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "references": items,
            "pageInfo": {
                "snapshotRevision": digest,
                "nextCursor": if complete { Value::Null } else { json!(format!("mref-{digest}-{end}")) },
                "complete": complete,
            },
            "referenceSetRevision": set_revision,
        }),
    ))
}

// ---------------------------------------------------------------------------
// plan / apply shared planning
// ---------------------------------------------------------------------------

/// The target provider row's revision (the catalog binding the plan digest
/// pins: any provider.write bumps it).
fn load_provider_revision(
    conn: &Connection,
    provider_id: &str,
) -> Result<Option<String>, ContractError> {
    conn.query_row(
        "SELECT revision FROM v1_providers WHERE provider_id = ?1",
        params![provider_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(sql_err)
}

fn model_available(
    conn: &Connection,
    provider_id: &str,
    model_id: &str,
) -> Result<bool, ContractError> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT m.enabled, p.enabled FROM v1_models m
             JOIN v1_providers p ON p.provider_id = m.provider_id
             WHERE m.provider_id = ?1 AND m.model_id = ?2",
            params![provider_id, model_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(matches!(row, Some((m, p)) if m != 0 && p != 0))
}

/// Validates the replacement and the removeSource interaction
/// (「无效替代不产生可提交计划」; self-destruct refusal).
fn validate_replacement(
    conn: &Connection,
    target_provider: &str,
    target_model: Option<&str>,
    replacement: &(String, String),
    remove_source: bool,
) -> Result<(), ContractError> {
    let model_unavailable = |message: String| {
        ContractError::new(
            CODE_MODEL_UNAVAILABLE,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    if !model_available(conn, &replacement.0, &replacement.1)? {
        return Err(model_unavailable(format!(
            "replacement model `{}/{}` is not registered, disabled, or its provider is disabled",
            replacement.0, replacement.1
        )));
    }
    if remove_source {
        if target_model.is_none() && replacement.0 == target_provider {
            return Err(model_unavailable(format!(
                "replacement `{}/{}` belongs to provider `{target_provider}`, which removeSource would delete",
                replacement.0, replacement.1
            )));
        }
        if let Some(model) = target_model
            && replacement.0 == target_provider
            && replacement.1 == model
        {
            return Err(model_unavailable(format!(
                "replacement `{}/{}` is the model removeSource would delete",
                replacement.0, replacement.1
            )));
        }
    }
    Ok(())
}

/// One planned replacement: the matching selection references (the changes)
/// plus the diagnostics the plan reports.
struct BuiltPlan {
    /// Current-form references that apply rewrites (selections only).
    changes: Vec<ReferenceEntry>,
    /// In-flight runs bound to the target (apply refuses: ACTIVE_BINDINGS).
    active_bindings: Vec<String>,
    /// Terminal runs referencing the target (kept as history).
    historical_runs: Vec<String>,
}

fn build_plan(
    conn: &Connection,
    target_provider: &str,
    target_model: Option<&str>,
) -> Result<BuiltPlan, ContractError> {
    let mut changes = Vec::new();
    let mut active_bindings = Vec::new();
    let mut historical_runs = Vec::new();
    for reference in collect_references(conn)? {
        if !matches_target(&reference, target_provider, target_model) {
            continue;
        }
        if reference.is_selection {
            changes.push(reference);
        } else {
            let terminal: i64 = conn
                .query_row(
                    "SELECT terminal FROM runs WHERE run_id = ?1",
                    params![reference.resource_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?
                .unwrap_or(1);
            if terminal == 0 {
                active_bindings.push(reference.resource_id);
            } else {
                historical_runs.push(reference.resource_id);
            }
        }
    }
    changes.sort_by(|a, b| a.resource_id.cmp(&b.resource_id));
    Ok(BuiltPlan {
        changes,
        active_bindings,
        historical_runs,
    })
}

/// The plan digest: pins target, replacement, removeSource, the exact change
/// list and the affected catalog rows' revisions (a concurrent catalog
/// change invalidates the plan).
fn plan_digest(
    target_provider: &str,
    target_model: Option<&str>,
    replacement: &(String, String),
    remove_source: bool,
    plan: &BuiltPlan,
    conn: &Connection,
) -> Result<String, ContractError> {
    let mut hasher = Sha256::new();
    hasher.update(b"agentcpm-pd.model-references.plan/1");
    hasher.update([0u8]);
    hasher.update(target_provider.as_bytes());
    hasher.update([0u8]);
    hasher.update(target_model.unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(replacement.0.as_bytes());
    hasher.update([0u8]);
    hasher.update(replacement.1.as_bytes());
    hasher.update([0u8]);
    hasher.update([u8::from(remove_source)]);
    for change in &plan.changes {
        hasher.update([0u8]);
        hasher.update(change.digest_part().as_bytes());
    }
    // Catalog binding.
    if let Some(provider_revision) = load_provider_revision(conn, target_provider)? {
        hasher.update([0u8]);
        hasher.update(provider_revision.as_bytes());
    }
    match target_model {
        Some(model) => {
            let revision: Option<String> = conn
                .query_row(
                    "SELECT revision FROM v1_models WHERE provider_id = ?1 AND model_id = ?2",
                    params![target_provider, model],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            hasher.update([0u8]);
            hasher.update(revision.as_deref().unwrap_or("<absent>").as_bytes());
        }
        None => {
            let mut stmt = conn
                .prepare(
                    "SELECT model_id, revision FROM v1_models WHERE provider_id = ?1 ORDER BY model_id ASC",
                )
                .map_err(sql_err)?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![target_provider], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            for (model_id, revision) in rows {
                hasher.update([0u8]);
                hasher.update(model_id.as_bytes());
                hasher.update([0u8]);
                hasher.update(revision.as_bytes());
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// model_references.plan
// ---------------------------------------------------------------------------

async fn model_references_plan(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let (target_provider, target_model) = parse_target(&input);
    if target_provider.is_empty() {
        return Err(invalid("providerId is required").into());
    }
    let Some(replacement) = parse_model_ref(&input["replacement"]) else {
        return Err(invalid("replacement.providerId and replacement.modelId are required").into());
    };
    let expected_set_revision = input["expectedReferenceSetRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let remove_source = input
        .get("removeSource")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let references = collect_references(&conn)?;
    let current = reference_set_revision(&references);
    if current != expected_set_revision {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "expectedReferenceSetRevision `{expected_set_revision}` does not match `{current}`"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    // 无效替代不产生可提交计划: MODEL_UNAVAILABLE (plan is read-only, the
    // digest below would never be committable).
    validate_replacement(
        &conn,
        &target_provider,
        target_model.as_deref(),
        &replacement,
        remove_source,
    )?;
    let plan = build_plan(&conn, &target_provider, target_model.as_deref())?;
    let digest = plan_digest(
        &target_provider,
        target_model.as_deref(),
        &replacement,
        remove_source,
        &plan,
        &conn,
    )?;

    let mut issues = Vec::new();
    for run_id in &plan.active_bindings {
        issues.push(json!({
            "code": "ACTIVE_BINDINGS",
            "field": "runs",
            "message": format!(
                "run `{run_id}` is in flight and bound to the target; apply is refused until it reaches a terminal state (在途绑定不热换)"
            ),
        }));
    }
    for run_id in &plan.historical_runs {
        issues.push(json!({
            "code": "HISTORICAL_RUN_REFERENCE",
            "field": "runs",
            "message": format!(
                "run `{run_id}` keeps its recorded model as history; it is not rewritten"
            ),
        }));
    }
    let changes: Vec<Value> = plan.changes.iter().map(|c| c.to_contract_json()).collect();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "planDigest": digest,
            "changes": changes,
            "referenceSetRevision": current,
            "replacement": {"providerId": replacement.0, "modelId": replacement.1},
            "issues": issues,
        }),
    ))
}

// ---------------------------------------------------------------------------
// model_references.apply
// ---------------------------------------------------------------------------

async fn model_references_apply(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let (target_provider, target_model) = parse_target(&input);
    if target_provider.is_empty() {
        return Err(invalid("providerId is required").into());
    }
    let Some(replacement) = parse_model_ref(&input["replacement"]) else {
        return Err(invalid("replacement.providerId and replacement.modelId are required").into());
    };
    let expected_set_revision = input["expectedReferenceSetRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let submitted_digest = input["planDigest"].as_str().unwrap_or_default().to_owned();
    let remove_source = input
        .get("removeSource")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 幂等 intake.
    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = replay_apply_receipt(&store, key, &input)?
    {
        return Ok((ResponseOutcome::Accepted, replay));
    }

    // Pre-accept re-validation: revision, replacement, digest, in-flight
    // bindings. All zero-side-effect; a rejection is recorded under the
    // caller's key so a retry of the same intent replays it.
    let precheck = (|| -> Result<(), ContractError> {
        let conn = store.conn.lock().expect("sqlite mutex");
        let references = collect_references(&conn)?;
        let current = reference_set_revision(&references);
        if current != expected_set_revision {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "expectedReferenceSetRevision `{expected_set_revision}` does not match `{current}`"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        validate_replacement(
            &conn,
            &target_provider,
            target_model.as_deref(),
            &replacement,
            remove_source,
        )?;
        let plan = build_plan(&conn, &target_provider, target_model.as_deref())?;
        let digest = plan_digest(
            &target_provider,
            target_model.as_deref(),
            &replacement,
            remove_source,
            &plan,
            &conn,
        )?;
        if digest != submitted_digest {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "planDigest `{submitted_digest}` does not match the current plan `{digest}`; the reference set or catalog changed since planning"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        if !plan.active_bindings.is_empty() {
            return Err(ContractError::new(
                CODE_ACTIVE_BINDINGS,
                format!(
                    "runs still bound to the target are in flight (`{}`); let them finish or cancel them first (在途绑定不热换)",
                    plan.active_bindings.join("`, `")
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        Ok(())
    })();
    if let Err(err) = precheck {
        if let Some(key) = idempotency_key.as_deref() {
            record_apply_rejection(&store, key, &input, &err.code);
        }
        return Err(err.into());
    }

    // 受理：长操作缝登记。The transaction below is short and atomic, so the
    // operation is not cancellable mid-flight.
    let (operation_id, accepted_revision) =
        store.begin_long_operation(METHOD_ID_APPLY, &json!({"kind": "instance"}), false)?;
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": accepted_revision,
        "deduplicated": false,
        "state": "queued",
    });
    if let Some(key) = idempotency_key.as_deref()
        && let Err(err) = record_apply_acceptance(
            &store,
            key,
            &input,
            &json!({"operationId": operation_id}),
            &receipt,
        )
    {
        let _ = store.finish_long_operation(
            &operation_id,
            LongOperationOutcome::Failed {
                code: err.code.clone(),
                message: err.message.clone(),
            },
        );
        return Err(err.into());
    }
    store.event_notifier().notify_waiters();

    // 驱动：单事务复验 + 全部写入（「任一冲突整体拒绝」「过期 revision 无部分写入」）。
    let driver_store = store.clone();
    let driver_key = idempotency_key.clone();
    tokio::spawn(async move {
        let _ = driver_store.mark_operation_running(&operation_id);
        let outcome = execute_apply(
            &driver_store,
            &target_provider,
            target_model.as_deref(),
            &replacement,
            remove_source,
            &expected_set_revision,
            &submitted_digest,
        );
        match outcome {
            Ok(completion) => {
                let _ = driver_store.finish_long_operation(
                    &operation_id,
                    LongOperationOutcome::Succeeded {
                        result: completion,
                        result_schema_id: Some(format!(
                            "agentcpm-pd.{METHOD_ID_APPLY}.completion/1"
                        )),
                    },
                );
                update_apply_receipt_state(&driver_store, driver_key.as_deref(), "succeeded");
            }
            Err(err) => {
                let _ = driver_store.finish_long_operation(
                    &operation_id,
                    LongOperationOutcome::Failed {
                        code: err.code.clone(),
                        message: err.message.clone(),
                    },
                );
                update_apply_receipt_state(&driver_store, driver_key.as_deref(), "failed");
            }
        }
    });

    Ok((ResponseOutcome::Accepted, receipt))
}

/// The apply body: one transaction that re-validates everything and writes
/// every selection update (plus the optional source removal) or nothing.
#[allow(clippy::too_many_arguments)]
fn execute_apply(
    store: &Arc<V1Store>,
    target_provider: &str,
    target_model: Option<&str>,
    replacement: &(String, String),
    remove_source: bool,
    expected_set_revision: &str,
    submitted_digest: &str,
) -> Result<Value, ContractError> {
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;

    // 复验（事务内重做 precheck —— 受理与执行之间的任何变化都让整体回滚）.
    let references = collect_references(&tx)?;
    let current = reference_set_revision(&references);
    if current != expected_set_revision {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "expectedReferenceSetRevision `{expected_set_revision}` does not match `{current}`"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    validate_replacement(
        &tx,
        target_provider,
        target_model,
        replacement,
        remove_source,
    )?;
    let plan = build_plan(&tx, target_provider, target_model)?;
    let digest = plan_digest(
        target_provider,
        target_model,
        replacement,
        remove_source,
        &plan,
        &tx,
    )?;
    if digest != submitted_digest {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "planDigest `{submitted_digest}` does not match the current plan `{digest}`; the reference set or catalog changed since planning"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    if !plan.active_bindings.is_empty() {
        return Err(ContractError::new(
            CODE_ACTIVE_BINDINGS,
            format!(
                "runs still bound to the target are in flight (`{}`)",
                plan.active_bindings.join("`, `")
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }

    let now = now_rfc3339();
    let mut updated: Vec<ReferenceEntry> = Vec::with_capacity(plan.changes.len());
    for change in &plan.changes {
        let scope_json: Option<String> = tx
            .query_row(
                "SELECT scope_json FROM v1_model_selections WHERE scope_key = ?1",
                params![change.resource_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let new_rev = next_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE v1_model_selections
             SET provider_id = ?1, model_id = ?2, revision = ?3, updated_at = ?4
             WHERE scope_key = ?5",
            params![
                replacement.0,
                replacement.1,
                new_rev,
                now,
                change.resource_id
            ],
        )
        .map_err(sql_err)?;
        append_selection_event_in_tx(
            &tx,
            &change.resource_id,
            scope_json.as_deref().unwrap_or(""),
            "modelSelection.changed",
            &json!({
                "scope": serde_json::from_str::<Value>(scope_json.as_deref().unwrap_or("{}"))
                    .unwrap_or(Value::Null),
                "model": {"providerId": replacement.0, "modelId": replacement.1},
                "operationId": "model_references.apply",
            }),
        )?;
        updated.push(ReferenceEntry {
            resource_type: SELECTION_RESOURCE_TYPE,
            resource_id: change.resource_id.clone(),
            revision: new_rev,
            provider_id: replacement.0.clone(),
            model_id: replacement.1.clone(),
            is_selection: true,
        });
    }

    let source_removed = if remove_source {
        match target_model {
            Some(model) => {
                tx.execute(
                    "DELETE FROM v1_models WHERE provider_id = ?1 AND model_id = ?2",
                    params![target_provider, model],
                )
                .map_err(sql_err)?
                    > 0
            }
            None => {
                let models = tx
                    .execute(
                        "DELETE FROM v1_models WHERE provider_id = ?1",
                        params![target_provider],
                    )
                    .map_err(sql_err)?;
                let provider = tx
                    .execute(
                        "DELETE FROM v1_providers WHERE provider_id = ?1",
                        params![target_provider],
                    )
                    .map_err(sql_err)?;
                models > 0 || provider > 0
            }
        }
    } else {
        false
    };

    // 悬空防御（基线 findDeletedModelReferences）：替换后每个 selection 引用
    // 必须仍指向目录中的模型 —— 否则整体回滚。
    let dangling: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM v1_model_selections s
             LEFT JOIN v1_models m ON m.provider_id = s.provider_id AND m.model_id = s.model_id
             WHERE m.provider_id IS NULL",
            [],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if dangling > 0 {
        return Err(ContractError::new(
            CODE_MODEL_UNAVAILABLE,
            format!(
                "the plan would leave {dangling} selection reference(s) dangling; nothing was written"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    tx.commit().map_err(sql_err)?;
    drop(conn);
    store.event_notifier().notify_waiters();
    store.state_notifier().notify_waiters();

    let after = {
        let conn = store.conn.lock().expect("sqlite mutex");
        collect_references(&conn)?
    };
    let updated_references: Vec<Value> = updated.iter().map(|r| r.to_contract_json()).collect();
    Ok(json!({
        "updatedReferences": updated_references,
        "referenceSetRevision": reference_set_revision(&after),
        "sourceRemoved": source_removed,
    }))
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

/// Appends a `modelSelection.changed` event (models.rs's exact shape).
fn append_selection_event_in_tx(
    conn: &Connection,
    scope_key: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    let placeholder = format!("tmp-mr-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
        params![
            placeholder,
            scope_key,
            session_id,
            event_type,
            MODEL_EVENT_SCHEMA,
            payload_str,
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
// Idempotency (same table/shape as provider_test.rs / workspaces.rs)
// ---------------------------------------------------------------------------

fn replay_apply_receipt(
    store: &V1Store,
    key: &str,
    input: &Value,
) -> Result<Option<Value>, ContractError> {
    let digest = compute_input_digest(METHOD_ID_APPLY, input);
    let conn = store.conn.lock().expect("sqlite mutex");
    let existing: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT input_digest, receipt_json, rejection_code
             FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, METHOD_ID_APPLY],
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
                        "idempotencyKey `{key}` already used for {METHOD_ID_APPLY} with different input"
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

fn record_apply_acceptance(
    store: &V1Store,
    key: &str,
    input: &Value,
    resource: &Value,
    receipt: &Value,
) -> Result<(), ContractError> {
    let digest = compute_input_digest(METHOD_ID_APPLY, input);
    let now = now_rfc3339();
    let retained = (Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')",
        params![
            key,
            METHOD_ID_APPLY,
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

fn record_apply_rejection(store: &V1Store, key: &str, input: &Value, code: &str) {
    let digest = compute_input_digest(METHOD_ID_APPLY, input);
    let now = now_rfc3339();
    let retained = (Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6, 'correctRejectedInputWithNewKey')",
        params![key, METHOD_ID_APPLY, digest, code, now, retained],
    );
}

fn update_apply_receipt_state(store: &V1Store, key: Option<&str>, state: &str) {
    let Some(key) = key else {
        return;
    };
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let Ok(raw) = conn.query_row(
        "SELECT receipt_json FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
        params![key, METHOD_ID_APPLY],
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
                METHOD_ID_APPLY
            ],
        );
    }
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

    async fn seed_catalog(rt: &Arc<PublicV1Runtime>, iid: &str, provider: &str, models: &[&str]) {
        rt.dispatch(
            "provider.write",
            json!({
                "instanceId": iid,
                "providerId": provider,
                "name": format!("{provider} display"),
                "protocol": "openai",
                "endpoint": "https://api.example.com/v1",
            }),
            None,
            None,
        )
        .await
        .expect("seed provider");
        for model in models {
            rt.dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": provider, "modelId": model},
                    "name": format!("{provider}/{model}"),
                }),
                None,
                None,
            )
            .await
            .expect("seed model");
        }
    }

    async fn set_selection(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        scope: Value,
        provider: &str,
        model: &str,
    ) {
        let (_, current) = rt
            .dispatch(
                "model_selection.get",
                json!({"instanceId": iid, "scope": scope}),
                None,
                None,
            )
            .await
            .expect("model_selection.get");
        rt.dispatch(
            "model_selection.set",
            json!({
                "instanceId": iid,
                "scope": scope,
                "expectedRevision": current["revision"],
                "model": {"providerId": provider, "modelId": model},
            }),
            None,
            None,
        )
        .await
        .expect("model_selection.set");
    }

    async fn wait_apply_terminal(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        loop {
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
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn inspect_lists_selections_and_runs_with_stable_paging() {
        let (_tmp, rt, iid) = fresh_home("mr-inspect");
        seed_catalog(&rt, &iid, "prov-a", &["m-old", "m-new"]).await;
        set_selection(&rt, &iid, json!({"kind": "instance"}), "prov-a", "m-old").await;
        // A run with an explicit model (history after completion).
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": "p-mr",
                "name": "mr project",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "r-mr", "relativePath": "ws"},
            }),
            None,
            None,
        )
        .await
        .expect("register project");
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "s-mr", "projectId": "p-mr"}),
            None,
            None,
        )
        .await
        .expect("create session");
        rt.dispatch(
            "run.submit",
            json!({
                "instanceId": iid,
                "sessionId": "s-mr",
                "input": [{"kind": "text", "text": "hi"}],
                "model": {"providerId": "prov-a", "modelId": "m-old"},
            }),
            Some("key-mr-run-1"),
            None,
        )
        .await
        .expect("submit run");

        let (_, result) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .expect("inspect");
        OperationRegistry::global()
            .validate_result("model_references.inspect", &result)
            .unwrap();
        let references = result["references"].as_array().unwrap().clone();
        assert_eq!(references.len(), 2, "one selection + one run reference");
        assert!(
            references
                .iter()
                .any(|r| r["resourceType"] == "model_selection")
        );
        assert!(references.iter().any(|r| r["resourceType"] == "run"));
        // Model-scoped filter narrows to both (same model) — and a different
        // model narrows to none.
        let (_, scoped) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a", "modelId": "m-new"}),
                None,
                None,
            )
            .await
            .expect("inspect scoped");
        assert_eq!(scoped["references"].as_array().map(Vec::len), Some(0));
        // Unknown provider: empty, not error.
        let (_, empty) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-none"}),
                None,
                None,
            )
            .await
            .expect("inspect unknown");
        assert_eq!(empty["references"].as_array().map(Vec::len), Some(0));

        // Paged view: two pages of one item; a set change between pages
        // answers SNAPSHOT_CHANGED.
        let (_, page1) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a", "page": {"limit": 1}}),
                None,
                None,
            )
            .await
            .expect("page 1");
        assert_eq!(page1["references"].as_array().map(Vec::len), Some(1));
        assert_eq!(page1["pageInfo"]["complete"], false);
        let next = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        set_selection(
            &rt,
            &iid,
            json!({"kind": "project", "projectId": "p-mr"}),
            "prov-a",
            "m-old",
        )
        .await;
        let err = rt
            .dispatch(
                "model_references.inspect",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "page": {"limit": 1, "cursor": next},
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "SNAPSHOT_CHANGED");
    }

    #[tokio::test]
    async fn plan_is_read_only_and_validates_inputs() {
        let (_tmp, rt, iid) = fresh_home("mr-plan");
        seed_catalog(&rt, &iid, "prov-a", &["m-old", "m-new"]).await;
        set_selection(&rt, &iid, json!({"kind": "instance"}), "prov-a", "m-old").await;
        let (_, base) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .expect("inspect");
        let set_rev = base["referenceSetRevision"].as_str().unwrap().to_owned();
        let selections_before: i64 = {
            let conn_before = rt.store().conn.lock().expect("sqlite mutex");
            conn_before
                .query_row("SELECT COUNT(*) FROM v1_model_selections", [], |r| r.get(0))
                .unwrap()
        };

        // Stale set revision.
        let err = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "replacement": {"providerId": "prov-a", "modelId": "m-new"},
                    "expectedReferenceSetRevision": "refset-stale",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // 无效替代: unregistered replacement → MODEL_UNAVAILABLE, no plan.
        let err = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "replacement": {"providerId": "prov-a", "modelId": "m-missing"},
                    "expectedReferenceSetRevision": set_rev,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "MODEL_UNAVAILABLE");

        // Self-destruct: provider-wide removal with a replacement from the
        // same provider.
        let err = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "replacement": {"providerId": "prov-a", "modelId": "m-new"},
                    "expectedReferenceSetRevision": set_rev,
                    "removeSource": true,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "MODEL_UNAVAILABLE");

        // A valid plan: dry-run zero writes.
        let (_, plan) = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "replacement": {"providerId": "prov-a", "modelId": "m-new"},
                    "expectedReferenceSetRevision": set_rev,
                }),
                None,
                None,
            )
            .await
            .expect("plan");
        OperationRegistry::global()
            .validate_result("model_references.plan", &plan)
            .unwrap();
        assert_eq!(plan["changes"].as_array().map(Vec::len), Some(1));
        assert_eq!(plan["changes"][0]["resourceType"], "model_selection");
        assert_eq!(plan["issues"].as_array().map(Vec::len), Some(0));
        assert!(plan["planDigest"].as_str().is_some_and(|d| d.len() == 64));
        let conn_after = rt.store().conn.lock().expect("sqlite mutex");
        let selections_after: i64 = conn_after
            .query_row("SELECT COUNT(*) FROM v1_model_selections", [], |r| r.get(0))
            .unwrap();
        drop(conn_after);
        assert_eq!(selections_after, selections_before, "dryRun零写");
    }

    #[tokio::test]
    async fn apply_replaces_atomically_and_rejects_stale_plans() {
        let (_tmp, rt, iid) = fresh_home("mr-apply");
        seed_catalog(&rt, &iid, "prov-a", &["m-old", "m-new"]).await;
        seed_catalog(&rt, &iid, "prov-b", &["m-alt"]).await;
        set_selection(&rt, &iid, json!({"kind": "instance"}), "prov-a", "m-old").await;
        // A second selection at project scope (registered project).
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": "p-mr2",
                "name": "mr project 2",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "r-mr2", "relativePath": "ws"},
            }),
            None,
            None,
        )
        .await
        .expect("register project");
        set_selection(
            &rt,
            &iid,
            json!({"kind": "project", "projectId": "p-mr2"}),
            "prov-a",
            "m-old",
        )
        .await;

        let (_, base) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .expect("inspect");
        let set_rev = base["referenceSetRevision"].as_str().unwrap().to_owned();
        let (_, plan) = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-b", "modelId": "m-alt"},
                    "expectedReferenceSetRevision": set_rev,
                }),
                None,
                None,
            )
            .await
            .expect("plan");
        let digest = plan["planDigest"].as_str().unwrap().to_owned();
        assert_eq!(plan["changes"].as_array().map(Vec::len), Some(2));

        // 新引用使旧计划失效: a fresh reference (a session-scoped selection)
        // changes the set revision; applying the old plan is refused whole.
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "s-mr2", "projectId": "p-mr2"}),
            None,
            None,
        )
        .await
        .expect("create session");
        set_selection(
            &rt,
            &iid,
            json!({"kind": "session", "projectId": "p-mr2", "sessionId": "s-mr2"}),
            "prov-a",
            "m-old",
        )
        .await;
        let err = rt
            .dispatch(
                "model_references.apply",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-b", "modelId": "m-alt"},
                    "expectedReferenceSetRevision": set_rev,
                    "planDigest": digest,
                }),
                Some("key-mr-apply-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // Re-plan and apply for real.
        let (_, fresh) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .expect("inspect after");
        let set_rev2 = fresh["referenceSetRevision"].as_str().unwrap().to_owned();
        let (_, plan2) = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-b", "modelId": "m-alt"},
                    "expectedReferenceSetRevision": set_rev2,
                }),
                None,
                None,
            )
            .await
            .expect("re-plan");
        let digest2 = plan2["planDigest"].as_str().unwrap().to_owned();
        let (outcome, receipt) = rt
            .dispatch(
                "model_references.apply",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-b", "modelId": "m-alt"},
                    "expectedReferenceSetRevision": set_rev2,
                    "planDigest": digest2,
                }),
                Some("key-mr-apply-2"),
                None,
            )
            .await
            .expect("apply accepts");
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result("model_references.apply", &receipt)
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_apply_terminal(&rt, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        let completion = operation["result"].clone();
        assert_eq!(
            completion["updatedReferences"].as_array().map(Vec::len),
            Some(3),
            "all three selections replaced: {completion}"
        );
        assert_eq!(completion["sourceRemoved"], false);

        // 多处引用无悬空: every selection now resolves to the replacement.
        let (_, after) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-b", "modelId": "m-alt"}),
                None,
                None,
            )
            .await
            .expect("inspect after apply");
        assert_eq!(after["references"].as_array().map(Vec::len), Some(3));
        let (_, old_refs) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .expect("inspect old model");
        assert_eq!(old_refs["references"].as_array().map(Vec::len), Some(0));

        // Replay: same key + same input deduplicates onto the same
        // operation (丢响应同幂等键唯一).
        let (_, replay) = rt
            .dispatch(
                "model_references.apply",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-b", "modelId": "m-alt"},
                    "expectedReferenceSetRevision": set_rev2,
                    "planDigest": digest2,
                }),
                Some("key-mr-apply-2"),
                None,
            )
            .await
            .expect("replay");
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"], json!(operation_id));

        // Same key, different input: conflict.
        let err = rt
            .dispatch(
                "model_references.apply",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-b",
                    "modelId": "m-alt",
                    "replacement": {"providerId": "prov-a", "modelId": "m-old"},
                    "expectedReferenceSetRevision": set_rev2,
                    "planDigest": digest2,
                }),
                Some("key-mr-apply-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "IDEMPOTENCY_CONFLICT");
    }

    #[tokio::test]
    async fn apply_refuses_in_flight_bindings_and_removes_source() {
        let (_tmp, rt, iid) = fresh_home("mr-active");
        seed_catalog(&rt, &iid, "prov-a", &["m-old", "m-new"]).await;
        set_selection(&rt, &iid, json!({"kind": "instance"}), "prov-a", "m-old").await;
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": "p-mr3",
                "name": "mr project 3",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "r-mr3", "relativePath": "ws"},
            }),
            None,
            None,
        )
        .await
        .expect("register project");
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "s-mr3", "projectId": "p-mr3"}),
            None,
            None,
        )
        .await
        .expect("create session");
        // A run that stays non-terminal deterministically (the offline
        // driver's await_steer directive holds it until steered or
        // cancelled), still bound to the target model.
        let (_, submitted) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": "s-mr3",
                    "input": [{"kind": "text", "text": "hold [pd-driver:await_steer]"}],
                    "model": {"providerId": "prov-a", "modelId": "m-old"},
                }),
                Some("key-mr-active-run"),
                None,
            )
            .await
            .expect("submit held run");
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        // Let the worker claim it (queued → waiting on the steer seam).
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let (_, held) = rt
            .dispatch("run.get", json!({"runId": run_id}), None, None)
            .await
            .expect("run.get");
        assert!(
            !held["run"]["terminal"].as_bool().unwrap_or(true),
            "the await_steer run must stay non-terminal: {held}"
        );

        let (_, base) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .expect("inspect");
        let set_rev = base["referenceSetRevision"].as_str().unwrap().to_owned();
        let (_, plan) = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-a", "modelId": "m-new"},
                    "expectedReferenceSetRevision": set_rev,
                }),
                None,
                None,
            )
            .await
            .expect("plan with active binding");
        // The plan reports the in-flight run as an issue…
        let issues = plan["issues"].as_array().unwrap().clone();
        assert!(issues.iter().any(|i| i["code"] == "ACTIVE_BINDINGS"));
        let digest = plan["planDigest"].as_str().unwrap().to_owned();

        // …and apply refuses it (在途绑定不热换).
        let err = rt
            .dispatch(
                "model_references.apply",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-a", "modelId": "m-new"},
                    "expectedReferenceSetRevision": set_rev,
                    "planDigest": digest,
                }),
                Some("key-mr-active-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "ACTIVE_BINDINGS");
        // Nothing was written.
        let (_, unchanged) = rt
            .dispatch(
                "model_references.inspect",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .expect("inspect unchanged");
        assert_eq!(unchanged["referenceSetRevision"].as_str().unwrap(), set_rev);

        // Drop the binding (cancel the held run), then apply with
        // removeSource: the source model row is deleted and the selection
        // is replaced with no dangling reference.
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": run_id}),
            Some("key-mr-cancel-2"),
            None,
        )
        .await
        .expect("cancel held run");
        let (_, cancelled) = rt
            .dispatch("run.get", json!({"runId": run_id}), None, None)
            .await
            .expect("run.get after cancel");
        assert_eq!(cancelled["run"]["terminal"], true);

        let (_, plan2) = rt
            .dispatch(
                "model_references.plan",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-a", "modelId": "m-new"},
                    "expectedReferenceSetRevision": set_rev,
                    "removeSource": true,
                }),
                None,
                None,
            )
            .await
            .expect("plan 2");
        let digest2 = plan2["planDigest"].as_str().unwrap().to_owned();
        let (_, receipt) = rt
            .dispatch(
                "model_references.apply",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-a",
                    "modelId": "m-old",
                    "replacement": {"providerId": "prov-a", "modelId": "m-new"},
                    "expectedReferenceSetRevision": set_rev,
                    "planDigest": digest2,
                    "removeSource": true,
                }),
                Some("key-mr-remove-1"),
                None,
            )
            .await
            .expect("apply with removal");
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_apply_terminal(&rt, &operation_id).await;
        assert_eq!(operation["state"], "succeeded");
        assert_eq!(operation["result"]["sourceRemoved"], true);
        // The source model row is gone; the selection now points at m-new.
        let err = rt
            .dispatch(
                "model.get",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-a", "modelId": "m-old"},
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "NOT_FOUND");
        let (_, selection) = rt
            .dispatch(
                "model_selection.get",
                json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                None,
                None,
            )
            .await
            .expect("selection after");
        assert_eq!(selection["overrideModel"]["modelId"], "m-new");
    }
}
