//! `model.*` and `model_selection.*` — model catalog registration and
//! scoped model selection (`cli.task.t016`, S3).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `model.list` / `model.get` /
//! `model.write` / `model_selection.get` / `model_selection.set` 条目。TS 基线
//! (cc7c4e6) 是语义参考：`ui/server/routes/models.js` 与
//! `ui/server/index.js:637-639`（`GET/PUT/DELETE /api/sessions/model`）把会话
//! 选择与全局默认分开存放——本域沿用该分层：每个 scope 一条 override，解析
//! 链 session → project → instance，未定义任何 override 时 `effectiveModel`
//! 为 `null`（合同验收「无有效模型返回null不随机选择」）。
//!
//! 旧 Rust 线的 `model_catalog.rs` 是旧装配（`PILOT_HOME`/`~/.pilotdeck` 的
//! YAML 投影）上的目录视图，v1 实例数据在 `AGENTCPM_PD_HOME` 的 v1 store
//! （007 §8）；接线不成比例，按简报复用规则以本文件内的 v1 原生存储实现
//! （`configuration.*` 域的薄适配先例），差异见任务报告。
//!
//! 关键语义（按合同 acceptance）：
//! * `model.write` 的「创建省 revision 仅不存在可用」：省略 `expectedRevision`
//!   只能创建；已存在时省略 → `REVISION_CONFLICT`。给出 revision 时必须与
//!   当前记录一致（记录不存在 → `REVISION_CONFLICT`）。
//! * 「声明能力与验证证据分开」：`declaredCapabilities` 归 `model.write`，
//!   `capabilityEvidence` 只能由验证流程（`provider_test.confirm_capabilities`
//!   等后续任务）写入；本域的写操作永不合成 evidence，读操作原样返回
//!   （「文本成功不推出image」「未知能力不猜true/false」由此成立）。
//! * `model_selection.set` 的「null 清除而省略非法」由 input schema 强制
//!   （`model` 必填、可空）；「无效模型拒绝」→ `MODEL_UNAVAILABLE`（目标不
//!   在目录中，或模型/其 provider 被 `enabled=false`）；「在途 run 模型不换」
//!   ——本域只持久化选择，不改任何 run 记录；运行中 run 的模型在其提交时
//!   已定（见任务报告的 run.submit 缝合点说明）。
//! * selection 的 CAS 与 `policy.update` 同构：`model_selection.get` 报告的
//!   revision（本 scope 的 override、最近已定义祖先的 override，或全链未
//!   定义时的实例 initialization_revision）就是 `model_selection.set` 的
//!   CAS 目标；祖先的变更会使继承 scope 的过期 CAS 失败。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::configuration::{self, VIssue};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// Contract error codes declared by `model.write` / `model_selection.set`
/// that the protocol crate has no shared constant for yet.
const CODE_PROVIDER_NOT_FOUND: &str = "PROVIDER_NOT_FOUND";
const CODE_MODEL_INVALID: &str = "MODEL_INVALID";
const CODE_MODEL_UNAVAILABLE: &str = "MODEL_UNAVAILABLE";

