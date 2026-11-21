//! `context.*` — 上下文与输出风格控制（`cli.task.context`, S3；能力
//! `cli.capability.context`）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `context.inspect` /
//! `context.styles` / `context.configure` / `context.reload_styles` /
//! `context.compact` 条目。TS 基线（cc7c4e6）是语义参考：
//! * 风格目录：`ProjectRuntimeRegistry.outputStylesListForSdk` /
//!   `setOutputStyleForSdk` / `reloadOutputStylesForSdk`（网关方法
//!   `output_styles_list` / `set_output_style` / `reload_output_styles`），
//!   底层 `PluginRuntime.reloadOutputStyles`（`PluginRuntime.ts:388`）；
//! * 会话 thinking：`set_session_thinking`（`GatewayThinkingConfig`，
//!   `gateway/protocol/types.ts:914`；effort 级别集合见
//!   `model/thinking/settings.ts::THINKING_EFFORTS`）。
//!
//! v1 投影（与基线/合同的对齐与差异在任务报告登记）：
//! * **风格目录**：`<AGENTCPM_PD_HOME>/styles/**/*.md`（user 命名空间）
//!   加 `<工作区>/.pilotdeck/styles/**/*.md`（project 命名空间，工作区经
//!   `files.rs::resolve_project` 解析；虚拟项目无 project 命名空间——同
//!   `commands.rs` 的目录先例）。`styleId = <namespace>:<相对路径去.md>`，
//!   `name = 文件名去.md`（TS 的 style.name 即文件名），同名去重
//!   project 优先（TS 的 project>global 覆盖序），`revision` 为内容哈希
//!   `stylerev-<sha256前24>`。TS 的插件清单/`outputStyles` 配置面
//!   （plugin.json、builtin 插件、会话级插件注入）本轮没有对应 v1 面，
//!   不做假实现——目录只含文件风格。
//! * **加载失败不同于空**：目录读失败（存在但不可读）答 `UNAVAILABLE`，
//!   目录不存在才是空目录；单个坏风格文件（读失败或正文为空）跳过并在
//!   `reload_styles` 的 diagnostics 里报告（TS 逐插件 catch 跳过的对应物，
//!   「坏风格保留旧有效目录」——有效条目照常列出）。
//! * **会话上下文设置**（thinking / outputStyleId）：`v1_context_settings`
//!   按 scope 键存覆盖层，session→project→instance 链解析有效值与来源
//!   （`models.rs` 的 selection 链同款方案）。`configure` 只写 session 层；
//!   `null` 清除、省略保留（合同 acceptance）；`thinking` 合法级别取 TS
//!   `GatewayThinkingConfig.mode` 的闭集，effort 级别（low/medium/high/
//!   xhigh/max）还要过有效模型的声明能力核验（`v1_models` 的 declared
//!   capabilities 里 key=`thinking`，值为 effort 数组或
//!   `{"state","efforts"}`——TS `ModelThinkingSettings` 的投影），核验
//!   不通过答 `MODEL_CAPABILITY_UNSUPPORTED`，绝不假成功。
//! * **revision / effectiveGeneration**：`revision` 是会话上下文的 CAS 目标
//!   （自身记录的 revision，否则链上贡献层的，否则实例初始化锚点——
//!   `models.rs` 同款确定性锚）；`effectiveGeneration` 直接报告
//!   `store.meta().generation`：`submit_run` 在排队时把 run 的执行期快照
//!   写成 `{configRevision: initialization_revision, effectiveGeneration:
//!   meta.generation}`（store.rs:1013），即 v1 已建立的 run 执行期配置快照
//!   语义——在途 run 永远看提交时快照，configure 不触碰任何 run 行，
//!   「在途run不热换」结构性成立。
//! * **compact**：安全会话边界压缩。会话仍有活动/排队 run 时拒绝
//!   `SESSION_BUSY`（合同 errors）；压缩=把历史折叠为摘要需要模型调用
//!   （TS `CompactionEngine` 的 summary 是第二次模型调用），本构建没有
//!   `models.invoke` seam——按 `git.rs::invoke_suggestion_model` 的先例明确
//!   失败 `CAPABILITY_UNAVAILABLE`（compact 的合同错误表里有它），历史
//!   原样保留（「失败原历史工具配对保持」）。无历史条目的会话压缩为
//!   no-op 成功（无物可折，before==after）。空会话成功路径可
//!   inspect 核验；带历史的成功路径待模型 seam 落地。
//! * **异步操作**（reload_styles / compact，`retry=idempotentSubmit`）复用
//!   `schedules.rs` 的 `v1_domain_operations` 接受/完成记录与幂等键语义
//!   （`commands.rs::commands_execute` 同款内联执行模式），完成值经
//!   `operations.get` 可查。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_CAPABILITY_UNAVAILABLE, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::{CODE_PATH_ESCAPE, require_within, resolve_project};
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for yet.
const CODE_SESSION_BUSY: &str = "SESSION_BUSY";
const CODE_MODEL_CAPABILITY_UNSUPPORTED: &str = "MODEL_CAPABILITY_UNSUPPORTED";
const CODE_STYLE_NOT_FOUND: &str = "STYLE_NOT_FOUND";

/// The accepted `thinking` strings: the TS `GatewayThinkingConfig["mode"]`
/// union (`gateway/protocol/types.ts:916`). Levels outside this set are
/// malformed input, not model-unsupported.
const THINKING_MODES: [&str; 8] = [
    "default", "off", "minimal", "low", "medium", "high", "xhigh", "max",
];
/// The effort subset that requires the effective model's declared support
/// (`model/thinking/settings.ts::THINKING_EFFORTS`); `default`/`off`/
/// `minimal` carry no effort requirement.
const THINKING_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const MAX_STYLE_DEPTH: usize = 16;
const MAX_STYLE_FILES: usize = 1_000;
const MAX_STYLE_FILE_BYTES: usize = 256 * 1024;
const MAX_BUDGET_HISTORY_ROWS: i64 = 2_048;
const MAX_BUDGET_HISTORY_BYTES: usize = 4 * 1024 * 1024;

const DDL_CONTEXT_SETTINGS: &str = "CREATE TABLE IF NOT EXISTS v1_context_settings (
    scope_key TEXT PRIMARY KEY,
    scope_json TEXT NOT NULL,
    thinking TEXT,
    output_style_id TEXT,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const DDL_CONTEXT_STYLE_STATE: &str = "CREATE TABLE IF NOT EXISTS v1_context_style_state (
    scope_key TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const DDL_CONTEXT_COMPACTIONS: &str = "CREATE TABLE IF NOT EXISTS v1_context_compactions (
    compaction_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    state TEXT NOT NULL,
    entries_folded INTEGER NOT NULL DEFAULT 0,
    before_revision TEXT NOT NULL,
    after_revision TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_context_settings", DDL_CONTEXT_SETTINGS);
    reg.schema("v1_context_style_state", DDL_CONTEXT_STYLE_STATE);
    reg.schema("v1_context_compactions", DDL_CONTEXT_COMPACTIONS);
    crate::public_v1::domains::configuration::register_named_key_validator(
        "context",
        validate_context_config_key,
    );
    reg.handler("context.inspect", |rt, input, _key, _rev| {
        Box::pin(context_inspect(rt, input))
    });
    reg.handler("context.styles", |rt, input, _key, _rev| {
        Box::pin(context_styles(rt, input))
    });
    reg.handler("context.configure", |rt, input, _key, rev| {
        Box::pin(context_configure(rt, input, rev))
    });
    reg.handler("context.reload_styles", |rt, input, key, _rev| {
        Box::pin(context_reload_styles(rt, input, key))
    });
    reg.handler("context.compact", |rt, input, key, _rev| {
        Box::pin(context_compact(rt, input, key))
    });
}