const MODELS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS v1_models (
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    declared_capabilities_json TEXT NOT NULL DEFAULT '[]',
    capability_evidence_json TEXT NOT NULL DEFAULT '[]',
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (provider_id, model_id)
)";
const MODEL_SELECTIONS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS v1_model_selections (
    scope_key TEXT PRIMARY KEY,
    scope_json TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

fn validate_model_config_key(
    conn: &Connection,
    key: &str,
    value: &Value,
) -> Result<Vec<VIssue>, ContractError> {
    if !matches!(
        key,
        "defaultModel" | "models.default" | "modelSelection.default"
    ) {
        return Ok(Vec::new());
    }
    if value.is_null() {
        return Ok(Vec::new());
    }
    let pair: Option<(String, String)> = if let Some(s) = value.as_str() {
        s.split_once('/')
            .filter(|(p, m)| !p.trim().is_empty() && !m.trim().is_empty())
            .map(|(p, m)| (p.trim().to_owned(), m.trim().to_owned()))
    } else if let Some(obj) = value.as_object() {
        match (
            obj.get("providerId").and_then(Value::as_str),
            obj.get("modelId").and_then(Value::as_str),
        ) {
            (Some(p), Some(m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
                Some((p.trim().to_owned(), m.trim().to_owned()))
            }
            _ => None,
        }
    } else {
        None
    };
    let Some((provider_id, model_id)) = pair else {
        return Ok(vec![VIssue::new(
            CODE_MODEL_INVALID,
            Some(key.to_owned()),
            "model reference must be `providerId/modelId` or `{providerId, modelId}`",
            true,
        )]);
    };
    let Some(model) = load_model(conn, &provider_id, &model_id)? else {
        return Ok(vec![VIssue::new(
            "DANGLING_MODEL_REFERENCE",
            Some(key.to_owned()),
            format!("model `{provider_id}/{model_id}` does not exist"),
            true,
        )]);
    };
    let provider_enabled: Option<i64> = conn
        .query_row(
            "SELECT enabled FROM v1_providers WHERE provider_id = ?1",
            params![provider_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(models_sql_err)?;
    if !model.enabled || provider_enabled != Some(1) {
        return Ok(vec![VIssue::new(
            CODE_MODEL_UNAVAILABLE,
            Some(key.to_owned()),
            format!("model `{provider_id}/{model_id}` or its provider is disabled"),
            true,
        )]);
    }
    Ok(Vec::new())
}

/// Resolves the effective `(providerId, modelId)` selection for a session run
/// (`session -> project -> instance`) inside an active SQLite transaction
/// (consumed by `runtime.rs::drive_owned_turn` hook).
#[allow(dead_code)]
pub(crate) fn resolve_effective_model_for_run_in_tx(
    conn: &Connection,
    initialization_revision: &str,
    project_id: &str,
    session_id: &str,
) -> Result<Option<(String, String)>, ContractError> {
    let scope = SelectionScope::Session {
        project_id: project_id.to_owned(),
        session_id: session_id.to_owned(),
    };
    Ok(resolve_selection(conn, initialization_revision, &scope)?.effective)
}

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_models", MODELS_TABLE_DDL);
    reg.schema("v1_model_selections", MODEL_SELECTIONS_TABLE_DDL);
    configuration::register_named_scoped_validator("models", validate_model_config_key);
    reg.handler("model.list", |rt, input, _key, _rev| {
        Box::pin(model_list(rt, input))
    });
    reg.handler("model.get", |rt, input, _key, _rev| {
        Box::pin(model_get(rt, input))
    });
    reg.handler("model.write", |rt, input, _key, rev| {
        Box::pin(model_write(rt, input, rev))
    });
    reg.handler("model_selection.get", |rt, input, _key, _rev| {
        Box::pin(model_selection_get(rt, input))
    });
    reg.handler("model_selection.set", |rt, input, _key, rev| {
        Box::pin(model_selection_set(rt, input, rev))
    });
}

// ---------------------------------------------------------------------------
// Scope model (mirrors `configuration.rs` / `policy.rs`; one local copy per
// domain is the substrate's established pattern — no cross-domain imports)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SelectionScope {
    Instance,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl SelectionScope {
    pub(crate) fn to_scope_json(&self) -> Value {
        match self {
            Self::Instance => json!({"kind": "instance"}),
            Self::Project { project_id } => json!({"kind": "project", "projectId": project_id}),
            Self::Session {
                project_id,
                session_id,
            } => json!({"kind": "session", "projectId": project_id, "sessionId": session_id}),
        }
    }

    /// Canonical JSON primary key (same scheme as `policies.scope_key`).
    fn key(&self) -> String {
        serde_json::to_string(&self.to_scope_json()).unwrap_or_else(|_| "?".to_owned())
    }

    /// Resolution order for `model_selection`: own scope first, then the
    /// nearest ancestors (session → project → instance).
    fn chain(&self) -> Vec<Self> {
        match self {
            Self::Instance => vec![Self::Instance],
            Self::Project { project_id } => vec![
                Self::Project {
                    project_id: project_id.clone(),
                },
                Self::Instance,
            ],
            Self::Session {
                project_id,
                session_id,
            } => vec![
                Self::Session {
                    project_id: project_id.clone(),
                    session_id: session_id.clone(),
                },
                Self::Project {
                    project_id: project_id.clone(),
                },
                Self::Instance,
            ],
        }
    }
}

/// Parses and validates the request scope. Dispatch already ran the schema
/// validation; this adds the existence checks (a scope naming a missing
/// project/session answers `INVALID_ARGUMENT` — the contract's error lists
/// for these operations carry no `NOT_FOUND`), following the
/// `configuration.rs` precedent.
fn parse_scope(rt: &PublicV1Runtime, input: &Value) -> Result<SelectionScope, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(models_invalid_argument("input.scope is required"));
    };
    match scope.get("kind").and_then(Value::as_str) {
        Some("instance") => Ok(SelectionScope::Instance),
        Some("project") => {
            let Some(project_id) = scope.get("projectId").and_then(Value::as_str) else {
                return Err(models_invalid_argument("scope.projectId is required"));
            };
            if rt.store().get_project(project_id).is_err() {
                return Err(models_invalid_argument(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(SelectionScope::Project {
                project_id: project_id.to_owned(),
            })
        }
        Some("session") => {
            let (Some(project_id), Some(session_id)) = (
                scope.get("projectId").and_then(Value::as_str),
                scope.get("sessionId").and_then(Value::as_str),
            ) else {
                return Err(models_invalid_argument(
                    "scope.projectId and scope.sessionId are required",
                ));
            };
            let session = rt.store().get_session(session_id).map_err(|_| {
                models_invalid_argument(format!("scope session `{session_id}` does not exist"))
            })?;
            // `get_session` wraps the row: {"session": {...}, "snapshotCursor"}.
            if session["session"].get("projectId").and_then(Value::as_str) != Some(project_id) {
                return Err(models_invalid_argument(format!(
                    "session `{session_id}` does not belong to project `{project_id}`"
                )));
            }
            Ok(SelectionScope::Session {
                project_id: project_id.to_owned(),
                session_id: session_id.to_owned(),
            })
        }
        other => Err(models_invalid_argument(format!(
            "scope.kind must be instance, project or session; got `{other:?}`"
        ))),
    }
}

fn models_invalid_argument(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

struct StoredModel {
    provider_id: String,
    model_id: String,
    name: String,
    enabled: bool,
    declared: Value,
    evidence: Value,
    revision: String,
}

impl StoredModel {
    /// The contract `Model` shape.
    fn to_contract_json(&self) -> Value {
        json!({
            "reference": {
                "providerId": self.provider_id,
                "modelId": self.model_id,
            },
            "name": self.name,
            "enabled": self.enabled,
            "revision": self.revision,
            "capabilities": self.declared,
            "capabilityEvidence": self.evidence,
        })
    }
}

const MODEL_COLUMNS: &str = "provider_id, model_id, name, enabled, declared_capabilities_json, capability_evidence_json, revision";

fn row_to_model(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredModel> {
    let declared_raw: String = row.get(4)?;
    let evidence_raw: String = row.get(5)?;
    Ok(StoredModel {
        provider_id: row.get(0)?,
        model_id: row.get(1)?,
        name: row.get(2)?,
        enabled: row.get::<_, i64>(3)? != 0,
        declared: serde_json::from_str(&declared_raw).unwrap_or(json!([])),
        evidence: serde_json::from_str(&evidence_raw).unwrap_or(json!([])),
        revision: row.get(6)?,
    })
}

fn load_model(
    conn: &Connection,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<StoredModel>, ContractError> {
    conn.query_row(
        &format!("SELECT {MODEL_COLUMNS} FROM v1_models WHERE provider_id = ?1 AND model_id = ?2"),
        params![provider_id, model_id],
        row_to_model,
    )
    .optional()
    .map_err(models_sql_err)
}

fn load_all_models(conn: &Connection) -> Result<Vec<StoredModel>, ContractError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {MODEL_COLUMNS} FROM v1_models ORDER BY provider_id ASC, model_id ASC"
        ))
        .map_err(models_sql_err)?;
    let models = stmt
        .query_map([], row_to_model)
        .map_err(models_sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(models_sql_err)?;
    Ok(models)
}

fn models_sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

/// Reads the current shared revision without incrementing it (the
/// `pageInfo.snapshotRevision` source). Same counter `store.rs` advances;
/// duplicated here because the store helper is module-private (the
/// `policy.rs` precedent).
fn models_read_current_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(models_sql_err)?
        .unwrap_or(1);
    Ok(format!("rev-{cur:08}"))
}

fn models_next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(models_sql_err)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(models_sql_err)?;
    Ok(format!("rev-{next:08}"))
}

// ---------------------------------------------------------------------------
// Pagination (offset-cursor scheme, mirroring `access_keys.rs` / `store.rs`)
// ---------------------------------------------------------------------------

fn parse_page(page: Option<&Value>) -> Result<(usize, usize), ContractError> {
    let obj = page.and_then(Value::as_object);
    let limit = obj
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let offset = match obj.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            models_invalid_argument(format!("invalid pagination cursor `{cursor}`"))
        })?,
        None => 0,
    };
    Ok((offset, limit))
}