fn validate_context_config_key(
    key: &str,
    value: &Value,
) -> Vec<crate::public_v1::domains::configuration::VIssue> {
    use crate::public_v1::domains::configuration::VIssue;
    match key {
        "context.thinking" => {
            let valid =
                value.is_null() || value.as_str().is_some_and(|s| THINKING_MODES.contains(&s));
            if valid {
                Vec::new()
            } else {
                vec![VIssue::new(
                    "INVALID_CONTEXT_THINKING",
                    Some(key.to_owned()),
                    format!("context.thinking must be null or one of {THINKING_MODES:?}"),
                    true,
                )]
            }
        }
        "context.outputStyleId" | "context.output_style_id" => {
            let valid = value.is_null() || value.as_str().is_some_and(|s| !s.trim().is_empty());
            if valid {
                Vec::new()
            } else {
                vec![VIssue::new(
                    "INVALID_CONTEXT_STYLE",
                    Some(key.to_owned()),
                    "context.outputStyleId must be null or a non-empty string",
                    true,
                )]
            }
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn context_invalid_argument(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn context_not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn context_revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("context expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn context_session_busy(session_id: &str) -> ContractError {
    ContractError::new(
        CODE_SESSION_BUSY,
        format!(
            "session `{session_id}` still has an active or queued run; \
             context.compact requires an idle session"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn context_unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn model_capability_unsupported(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_MODEL_CAPABILITY_UNSUPPORTED,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn style_not_found(style_id: &str) -> ContractError {
    ContractError::new(
        CODE_STYLE_NOT_FOUND,
        format!("output style `{style_id}` is not in the visible style catalog"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn context_sql_err(error: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {error}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

// ---------------------------------------------------------------------------
// Scope model (mirrors `configuration.rs` / `models.rs`; one local copy per
// domain is the substrate's established pattern — no cross-domain imports)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum ContextScope {
    Instance,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl ContextScope {
    fn to_scope_json(&self) -> Value {
        match self {
            Self::Instance => json!({"kind": "instance"}),
            Self::Project { project_id } => {
                json!({"kind": "project", "projectId": project_id})
            }
            Self::Session {
                project_id,
                session_id,
            } => {
                json!({"kind": "session", "projectId": project_id, "sessionId": session_id})
            }
        }
    }

    /// Canonical JSON primary key (the same scheme `v1_model_selections` and
    /// the configuration layers use).
    fn key(&self) -> String {
        serde_json::to_string(&self.to_scope_json()).unwrap_or_else(|_| "?".to_owned())
    }

    /// Resolution order: own scope first, then ancestors
    /// (session → project → instance).
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

/// Parses and validates the request scope (the `configuration.rs` /
/// `models.rs` precedent: the error lists of these operations carry no
/// `NOT_FOUND`, so a scope naming a missing project/session answers
/// `INVALID_ARGUMENT`).
fn parse_scope(rt: &PublicV1Runtime, input: &Value) -> Result<ContextScope, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(context_invalid_argument("input.scope is required"));
    };
    match scope.get("kind").and_then(Value::as_str) {
        Some("instance") => Ok(ContextScope::Instance),
        Some("project") => {
            let Some(project_id) = scope.get("projectId").and_then(Value::as_str) else {
                return Err(context_invalid_argument("scope.projectId is required"));
            };
            if rt.store().get_project(project_id).is_err() {
                return Err(context_invalid_argument(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(ContextScope::Project {
                project_id: project_id.to_owned(),
            })
        }
        Some("session") => {
            let (Some(project_id), Some(session_id)) = (
                scope.get("projectId").and_then(Value::as_str),
                scope.get("sessionId").and_then(Value::as_str),
            ) else {
                return Err(context_invalid_argument(
                    "scope.projectId and scope.sessionId are required",
                ));
            };
            let session = rt.store().get_session(session_id).map_err(|_| {
                context_invalid_argument(format!("scope session `{session_id}` does not exist"))
            })?;
            // `get_session` wraps the row: {"session": {...}, "snapshotCursor"}.
            if session["session"].get("projectId").and_then(Value::as_str) != Some(project_id) {
                return Err(context_invalid_argument(format!(
                    "session `{session_id}` does not belong to project `{project_id}`"
                )));
            }
            Ok(ContextScope::Session {
                project_id: project_id.to_owned(),
                session_id: session_id.to_owned(),
            })
        }
        other => Err(context_invalid_argument(format!(
            "scope.kind must be instance, project or session; got `{other:?}`"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Shared revision counter (the `policy.rs` / `models.rs` precedent: the core
// store helper is module-private, so each domain reads/bumps `meta.rev_counter`
// itself — one shared revision space for the whole instance)
// ---------------------------------------------------------------------------

fn context_next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(context_sql_err)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(context_sql_err)?;
    Ok(format!("rev-{next:08}"))
}

// ---------------------------------------------------------------------------
// Session context settings: layered overrides + chain resolution
// ---------------------------------------------------------------------------

struct StoredSetting {
    thinking: Option<String>,
    output_style_id: Option<String>,
    revision: String,
}

fn load_setting(
    conn: &Connection,
    scope: &ContextScope,
) -> Result<Option<StoredSetting>, ContractError> {
    conn.query_row(
        "SELECT thinking, output_style_id, revision FROM v1_context_settings WHERE scope_key = ?1",
        params![scope.key()],
        |r| {
            Ok(StoredSetting {
                thinking: r.get(0)?,
                output_style_id: r.get(1)?,
                revision: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(context_sql_err)
}

/// The chain-resolved view of one scope's context settings: per-field
/// effective value plus the layer that provided it, and the revision a
/// conditional write at this scope must present (own record's, the nearest
/// contributing layer's, or the instance initialization anchor when the
/// whole chain is undefined — the `models.rs::resolve_selection` scheme).
struct SettingsView {
    revision: String,
    thinking: Option<String>,
    thinking_source: (ContextScope, String),
    output_style_id: Option<String>,
    style_source: (ContextScope, String),
}

fn resolve_settings(
    conn: &Connection,
    initialization_revision: &str,
    scope: &ContextScope,
) -> Result<SettingsView, ContractError> {
    let mut revision: Option<String> = None;
    let mut thinking: Option<String> = None;
    let mut thinking_source: Option<(ContextScope, String)> = None;
    let mut output_style_id: Option<String> = None;
    let mut style_source: Option<(ContextScope, String)> = None;
    for candidate in scope.chain() {
        let Some(row) = load_setting(conn, &candidate)? else {
            continue;
        };
        if revision.is_none() {
            revision = Some(row.revision.clone());
        }
        if thinking.is_none() && row.thinking.is_some() {
            thinking = row.thinking.clone();
            thinking_source = Some((candidate.clone(), row.revision.clone()));
        }
        if output_style_id.is_none() && row.output_style_id.is_some() {
            output_style_id = row.output_style_id.clone();
            style_source = Some((candidate.clone(), row.revision.clone()));
        }
    }
    let fallback = || (scope.clone(), initialization_revision.to_owned());
    Ok(SettingsView {
        revision: revision.unwrap_or_else(|| initialization_revision.to_owned()),
        thinking,
        thinking_source: thinking_source.unwrap_or_else(fallback),
        output_style_id,
        style_source: style_source.unwrap_or_else(fallback),
    })
}

/// Effective session-chain context settings exposed to the turn runtime (`drive_owned_turn`).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveContextForRun {
    pub(crate) thinking: Option<String>,
    pub(crate) output_style_id: Option<String>,
    pub(crate) revision: String,
}

#[allow(dead_code)]
pub(crate) fn resolve_effective_context_for_run_in_tx(
    conn: &Connection,
    initialization_revision: &str,
    project_id: &str,
    session_id: &str,
) -> Result<EffectiveContextForRun, ContractError> {
    let view = resolve_settings(
        conn,
        initialization_revision,
        &ContextScope::Session {
            project_id: project_id.to_owned(),
            session_id: session_id.to_owned(),
        },
    )?;
    Ok(EffectiveContextForRun {
        thinking: view.thinking,
        output_style_id: view.output_style_id,
        revision: view.revision,
    })
}

// ---------------------------------------------------------------------------
// Effective model (read-only read of the models domain's selection chain —
// same table, same scheme, no cross-module import per the substrate rule)
// ---------------------------------------------------------------------------

fn effective_model(
    conn: &Connection,
    scope: &ContextScope,
) -> Result<Option<(String, String)>, ContractError> {
    for candidate in scope.chain() {
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT provider_id, model_id FROM v1_model_selections WHERE scope_key = ?1",
                params![candidate.key()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(context_sql_err)?;
        if let Some(pair) = row {
            return Ok(Some(pair));
        }
    }
    Ok(None)
}

/// The effective model's declared thinking support, from the models domain's
/// `declared_capabilities` pairs: key `thinking`, value either an effort
/// string array or `{"state": "enabled"|"disabled", "efforts": [...]}` (the
/// TS `ModelThinkingSettings` projection). `Ok(None)` = support cannot be
/// established from the stored declaration.
fn model_thinking_efforts(
    conn: &Connection,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<Vec<String>>, ContractError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT declared_capabilities_json FROM v1_models WHERE provider_id = ?1 AND model_id = ?2",
            params![provider_id, model_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(context_sql_err)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let capabilities: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
    let Some(pairs) = capabilities.as_array() else {
        return Ok(None);
    };
    let Some(entry) = pairs
        .iter()
        .find(|pair| pair.get("key").and_then(Value::as_str) == Some("thinking"))
    else {
        return Ok(None);
    };
    let value = entry.get("value").cloned().unwrap_or(Value::Null);
    let string_array = |items: &[Value]| -> Vec<String> {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    match &value {
        Value::Array(items) => Ok(Some(string_array(items))),
        Value::Object(map) => {
            if map.get("state").and_then(Value::as_str) == Some("disabled") {
                // TS: a model-level `disabled` state accepts no effort level.
                return Ok(Some(Vec::new()));
            }
            Ok(map
                .get("efforts")
                .and_then(Value::as_array)
                .map(|items| string_array(items)))
        }
        _ => Ok(None),
    }
}

/// 「不支持thinking拒绝」: effort levels must be supported by the effective
/// model (declared `thinking` capability). Non-effort modes (`default`,
/// `off`, `minimal`) carry no model-effort requirement.
fn check_thinking_support(
    conn: &Connection,
    scope: &ContextScope,
    level: &str,
) -> Result<(), ContractError> {
    if !THINKING_EFFORTS.contains(&level) {
        return Ok(());
    }
    let Some((provider_id, model_id)) = effective_model(conn, scope)? else {
        return Err(model_capability_unsupported(format!(
            "no effective model is selected on the session→project→instance chain; \
             cannot accept thinking level `{level}`"
        )));
    };
    let Some(efforts) = model_thinking_efforts(conn, &provider_id, &model_id)? else {
        return Err(model_capability_unsupported(format!(
            "model `{provider_id}/{model_id}` declares no `thinking` capability; \
             cannot accept thinking level `{level}`"
        )));
    };
    if !efforts.iter().any(|effort| effort == level) {
        return Err(model_capability_unsupported(format!(
            "model `{provider_id}/{model_id}` supports thinking levels {efforts:?}; \
             `{level}` is not among them"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// History observation (the t012 history system's tables, read-only)
// ---------------------------------------------------------------------------

fn history_counts(conn: &Connection, session_id: &str) -> Result<(i64, i64), ContractError> {
    conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT turn_id) FROM history_entries WHERE session_id = ?1",
        params![session_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(context_sql_err)
}

fn history_entry_count(conn: &Connection, session_id: &str) -> Result<i64, ContractError> {
    Ok(history_counts(conn, session_id)?.0)
}

/// Fetches bounded history JSON rows inside the brief SQLite lock; large
/// histories (`> MAX_BUDGET_HISTORY_ROWS` or `> MAX_BUDGET_HISTORY_BYTES`)
/// skip fetching and degrade the budget report cleanly to `"unknown"`.
fn fetch_history_content_rows(
    conn: &Connection,
    session_id: &str,
    entry_count: i64,
) -> Result<Option<Vec<String>>, ContractError> {
    if entry_count > MAX_BUDGET_HISTORY_ROWS {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare("SELECT content_json FROM history_entries WHERE session_id = ?1 ORDER BY seq")
        .map_err(context_sql_err)?;
    let mut rows_iter = stmt
        .query_map(params![session_id], |r| r.get::<_, String>(0))
        .map_err(context_sql_err)?;
    let mut rows = Vec::with_capacity(entry_count.max(0) as usize);
    let mut total_bytes: usize = 0;
    for row in &mut rows_iter {
        let raw = row.map_err(context_sql_err)?;
        total_bytes = total_bytes.saturating_add(raw.len());
        if total_bytes > MAX_BUDGET_HISTORY_BYTES || rows.len() as i64 >= MAX_BUDGET_HISTORY_ROWS {
            return Ok(None);
        }
        rows.push(raw);
    }
    Ok(Some(rows))
}

fn unknown_history_budget() -> Value {
    json!({
        "status": "unknown",
        "value": Value::Null,
        "unit": "tokens",
        "source": "history_entries",
    })
}

fn estimate_part_tokens(part: &Value) -> Result<usize, ()> {
    let kind = part.get("kind").and_then(Value::as_str);
    match kind {
        Some("text") => {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                crate::model_cost::estimate_text_tokens(text).map_err(|_| ())
            } else {
                Ok(0)
            }
        }
        Some("thinking") => {
            if let Some(text) = part
                .get("reasoningContent")
                .or_else(|| part.get("text"))
                .and_then(Value::as_str)
            {
                crate::model_cost::estimate_text_tokens(text).map_err(|_| ())
            } else {
                Ok(0)
            }
        }
        Some("attachment")
        | Some("image")
        | Some("pdf")
        | Some("audio")
        | Some("mediaReference")
        | Some("media_reference") => Ok(crate::model_cost::MULTIMEDIA_TOKENS),
        Some("toolCall") | Some("tool_call") | Some("toolUse") | Some("tool_use") => {
            let mut combined = String::new();
            if let Some(name) = part.get("name").and_then(Value::as_str) {
                combined.push_str(name);
            }
            if let Some(input) = part.get("input").or_else(|| part.get("arguments"))
                && !input.is_null()
                && let Ok(serialized) = serde_json::to_string(input)
            {
                combined.push_str(&serialized);
            }
            crate::model_cost::estimate_text_tokens(&combined).map_err(|_| ())
        }
        Some("toolResult") | Some("tool_result") | Some("toolResultReference") => {
            if let Some(preview) = part.get("preview").and_then(Value::as_str) {
                crate::model_cost::estimate_text_tokens(preview).map_err(|_| ())
            } else if let Some(content) = part.get("content") {
                if let Some(text) = content.as_str() {
                    crate::model_cost::estimate_text_tokens(text).map_err(|_| ())
                } else if let Ok(serialized) = serde_json::to_string(content) {
                    crate::model_cost::estimate_text_tokens(&serialized).map_err(|_| ())
                } else {
                    Ok(0)
                }
            } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                crate::model_cost::estimate_text_tokens(text).map_err(|_| ())
            } else {
                Ok(0)
            }
        }
        _ => {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                crate::model_cost::estimate_text_tokens(text).map_err(|_| ())
            } else if let Ok(serialized) = serde_json::to_string(part) {
                crate::model_cost::estimate_text_tokens(&serialized).map_err(|_| ())
            } else {
                Ok(0)
            }
        }
    }
}

/// The context budget: CPU-bound o200k token estimation executed outside any
/// SQLite mutex lock and outside Tokio async workers. Includes message
/// overhead, text/thinking/tool parts, and multimedia placeholders matching
/// `model_cost`. Any parse, size-bound, or tokenizer failure degrades cleanly
/// to `status: "unknown"`.
fn estimate_history_budget(rows: Option<&[String]>) -> Value {
    let Some(rows) = rows else {
        return unknown_history_budget();
    };
    let mut tokens: usize = 0;
    for raw in rows {
        let Ok(content) = serde_json::from_str::<Value>(raw) else {
            return unknown_history_budget();
        };
        let Some(parts) = content.as_array() else {
            continue;
        };
        tokens = tokens.saturating_add(crate::model_cost::PER_MESSAGE_OVERHEAD);
        for part in parts {
            let Ok(count) = estimate_part_tokens(part) else {
                return unknown_history_budget();
            };
            tokens = tokens.saturating_add(count);
        }
    }
    json!({
        "status": "estimated",
        "value": tokens,
        "unit": "tokens",
        "source": "history_entries",
    })
}

/// The session's compaction state for `inspect`: `"none"` (nothing has ever
/// been folded), `"compacted"` (history was folded into a summary), or
/// `"failed"` (an attempt failed; history is intact).
fn compaction_state(conn: &Connection, session_id: &str) -> Result<String, ContractError> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT state, entries_folded FROM v1_context_compactions
             WHERE session_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(context_sql_err)?;
    Ok(match row {
        Some((state, folded)) => match (state.as_str(), folded) {
            ("failed", _) => "failed".to_owned(),
            ("succeeded", n) if n > 0 => "compacted".to_owned(),
            _ => "none".to_owned(),
        },
        None => "none".to_owned(),
    })
}

/// Whether the session still has an active (non-terminal) or queued run
/// (`commands.rs`'s busy gate; takes the held connection to avoid
/// double-locking the store mutex).
fn session_has_active_or_queued_runs(
    conn: &Connection,
    session_id: &str,
) -> Result<bool, ContractError> {
    let active: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id = ?1 AND terminal = 0)
             OR EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(context_sql_err)?;
    Ok(active != 0)
}

// ---------------------------------------------------------------------------
// Style catalog (on-disk discovery; the `commands.rs` scan precedent)
// ---------------------------------------------------------------------------

struct StyleFile {
    style_id: String,
    name: String,
    revision: String,
}

impl StyleFile {
    fn item_json(&self) -> Value {
        json!({
            "styleId": self.style_id,
            "name": self.name,
            "revision": self.revision,
            "available": true,
        })
    }
}

/// One reload diagnostic (the completion `Diagnostic` shape).
struct StyleDiagnostic {
    code: &'static str,
    message: String,
    resource_id: Option<String>,
}

impl StyleDiagnostic {
    fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("code".into(), json!(self.code));
        obj.insert("severity".into(), json!("warning"));
        obj.insert("message".into(), json!(self.message));
        if let Some(resource_id) = &self.resource_id {
            obj.insert("resourceId".into(), json!(resource_id));
        }
        Value::Object(obj)
    }
}

/// Splits a leading `---` frontmatter block off the raw file (the
/// `commands.rs` split: a missing or unterminated block leaves the whole
/// file as body).
fn split_frontmatter(raw: &str) -> (Option<String>, String) {
    let Some(after_open) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (None, raw.to_owned());
    };
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed == "---" {
            let block = after_open[..offset].to_owned();
            let body = after_open[offset + line.len()..].to_owned();
            return (Some(block), body);
        }
        offset += line.len();
    }
    (None, raw.to_owned())
}

/// Recursively scans one style namespace. A missing directory contributes
/// nothing (an empty namespace is not a failure); a directory that exists
/// but cannot be read fails the whole scan (`UNAVAILABLE` — 「加载失败不同于
/// 空」). Real directories and `.md` files only (symlinks skipped, matching
/// the `commands.rs` dirent-type checks). `base` carries the path already
/// descended relative to the namespace root, so a nested file's styleId keeps
/// its directory prefix (`styleId = <namespace>:<相对路径去.md>`).
fn style_path_escape(message: String) -> ContractError {
    ContractError::new(
        CODE_PATH_ESCAPE,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn open_style_file_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    opts.open(path)
}

/// Inspects each path component from `boundary_root` down to the target styles
/// directory with `symlink_metadata`, refusing symlinks and verifying canonical
/// containment via `files::require_within`.
fn check_styles_root_dir(
    boundary_root: &Path,
    components: &[&Path],
    display_root: &Path,
) -> Result<Option<PathBuf>, ContractError> {
    let mut last_path: Option<PathBuf> = None;
    for (idx, path) in components.iter().enumerate() {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(context_unavailable(format!(
                    "styles directory `{}` cannot be inspected: {error}",
                    path.display()
                )));
            }
        };
        if meta.file_type().is_symlink() {
            return Err(style_path_escape(format!(
                "styles directory `{}` must not be a symlink",
                display_root.display()
            )));
        }
        if meta.is_dir() {
            let canonical = std::fs::canonicalize(path).map_err(|error| {
                context_unavailable(format!(
                    "styles directory `{}` cannot be resolved: {error}",
                    path.display()
                ))
            })?;
            require_within(boundary_root, &canonical)?;
            last_path = Some(canonical);
        } else if idx + 1 == components.len() {
            return Ok(Some((*path).to_path_buf()));
        } else {
            return Err(context_unavailable(format!(
                "styles directory `{}` parent is not a directory",
                display_root.display()
            )));
        }
    }
    Ok(last_path)
}

/// Iteratively scans one style namespace with bounded depth (`MAX_STYLE_DEPTH`),
/// bounded file count (`MAX_STYLE_FILES`), per-file byte limit
/// (`MAX_STYLE_FILE_BYTES`), canonical containment verification, and `O_NOFOLLOW`
/// regular-file reads. Missing roots contribute nothing; unreadable roots fail
/// (`UNAVAILABLE` — 「加载失败不同于空」); individual bad/oversized/symlinked
/// entries skip cleanly with diagnostics.
fn scan_style_dir(
    root: &Path,
    namespace: &'static str,
    out: &mut Vec<StyleFile>,
    diagnostics: &mut Vec<StyleDiagnostic>,
) -> Result<(), ContractError> {
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    let mut files_seen: usize = 0;
    let mut file_limit_notified = false;

    while let Some((dir_path, base, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir_path) {
            Ok(entries) => entries,
            Err(error) if depth == 0 && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) if depth == 0 => {
                return Err(context_unavailable(format!(
                    "styles directory `{}` cannot be read: {error}",
                    dir_path.display()
                )));
            }
            Err(error) => {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_UNREADABLE",
                    message: format!(
                        "style directory `{namespace}:{base}` cannot be read: {error}"
                    ),
                    resource_id: None,
                });
                continue;
            }
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        let mut subdirs: Vec<(PathBuf, String, usize)> = Vec::new();

        for entry in entries {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name_str = entry.file_name().to_string_lossy().replace('\\', "/");
            let path = entry.path();
            let rel_path = if base.is_empty() {
                name_str.clone()
            } else {
                format!("{base}/{name_str}")
            };

            if file_type.is_symlink() {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_SYMLINK_SKIPPED",
                    message: format!("skipping symlinked style entry `{namespace}:{rel_path}`"),
                    resource_id: None,
                });
                continue;
            }
            if file_type.is_dir() {
                if depth >= MAX_STYLE_DEPTH {
                    diagnostics.push(StyleDiagnostic {
                        code: "STYLE_DEPTH_LIMIT",
                        message: format!(
                            "style directory `{namespace}:{rel_path}` exceeds max depth ({MAX_STYLE_DEPTH})"
                        ),
                        resource_id: None,
                    });
                    continue;
                }
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.file_type().is_symlink() {
                    diagnostics.push(StyleDiagnostic {
                        code: "STYLE_SYMLINK_SKIPPED",
                        message: format!(
                            "skipping symlinked style directory `{namespace}:{rel_path}`"
                        ),
                        resource_id: None,
                    });
                    continue;
                }
                if !meta.is_dir() {
                    continue;
                }
                let canonical = std::fs::canonicalize(&path).map_err(|error| {
                    context_unavailable(format!(
                        "style directory `{namespace}:{rel_path}` cannot be resolved: {error}"
                    ))
                })?;
                require_within(root, &canonical)?;
                subdirs.push((canonical, rel_path, depth + 1));
                continue;
            }
            if !file_type.is_file() || !name_str.ends_with(".md") {
                continue;
            }
            let Some(relative) = rel_path.strip_suffix(".md").map(str::to_owned) else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            if files_seen >= MAX_STYLE_FILES {
                if !file_limit_notified {
                    diagnostics.push(StyleDiagnostic {
                        code: "STYLE_FILE_LIMIT",
                        message: format!(
                            "style catalog limit ({MAX_STYLE_FILES} files) reached in `{namespace}` namespace"
                        ),
                        resource_id: None,
                    });
                    file_limit_notified = true;
                }
                break;
            }
            files_seen += 1;

            let file = match open_style_file_nofollow(&path) {
                Ok(file) => file,
                Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                    diagnostics.push(StyleDiagnostic {
                        code: "STYLE_SYMLINK_SKIPPED",
                        message: format!("skipping symlinked style file `{namespace}:{relative}`"),
                        resource_id: Some(format!("{namespace}:{relative}")),
                    });
                    continue;
                }
                Err(error) => {
                    diagnostics.push(StyleDiagnostic {
                        code: "STYLE_UNREADABLE",
                        message: format!(
                            "style file `{namespace}:{relative}` cannot be read: {error}"
                        ),
                        resource_id: Some(format!("{namespace}:{relative}")),
                    });
                    continue;
                }
            };
            let Ok(meta) = file.metadata() else {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_UNREADABLE",
                    message: format!("style file `{namespace}:{relative}` metadata unavailable"),
                    resource_id: Some(format!("{namespace}:{relative}")),
                });
                continue;
            };
            if !meta.is_file() {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_UNREADABLE",
                    message: format!("style file `{namespace}:{relative}` is not a regular file"),
                    resource_id: Some(format!("{namespace}:{relative}")),
                });
                continue;
            }
            if meta.len() > MAX_STYLE_FILE_BYTES as u64 {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_TOO_LARGE",
                    message: format!(
                        "style file `{namespace}:{relative}` exceeds maximum size ({MAX_STYLE_FILE_BYTES} bytes)"
                    ),
                    resource_id: Some(format!("{namespace}:{relative}")),
                });
                continue;
            }
            let mut bytes = Vec::new();
            if let Err(error) = file
                .take((MAX_STYLE_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
            {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_UNREADABLE",
                    message: format!("style file `{namespace}:{relative}` cannot be read: {error}"),
                    resource_id: Some(format!("{namespace}:{relative}")),
                });
                continue;
            }
            if bytes.len() > MAX_STYLE_FILE_BYTES {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_TOO_LARGE",
                    message: format!(
                        "style file `{namespace}:{relative}` exceeds maximum size ({MAX_STYLE_FILE_BYTES} bytes)"
                    ),
                    resource_id: Some(format!("{namespace}:{relative}")),
                });
                continue;
            }
            let raw = String::from_utf8_lossy(&bytes);
            let (_block, body) = split_frontmatter(&raw);
            if body.trim().is_empty() {
                diagnostics.push(StyleDiagnostic {
                    code: "STYLE_EMPTY",
                    message: format!(
                        "style file `{namespace}:{relative}` has no content; a style is \
                         prompt content and cannot be empty"
                    ),
                    resource_id: Some(format!("{namespace}:{relative}")),
                });
                continue;
            }
            let name = relative.rsplit('/').next().unwrap_or(&relative).to_owned();
            out.push(StyleFile {
                style_id: format!("{namespace}:{relative}"),
                name,
                revision: format!(
                    "stylerev-{}",
                    &format!("{:x}", Sha256::digest(&bytes))[..24]
                ),
            });
        }
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }
    Ok(())
}

/// The style catalog visible at one scope: the project workspace's
/// `.pilotdeck/styles` plus the instance home's `styles`, deduplicated by
/// name (project wins over user — the TS precedence), sorted by styleId.
fn style_catalog(
    rt: &PublicV1Runtime,
    scope: &ContextScope,
) -> Result<(Vec<StyleFile>, Vec<StyleDiagnostic>), ContractError> {
    let mut files: Vec<StyleFile> = Vec::new();
    let mut diagnostics: Vec<StyleDiagnostic> = Vec::new();
    if let ContextScope::Project { project_id } | ContextScope::Session { project_id, .. } = scope {
        match resolve_project(rt.store(), project_id) {
            Ok(workspace) => {
                let pilotdeck_dir = workspace.workspace.join(".pilotdeck");
                let project_styles = pilotdeck_dir.join("styles");
                if let Some(verified_root) = check_styles_root_dir(
                    &workspace.workspace,
                    &[&pilotdeck_dir, &project_styles],
                    &project_styles,
                )? {
                    scan_style_dir(&verified_root, "project", &mut files, &mut diagnostics)?;
                }
            }
            // A virtual project (no host-declared root) contributes no
            // project namespace — the TS scan of a missing directory.
            Err(crate::public_v1::domains::files::ProjectResolveError::Virtual(_)) => {}
            Err(crate::public_v1::domains::files::ProjectResolveError::Contract(error)) => {
                return Err(error);
            }
            // Unreachable after parse_scope's existence check; kept for
            // exhaustiveness.
            Err(crate::public_v1::domains::files::ProjectResolveError::NotFound(_)) => {}
        }
    }
    let home_root = std::fs::canonicalize(rt.store().home()).map_err(|error| {
        context_unavailable(format!(
            "instance home `{}` cannot be resolved: {error}",
            rt.store().home().display()
        ))
    })?;
    let user_styles = rt.store().home().join("styles");
    if let Some(verified_user) = check_styles_root_dir(&home_root, &[&user_styles], &user_styles)? {
        scan_style_dir(&verified_user, "user", &mut files, &mut diagnostics)?;
    }
    let mut seen: Vec<String> = Vec::new();
    files.retain(|file| {
        if seen.contains(&file.name) {
            return false;
        }
        seen.push(file.name.clone());
        true
    });
    files.sort_by(|a, b| a.style_id.cmp(&b.style_id));
    Ok((files, diagnostics))
}

/// The scope's applied style-catalog revision: the stored reload state, or
/// the instance initialization anchor when no reload has happened yet (the
/// deterministic-anchor scheme; this is the revision `context.styles`
/// reports as `snapshotRevision` and `context.reload_styles` CASes against).
fn style_state_revision(
    conn: &Connection,
    initialization_revision: &str,
    scope: &ContextScope,
) -> Result<String, ContractError> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT revision FROM v1_context_style_state WHERE scope_key = ?1",
            params![scope.key()],
            |r| r.get(0),
        )
        .optional()
        .map_err(context_sql_err)?;
    Ok(stored.unwrap_or_else(|| initialization_revision.to_owned()))
}

// ---------------------------------------------------------------------------
// Pagination (the `models.rs` offset-cursor scheme)
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
            context_invalid_argument(format!("invalid pagination cursor `{cursor}`"))
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
// Durable event append (the `models.rs` two-step cursor scheme)
// ---------------------------------------------------------------------------

fn append_context_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<(), ContractError> {
    let placeholder_cursor = format!(
        "tmp-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.context.event/1', ?5, ?6, ?7)",
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
    .map_err(context_sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = crate::public_v1::store::format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(context_sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The per-home domain DB (the `commands.rs` cache: the shared
// `v1_domain_operations` accept/finish records live on a second connection)
// ---------------------------------------------------------------------------

fn context_domain_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
    fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<DomainDb>>> {
        static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }
    let home = rt.store().home().to_path_buf();
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(db) = reg.get(&home).and_then(Weak::upgrade) {
        return Ok(db);
    }
    let db = Arc::new(DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?);
    reg.insert(home, Arc::downgrade(&db));
    Ok(db)
}

// ---------------------------------------------------------------------------
// context.inspect
// ---------------------------------------------------------------------------

async fn context_inspect(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = input
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| context_invalid_argument("`sessionId` is required"))?
        .to_owned();
    let run_id = input
        .get("runId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let rt_cloned = Arc::clone(&rt);
    let payload = tokio::task::spawn_blocking(move || -> Result<Value, ContractError> {
        let store = rt_cloned.store();
        let session = store
            .get_session(&session_id)
            .map_err(|_| context_not_found("session", &session_id))?;
        // `get_session` wraps the row: {"session": {...}, "snapshotCursor"}.
        let Some(project_id) = session["session"].get("projectId").and_then(Value::as_str) else {
            return Err(context_invalid_argument(format!(
                "session `{session_id}` has no project"
            )));
        };
        let scope = ContextScope::Session {
            project_id: project_id.to_owned(),
            session_id: session_id.clone(),
        };

        // Optional run observation: the run must exist and belong to this
        // session (`NOT_FOUND` otherwise — it is in this operation's error list).
        let run_view = match run_id.as_deref() {
            None => None,
            Some(rid) => {
                let run = store
                    .get_run(rid)
                    .map_err(|_| context_not_found("run", rid))?;
                if run.session_id != session_id {
                    return Err(context_not_found("run in this session", rid));
                }
                Some((
                    run.state.as_str().to_owned(),
                    run.config_revision.clone(),
                    run.effective_generation.clone(),
                ))
            }
        };

        let (
            view,
            entry_count,
            turn_count,
            history_rows,
            model,
            compaction,
            effective_generation,
        ) = {
            let conn = store
                .conn
                .lock()
                .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
            let view = resolve_settings(&conn, &store.meta().initialization_revision, &scope)?;
            let (entry_count, turn_count) = history_counts(&conn, &session_id)?;
            let history_rows = fetch_history_content_rows(&conn, &session_id, entry_count)?;
            let model = effective_model(&conn, &scope)?;
            let compaction = compaction_state(&conn, &session_id)?;
            let effective_generation =
                crate::public_v1::domains::configuration::current_effective_generation_or(
                    &conn,
                    &store.meta().generation,
                )?;
            (
                view,
                entry_count,
                turn_count,
                history_rows,
                model,
                compaction,
                effective_generation,
            )
        };
        // BPE tokenization and JSON parsing happen after releasing `store.conn`
        // inside `spawn_blocking`.
        let budget = estimate_history_budget(history_rows.as_deref());

        // The composition observation: server-derived pairs only — no raw
        // internal snapshots leave the gateway (「不泄露原始内部快照」).
        let mut sources: Vec<Value> = vec![
            json!({"key": "historyEntryCount", "value": entry_count}),
            json!({"key": "historyTurnCount", "value": turn_count}),
            json!({"key": "thinking", "value": view.thinking}),
            json!({"key": "thinkingSourceScope", "value": view.thinking_source.0.to_scope_json()}),
            json!({"key": "outputStyleId", "value": view.output_style_id}),
            json!({"key": "outputStyleSourceScope", "value": view.style_source.0.to_scope_json()}),
            // 「实际模型配置可核对」: the effective model through the models
            // domain's own selection chain (session→project→instance).
            json!({"key": "effectiveModel", "value": match &model {
                Some((provider_id, model_id)) => json!({"providerId": provider_id, "modelId": model_id}),
                None => Value::Null,
            }}),
            json!({"key": "compactionState", "value": compaction}),
        ];
        if let Some((state, config_revision, generation)) = run_view {
            // The run's submit-time execution snapshot — the same values
            // `run.get` reports, proving the in-flight turn keeps its own view.
            sources.push(json!({"key": "runState", "value": state}));
            sources.push(json!({"key": "runConfigRevision", "value": config_revision}));
            sources.push(json!({"key": "runEffectiveGeneration", "value": generation}));
        }

        Ok(json!({
            "sources": sources,
            "budget": budget,
            "compactionState": compaction,
            "revision": view.revision,
            "effectiveGeneration": effective_generation,
        }))
    })
    .await
    .map_err(|err| {
        DispatchError::from(context_unavailable(format!(
            "blocking context inspect failed: {err}"
        )))
    })??;

    Ok((ResponseOutcome::Succeeded, payload))
}

// ---------------------------------------------------------------------------
// context.styles
// ---------------------------------------------------------------------------

async fn context_styles(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rt_cloned = Arc::clone(&rt);
    let payload = tokio::task::spawn_blocking(move || -> Result<Value, ContractError> {
        let scope = parse_scope(&rt_cloned, &input)?;
        let (offset, limit) = parse_page(input.get("page"))?;
        let store = rt_cloned.store();
        let snapshot_revision = {
            let conn = store
                .conn
                .lock()
                .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
            style_state_revision(&conn, &store.meta().initialization_revision, &scope)?
        };
        let (catalog, _diagnostics) = style_catalog(&rt_cloned, &scope)?;
        let total = catalog.len();
        if offset > total {
            return Err(context_invalid_argument(
                "pagination cursor beyond end of collection",
            ));
        }
        let end = offset.saturating_add(limit).min(total);
        let items: Vec<Value> = catalog[offset..end]
            .iter()
            .map(|style| style.item_json())
            .collect();
        Ok(json!({
            "items": items,
            "pageInfo": page_info(&snapshot_revision, end, total),
        }))
    })
    .await
    .map_err(|err| {
        DispatchError::from(context_unavailable(format!(
            "blocking context styles failed: {err}"
        )))
    })??;

    Ok((ResponseOutcome::Succeeded, payload))
}

// ---------------------------------------------------------------------------
// context.configure
// ---------------------------------------------------------------------------

async fn context_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = input
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| context_invalid_argument("`sessionId` is required"))?
        .to_owned();
    let rt_cloned = Arc::clone(&rt);
    let (changed, payload) =
        tokio::task::spawn_blocking(move || -> Result<(bool, Value), ContractError> {
            let store = rt_cloned.store();
            // The error list carries no NOT_FOUND: a missing session is malformed
            // input for this write (the `parse_scope` precedent).
            let session = store.get_session(&session_id).map_err(|_| {
                context_invalid_argument(format!("session `{session_id}` does not exist"))
            })?;
            // `get_session` wraps the row: {"session": {...}, "snapshotCursor"}.
            let Some(project_id) = session["session"].get("projectId").and_then(Value::as_str)
            else {
                return Err(context_invalid_argument(format!(
                    "session `{session_id}` has no project"
                )));
            };
            let scope = ContextScope::Session {
                project_id: project_id.to_owned(),
                session_id: session_id.clone(),
            };

            // Envelope-level expectedRevision wins over the input field (the
            // `model_selection.set` precedence).
            let expected = expected_revision
                .as_deref()
                .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned();

            // Present-vs-absent distinction: `null` clears, omission preserves
            // (「null清除/省略保留」).
            let requested_thinking = input.get("thinking");
            let requested_style = input.get("outputStyleId");

            // Read-only validations before the CAS transaction (filesystem scan
            // happens outside any SQLite lock).
            if let Some(Value::String(level)) = requested_thinking
                && !THINKING_MODES.contains(&level.as_str())
            {
                return Err(context_invalid_argument(format!(
                    "thinking must be one of {THINKING_MODES:?}; got `{level}`"
                )));
            }
            if let Some(Value::String(style_id)) = requested_style {
                let (catalog, _diagnostics) = style_catalog(&rt_cloned, &scope)?;
                if !catalog.iter().any(|style| style.style_id == *style_id) {
                    return Err(style_not_found(style_id));
                }
            }

            let mut conn = store
                .conn
                .lock()
                .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(context_sql_err)?;

            // CAS against the session's chain revision.
            let current = resolve_settings(&tx, &store.meta().initialization_revision, &scope)?;
            if current.revision != expected {
                return Err(context_revision_conflict(&expected, &current.revision));
            }
            // 「不支持thinking拒绝」: the effective model must declare support for
            // the requested effort level (after the CAS so the conflict answer wins).
            if let Some(Value::String(level)) = requested_thinking {
                check_thinking_support(&tx, &scope, level)?;
            }

            let own = load_setting(&tx, &scope)?;
            let new_thinking = match requested_thinking {
                Some(Value::Null) => None,
                Some(Value::String(level)) => Some(level.clone()),
                _ => own.as_ref().and_then(|row| row.thinking.clone()),
            };
            let new_style = match requested_style {
                Some(Value::Null) => None,
                Some(Value::String(style_id)) => Some(style_id.clone()),
                _ => own.as_ref().and_then(|row| row.output_style_id.clone()),
            };
            let changed = match &own {
                Some(row) => row.thinking != new_thinking || row.output_style_id != new_style,
                None => new_thinking.is_some() || new_style.is_some(),
            };
            let now = crate::public_v1::store::now_rfc3339();
            if changed {
                let new_revision = context_next_rev_in_tx(&tx)?;
                if new_thinking.is_none() && new_style.is_none() {
                    // Clearing the last field removes the session's override row —
                    // the ancestor layers show through again.
                    tx.execute(
                        "DELETE FROM v1_context_settings WHERE scope_key = ?1",
                        params![scope.key()],
                    )
                    .map_err(context_sql_err)?;
                } else {
                    tx.execute(
                        "INSERT INTO v1_context_settings
                             (scope_key, scope_json, thinking, output_style_id, revision, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT(scope_key) DO UPDATE SET
                            scope_json = excluded.scope_json,
                            thinking = excluded.thinking,
                            output_style_id = excluded.output_style_id,
                            revision = excluded.revision,
                            updated_at = excluded.updated_at",
                        params![
                            scope.key(),
                            serde_json::to_string(&scope.to_scope_json()).unwrap_or_default(),
                            new_thinking,
                            new_style,
                            new_revision,
                            now,
                        ],
                    )
                    .map_err(context_sql_err)?;
                }
                // The contract's declared event (`configuration.changed`). The
                // in-flight runs are untouched — they keep their submit-time
                // snapshot (`store.rs::submit_run` writes it at queue time), so a
                // running turn is never hot-swapped (「在途run不热换」).
                append_context_event_in_tx(
                    &tx,
                    &scope.key(),
                    &session_id,
                    "configuration.changed",
                    &json!({
                        "sessionId": session_id,
                        "thinking": new_thinking,
                        "outputStyleId": new_style,
                        "revision": new_revision,
                    }),
                    &now,
                )?;
            }
            tx.commit().map_err(context_sql_err)?;

            // Report the post-write truth: per-key effective values attributed to
            // the layer that provides them (「有效值和继承来源可读」).
            let after = resolve_settings(&conn, &store.meta().initialization_revision, &scope)?;
            let effective_generation =
                crate::public_v1::domains::configuration::current_effective_generation_or(
                    &conn,
                    &store.meta().generation,
                )?;
            let setting = |key: &str, value: &Option<String>, source: &(ContextScope, String)| -> Value {
                json!({
                    "key": key,
                    "value": value,
                    "sourceScope": source.0.to_scope_json(),
                    "sourceRevision": source.1,
                    "sensitive": false,
                })
            };
            Ok((
                changed,
                json!({
                    "revision": after.revision,
                    "effectiveGeneration": effective_generation,
                    "values": [
                        setting("thinking", &after.thinking, &after.thinking_source),
                        setting("outputStyleId", &after.output_style_id, &after.style_source),
                    ],
                }),
            ))
        })
        .await
        .map_err(|err| {
            DispatchError::from(context_unavailable(format!(
                "blocking context configure failed: {err}"
            )))
        })??;

    if changed {
        rt.store().event_notifier().notify_waiters();
        rt.store().state_notifier().notify_waiters();
    }
    Ok((ResponseOutcome::Succeeded, payload))
}

// ---------------------------------------------------------------------------
// context.reload_styles
// ---------------------------------------------------------------------------

/// The reload's execution after acceptance: re-scan the catalog (bad files
/// surface as diagnostics while the valid entries stay listed — 「坏风格保留
/// 旧有效目录」), re-verify `expectedRevision` under `BEGIN IMMEDIATE`, then
/// advance the scope's applied catalog revision (「成功revision改变」).
fn reload_styles_execution(
    rt: &PublicV1Runtime,
    scope: &ContextScope,
    session_id: &str,
    expected: &str,
) -> Result<Value, ContractError> {
    let (catalog, diagnostics) = style_catalog(rt, scope)?;
    let diagnostics_json: Vec<Value> = diagnostics.iter().map(StyleDiagnostic::to_json).collect();
    let store = rt.store();
    let mut conn = store
        .conn
        .lock()
        .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(context_sql_err)?;
    let current = style_state_revision(&tx, &store.meta().initialization_revision, scope)?;
    if expected != current {
        return Err(context_revision_conflict(expected, &current));
    }
    let new_revision = context_next_rev_in_tx(&tx)?;
    let now = crate::public_v1::store::now_rfc3339();
    tx.execute(
        "INSERT INTO v1_context_style_state (scope_key, revision, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(scope_key) DO UPDATE SET
            revision = excluded.revision,
            updated_at = excluded.updated_at",
        params![scope.key(), new_revision, now],
    )
    .map_err(context_sql_err)?;
    append_context_event_in_tx(
        &tx,
        &scope.key(),
        session_id,
        "context.styles_reloaded",
        &json!({
            "scope": scope.to_scope_json(),
            "revision": new_revision,
            "styleCount": catalog.len(),
        }),
        &now,
    )?;
    tx.commit().map_err(context_sql_err)?;
    Ok(json!({
        "revision": new_revision,
        "diagnostics": diagnostics_json,
    }))
}

async fn context_reload_styles(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rt_cloned = Arc::clone(&rt);
    let (should_notify, receipt) =
        tokio::task::spawn_blocking(move || -> Result<(bool, Value), ContractError> {
            let scope = parse_scope(&rt_cloned, &input)?;
            let expected = input
                .get("expectedRevision")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let db = context_domain_db(&rt_cloned)?;
            let store = rt_cloned.store();

            let session_id = match &scope {
                ContextScope::Session { session_id, .. } => session_id.clone(),
                _ => String::new(),
            };
            let scope_for_validation = scope.clone();
            let store_for_validation = store.clone();
            let expected_for_validation = expected.clone();
            let validation = move || {
                let conn = store_for_validation
                    .conn
                    .lock()
                    .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
                let current = style_state_revision(
                    &conn,
                    &store_for_validation.meta().initialization_revision,
                    &scope_for_validation,
                )?;
                if expected_for_validation != current {
                    return Err(context_revision_conflict(
                        &expected_for_validation,
                        &current,
                    ));
                }
                Ok(())
            };
            let operation = accept_domain_operation(
                &db,
                "context.reload_styles",
                &input,
                idempotency_key.as_deref(),
                || format!("op-{}", rt_cloned.store().next_id()),
                validation,
            )?;
            // A deduplicated replay answers with the original receipt and must NOT
            // re-execute: the reload already applied (its revision bump is durable),
            // and running it again would double-apply the very operation the key
            // deduplicates — the follow-up CAS against the advanced revision would
            // then falsely conflict.
            if operation.receipt.get("deduplicated") == Some(&json!(true)) {
                return Ok((false, operation.receipt));
            }
            let operation_id = operation.operation_id.clone();

            // requests.lookup visibility (the `run.submit` / `git.*` /
            // `configuration.reload` precedent): a fresh acceptance also registers
            // its idempotent identity in the store's `idempotency_records`, so the
            // lost-response recovery lookup resolves the operation id. The domain
            // DB's own dedup table is not what `requests.lookup` reads.
            if let Some(key) = idempotency_key.as_deref() {
                record_context_operation_idempotency(
                    store,
                    key,
                    "context.reload_styles",
                    &compute_input_digest("context.reload_styles", &input),
                    &operation_id,
                    &operation.receipt,
                )?;
            }

            let _ = mark_domain_operation_running(&db, &operation_id, &session_id);
            // A post-accept failure (e.g. the catalog directory turning unreadable
            // or a concurrent CAS change) terminalizes the operation instead of
            // leaving it stuck mid-flight; the receipt stays the durable answer.
            let (state, completion) =
                match reload_styles_execution(&rt_cloned, &scope, &session_id, &expected) {
                    Ok(completion) => ("succeeded".to_owned(), completion),
                    Err(error) => (
                        "failed".to_owned(),
                        json!({"code": error.code, "message": error.message}),
                    ),
                };
            let _ = finish_domain_operation(&db, &operation_id, &session_id, &state, &completion);
            Ok((state == "succeeded", operation.receipt))
        })
        .await
        .map_err(|err| {
            DispatchError::from(context_unavailable(format!(
                "blocking context reload_styles failed: {err}"
            )))
        })??;

    if should_notify {
        rt.store().event_notifier().notify_waiters();
        rt.store().state_notifier().notify_waiters();
    }
    Ok((ResponseOutcome::Accepted, receipt))
}

// ---------------------------------------------------------------------------
// context.compact
// ---------------------------------------------------------------------------

/// Registers one accepted `context.*` operation in the store's
/// `idempotency_records` so `requests.lookup` can resolve it (mirrors
/// `configuration_store.rs::record_acceptance`).
fn record_context_operation_idempotency(
    store: &crate::public_v1::store::V1Store,
    key: &str,
    method_id: &str,
    digest: &str,
    operation_id: &str,
    receipt: &Value,
) -> Result<(), ContractError> {
    let now = crate::public_v1::store::now_rfc3339();
    let retained_until = match chrono::DateTime::parse_from_rfc3339(&now) {
        Ok(parsed) => (parsed + chrono::Duration::hours(24)).to_rfc3339(),
        Err(_) => now.clone(),
    };
    let resource = json!({"kind": "operation", "operationId": operation_id});
    let mut conn = store
        .conn
        .lock()
        .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(context_sql_err)?;
    tx.execute(
        "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest,
            request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')
         ON CONFLICT(idempotency_key, method_id) DO NOTHING",
        params![
            key,
            method_id,
            digest,
            serde_json::to_string(&resource).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(receipt).unwrap_or_else(|_| "null".into()),
            now,
            retained_until,
        ],
    )
    .map_err(context_sql_err)?;
    tx.commit().map_err(context_sql_err)?;
    Ok(())
}

/// Placeholder for the runtime task's `models.invoke` seam (the
/// `git.rs::invoke_suggestion_model` precedent): compaction's summary is a
/// model call (`CompactionEngine` performs "the second model call required to
/// summarize a conversation"). Until the seam lands, compact fails with
/// `CAPABILITY_UNAVAILABLE` (the code compact's contract error list carries
/// for exactly this) and never fakes a summary.
async fn invoke_compaction_model(session_id: &str, entry_count: i64) -> Result<(), ContractError> {
    Err(ContractError::new(
        CODE_CAPABILITY_UNAVAILABLE,
        format!(
            "compacting session `{session_id}` ({entry_count} history entries) needs the model \
             invocation seam (models.invoke), which is not available in this build"
        ),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    ))
}

/// Writes one audit row into `v1_context_compactions`.
#[allow(clippy::too_many_arguments)]
fn record_compaction(
    conn: &Connection,
    compaction_id: &str,
    session_id: &str,
    operation_id: &str,
    state: &str,
    entries_folded: i64,
    before_revision: &str,
    after_revision: Option<&str>,
) -> Result<(), ContractError> {
    conn.execute(
        "INSERT INTO v1_context_compactions
             (compaction_id, session_id, operation_id, state, entries_folded,
              before_revision, after_revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![
            compaction_id,
            session_id,
            operation_id,
            state,
            entries_folded,
            before_revision,
            after_revision,
            crate::public_v1::store::now_rfc3339(),
        ],
    )
    .map_err(context_sql_err)?;
    Ok(())
}

enum CompactPhase {
    Replayed(Value),
    Completed {
        receipt: Value,
        succeeded: bool,
    },
    NeedsModel {
        db: Arc<DomainDb>,
        operation_id: String,
        compaction_id: String,
        session_id: String,
        before_revision: String,
        entry_count: i64,
        receipt: Value,
    },
}

async fn context_compact(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = input
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| context_invalid_argument("`sessionId` is required"))?
        .to_owned();
    let rt_cloned = Arc::clone(&rt);
    let session_id_phase1 = session_id.clone();

    let stage = tokio::task::spawn_blocking(move || -> Result<CompactPhase, ContractError> {
        let store = rt_cloned.store();
        // No NOT_FOUND in compact's error list: a missing session is malformed
        // input for this write.
        let session = store.get_session(&session_id_phase1).map_err(|_| {
            context_invalid_argument(format!("session `{session_id_phase1}` does not exist"))
        })?;
        // `get_session` wraps the row: {"session": {...}, "snapshotCursor"}.
        let Some(project_id) = session["session"].get("projectId").and_then(Value::as_str) else {
            return Err(context_invalid_argument(format!(
                "session `{session_id_phase1}` has no project"
            )));
        };
        let scope = ContextScope::Session {
            project_id: project_id.to_owned(),
            session_id: session_id_phase1.clone(),
        };
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let db = context_domain_db(&rt_cloned)?;
        let scope_for_validation = scope.clone();
        let store_for_validation = store.clone();
        let session_for_validation = session_id_phase1.clone();
        let expected_for_validation = expected.clone();
        let validation = move || {
            let conn = store_for_validation
                .conn
                .lock()
                .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
            let view = resolve_settings(
                &conn,
                &store_for_validation.meta().initialization_revision,
                &scope_for_validation,
            )?;
            if view.revision != expected_for_validation {
                return Err(context_revision_conflict(
                    &expected_for_validation,
                    &view.revision,
                ));
            }
            // 「活动run拒绝SESSION_BUSY」: compaction only happens at a safe
            // session boundary — an idle session.
            if session_has_active_or_queued_runs(&conn, &session_for_validation)? {
                return Err(context_session_busy(&session_for_validation));
            }
            Ok(())
        };
        let operation = accept_domain_operation(
            &db,
            "context.compact",
            &input,
            idempotency_key.as_deref(),
            || format!("op-{}", rt_cloned.store().next_id()),
            validation,
        )?;
        // Deduplicated replays must immediately return the durable receipt
        // without resetting operation state or appending duplicate compaction rows
        // or lifecycle events.
        if operation.receipt.get("deduplicated") == Some(&json!(true)) {
            return Ok(CompactPhase::Replayed(operation.receipt));
        }

        let operation_id = operation.operation_id.clone();
        if let Some(key) = idempotency_key.as_deref() {
            record_context_operation_idempotency(
                store,
                key,
                "context.compact",
                &compute_input_digest("context.compact", &input),
                &operation_id,
                &operation.receipt,
            )?;
        }
        let _ = mark_domain_operation_running(&db, &operation_id, &session_id_phase1);

        let compaction_id = format!("cmp-{}", rt_cloned.store().next_id());
        let check_result: Result<(String, i64), ContractError> = (|| {
            let mut conn = store
                .conn
                .lock()
                .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(context_sql_err)?;
            let view = resolve_settings(&tx, &store.meta().initialization_revision, &scope)?;
            if view.revision != expected {
                return Err(context_revision_conflict(&expected, &view.revision));
            }
            if session_has_active_or_queued_runs(&tx, &session_id_phase1)? {
                return Err(context_session_busy(&session_id_phase1));
            }
            let entry_count = history_entry_count(&tx, &session_id_phase1)?;
            if entry_count == 0 {
                // Nothing to fold: an explicit no-op success. before == after;
                // the compaction state stays "none".
                record_compaction(
                    &tx,
                    &compaction_id,
                    &session_id_phase1,
                    &operation_id,
                    "succeeded",
                    0,
                    &view.revision,
                    Some(&view.revision),
                )?;
                tx.commit().map_err(context_sql_err)?;
                return Ok((view.revision, 0));
            }
            Ok((view.revision, entry_count))
        })();

        match check_result {
            Err(error) => {
                let completion = json!({"code": error.code, "message": error.message});
                let _ = finish_domain_operation(
                    &db,
                    &operation_id,
                    &session_id_phase1,
                    "failed",
                    &completion,
                );
                Ok(CompactPhase::Completed {
                    receipt: operation.receipt,
                    succeeded: false,
                })
            }
            Ok((before_revision, 0)) => {
                let completion = json!({
                    "beforeRevision": before_revision,
                    "afterRevision": before_revision,
                    "budget": json!({
                        "status": "estimated",
                        "value": 0,
                        "unit": "tokens",
                        "source": "history_entries",
                    }),
                });
                let _ = finish_domain_operation(
                    &db,
                    &operation_id,
                    &session_id_phase1,
                    "succeeded",
                    &completion,
                );
                Ok(CompactPhase::Completed {
                    receipt: operation.receipt,
                    succeeded: true,
                })
            }
            Ok((before_revision, entry_count)) => Ok(CompactPhase::NeedsModel {
                db,
                operation_id,
                compaction_id,
                session_id: session_id_phase1,
                before_revision,
                entry_count,
                receipt: operation.receipt,
            }),
        }
    })
    .await
    .map_err(|err| {
        DispatchError::from(context_unavailable(format!(
            "blocking context compact failed: {err}"
        )))
    })??;

    match stage {
        CompactPhase::Replayed(receipt) => Ok((ResponseOutcome::Accepted, receipt)),
        CompactPhase::Completed { receipt, succeeded } => {
            if succeeded {
                rt.store().event_notifier().notify_waiters();
                rt.store().state_notifier().notify_waiters();
            }
            Ok((ResponseOutcome::Accepted, receipt))
        }
        CompactPhase::NeedsModel {
            db,
            operation_id,
            compaction_id,
            session_id: compact_session_id,
            before_revision,
            entry_count,
            receipt,
        } => {
            // The fold needs the model seam. Until it lands the operation
            // fails explicitly; nothing is written to the history, so the
            // original entries — tool call/result pairing included — stay untouched.
            let outcome = invoke_compaction_model(&compact_session_id, entry_count).await;
            let error = match outcome {
                Err(error) => error,
                Ok(()) => ContractError::new(
                    CODE_CAPABILITY_UNAVAILABLE,
                    "compaction summary model invocation is not available in this build".to_owned(),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ),
            };
            let rt_finish = Arc::clone(&rt);
            tokio::task::spawn_blocking(move || -> Result<(), ContractError> {
                let recorded = (|| -> Result<(), ContractError> {
                    let mut conn = rt_finish
                        .store()
                        .conn
                        .lock()
                        .map_err(|_| context_unavailable("sqlite mutex poisoned"))?;
                    let tx = conn
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .map_err(context_sql_err)?;
                    record_compaction(
                        &tx,
                        &compaction_id,
                        &compact_session_id,
                        &operation_id,
                        "failed",
                        0,
                        &before_revision,
                        None,
                    )?;
                    tx.commit().map_err(context_sql_err)?;
                    Ok(())
                })();
                let completion = match recorded {
                    Err(record_error) => {
                        json!({"code": record_error.code, "message": record_error.message})
                    }
                    Ok(()) => json!({"code": error.code, "message": error.message}),
                };
                let _ = finish_domain_operation(
                    &db,
                    &operation_id,
                    &compact_session_id,
                    "failed",
                    &completion,
                );
                Ok(())
            })
            .await
            .map_err(|err| {
                DispatchError::from(context_unavailable(format!(
                    "blocking context compact finish failed: {err}"
                )))
            })??;
            Ok((ResponseOutcome::Accepted, receipt))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: contract positive/negative cases against the real dispatch table
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use agentcpm_pd_protocol::contract::RunState;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::{InstanceMeta, V1Store, now_rfc3339};
    use crate::public_v1::test_support::TempDir;

    use super::*;

    fn open_runtime(root_dir: &Path) -> (Arc<PublicV1Runtime>, String) {
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

    fn fresh_home(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, String, String) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, instance_id) = open_runtime(&root_dir);
        let initialization_revision = rt.store().meta().initialization_revision.clone();
        (tmp, rt, instance_id, initialization_revision)
    }

    /// Seeds a project with a real authorized workspace (the `commands.rs`
    /// test pattern) and a session; returns (workspace, project, session).
    fn seed_project_session(
        rt: &Arc<PublicV1Runtime>,
        tmp_root: &Path,
        project_id: &str,
    ) -> (PathBuf, String, String) {
        let store = rt.store();
        let declared_root = tmp_root.join("authorized-ctx");
        std::fs::create_dir_all(&declared_root).unwrap();
        let workspace = declared_root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    format!("root-{project_id}"),
                    store.meta().instance_id,
                    declared_root.display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        store
            .register_project(&json!({
                "projectId": project_id,
                "name": "context test project",
                "environmentId": "local",
                "root": {
                    "instanceId": store.meta().instance_id,
                    "rootId": format!("root-{project_id}"),
                    "relativePath": "ws",
                },
            }))
            .unwrap();
        let session_id = format!("s-{project_id}");
        store
            .create_session(&json!({"sessionId": session_id, "projectId": project_id}))
            .unwrap();
        (workspace, project_id.to_owned(), session_id)
    }

    fn write_style(dir: &Path, relative: &str, body: &str) {
        let path = dir.join(format!("{relative}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Registers a provider + model (with the declared thinking capability)
    /// and points the session-scope model selection at it.
    async fn seed_model(
        rt: &Arc<PublicV1Runtime>,
        instance_id: &str,
        initialization_revision: &str,
        project_id: &str,
        session_id: &str,
        thinking: Value,
    ) {
        rt.dispatch(
            "provider.write",
            json!({
                "instanceId": instance_id,
                "providerId": "prov-ctx",
                "name": "context seed provider",
                "protocol": "openai",
                "endpoint": "https://api.example.com/v1",
            }),
            None,
            None,
        )
        .await
        .expect("seed provider");
        rt.dispatch(
            "model.write",
            json!({
                "instanceId": instance_id,
                "model": {"providerId": "prov-ctx", "modelId": "m-ctx"},
                "name": "context seed model",
                "declaredCapabilities": [
                    {"key": "thinking", "value": thinking},
                ],
            }),
            None,
            None,
        )
        .await
        .expect("seed model");
        rt.dispatch(
            "model_selection.set",
            json!({
                "instanceId": instance_id,
                "scope": {"kind": "session", "projectId": project_id, "sessionId": session_id},
                "model": {"providerId": "prov-ctx", "modelId": "m-ctx"},
                "expectedRevision": initialization_revision,
            }),
            None,
            None,
        )
        .await
        .expect("seed model selection");
    }

    async fn dispatch_ok(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> Value {
        let (outcome, result) = rt.dispatch(method, input, None, None).await.unwrap();
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result(method, &result)
            .unwrap_or_else(|e| panic!("`{method}` result fails schema: {e:?}"));
        result
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> String {
        match rt.dispatch(method, input, None, None).await {
            Err(DispatchError::Application(error)) => error.code,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    async fn dispatch_accepted(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: &str,
    ) -> Value {
        let (outcome, receipt) = rt.dispatch(method, input, Some(key), None).await.unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result(method, &receipt)
            .unwrap_or_else(|e| panic!("`{method}` receipt fails schema: {e:?}"));
        receipt
    }

    async fn dispatch_err_with_key(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: &str,
    ) -> String {
        match rt.dispatch(method, input, Some(key), None).await {
            Err(DispatchError::Application(error)) => error.code,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    async fn wait_operation(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let (_, page) = rt
                .dispatch(
                    "operations.get",
                    json!({"operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            let operation = page["operation"].clone();
            let state = operation["state"].as_str().unwrap_or_default().to_owned();
            if matches!(
                state.as_str(),
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return operation;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "operation `{operation_id}` never became terminal (state `{state}`)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    fn source_value<'a>(result: &'a Value, key: &str) -> &'a Value {
        result["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pair| pair["key"].as_str() == Some(key))
            .map(|pair| &pair["value"])
            .unwrap_or(&Value::Null)
    }

    fn setting<'a>(result: &'a Value, key: &str) -> &'a Value {
        result["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|setting| setting["key"].as_str() == Some(key))
            .unwrap()
    }

    async fn wait_run_terminal(rt: &Arc<PublicV1Runtime>, run_id: &str) {
        // Async sleep: on a current_thread runtime the session worker only
        // progresses while the test yields.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !rt.store().get_run(run_id).unwrap().terminal {
            assert!(
                std::time::Instant::now() < deadline,
                "run `{run_id}` never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    // -----------------------------------------------------------------------
    // context.inspect
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn inspect_reports_composition_budget_and_model() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-inspect");
        let (workspace, project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-inspect");
        write_style(
            &workspace.join(".pilotdeck").join("styles"),
            "concise",
            "Be concise.",
        );
        seed_model(
            &rt,
            &iid,
            &init_rev,
            &project_id,
            &session_id,
            json!(["low", "high"]),
        )
        .await;
        rt.store()
            .append_history_entries(
                &session_id,
                &[
                    json!({"turnId": "t1", "role": "user", "content": [{"kind": "text", "text": "hello world"}]}),
                    json!({"turnId": "t1", "role": "assistant", "content": [{"kind": "text", "text": "hi there"}]}),
                    json!({"turnId": "t2", "role": "user", "content": [{"kind": "text", "text": "second turn"}]}),
                ],
            )
            .unwrap();

        let result = dispatch_ok(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id}),
        )
        .await;
        assert_eq!(result["compactionState"], json!("none"));
        assert_eq!(
            result["effectiveGeneration"],
            json!(rt.store().meta().generation)
        );
        // The revision is the deterministic anchor before any configure.
        assert_eq!(result["revision"], json!(init_rev));
        assert_eq!(*source_value(&result, "historyEntryCount"), json!(3));
        assert_eq!(*source_value(&result, "historyTurnCount"), json!(2));
        assert_eq!(*source_value(&result, "thinking"), json!(Value::Null));
        assert_eq!(*source_value(&result, "outputStyleId"), json!(Value::Null));
        // 「实际模型配置可核对」: the model the selection chain resolves.
        assert_eq!(
            *source_value(&result, "effectiveModel"),
            json!({"providerId": "prov-ctx", "modelId": "m-ctx"})
        );
        let budget = &result["budget"];
        assert_eq!(budget["status"], json!("estimated"));
        assert_eq!(budget["unit"], json!("tokens"));
        assert_eq!(budget["source"], json!("history_entries"));
        let value = budget["value"].as_u64().unwrap();
        assert!(value > 0, "token estimate over history text is positive");
    }

    #[tokio::test]
    async fn inspect_missing_session_and_foreign_run_are_not_found() {
        let (tmp, rt, iid, _init_rev) = fresh_home("ctx-inspect-nf");
        let (_ws, project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-nf");

        let code = dispatch_err(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": "s-none"}),
        )
        .await;
        assert_eq!(code, "NOT_FOUND");

        // A run of another session is not this session's context.
        rt.store()
            .create_session(&json!({"sessionId": "s-other", "projectId": project_id}))
            .unwrap();
        let receipt = rt
            .dispatch(
                "run.submit",
                json!({"sessionId": "s-other", "input": [{"kind": "text", "text": "x"}]}),
                Some("key-run-other"),
                None,
            )
            .await
            .unwrap()
            .1;
        let other_run = receipt["runId"].as_str().unwrap().to_owned();
        let code = dispatch_err(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id, "runId": other_run}),
        )
        .await;
        assert_eq!(code, "NOT_FOUND");

        // A missing runId is NOT_FOUND as well.
        let code = dispatch_err(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id, "runId": "run-none"}),
        )
        .await;
        assert_eq!(code, "NOT_FOUND");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inspect_during_active_run_does_not_damage_the_turn() {
        let (tmp, rt, iid, _init_rev) = fresh_home("ctx-inspect-busy");
        let (_ws, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-busy");
        rt.store()
            .append_history_entries(
                &session_id,
                &[json!({"turnId": "t1", "role": "user", "content": [{"kind": "text", "text": "before run"}]})],
            )
            .unwrap();
        // A run that stays busy for a bounded window (the offline driver's
        // sleep directive); `run.submit` starts the session worker itself.
        let receipt = rt
            .dispatch(
                "run.submit",
                json!({
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=800]"}],
                }),
                Some("key-inspect-busy"),
                None,
            )
            .await
            .unwrap()
            .1;
        let run_id = receipt["runId"].as_str().unwrap().to_owned();
        // Observe mid-turn: wait for the running state, then inspect.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while rt.store().get_run(&run_id).unwrap().state != RunState::Running {
            assert!(
                std::time::Instant::now() < deadline,
                "run never reached the running state"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let result = dispatch_ok(
            &rt,
            "context.inspect",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "runId": run_id,
            }),
        )
        .await;
        // The run observation is present and the run is still executing.
        assert_eq!(*source_value(&result, "runState"), json!("running"));
        let run = rt.store().get_run(&run_id).unwrap();
        assert!(
            !run.terminal,
            "the busy maintenance read must not end the turn"
        );
        // The history the turn is built on is unchanged.
        let count = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            history_entry_count(&conn, &session_id).unwrap()
        };
        assert_eq!(count, 1);

        // The run then completes on its own terms.
        wait_run_terminal(&rt, &run_id).await;
        let run = rt.store().get_run(&run_id).unwrap();
        assert_eq!(run.state, RunState::Succeeded);
    }

    // -----------------------------------------------------------------------
    // context.styles
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn styles_lists_namespaces_with_project_precedence_and_pagination() {
        let (tmp, rt, iid, _init_rev) = fresh_home("ctx-styles");
        let (workspace, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-sty");
        let home_styles = rt.store().home().join("styles");
        write_style(&home_styles, "concise", "user concise body");
        write_style(&home_styles, "nested/deep", "user nested body");
        let project_styles = workspace.join(".pilotdeck").join("styles");
        write_style(&project_styles, "concise", "project concise body");
        write_style(&project_styles, "team/detailed", "project detailed body");

        let result = dispatch_ok(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-sty", "sessionId": session_id},
            }),
        )
        .await;
        let ids: Vec<String> = result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["styleId"].as_str().unwrap().to_owned())
            .collect();
        // Project wins the "concise" name; the shadowed user entry is gone.
        assert_eq!(
            ids,
            vec![
                "project:concise".to_owned(),
                "project:team/detailed".to_owned(),
                "user:nested/deep".to_owned(),
            ]
        );
        let concise = &result["items"][0];
        assert_eq!(concise["name"], json!("concise"));
        assert_eq!(concise["available"], json!(true));
        assert!(
            concise["revision"]
                .as_str()
                .unwrap()
                .starts_with("stylerev-")
        );
        assert_eq!(result["pageInfo"]["complete"], json!(true));
        assert_eq!(result["pageInfo"]["nextCursor"], json!(Value::Null));

        // The instance scope sees only the user namespace.
        let result = dispatch_ok(
            &rt,
            "context.styles",
            json!({"instanceId": iid, "scope": {"kind": "instance"}}),
        )
        .await;
        let ids: Vec<String> = result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["styleId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            ids,
            vec!["user:concise".to_owned(), "user:nested/deep".to_owned()]
        );

        // Pagination: one item per page, cursor resume without loss.
        let page1 = dispatch_ok(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "instance"},
                "page": {"limit": 1},
            }),
        )
        .await;
        assert_eq!(page1["items"].as_array().unwrap().len(), 1);
        assert_eq!(page1["pageInfo"]["complete"], json!(false));
        let cursor = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        let page2 = dispatch_ok(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "instance"},
                "page": {"limit": 1, "cursor": cursor},
            }),
        )
        .await;
        assert_eq!(page2["items"].as_array().unwrap().len(), 1);
        assert_eq!(page2["pageInfo"]["complete"], json!(true));
        // A cursor beyond the end is rejected.
        let code = dispatch_err(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "instance"},
                "page": {"limit": 1, "cursor": "9"},
            }),
        )
        .await;
        assert_eq!(code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn styles_empty_catalog_differs_from_load_failure() {
        let (_tmp, rt, iid, _init_rev) = fresh_home("ctx-styles-empty");
        // No styles directory at all: an empty catalog, complete.
        let result = dispatch_ok(
            &rt,
            "context.styles",
            json!({"instanceId": iid, "scope": {"kind": "instance"}}),
        )
        .await;
        assert_eq!(result["items"], json!([]));
        assert_eq!(result["pageInfo"]["complete"], json!(true));

        // A load failure (the styles path exists but is a regular file →
        // unreadable directory) is an error, never an empty list
        // (「加载失败不同于空」).
        std::fs::write(rt.store().home().join("styles"), "not a directory").unwrap();
        let code = dispatch_err(
            &rt,
            "context.styles",
            json!({"instanceId": iid, "scope": {"kind": "instance"}}),
        )
        .await;
        assert_eq!(code, "UNAVAILABLE");

        // Scope validation: a missing project / session is INVALID_ARGUMENT
        // (no NOT_FOUND in the styles error list).
        let code = dispatch_err(
            &rt,
            "context.styles",
            json!({"instanceId": iid, "scope": {"kind": "project", "projectId": "p-none"}}),
        )
        .await;
        assert_eq!(code, "INVALID_ARGUMENT");
        let code = dispatch_err(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "p-none", "sessionId": "s-none"},
            }),
        )
        .await;
        assert_eq!(code, "INVALID_ARGUMENT");
    }

    // -----------------------------------------------------------------------
    // context.configure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn configure_thinking_roundtrip_with_model_support_gate() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-cfg-think");
        let (_ws, project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-think");
        seed_model(
            &rt,
            &iid,
            &init_rev,
            &project_id,
            &session_id,
            json!(["low", "high"]),
        )
        .await;

        // A supported level stores, with the session layer as its source.
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "thinking": "low",
            }),
        )
        .await;
        assert_eq!(setting(&result, "thinking")["value"], json!("low"));
        assert_eq!(
            setting(&result, "thinking")["sourceScope"],
            json!({"kind": "session", "projectId": project_id, "sessionId": session_id})
        );
        assert_eq!(setting(&result, "thinking")["sensitive"], json!(false));
        let revision = result["revision"].as_str().unwrap().to_owned();
        assert_ne!(revision, init_rev);

        // An unsupported level (not in the declared efforts) is rejected
        // without fake success (「不支持thinking拒绝」).
        let code = dispatch_err(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": revision,
                "thinking": "xhigh",
            }),
        )
        .await;
        assert_eq!(code, "MODEL_CAPABILITY_UNSUPPORTED");

        // Stale expectedRevision conflicts.
        let code = dispatch_err(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "thinking": "high",
            }),
        )
        .await;
        assert_eq!(code, "REVISION_CONFLICT");

        // null clears the override (「null清除/省略保留」).
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": revision,
                "thinking": null,
            }),
        )
        .await;
        assert_eq!(setting(&result, "thinking")["value"], json!(Value::Null));
        // Clearing the last field removed the session row: the chain
        // revision falls back to the deterministic anchor.
        assert_eq!(result["revision"], json!(init_rev));

        // Re-set, then verify that omission preserves (an inspect in
        // between must still read the stored value).
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "thinking": "high",
            }),
        )
        .await;
        let inspect = dispatch_ok(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id}),
        )
        .await;
        assert_eq!(*source_value(&inspect, "thinking"), json!("high"));

        // A syntactically unknown level is INVALID_ARGUMENT, not a model
        // capability answer.
        let code = dispatch_err(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": result["revision"].as_str().unwrap(),
                "thinking": "ultra",
            }),
        )
        .await;
        assert_eq!(code, "INVALID_ARGUMENT");

        // A missing session is INVALID_ARGUMENT (no NOT_FOUND in the list).
        let code = dispatch_err(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": "s-none",
                "expectedRevision": init_rev,
                "thinking": "low",
            }),
        )
        .await;
        assert_eq!(code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn configure_effort_requires_a_model_selection() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-cfg-nomodel");
        let (_ws, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-nomodel");
        // No provider/model/selection at all: an effort level cannot be
        // verified, so it is rejected — no fake success.
        let code = dispatch_err(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "thinking": "high",
            }),
        )
        .await;
        assert_eq!(code, "MODEL_CAPABILITY_UNSUPPORTED");
        // Non-effort modes carry no model requirement.
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "thinking": "off",
            }),
        )
        .await;
        assert_eq!(setting(&result, "thinking")["value"], json!("off"));
    }

    #[tokio::test]
    async fn configure_output_style_roundtrip() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-cfg-style");
        let (workspace, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-stycfg");
        write_style(
            &workspace.join(".pilotdeck").join("styles"),
            "concise",
            "project concise body",
        );

        // An unknown style is STYLE_NOT_FOUND.
        let code = dispatch_err(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "outputStyleId": "user:none",
            }),
        )
        .await;
        assert_eq!(code, "STYLE_NOT_FOUND");

        // A catalog styleId configures (「稳定ID可用于configure」) — a
        // style-only configure needs no model.
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "outputStyleId": "project:concise",
            }),
        )
        .await;
        assert_eq!(
            setting(&result, "outputStyleId")["value"],
            json!("project:concise")
        );

        // null clears the override; inspect reflects it.
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": result["revision"].as_str().unwrap(),
                "outputStyleId": null,
            }),
        )
        .await;
        assert_eq!(
            setting(&result, "outputStyleId")["value"],
            json!(Value::Null)
        );
        let inspect = dispatch_ok(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id}),
        )
        .await;
        assert_eq!(*source_value(&inspect, "outputStyleId"), json!(Value::Null));
    }

    #[tokio::test]
    async fn configure_layering_and_busy_turn_snapshot() {
        let (tmp, rt, iid, _init_rev) = fresh_home("ctx-cfg-layer");
        let (_ws, project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-layer");
        let session2 = "s-layer-2";
        rt.store()
            .create_session(&json!({"sessionId": session2, "projectId": project_id}))
            .unwrap();

        // A project-layer override (written directly through the domain
        // table — the layer only reachable by chain resolution, since
        // configure writes the session layer) shows through to both
        // sessions with its source scope.
        let project_scope = ContextScope::Project {
            project_id: project_id.clone(),
        };
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO v1_context_settings
                     (scope_key, scope_json, thinking, output_style_id, revision, updated_at)
                 VALUES (?1, ?2, 'medium', NULL, 'rev-00000099', ?3)",
                params![
                    project_scope.key(),
                    serde_json::to_string(&project_scope.to_scope_json()).unwrap(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        let inspect = dispatch_ok(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id}),
        )
        .await;
        assert_eq!(*source_value(&inspect, "thinking"), json!("medium"));
        assert_eq!(
            *source_value(&inspect, "thinkingSourceScope"),
            json!({"kind": "project", "projectId": project_id})
        );

        // The CAS revision for an unconfigured session is the contributing
        // layer's revision.
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session2,
                "expectedRevision": "rev-00000099",
                "thinking": "off",
            }),
        )
        .await;
        assert_eq!(setting(&result, "thinking")["value"], json!("off"));
        assert_eq!(
            setting(&result, "thinking")["sourceScope"],
            json!({"kind": "session", "projectId": project_id, "sessionId": session2})
        );

        // 「在途run不热换」: while a run of the other session is in flight,
        // configure lands on this session — the run keeps its submit-time
        // snapshot (nothing in the configure path touches a run row), and
        // the run terminates normally.
        let receipt = rt
            .dispatch(
                "run.submit",
                json!({
                    "sessionId": session2,
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=200]"}],
                }),
                Some("key-layer-run"),
                None,
            )
            .await
            .unwrap()
            .1;
        let run_id = receipt["runId"].as_str().unwrap().to_owned();
        let result = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": "rev-00000099",
                "thinking": "off",
            }),
        )
        .await;
        assert_eq!(setting(&result, "thinking")["value"], json!("off"));
        wait_run_terminal(&rt, &run_id).await;
        let run = rt.store().get_run(&run_id).unwrap();
        assert_eq!(run.state, RunState::Succeeded);
    }

    // -----------------------------------------------------------------------
    // context.reload_styles
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reload_styles_advances_revision_and_reports_diagnostics() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-reload");
        let (workspace, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-reload");
        let home_styles = rt.store().home().join("styles");
        write_style(&home_styles, "concise", "user concise body");
        write_style(&home_styles, "bad", "   \n");

        // The pre-reload snapshotRevision is the deterministic anchor, and
        // the bad style is absent from the listing.
        let listed = dispatch_ok(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-reload", "sessionId": session_id},
            }),
        )
        .await;
        assert_eq!(listed["pageInfo"]["snapshotRevision"], json!(init_rev));
        let ids: Vec<String> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["styleId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, vec!["user:concise".to_owned()]);

        // Stale expectedRevision conflicts before acceptance.
        let code = dispatch_err_with_key(
            &rt,
            "context.reload_styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-reload", "sessionId": session_id},
                "expectedRevision": "rev-deadbeef",
            }),
            "key-reload-stale",
        )
        .await;
        assert_eq!(code, "REVISION_CONFLICT");

        // A successful reload: receipt, then a terminal operation whose
        // completion carries the new revision and a diagnostic for the bad
        // style (「坏风格保留旧有效目录」: the valid entry survives).
        let receipt = dispatch_accepted(
            &rt,
            "context.reload_styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-reload", "sessionId": session_id},
                "expectedRevision": init_rev,
            }),
            "key-reload-1",
        )
        .await;
        assert_eq!(receipt["kind"], json!("operation"));
        assert_eq!(receipt["durable"], json!(true));
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let completion = &operation["result"];
        let new_revision = completion["revision"].as_str().unwrap().to_owned();
        assert_ne!(new_revision, init_rev, "「成功revision改变」");
        let diagnostics = completion["diagnostics"].as_array().unwrap().clone();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0]["code"], json!("STYLE_EMPTY"));
        assert_eq!(diagnostics[0]["severity"], json!("warning"));
        assert_eq!(diagnostics[0]["resourceId"], json!("user:bad"));
        assert!(
            diagnostics[0]["message"]
                .as_str()
                .unwrap()
                .contains("user:bad")
        );

        // The styles view now reports the reloaded revision and the same
        // valid catalog.
        let listed = dispatch_ok(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-reload", "sessionId": session_id},
            }),
        )
        .await;
        assert_eq!(listed["pageInfo"]["snapshotRevision"], json!(new_revision));

        // Idempotency: the same key + input replays the receipt
        // (deduplicated); the same key with different input conflicts.
        let replay = dispatch_accepted(
            &rt,
            "context.reload_styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-reload", "sessionId": session_id},
                "expectedRevision": init_rev,
            }),
            "key-reload-1",
        )
        .await;
        assert_eq!(replay["deduplicated"], json!(true));
        assert_eq!(replay["operationId"], receipt["operationId"]);
        let code = dispatch_err_with_key(
            &rt,
            "context.reload_styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "instance"},
                "expectedRevision": init_rev,
            }),
            "key-reload-1",
        )
        .await;
        assert_eq!(code, "IDEMPOTENCY_CONFLICT");

        // A reload against the advanced revision works again (the CAS
        // target moved with the state).
        let receipt2 = dispatch_accepted(
            &rt,
            "context.reload_styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-reload", "sessionId": session_id},
                "expectedRevision": new_revision,
            }),
            "key-reload-2",
        )
        .await;
        let operation2 = wait_operation(&rt, receipt2["operationId"].as_str().unwrap()).await;
        assert_eq!(operation2["state"], json!("succeeded"));
        assert_ne!(
            operation2["result"]["revision"].as_str().unwrap(),
            new_revision
        );

        // Project-namespace styles appear in the session view (a live scan).
        write_style(
            &workspace.join(".pilotdeck").join("styles"),
            "detailed",
            "project detailed body",
        );
        let receipt3 = dispatch_accepted(
            &rt,
            "context.reload_styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "instance"},
                "expectedRevision": init_rev,
            }),
            "key-reload-3",
        )
        .await;
        let operation3 = wait_operation(&rt, receipt3["operationId"].as_str().unwrap()).await;
        assert_eq!(operation3["state"], json!("succeeded"));
        let listed = dispatch_ok(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": "proj-reload", "sessionId": session_id},
            }),
        )
        .await;
        let ids: Vec<String> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["styleId"].as_str().unwrap().to_owned())
            .collect();
        assert!(ids.contains(&"project:detailed".to_owned()));
        assert!(ids.contains(&"user:concise".to_owned()));
    }

    // -----------------------------------------------------------------------
    // context.compact
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compact_busy_session_is_rejected_then_noop_succeeds_when_idle() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-compact");
        let (_ws, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-compact");

        // An active run refuses with SESSION_BUSY (「活动run拒绝」).
        let receipt = rt
            .dispatch(
                "run.submit",
                json!({
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=600]"}],
                }),
                Some("key-compact-run"),
                None,
            )
            .await
            .unwrap()
            .1;
        let run_id = receipt["runId"].as_str().unwrap().to_owned();
        let code = dispatch_err_with_key(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
            }),
            "key-compact-busy",
        )
        .await;
        assert_eq!(code, "SESSION_BUSY");
        // The busy refusal does not pin the key (after_reconcile): once the
        // session is idle, the same key + input is accepted fresh.
        wait_run_terminal(&rt, &run_id).await;

        // The session has no history entries: compaction is a no-op success
        // whose completion reports before == after and an estimated budget.
        let receipt = dispatch_accepted(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
            }),
            "key-compact-busy",
        )
        .await;
        assert_eq!(receipt["kind"], json!("operation"));
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let completion = &operation["result"];
        assert_eq!(completion["beforeRevision"], json!(init_rev));
        assert_eq!(completion["afterRevision"], json!(init_rev));
        assert_eq!(completion["budget"]["status"], json!("estimated"));
        assert_eq!(completion["budget"]["value"], json!(0));
        // 「成功可inspect核验」: the session still reads "none" (nothing was
        // folded) with its revision unchanged.
        let inspect = dispatch_ok(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id}),
        )
        .await;
        assert_eq!(inspect["compactionState"], json!("none"));
        assert_eq!(inspect["revision"], json!(init_rev));

        // Idempotency: same key + input replays; different input conflicts.
        let replay = dispatch_accepted(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
            }),
            "key-compact-busy",
        )
        .await;
        assert_eq!(replay["deduplicated"], json!(true));
        let code = dispatch_err_with_key(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": "rev-deadbeef",
            }),
            "key-compact-busy",
        )
        .await;
        assert_eq!(code, "IDEMPOTENCY_CONFLICT");

        // A missing session is INVALID_ARGUMENT (no NOT_FOUND in the list).
        let code = dispatch_err_with_key(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": "s-none",
                "expectedRevision": init_rev,
            }),
            "key-compact-nosess",
        )
        .await;
        assert_eq!(code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn compact_with_history_fails_on_the_missing_model_seam_and_keeps_history() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-compact-hist");
        let (_ws, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-cmpt-hist");
        // Seed a tool call / tool result pair plus a plain turn: the pairing
        // that a failed compaction must leave intact.
        rt.store()
            .append_history_entries(
                &session_id,
                &[
                    json!({"turnId": "t1", "role": "user", "content": [{"kind": "text", "text": "run the build"}]}),
                    json!({
                        "turnId": "t1",
                        "role": "assistant",
                        "content": [{"kind": "text", "text": "calling the tool"}],
                        "toolCallId": "call-1",
                    }),
                    json!({
                        "turnId": "t1",
                        "role": "tool",
                        "content": [{"kind": "text", "text": "build ok"}],
                        "toolCallId": "call-1",
                    }),
                ],
            )
            .unwrap();

        let receipt = dispatch_accepted(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
            }),
            "key-compact-hist",
        )
        .await;
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        // The model seam is absent: the operation fails with the contract's
        // CAPABILITY_UNAVAILABLE (never a faked summary).
        assert_eq!(operation["state"], json!("failed"));
        assert_eq!(operation["result"]["code"], json!("CAPABILITY_UNAVAILABLE"));
        // 「失败原历史工具配对保持」: the exact entries (including the tool
        // call/result pairing) are still there.
        let (count, pairs) = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let count = history_entry_count(&conn, &session_id).unwrap();
            let pairs: Vec<(String, Option<String>)> = {
                let mut stmt = conn
                    .prepare(
                        "SELECT role, tool_call_id FROM history_entries
                         WHERE session_id = ?1 ORDER BY seq",
                    )
                    .unwrap();
                stmt.query_map(params![session_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
            };
            (count, pairs)
        };
        assert_eq!(count, 3);
        assert_eq!(
            pairs,
            vec![
                ("user".to_owned(), None),
                ("assistant".to_owned(), Some("call-1".to_owned())),
                ("tool".to_owned(), Some("call-1".to_owned())),
            ]
        );
        // inspect reports the failed compaction state and the budget over
        // the intact history.
        let inspect = dispatch_ok(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id}),
        )
        .await;
        assert_eq!(inspect["compactionState"], json!("failed"));
        assert_eq!(*source_value(&inspect, "historyEntryCount"), json!(3));
        assert_eq!(inspect["revision"], json!(init_rev));
    }

    #[tokio::test]
    async fn compact_replay_is_idempotent_without_duplicate_rows_and_registers_lookup() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-compact-idem");
        let (_ws, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-cmpt-idem");

        let receipt = dispatch_accepted(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
            }),
            "key-compact-idem",
        )
        .await;
        let op_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_operation(&rt, &op_id).await;
        assert_eq!(operation["state"], json!("succeeded"));

        let (compaction_count_before, event_count_before) = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let c: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM v1_context_compactions WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .unwrap();
            let e: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE resource_id = ?1",
                    params![op_id],
                    |r| r.get(0),
                )
                .unwrap();
            (c, e)
        };
        assert_eq!(compaction_count_before, 1);

        let replay = dispatch_accepted(
            &rt,
            "context.compact",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
            }),
            "key-compact-idem",
        )
        .await;
        assert_eq!(replay["deduplicated"], json!(true));
        assert_eq!(replay["operationId"], json!(op_id));

        let (compaction_count_after, event_count_after) = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let c: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM v1_context_compactions WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .unwrap();
            let e: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE resource_id = ?1",
                    params![op_id],
                    |r| r.get(0),
                )
                .unwrap();
            (c, e)
        };
        assert_eq!(compaction_count_after, compaction_count_before);
        assert_eq!(event_count_after, event_count_before);

        let lookup = dispatch_ok(
            &rt,
            "requests.lookup",
            json!({
                "methodId": "context.compact",
                "idempotencyKey": "key-compact-idem",
            }),
        )
        .await;
        assert_eq!(lookup["requestState"], json!("accepted"));
        assert_eq!(
            lookup["resource"],
            json!({"kind": "operation", "operationId": op_id})
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn styles_rejects_workspace_symlink_escape_and_skips_oversized_entries() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-styles-sec");
        let (workspace, project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-sec");
        let outside_dir = tmp.0.join("outside-styles");
        std::fs::create_dir_all(&outside_dir).unwrap();
        write_style(&outside_dir, "secret", "stolen secret style");

        let pilotdeck_dir = workspace.join(".pilotdeck");
        std::fs::create_dir_all(&pilotdeck_dir).unwrap();
        let ws_styles = pilotdeck_dir.join("styles");
        std::os::unix::fs::symlink(&outside_dir, &ws_styles).unwrap();

        let code = dispatch_err(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": project_id, "sessionId": session_id},
            }),
        )
        .await;
        assert_eq!(code, "PATH_ESCAPE");

        // Replace symlinked directory with a real directory, then test child
        // symlink and oversized (>256 KiB) file handling via diagnostics.
        std::fs::remove_file(&ws_styles).unwrap();
        std::fs::create_dir_all(&ws_styles).unwrap();
        write_style(&ws_styles, "valid", "valid prompt body");
        std::os::unix::fs::symlink(
            outside_dir.join("secret.md"),
            ws_styles.join("escape-leaf.md"),
        )
        .unwrap();
        let oversized = "x".repeat(MAX_STYLE_FILE_BYTES + 128);
        write_style(&ws_styles, "huge", &oversized);

        let listed = dispatch_ok(
            &rt,
            "context.styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": project_id, "sessionId": session_id},
            }),
        )
        .await;
        let ids: Vec<String> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["styleId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, vec!["project:valid".to_owned()]);

        let receipt = dispatch_accepted(
            &rt,
            "context.reload_styles",
            json!({
                "instanceId": iid,
                "scope": {"kind": "session", "projectId": project_id, "sessionId": session_id},
                "expectedRevision": init_rev,
            }),
            "key-reload-sec",
        )
        .await;
        let op = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(op["state"], json!("succeeded"));
        let diag_codes: Vec<&str> = op["result"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["code"].as_str())
            .collect();
        assert!(diag_codes.contains(&"STYLE_SYMLINK_SKIPPED"));
        assert!(diag_codes.contains(&"STYLE_TOO_LARGE"));
    }

    #[tokio::test]
    async fn inspect_accounts_for_multimedia_attachments_and_applied_config_generation() {
        let (tmp, rt, iid, init_rev) = fresh_home("ctx-budget-gen");
        let (_ws, _project_id, session_id) = seed_project_session(&rt, &tmp.0, "proj-bgen");
        rt.store()
            .append_history_entries(
                &session_id,
                &[json!({
                    "turnId": "t1",
                    "role": "user",
                    "content": [{"kind": "attachment", "artifactId": "art-img-1"}]
                })],
            )
            .unwrap();

        let cfg_before = dispatch_ok(
            &rt,
            "configuration.resolve",
            json!({"instanceId": iid, "scope": {"kind": "instance"}}),
        )
        .await;
        let cfg_rev = cfg_before["storedRevision"].as_str().unwrap().to_owned();
        let cfg_updated = dispatch_ok(
            &rt,
            "configuration.update",
            json!({
                "instanceId": iid,
                "scope": {"kind": "instance"},
                "expectedRevision": cfg_rev,
                "changes": [{"key": "ui.theme", "action": "set", "value": "dark"}],
            }),
        )
        .await;
        let applied_gen = cfg_updated["effectiveGeneration"]
            .as_str()
            .unwrap()
            .to_owned();

        let inspect = dispatch_ok(
            &rt,
            "context.inspect",
            json!({"instanceId": iid, "sessionId": session_id}),
        )
        .await;
        assert_eq!(inspect["budget"]["status"], json!("estimated"));
        let tokens = inspect["budget"]["value"].as_u64().unwrap() as usize;
        assert!(
            tokens
                >= crate::model_cost::MULTIMEDIA_TOKENS + crate::model_cost::PER_MESSAGE_OVERHEAD,
            "attachment + message overhead must be counted in budget (got {tokens})"
        );
        assert_eq!(inspect["effectiveGeneration"], json!(applied_gen));

        let configured = dispatch_ok(
            &rt,
            "context.configure",
            json!({
                "instanceId": iid,
                "sessionId": session_id,
                "expectedRevision": init_rev,
                "thinking": "off",
            }),
        )
        .await;
        assert_eq!(configured["effectiveGeneration"], json!(applied_gen));
    }
}