fn page_info(snapshot_revision: &str, end: usize, total: usize) -> Value {
    let complete = end >= total;
    json!({
        "snapshotRevision": snapshot_revision,
        "nextCursor": if complete { Value::Null } else { json!(end.to_string()) },
        "complete": complete,
    })
}

// ---------------------------------------------------------------------------
// model.list / model.get
// ---------------------------------------------------------------------------

fn models_mutex_err() -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        "sqlite store mutex poisoned",
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

async fn run_blocking<F>(rt: &Arc<PublicV1Runtime>, f: F) -> Result<Value, DispatchError>
where
    F: FnOnce(&PublicV1Runtime) -> Result<Value, ContractError> + Send + 'static,
{
    let rt_cloned = Arc::clone(rt);
    let res = tokio::task::spawn_blocking(move || f(&rt_cloned))
        .await
        .map_err(|e| {
            DispatchError::from(ContractError::new(
                CODE_UNAVAILABLE,
                format!("blocking model task failed: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            ))
        })?;
    res.map_err(DispatchError::from)
}

async fn model_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let body = run_blocking(&rt, move |rt| {
        let (offset, limit) = parse_page(input.get("page"))?;
        let provider_filter = input.get("providerId").and_then(Value::as_str);
        let available_only = input
            .get("availableOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let conn = rt.store().conn.lock().map_err(|_| models_mutex_err())?;
        let snapshot_revision = models_read_current_rev(&conn)?;
        let mut models = load_all_models(&conn)?;
        if let Some(provider_id) = provider_filter {
            models.retain(|m| m.provider_id == provider_id);
        }
        if available_only {
            let enabled_providers: std::collections::BTreeSet<String> = {
                let mut stmt = conn
                    .prepare("SELECT provider_id FROM v1_providers WHERE enabled = 1")
                    .map_err(models_sql_err)?;

                stmt.query_map([], |r| r.get::<_, String>(0))
                    .map_err(models_sql_err)?
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()
                    .map_err(models_sql_err)?
            };
            models.retain(|m| m.enabled && enabled_providers.contains(&m.provider_id));
        }
        let total = models.len();
        let end = offset.saturating_add(limit).min(total);
        if offset > total {
            return Err(models_invalid_argument(
                "pagination cursor beyond end of collection",
            ));
        }
        let items: Vec<Value> = models
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|m| m.to_contract_json())
            .collect();
        drop(conn);
        Ok(json!({
            "items": items,
            "pageInfo": page_info(&snapshot_revision, end, total),
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, body))
}

async fn model_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let body = run_blocking(&rt, move |rt| {
        let (provider_id, model_id) = parse_model_ref(&input)?;
        let conn = rt.store().conn.lock().map_err(|_| models_mutex_err())?;
        let found = load_model(&conn, &provider_id, &model_id)?;
        drop(conn);
        // "同名不同provider不混" / "错误ID不回落默认": exact (provider, model)
        // lookup, no fallback.
        let Some(model) = found else {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("model `{provider_id}/{model_id}` not found"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        Ok(json!({"model": model.to_contract_json()}))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, body))
}

/// Parses the contract `ModelRef` (`{providerId, modelId}`) from
/// `input.model` (model.get / model.write) — validated structurally by the
/// schema gate; empty ids are rejected defensively.
fn parse_model_ref(input: &Value) -> Result<(String, String), ContractError> {
    let reference = input
        .get("model")
        .ok_or_else(|| models_invalid_argument("input.model (providerId + modelId) is required"))?;
    let provider_id = reference
        .get("providerId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let model_id = reference
        .get("modelId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if provider_id.is_empty() || model_id.is_empty() {
        return Err(models_invalid_argument(
            "model.providerId and model.modelId must be non-empty",
        ));
    }
    Ok((provider_id.to_owned(), model_id.to_owned()))
}

// ---------------------------------------------------------------------------
// model.write
// ---------------------------------------------------------------------------

async fn model_write(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let body = run_blocking(&rt, move |rt| {
        let (provider_id, model_id) = parse_model_ref(&input)?;
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if name.is_empty() {
            return Err(ContractError::new(
                CODE_MODEL_INVALID,
                "model name is required and must not be blank",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let enabled = input
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let declared =
            validate_capability_pairs(input.get("declaredCapabilities").and_then(Value::as_array))?;

        let expected = expected_revision
            .as_deref()
            .or_else(|| input.get("expectedRevision").and_then(Value::as_str));

        let store = rt.store();
        let mut conn = store.conn.lock().map_err(|_| models_mutex_err())?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(models_sql_err)?;

        let provider_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM v1_providers WHERE provider_id = ?1",
                params![provider_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(models_sql_err)?;
        if provider_exists.is_none() {
            return Err(ContractError::new(
                CODE_PROVIDER_NOT_FOUND,
                format!("provider `{provider_id}` does not exist"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let existing = load_model(&tx, &provider_id, &model_id)?;
        match (expected, &existing) {
            (None, Some(current)) => {
                return Err(models_revision_conflict("<omitted>", &current.revision));
            }
            (Some(_), None) => {
                return Err(models_revision_conflict(
                    expected.unwrap_or_default(),
                    "<absent>",
                ));
            }
            (Some(expected), Some(current)) if current.revision != expected => {
                return Err(models_revision_conflict(expected, &current.revision));
            }
            _ => {}
        }

        let new_rev = models_next_rev_in_tx(&tx)?;
        let now = Utc::now().to_rfc3339();
        let evidence_json = existing
            .as_ref()
            .map(|m| serde_json::to_string(&m.evidence).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| "[]".to_owned());
        tx.execute(
            "INSERT INTO v1_models
                 (provider_id, model_id, name, enabled, declared_capabilities_json,
                  capability_evidence_json, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(provider_id, model_id) DO UPDATE SET
                name = excluded.name,
                enabled = excluded.enabled,
                declared_capabilities_json = excluded.declared_capabilities_json,
                capability_evidence_json = excluded.capability_evidence_json,
                revision = excluded.revision,
                updated_at = excluded.updated_at",
            params![
                provider_id,
                model_id,
                name,
                i64::from(enabled),
                serde_json::to_string(&declared).unwrap_or_else(|_| "[]".into()),
                evidence_json,
                new_rev,
                now,
            ],
        )
        .map_err(models_sql_err)?;
        tx.commit().map_err(models_sql_err)?;

        Ok(json!({"model": {
            "reference": {"providerId": provider_id, "modelId": model_id},
            "name": name,
            "enabled": enabled,
            "revision": new_rev,
            "capabilities": declared,
            "capabilityEvidence": serde_json::from_str::<Value>(&evidence_json)
                .unwrap_or(json!([])),
        }}))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, body))
}

/// Contract `Pair[]` validation: non-empty, unique keys (`MODEL_INVALID`
/// otherwise — the whole write is rejected).
fn validate_capability_pairs(pairs: Option<&Vec<Value>>) -> Result<Value, ContractError> {
    let Some(pairs) = pairs else {
        return Ok(json!([]));
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let key = pair.get("key").and_then(Value::as_str).unwrap_or_default();
        if key.is_empty() {
            return Err(ContractError::new(
                CODE_MODEL_INVALID,
                "declared capability key must be a non-empty string",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if !seen.insert(key.to_owned()) {
            return Err(ContractError::new(
                CODE_MODEL_INVALID,
                format!("duplicate declared capability key `{key}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        out.push(json!({
            "key": key,
            "value": pair.get("value").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(Value::Array(out))
}

fn models_revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("model expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// model_selection.get / model_selection.set
// ---------------------------------------------------------------------------

struct StoredSelection {
    provider_id: String,
    model_id: String,
    revision: String,
}

fn load_selection(
    conn: &Connection,
    scope: &SelectionScope,
) -> Result<Option<StoredSelection>, ContractError> {
    conn.query_row(
        "SELECT provider_id, model_id, revision FROM v1_model_selections WHERE scope_key = ?1",
        params![scope.key()],
        |r| {
            Ok(StoredSelection {
                provider_id: r.get(0)?,
                model_id: r.get(1)?,
                revision: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(models_sql_err)
}

/// The resolved answer for `model_selection.get`: the effective model (own
/// override or nearest ancestor's), the scope that provided it, and the
/// revision a conditional `model_selection.set` at this scope must present
/// (own record's, the contributing ancestor's, or the instance
/// initialization revision when the whole chain is undefined — the same
/// deterministic-anchor scheme as `policy.rs`).
struct SelectionView {
    effective: Option<(String, String)>,
    source_scope: SelectionScope,
    revision: String,
}

fn resolve_selection(
    conn: &Connection,
    initialization_revision: &str,
    scope: &SelectionScope,
) -> Result<SelectionView, ContractError> {
    for candidate in scope.chain() {
        if let Some(record) = load_selection(conn, &candidate)? {
            return Ok(SelectionView {
                effective: Some((record.provider_id, record.model_id)),
                source_scope: candidate,
                revision: record.revision,
            });
        }
    }
    Ok(SelectionView {
        effective: None,
        source_scope: scope.clone(),
        revision: initialization_revision.to_owned(),
    })
}

async fn model_selection_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let body = run_blocking(&rt, move |rt| {
        let scope = parse_scope(rt, &input)?;
        let store = rt.store();
        let conn = store.conn.lock().map_err(|_| models_mutex_err())?;
        let view = resolve_selection(&conn, &store.meta().initialization_revision, &scope)?;
        let own = load_selection(&conn, &scope)?;
        drop(conn);

        let model_ref = |pair: &Option<(String, String)>| match pair {
            Some((provider_id, model_id)) => {
                json!({"providerId": provider_id, "modelId": model_id})
            }
            None => Value::Null,
        };
        Ok(json!({
            "effectiveModel": model_ref(&view.effective),
            "overrideModel": match &own {
                Some(record) => json!({
                    "providerId": record.provider_id,
                    "modelId": record.model_id,
                }),
                None => Value::Null,
            },
            "sourceScope": view.source_scope.to_scope_json(),
            "revision": view.revision,
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, body))
}

async fn model_selection_set(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let payload = run_blocking(&rt, move |rt| {
        let scope = parse_scope(rt, &input)?;
        let expected = expected_revision
            .as_deref()
            .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned();
        let requested = input.get("model").cloned().unwrap_or(Value::Null);

        let store = rt.store();
        let mut conn = store.conn.lock().map_err(|_| models_mutex_err())?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(models_sql_err)?;

        let current = resolve_selection(&tx, &store.meta().initialization_revision, &scope)?;
        if current.revision != expected {
            return Err(models_revision_conflict(&expected, &current.revision));
        }

        let now = Utc::now().to_rfc3339();
        let mut changed = false;
        match &requested {
            Value::Null => {
                if load_selection(&tx, &scope)?.is_some() {
                    tx.execute(
                        "DELETE FROM v1_model_selections WHERE scope_key = ?1",
                        params![scope.key()],
                    )
                    .map_err(models_sql_err)?;
                    changed = true;
                }
            }
            reference => {
                let provider_id = reference
                    .get("providerId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let model_id = reference
                    .get("modelId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(model) = load_model(&tx, provider_id, model_id)? else {
                    return Err(ContractError::new(
                        CODE_MODEL_UNAVAILABLE,
                        format!("model `{provider_id}/{model_id}` is not registered"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                };
                let provider_enabled: Option<i64> = tx
                    .query_row(
                        "SELECT enabled FROM v1_providers WHERE provider_id = ?1",
                        params![provider_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(models_sql_err)?;
                if !model.enabled || provider_enabled != Some(1) {
                    return Err(ContractError::new(
                        CODE_MODEL_UNAVAILABLE,
                        format!(
                            "model `{provider_id}/{model_id}` is disabled or its provider is disabled"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                let new_rev = models_next_rev_in_tx(&tx)?;
                tx.execute(
                    "INSERT INTO v1_model_selections
                         (scope_key, scope_json, provider_id, model_id, revision, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(scope_key) DO UPDATE SET
                        scope_json = excluded.scope_json,
                        provider_id = excluded.provider_id,
                        model_id = excluded.model_id,
                        revision = excluded.revision,
                        updated_at = excluded.updated_at",
                    params![
                        scope.key(),
                        serde_json::to_string(&scope.to_scope_json()).unwrap_or_default(),
                        provider_id,
                        model_id,
                        new_rev,
                        now,
                    ],
                )
                .map_err(models_sql_err)?;
                changed = true;
            }
        }

        if changed {
            append_model_event_in_tx(
                &tx,
                &scope.key(),
                &match &scope {
                    SelectionScope::Session { session_id, .. } => session_id.clone(),
                    _ => String::new(),
                },
                "modelSelection.changed",
                &json!({
                    "scope": scope.to_scope_json(),
                    "model": requested,
                }),
                &now,
            )?;
        }
        let after = resolve_selection(&tx, &store.meta().initialization_revision, &scope)?;
        let own = load_selection(&tx, &scope)?;
        let effective_generation =
            configuration::current_effective_generation_or(&tx, &store.meta().generation)?;
        tx.commit().map_err(models_sql_err)?;

        let effective_model = match &after.effective {
            Some((provider_id, model_id)) => {
                json!({"providerId": provider_id, "modelId": model_id})
            }
            None => Value::Null,
        };
        Ok(json!({
            "_changed": changed,
            "body": {
                "effectiveModel": effective_model,
                "overrideModel": match &own {
                    Some(record) => json!({
                        "providerId": record.provider_id,
                        "modelId": record.model_id,
                    }),
                    None => Value::Null,
                },
                "revision": after.revision,
                "effectiveGeneration": effective_generation,
            }
        }))
    })
    .await?;
    if payload["_changed"].as_bool() == Some(true) {
        rt.store().event_notifier().notify_waiters();
        rt.store().state_notifier().notify_waiters();
    }
    Ok((ResponseOutcome::Succeeded, payload["body"].clone()))
}

/// Durable event append into the shared `events` table (same two-step cursor
/// scheme as `store.rs::append_event_in_tx`, duplicated per the `policy.rs`
/// precedent).
fn append_model_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<(), ContractError> {
    let placeholder_cursor = format!("tmp-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.model.event/1', ?5, ?6, ?7)",
        params![
            placeholder_cursor,
            resource_id,
            session_id,
            event_type,
            payload_str,
            payload_str.clone(),
            occurred_at,
        ],
    )
    .map_err(models_sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = crate::public_v1::store::format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(models_sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests: contract-shaped assertions against the real store + dispatch path
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

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

    /// Registers a provider directly through the dispatch path (the provider
    /// domain owns that table; this file only needs the row to exist).
    async fn seed_provider(rt: &Arc<PublicV1Runtime>, iid: &str, provider_id: &str) {
        rt.dispatch(
            "provider.write",
            json!({
                "instanceId": iid,
                "providerId": provider_id,
                "name": "seed provider",
                "protocol": "openai",
                "endpoint": "https://api.example.com/v1",
            }),
            None,
            None,
        )
        .await
        .expect("seed provider");
    }

    async fn write_model(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        provider_id: &str,
        model_id: &str,
        name: &str,
    ) -> Value {
        let (outcome, result) = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": provider_id, "modelId": model_id},
                    "name": name,
                }),
                None,
                None,
            )
            .await
            .expect("model.write dispatches");
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("model.write", &result)
            .unwrap_or_else(|e| panic!("model.write result fails schema: {e:?}"));
        result
    }

    async fn list_models(rt: &Arc<PublicV1Runtime>, iid: &str) -> Value {
        let (outcome, result) = rt
            .dispatch("model.list", json!({"instanceId": iid}), None, None)
            .await
            .expect("model.list dispatches");
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("model.list", &result)
            .unwrap_or_else(|e| panic!("model.list result fails schema: {e:?}"));
        result
    }

    #[tokio::test]
    async fn model_write_create_then_conditional_update() {
        let (_tmp, rt, iid) = fresh_home("model-cas");
        seed_provider(&rt, &iid, "prov-a").await;

        // Create with omitted revision.
        let created = write_model(&rt, &iid, "prov-a", "m-1", "Model One").await;
        assert_eq!(created["model"]["name"], "Model One");
        assert_eq!(created["model"]["enabled"], true);
        assert_eq!(created["model"]["capabilities"], json!([]));
        assert_eq!(created["model"]["capabilityEvidence"], json!([]));
        let rev1 = created["model"]["revision"].as_str().unwrap().to_owned();

        // 「创建省revision仅不存在可用」: omitting on an existing model conflicts.
        let err = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-a", "modelId": "m-1"},
                    "name": "renamed",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // Stale revision conflicts.
        let err = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-a", "modelId": "m-1"},
                    "expectedRevision": "rev-00000001",
                    "name": "renamed",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // Update with the current revision; declared capabilities replace,
        // evidence stays empty (declaration and evidence are separate).
        let updated = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-a", "modelId": "m-1"},
                    "expectedRevision": rev1,
                    "name": "Model One Prime",
                    "enabled": false,
                    "declaredCapabilities": [
                        {"key": "text", "value": true},
                        {"key": "reasoning", "value": "high"},
                    ],
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        OperationRegistry::global()
            .validate_result("model.write", &updated)
            .unwrap();
        assert_eq!(updated["model"]["name"], "Model One Prime");
        assert_eq!(updated["model"]["enabled"], false);
        assert_eq!(
            updated["model"]["capabilities"],
            json!([
                {"key": "text", "value": true},
                {"key": "reasoning", "value": "high"},
            ])
        );
        assert_eq!(updated["model"]["capabilityEvidence"], json!([]));
        let rev2 = updated["model"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev1, rev2);

        // model.get returns the stored record; model.list sees one item.
        let (_, got) = rt
            .dispatch(
                "model.get",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-a", "modelId": "m-1"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("model.get", &got)
            .unwrap();
        assert_eq!(got["model"]["revision"], rev2.as_str());
        let listed = list_models(&rt, &iid).await;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["items"][0]["reference"]["providerId"], "prov-a");
    }

    #[tokio::test]
    async fn same_name_models_do_not_mix_across_providers() {
        let (_tmp, rt, iid) = fresh_home("model-nomix");
        seed_provider(&rt, &iid, "prov-a").await;
        seed_provider(&rt, &iid, "prov-b").await;
        write_model(&rt, &iid, "prov-a", "shared", "A shared").await;
        write_model(&rt, &iid, "prov-b", "shared", "B shared").await;

        for (provider, name) in [("prov-a", "A shared"), ("prov-b", "B shared")] {
            let (_, got) = rt
                .dispatch(
                    "model.get",
                    json!({
                        "instanceId": iid,
                        "model": {"providerId": provider, "modelId": "shared"},
                    }),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(got["model"]["name"], name, "provider {provider}");
        }

        // providerId filter.
        let listed = rt
            .dispatch(
                "model.list",
                json!({"instanceId": iid, "providerId": "prov-a"}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["items"][0]["reference"]["providerId"], "prov-a");

        // Unknown model id: no fallback to the same-named sibling.
        let err = rt
            .dispatch(
                "model.get",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-c", "modelId": "shared"},
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "NOT_FOUND");
    }

    #[tokio::test]
    async fn model_write_validates_provider_and_capabilities() {
        let (_tmp, rt, iid) = fresh_home("model-invalid");
        // Unknown provider.
        let err = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "nope", "modelId": "m"},
                    "name": "x",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "PROVIDER_NOT_FOUND");

        seed_provider(&rt, &iid, "prov-a").await;
        // Blank name.
        let err = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-a", "modelId": "m"},
                    "name": "   ",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "MODEL_INVALID");

        // Duplicate capability keys: rejected as a whole.
        let err = rt
            .dispatch(
                "model.write",
                json!({
                    "instanceId": iid,
                    "model": {"providerId": "prov-a", "modelId": "m"},
                    "name": "x",
                    "declaredCapabilities": [
                        {"key": "text", "value": true},
                        {"key": "text", "value": false},
                    ],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "MODEL_INVALID");
        let listed = list_models(&rt, &iid).await;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn model_list_available_only_separates_disabled_and_unassembled() {
        let (_tmp, rt, iid) = fresh_home("model-avail");
        seed_provider(&rt, &iid, "prov-on").await;
        seed_provider(&rt, &iid, "prov-off").await;
        // Disable prov-off through the provider domain.
        let prov = rt
            .dispatch(
                "provider.get",
                json!({"instanceId": iid, "providerId": "prov-off"}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        rt.dispatch(
            "provider.write",
            json!({
                "instanceId": iid,
                "providerId": "prov-off",
                "expectedRevision": prov["provider"]["revision"],
                "name": "seed provider",
                "protocol": "openai",
                "endpoint": "https://api.example.com/v1",
                "enabled": false,
            }),
            None,
            None,
        )
        .await
        .unwrap();
        write_model(&rt, &iid, "prov-on", "m-on", "on").await;
        write_model(&rt, &iid, "prov-off", "m-off", "off").await;
        let disabled = write_model(&rt, &iid, "prov-on", "m-dis", "dis").await;
        rt.dispatch(
            "model.write",
            json!({
                "instanceId": iid,
                "model": {"providerId": "prov-on", "modelId": "m-dis"},
                "expectedRevision": disabled["model"]["revision"],
                "name": "dis",
                "enabled": false,
            }),
            None,
            None,
        )
        .await
        .unwrap();

        let all = list_models(&rt, &iid).await;
        assert_eq!(all["items"].as_array().map(Vec::len), Some(3));
        let available = rt
            .dispatch(
                "model.list",
                json!({"instanceId": iid, "availableOnly": true}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        let ids: Vec<&str> = available["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["reference"]["modelId"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["m-on"],
            "disabled model and disabled provider are filtered"
        );
    }

    #[tokio::test]
    async fn model_list_paginates_with_offset_cursors() {
        let (_tmp, rt, iid) = fresh_home("model-page");
        seed_provider(&rt, &iid, "prov-a").await;
        for n in 0..3 {
            write_model(&rt, &iid, "prov-a", &format!("m-{n}"), &format!("m{n}")).await;
        }
        let first = rt
            .dispatch(
                "model.list",
                json!({"instanceId": iid, "page": {"limit": 2}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(first["items"].as_array().map(Vec::len), Some(2));
        assert_eq!(first["pageInfo"]["complete"], false);
        let cursor = first["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        let second = rt
            .dispatch(
                "model.list",
                json!({"instanceId": iid, "page": {"limit": 2, "cursor": cursor}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(second["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(second["pageInfo"]["complete"], true);

        let err = rt
            .dispatch(
                "model.list",
                json!({"instanceId": iid, "page": {"cursor": "bogus"}}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");
    }

    // -------------------------------------------------------------------
    // model_selection.*
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn selection_get_reports_fallback_and_null_when_undefined() {
        let (_tmp, rt, iid) = fresh_home("sel-default");
        let (_, got) = rt
            .dispatch(
                "model_selection.get",
                json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("model_selection.get", &got)
            .unwrap();
        // 全链未定义: null, not a random pick; revision is the deterministic
        // initialization revision so the first set can CAS.
        assert_eq!(got["effectiveModel"], Value::Null);
        assert_eq!(got["overrideModel"], Value::Null);
        assert_eq!(got["sourceScope"], json!({"kind": "instance"}));
        assert!(got["revision"].as_str().is_some_and(|r| !r.is_empty()));
    }

    #[tokio::test]
    async fn selection_set_cas_and_inheritance_chain() {
        let (_tmp, rt, iid) = fresh_home("sel-chain");
        seed_provider(&rt, &iid, "prov-a").await;
        write_model(&rt, &iid, "prov-a", "m-1", "one").await;
        write_model(&rt, &iid, "prov-a", "m-2", "two").await;

        // First write at instance scope: CAS against the default revision.
        let (_, init) = rt
            .dispatch(
                "model_selection.get",
                json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let init_rev = init["revision"].as_str().unwrap().to_owned();
        let (_, set) = rt
            .dispatch(
                "model_selection.set",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "instance"},
                    "expectedRevision": init_rev,
                    "model": {"providerId": "prov-a", "modelId": "m-1"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("model_selection.set", &set)
            .unwrap();
        assert_eq!(set["effectiveModel"]["modelId"], "m-1");
        assert_eq!(set["overrideModel"]["modelId"], "m-1");
        assert!(
            set["effectiveGeneration"]
                .as_str()
                .is_some_and(|g| !g.is_empty()),
            "effectiveGeneration is required: {set}"
        );

        // Stale CAS against the pre-write default fails.
        let err = rt
            .dispatch(
                "model_selection.set",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "instance"},
                    "expectedRevision": init_rev,
                    "model": {"providerId": "prov-a", "modelId": "m-2"},
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // 「无效模型拒绝」: unknown / disabled models never become selections.
        for model in [
            json!({"providerId": "prov-a", "modelId": "nope"}),
            json!({"providerId": "prov-z", "modelId": "m-1"}),
        ] {
            let err = rt
                .dispatch(
                    "model_selection.set",
                    json!({
                        "instanceId": iid,
                        "scope": {"kind": "instance"},
                        "expectedRevision": set["revision"].as_str().unwrap(),
                        "model": model,
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code_of(&err), "MODEL_UNAVAILABLE", "model {model}");
        }

        // A project scope inherits the instance override and explains it.
        // (The existence check requires a real project; register one through
        // the project domain's store API directly.)
        let project_id = "p-sel";
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
                 VALUES (?1, 'sel', '{}', 'registered', 'local', 'rev-1', ?2)",
                params![project_id, Utc::now().to_rfc3339()],
            )
            .unwrap();
        }
        let (_, inherited) = rt
            .dispatch(
                "model_selection.get",
                json!({"instanceId": iid, "scope": {"kind": "project", "projectId": project_id}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(inherited["effectiveModel"]["modelId"], "m-1");
        assert_eq!(inherited["overrideModel"], Value::Null);
        assert_eq!(
            inherited["sourceScope"],
            json!({"kind": "instance"}),
            "「无override能解释回落」: the contributing scope is named"
        );
        assert_eq!(inherited["revision"], set["revision"]);

        // Own override shadows the ancestor.
        let (_, own) = rt
            .dispatch(
                "model_selection.set",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "project", "projectId": project_id},
                    "expectedRevision": inherited["revision"],
                    "model": {"providerId": "prov-a", "modelId": "m-2"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(own["effectiveModel"]["modelId"], "m-2");
        assert_eq!(own["overrideModel"]["modelId"], "m-2");

        // Clear with null: the fallback to the ancestor becomes visible again.
        let (_, cleared) = rt
            .dispatch(
                "model_selection.set",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "project", "projectId": project_id},
                    "expectedRevision": own["revision"],
                    "model": null,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("model_selection.set", &cleared)
            .unwrap();
        assert_eq!(cleared["overrideModel"], Value::Null);
        assert_eq!(cleared["effectiveModel"]["modelId"], "m-1");
        assert_eq!(
            cleared["revision"], set["revision"],
            "the ancestor's revision is effective again"
        );

        // An ancestor change invalidates a stale CAS on the inheriting scope.
        let err = rt
            .dispatch(
                "model_selection.set",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "project", "projectId": project_id},
                    "expectedRevision": own["revision"],
                    "model": {"providerId": "prov-a", "modelId": "m-2"},
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // The modelSelection.changed event is durable (set + clear + own set).
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'modelSelection.changed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn selection_scopes_must_reference_existing_targets() {
        let (_tmp, rt, iid) = fresh_home("sel-scope");
        for scope in [
            json!({"kind": "project", "projectId": "nope"}),
            json!({"kind": "session", "projectId": "nope", "sessionId": "nope"}),
        ] {
            let err = rt
                .dispatch(
                    "model_selection.get",
                    json!({"instanceId": iid, "scope": scope}),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");
        }
    }
}
