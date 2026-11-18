//! `tools.*` — the product-level tool catalog and its controlled test
//! execution (`cli.task.t017`, S3; capability a029).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `tools.list` / `tools.describe`
//! / `tools.test` 条目。TS 基线（cc7c4e6）是语义参考：模块 host 的
//! `POST /api/module-host/call`（operation `list_tools` / `test_tool`，
//! `publicHostWire.ts`）——合同已把它重设计为带 scope 的公开操作。
//!
//! 目录来源（简报的「只读复用」裁决）：旧线 `runtime/tools/builtin` 的内建
//! 工具在 `native_process/tools.rs::native_tools` 里构成会话装配目录；本域在
//! 进程内构造同一组内建实例（构造器无副作用），把它们的
//! `name/description/inputSchema`（`ToolDefinition::fields`）投影成合同的
//! 目录形态。schemaId 形如 `agentcpm-pd.tool.<name>/1`，版本固定；schema 本体
//! 直接来自被移植工具自己的声明，不手抄。与 `native_tools` 的装配清单相比少
//! 这些条目（均为需运行时挂载的项，属 runtimes/extensions 任务）：`read_skill`
//! （需 plugins）、`LspTool`（需 LspService）、MCP 工具、每回合的
//! `agent/subagent`。`web_search` 是静态条目：v1 运行时未装配搜索 provider
//! （旧线 `world.web_search = None` 时同样以 diagnostic 标记不可用），
//! availability 恒为 unavailable/`SEARCH_PROVIDER_NOT_CONFIGURED`——「只配置
//! 未加载的工具不标 ready」由此成立。
//!
//! 逐 scope 差异（验收「不同项目/会话的工具装配及权限差异反映到结果」）：
//! v1 已交付的授权面是 t005 的 `policy.*`（scoped rules: effect
//! allow/deny/ask + tool）。本域按请求 scope 解析有效策略（session → project
//! → instance 继承，与 `policy.get` 同一读取路径），投影为：
//! * `deny` 规则命中（tool 精确匹配或 `*`，后者沿用旧线
//!   `matchesToolSelector` 的选择器语义）→ 该工具 blocked / `POLICY_DENIED`；
//! * `ask` 规则命中 → `permissionRequired` 列出这些规则的 ruleId（调用需
//!   这些授权；v1 没有更细的权限名字空间，ruleId 是策略域已有的稳定权限名）。
//!   目录本身的装配在本构建内对所有 scope 相同（内建目录是编译期事实）。
//!
//! `tools.test` 是「受控测试执行」：受理为异步域操作
//! （`v1_domain_operations`，与 schedules/background_tasks 共用接缝），
//! 执行体只挂载可安全直驱的子集——旧线只读家族
//! （`read_file`/`glob`/`grep`/`get_current_time`）以受控 `ToolRunContext`
//! 真实执行（cwd = 项目工作区；工具自身的
//! `resolve_workspace_path(allow_outside=false)` 路径约束在 cwd 根内生效，
//! 与 `files.*` 域的 authorized-root 防线一致）。副作用/上下文类工具
//! （bash、write 家族、task、web、交互类）不直驱：`allowEffects=false`
//! 时执行前拒绝 `EFFECT_APPROVAL_REQUIRED`；`allowEffects=true` 时受理后以
//! `TOOL_EXECUTION_UNAVAILABLE` 诚实失败（效果执行桥属 runtime 任务）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::schema_validator::validate_against_schema;
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use serde_json::{Map, Value, json};

use crate::host::modules::tool_context::{HostToolCheckpoint, ToolRuntimeContext};
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::{ProjectResolveError, resolve_project};
use crate::public_v1::domains::policy::PolicyScope;
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::runtime::tools::builtin;
use crate::runtime::tools::{ExecutableTool, ToolOutput, ToolRunContext};

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for yet.
const CODE_PERMISSION_DENIED: &str = "PERMISSION_DENIED";
const CODE_SCHEMA_MISMATCH: &str = "SCHEMA_MISMATCH";
const CODE_EFFECT_APPROVAL_REQUIRED: &str = "EFFECT_APPROVAL_REQUIRED";
const CODE_TOOL_EXECUTION_UNAVAILABLE: &str = "TOOL_EXECUTION_UNAVAILABLE";

/// The owner of every catalog entry: the native gateway's builtin assembly.
const OWNER_BUILTIN: &str = "builtin";
/// Uniform schema id of the controlled-test result envelope (`value`).
const TEST_RESULT_SCHEMA_ID: &str = "agentcpm-pd.tool.testresult/1";
/// The `web_search` entry is static (its constructor needs a transport the v1
/// runtime does not mount); the input schema below is the one the ported
/// tool declares (`builtin/web/search.rs::INPUT_SCHEMA`).
const WEB_SEARCH_INPUT_SCHEMA: &str = r#"{"type":"object","required":["query"],"additionalProperties":false,"properties":{"query":{"type":"string","description":"Search query string. Be specific, and include versions or the current year when looking for recent documentation, releases, or current events."},"gl":{"type":"string","description":"Optional country code for localized results. Defaults to \"us\"; use \"cn\" for China-localized results."}}}"#;
/// The controlled-test result envelope every executed tool reports through
/// `completionResult.value` (per-tool result schemas are the runtime task's
/// refinement; the v1-observable result today is this uniform envelope).
const TEST_RESULT_SCHEMA: &str = r#"{"type":"object","properties":{"content":{"type":"array"},"data":{}},"required":["content","data"],"additionalProperties":false}"#;

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("tools.list", |rt, input, _key, _rev| {
        Box::pin(tools_list(rt, input))
    });
    reg.handler("tools.describe", |rt, input, _key, _rev| {
        Box::pin(tools_describe(rt, input))
    });
    reg.handler("tools.test", |rt, input, key, _rev| {
        Box::pin(tools_test(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn tools_invalid_argument(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn tools_not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn tools_schema_mismatch(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_SCHEMA_MISMATCH,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// `EFFECT_APPROVAL_REQUIRED` is not a definitive rejection: the same key
/// retries after the caller authorizes the effects (`after_reconcile`, so
/// the idempotency record is not pinned to the unanswered approval).
fn effect_approval_required(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_EFFECT_APPROVAL_REQUIRED,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// The catalog (old-line builtin registry projected into the contract shape)
// ---------------------------------------------------------------------------

struct CatalogTool {
    tool: Option<Arc<dyn ExecutableTool>>,
    tool_id: String,
    description: String,
    input_schema: Value,
    schema_id: String,
    side_effects: &'static [&'static str],
    retry_policy: &'static str,
    /// Static unavailability (no runtime assembly for this tool in v1).
    static_reason: Option<&'static str>,
    /// The controlled test executor can drive this tool directly.
    test_executable: bool,
    /// The tool resolves filesystem paths against the context cwd (the
    /// controlled test anchors it to the project workspace).
    needs_workspace: bool,
}

/// Effects / retry classification of the catalog families. Side-effect
/// strings are this domain's v1 declaration (the old line only carries a
/// read-only flag); the second bool marks the controlled-test subset.
fn classify(tool_id: &str) -> (&'static [&'static str], &'static str, bool, bool) {
    match tool_id {
        "read_file" | "glob" | "grep" => (&[], "readOnly", true, true),
        "get_current_time" => (&[], "readOnly", true, false),
        "write_file" | "edit_file" | "edit_notebook" => {
            (&["filesystem:write"], "never", false, false)
        }
        "bash" | "execute_code" | "task_create" => (
            &["process:spawn", "filesystem:write", "network"],
            "never",
            false,
            false,
        ),
        "task_list" | "task_output" | "task_wait" | "structured_output" | "ask_user_question"
        | "enter_plan_mode" | "exit_plan_mode" | "todo_write" | "send_attachment" | "get_goal"
        | "web_search" => (&[], "readOnly", false, false),
        "task_stop" => (&["process:signal"], "never", false, false),
        "web_fetch" => (&["network"], "never", false, false),
        "create_goal" | "update_goal" => (&["session-state:write"], "never", false, false),
        // Defensive default for a future builtin the classification table has
        // not learned yet: never claim it is effect-free or test-executable.
        _ => (&["unknown"], "never", false, false),
    }
}

fn js_field_text(tool: &dyn ExecutableTool, key: &str) -> String {
    tool.fields()
        .get(key)
        .and_then(JsValue::as_string)
        .map(|value| String::from_utf16_lossy(value.units()))
        .unwrap_or_default()
}

fn js_field_schema(tool: &dyn ExecutableTool, key: &str) -> Option<Value> {
    let raw = tool.fields().get(key)?;
    let text = stringify_json(raw).ok()??;
    serde_json::from_str(&text).ok()
}

fn catalog_entry_from_tool(tool: Arc<dyn ExecutableTool>) -> CatalogTool {
    let tool_id = js_field_text(tool.as_ref(), "name");
    let (side_effects, retry_policy, test_executable, needs_workspace) = classify(&tool_id);
    CatalogTool {
        input_schema: js_field_schema(tool.as_ref(), "inputSchema").unwrap_or(json!({})),
        schema_id: format!("agentcpm-pd.tool.{tool_id}/1"),
        description: js_field_text(tool.as_ref(), "description"),
        tool: Some(tool),
        tool_id,
        static_reason: None,
        side_effects,
        retry_policy,
        test_executable,
        needs_workspace,
    }
}

/// Builds the catalog once per process from the old-line builtin families —
/// the same set `native_process::tools::native_tools` assembles for a
/// session, minus the entries that need a mounted runtime (see module doc).
fn builtin_catalog() -> &'static Vec<CatalogTool> {
    static CATALOG: OnceLock<Vec<CatalogTool>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut tools: Vec<Arc<dyn ExecutableTool>> = builtin::read_only_tools();
        tools.extend(builtin::write_tools());
        #[cfg(unix)]
        {
            tools.extend(builtin::shell_tools());
            tools.push(Arc::new(builtin::execute_code::ExecuteCodeTool::new(
                builtin::execute_code::ExecuteCodeToolOptions {
                    web_search: false,
                    sandbox: Default::default(),
                },
            )));
            tools.extend(builtin::task::task_tools(None));
        }
        tools.push(Arc::new(builtin::web::fetch_tool::WebFetchTool::new(
            builtin::web::fetch_tool::WebFetchToolOptions {
                model: None,
                provider: None,
                model_id: None,
                max_output_tokens: None,
                // The production transport the native assembly wires
                // (`native_process/tools.rs:2566`); only the catalog projection
                // reads its fields, so the tool is never executed here.
                fetcher: builtin::web::fetcher::UrlFetcher::new(Arc::new(
                    crate::runtime::model::http::HyperModelTransport::new(),
                )),
            },
        )));
        tools.push(Arc::new(
            builtin::structured_output::StructuredOutputTool::new(),
        ));
        tools.push(Arc::new(
            builtin::ask_user_question::AskUserQuestionTool::new(),
        ));
        tools.extend(builtin::plan_mode::plan_mode_tools());
        tools.push(Arc::new(builtin::todo_write::TodoWriteTool::new()));
        tools.push(Arc::new(builtin::send_attachment::SendAttachmentTool::new()));
        tools.extend(builtin::goal::goal_tools());

        let mut catalog: Vec<CatalogTool> =
            tools.into_iter().map(catalog_entry_from_tool).collect();
        // `web_search`: static entry — the v1 runtime mounts no search
        // provider, so the tool is never ready this round (the old line
        // marks the unconfigured tool unavailable the same way).
        catalog.push(CatalogTool {
            tool: None,
            tool_id: "web_search".to_owned(),
            description: "Search the web for current information.".to_owned(),
            input_schema: serde_json::from_str(WEB_SEARCH_INPUT_SCHEMA).unwrap_or(json!({})),
            schema_id: "agentcpm-pd.tool.web_search/1".to_owned(),
            side_effects: &[],
            retry_policy: "readOnly",
            static_reason: Some("SEARCH_PROVIDER_NOT_CONFIGURED"),
            test_executable: false,
            needs_workspace: false,
        });
        catalog.sort_by(|a, b| a.tool_id.cmp(&b.tool_id));
        catalog.dedup_by(|a, b| a.tool_id == b.tool_id);
        catalog
    })
}

// ---------------------------------------------------------------------------
// Scope (the tools/commands shape: kind global|project|session + id)
// ---------------------------------------------------------------------------

pub(crate) enum ToolScope {
    Global,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl ToolScope {
    fn policy_scope(&self) -> PolicyScope {
        match self {
            Self::Global => PolicyScope::Instance,
            Self::Project { project_id } => PolicyScope::Project {
                project_id: project_id.clone(),
            },
            Self::Session {
                project_id,
                session_id,
            } => PolicyScope::Session {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            },
        }
    }

    fn session_column(&self) -> String {
        match self {
            Self::Session { session_id, .. } => session_id.clone(),
            _ => String::new(),
        }
    }

    /// The project workspace this scope anchors filesystem paths to (the
    /// project of a project/session scope; `None` for global or a virtual
    /// project with no host-declared root).
    fn workspace(&self, rt: &PublicV1Runtime) -> Result<Option<PathBuf>, ContractError> {
        let project_id = match self {
            Self::Global => return Ok(None),
            Self::Project { project_id } => project_id,
            Self::Session { project_id, .. } => project_id,
        };
        match resolve_project(rt.store(), project_id) {
            Ok(workspace) => Ok(Some(workspace.workspace)),
            Err(ProjectResolveError::NotFound(message)) => Err(tools_invalid_argument(message)),
            Err(ProjectResolveError::Contract(error)) => Err(error),
            // A virtual project contributes no workspace anchor; the tools
            // that need one are not test-executable there (rejected
            // pre-accept with a precise message).
            Err(ProjectResolveError::Virtual(_)) => Ok(None),
        }
    }
}

/// Parses the contract `scope` object (`{kind, id}`). Dispatch already ran
/// the schema validation; the existence checks follow the `models.rs`
/// precedent (a scope naming a missing project/session answers
/// `INVALID_ARGUMENT` — the tools.list error list carries no NOT_FOUND).
pub(super) fn parse_tool_scope(
    rt: &PublicV1Runtime,
    input: &Value,
) -> Result<ToolScope, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(tools_invalid_argument("input.scope is required"));
    };
    let id = scope.get("id").and_then(Value::as_str);
    match scope.get("kind").and_then(Value::as_str) {
        Some("global") => {
            if id.is_some() {
                return Err(tools_invalid_argument(
                    "scope.id must be null for kind=global",
                ));
            }
            Ok(ToolScope::Global)
        }
        Some("project") => {
            let Some(project_id) = id.filter(|value| !value.is_empty()) else {
                return Err(tools_invalid_argument(
                    "scope.id is required for kind=project",
                ));
            };
            if rt.store().get_project(project_id).is_err() {
                return Err(tools_invalid_argument(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(ToolScope::Project {
                project_id: project_id.to_owned(),
            })
        }
        Some("session") => {
            let Some(session_id) = id.filter(|value| !value.is_empty()) else {
                return Err(tools_invalid_argument(
                    "scope.id is required for kind=session",
                ));
            };
            let session = rt.store().get_session(session_id).map_err(|_| {
                tools_invalid_argument(format!("scope session `{session_id}` does not exist"))
            })?;
            let Some(project_id) = session["session"].get("projectId").and_then(Value::as_str)
            else {
                return Err(tools_invalid_argument(format!(
                    "scope session `{session_id}` has no project"
                )));
            };
            Ok(ToolScope::Session {
                project_id: project_id.to_owned(),
                session_id: session_id.to_owned(),
            })
        }
        _ => Err(tools_invalid_argument(
            "scope.kind must be global|project|session",
        )),
    }
}

// ---------------------------------------------------------------------------
// Policy projection (the delivered v1 authorization surface, read through
// the same resolution `policy.get` uses)
// ---------------------------------------------------------------------------

/// A policy rule matches a tool when its `tool` selector is the exact id or
/// `*` (the old line's `matchesToolSelector` selector semantics).
fn rule_matches_tool(rule: &Value, tool_id: &str) -> bool {
    rule.get("tool")
        .and_then(Value::as_str)
        .is_some_and(|selector| selector == "*" || selector == tool_id)
}

/// The effective policy state one catalog view is projected from.
struct PolicyProjection {
    rules: Vec<Value>,
    revision: String,
    generation: String,
}

fn effective_policy(
    rt: &PublicV1Runtime,
    scope: &ToolScope,
) -> Result<PolicyProjection, ContractError> {
    let view = rt.store().get_policy(&scope.policy_scope())?;
    Ok(PolicyProjection {
        rules: view
            .policy
            .get("rules")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        revision: view.policy["revision"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        generation: view.policy["effectiveGeneration"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    })
}

impl PolicyProjection {
    /// Projects the rules onto one tool: whether a deny rule blocks it and
    /// the ruleIds of the ask rules gating it.
    fn gates(&self, tool_id: &str) -> (bool, Vec<String>) {
        let mut denied = false;
        let mut ask_rule_ids = Vec::new();
        for rule in &self.rules {
            if !rule_matches_tool(rule, tool_id) {
                continue;
            }
            match rule.get("effect").and_then(Value::as_str) {
                Some("deny") => denied = true,
                Some("ask") => {
                    if let Some(rule_id) = rule.get("ruleId").and_then(Value::as_str) {
                        ask_rule_ids.push(rule_id.to_owned());
                    }
                }
                _ => {}
            }
        }
        ask_rule_ids.sort();
        ask_rule_ids.dedup();
        (denied, ask_rule_ids)
    }
}

fn availability_json(entry: &CatalogTool, denied: bool, generation: &str) -> Value {
    let (state, available, reason_code) = if let Some(reason) = entry.static_reason {
        ("unavailable", false, json!(reason))
    } else if denied {
        ("blocked", false, json!("POLICY_DENIED"))
    } else {
        ("ready", true, Value::Null)
    };
    json!({
        "state": state,
        "available": available,
        "reasonCode": reason_code,
        "generation": generation,
    })
}

fn tool_item_json(
    entry: &CatalogTool,
    denied: bool,
    ask_rule_ids: &[String],
    generation: &str,
) -> Value {
    json!({
        "toolId": entry.tool_id,
        "name": entry.tool_id,
        "ownerId": OWNER_BUILTIN,
        "schemaId": entry.schema_id,
        "availability": availability_json(entry, denied, generation),
        "permissionRequired": ask_rule_ids,
    })
}

// ---------------------------------------------------------------------------
// tools.list / tools.describe
// ---------------------------------------------------------------------------

pub(super) fn required_str(input: &Value, field: &str) -> Result<String, ContractError> {
    input
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| tools_invalid_argument(format!("`{field}` is required")))
}

/// Keyset pagination cursor (`after:<toolId>`): resuming after the last
/// returned id has no loss or duplication even when the view shifts between
/// pages (the catalog is static per process; policy edits only re-order
/// availability, never the id order).
fn parse_after_cursor(input: &Value) -> Result<Option<String>, ContractError> {
    match input.get("cursor").and_then(Value::as_str) {
        None => Ok(None),
        Some(cursor) => cursor
            .strip_prefix("after:")
            .filter(|rest| !rest.is_empty())
            .map(ToOwned::to_owned)
            .map(Some)
            .ok_or_else(|| {
                tools_invalid_argument(format!("malformed cursor `{cursor}` (expected after:<id>)"))
            }),
    }
}

async fn tools_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let after = parse_after_cursor(&input)?;

    let policy = effective_policy(&rt, &scope)?;
    let catalog = builtin_catalog();
    let mut items: Vec<Value> = Vec::new();
    for entry in catalog {
        if let Some(after) = &after
            && &entry.tool_id <= after
        {
            continue;
        }
        if let Some(query) = &query {
            let matched = entry.tool_id.to_lowercase().contains(query.as_str())
                || entry.description.to_lowercase().contains(query.as_str());
            if !matched {
                continue;
            }
        }
        let (denied, ask_rule_ids) = policy.gates(&entry.tool_id);
        items.push(tool_item_json(
            entry,
            denied,
            &ask_rule_ids,
            &policy.generation,
        ));
        if items.len() > limit {
            break;
        }
    }
    let next_cursor = if items.len() > limit {
        items.pop();
        items
            .last()
            .and_then(|item| item["toolId"].as_str())
            .map(|last| format!("after:{last}"))
    } else {
        None
    };
    // The snapshot revision pins the policy state the view reflects; the
    // static catalog has no revision of its own.
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "nextCursor": next_cursor,
            "snapshotRevision": format!("tools-{}", policy.revision),
        }),
    ))
}

async fn tools_describe(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let tool_id = required_str(&input, "toolId")?;
    let catalog = builtin_catalog();
    let entry = catalog
        .iter()
        .find(|entry| entry.tool_id == tool_id)
        .ok_or_else(|| tools_not_found("tool", &tool_id))?;
    let policy = effective_policy(&rt, &scope)?;
    let (denied, ask_rule_ids) = policy.gates(&entry.tool_id);
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "tool": tool_item_json(entry, denied, &ask_rule_ids, &policy.generation),
            "inputSchema": entry.input_schema,
            "resultSchema": serde_json::from_str::<Value>(TEST_RESULT_SCHEMA)
                .unwrap_or_else(|_| json!({})),
            "sideEffects": entry.side_effects,
            "retryPolicy": entry.retry_policy,
        }),
    ))
}

// ---------------------------------------------------------------------------
// tools.test (async, retry=idempotentSubmit)
// ---------------------------------------------------------------------------

/// The per-dataRoot domain DB for the tools operations (the same seam
/// schedules/background_tasks accept their async operations against).
fn tools_domain_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
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

fn value_to_js(value: &Value) -> Result<JsValue, ContractError> {
    parse_json(&value.to_string()).map_err(|error| {
        tools_schema_mismatch(format!("arguments did not round-trip as JSON: {error}"))
    })
}

fn js_to_value(value: &JsValue) -> Option<Value> {
    let text = stringify_json(value).ok()??;
    serde_json::from_str(&text).ok()
}

/// The controlled execution context: absolute workspace cwd, default
/// permission mode (never `bypassPermissions` — the tools' own
/// `resolve_workspace_path` containment stays in force), no host services.
fn controlled_context(cwd: Option<&Path>) -> ToolRunContext {
    let mut fields = JsObject::new();
    if let Some(cwd) = cwd {
        fields.insert(
            "cwd".into(),
            JsValue::String(JsString::from(cwd.to_string_lossy().as_ref())),
        );
    }
    let host = ToolRuntimeContext {
        fields,
        files: HostToolCheckpoint::default().tool_context_state(),
        abort_signal: None,
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: None,
        progress: None,
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    };
    ToolRunContext::new(Arc::new(host))
}

fn tool_output_value(output: &ToolOutput) -> Value {
    let content: Vec<Value> = output.content.iter().filter_map(js_to_value).collect();
    json!({
        "content": content,
        "data": output
            .data
            .as_ref()
            .and_then(js_to_value)
            .unwrap_or(Value::Null),
    })
}

async fn tools_test(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let tool_id = required_str(&input, "toolId")?;
    let schema_id = required_str(&input, "schemaId")?;
    let arguments = input
        .get("arguments")
        .cloned()
        .ok_or_else(|| tools_invalid_argument("`arguments` is required"))?;
    let allow_effects = input
        .get("allowEffects")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let catalog = builtin_catalog();
    let entry = catalog
        .iter()
        .find(|entry| entry.tool_id == tool_id)
        .ok_or_else(|| tools_not_found("tool", &tool_id))?;
    let (policy_denied, ask_rule_ids) = effective_policy(&rt, &scope)?.gates(&entry.tool_id);
    let workspace = scope.workspace(&rt)?;
    let db = tools_domain_db(&rt)?;

    // Captured plain values for the detached executor (the catalog entry
    // itself is borrowed from the static table).
    let entry_id = entry.tool_id.clone();
    let entry_schema_id = entry.schema_id.clone();
    let entry_input_schema = entry.input_schema.clone();
    let entry_effects: Vec<&'static str> = entry.side_effects.to_vec();
    let test_executable = entry.test_executable;
    let needs_workspace = entry.needs_workspace;

    let validation = || -> Result<(), ContractError> {
        if entry_schema_id != schema_id {
            return Err(tools_schema_mismatch(format!(
                "schemaId `{schema_id}` does not match the registered schema `{entry_schema_id}` \
                 for tool `{entry_id}`"
            )));
        }
        if let Some(reason) = entry.static_reason {
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!("tool `{entry_id}` is unavailable in this runtime assembly: {reason}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if !arguments.is_object() {
            return Err(tools_schema_mismatch("`arguments` must be an object"));
        }
        if let Err(errors) = validate_against_schema(&arguments, &entry_input_schema) {
            let details = errors
                .iter()
                .map(|error| json!({"path": error.path, "message": error.message}))
                .collect::<Vec<_>>();
            let mut error = tools_schema_mismatch(format!(
                "arguments do not satisfy the input schema of tool `{entry_id}`"
            ));
            let mut map = Map::new();
            map.insert("validationErrors".to_owned(), json!(details));
            error.details = Some(map);
            return Err(error);
        }
        if policy_denied {
            return Err(ContractError::new(
                CODE_PERMISSION_DENIED,
                format!("tool `{entry_id}` is denied by policy for this scope"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if !ask_rule_ids.is_empty() && !allow_effects {
            return Err(effect_approval_required(format!(
                "tool `{entry_id}` is gated by ask rules [{}]; pass allowEffects=true to \
                 authorize this controlled test",
                ask_rule_ids.join(", ")
            )));
        }
        if !entry_effects.is_empty() && !allow_effects {
            return Err(effect_approval_required(format!(
                "tool `{entry_id}` declares side effects [{}]; pass allowEffects=true to \
                 authorize this controlled test",
                entry_effects.join(", ")
            )));
        }
        if test_executable && needs_workspace && workspace.is_none() {
            return Err(tools_invalid_argument(format!(
                "controlled test of tool `{entry_id}` requires a project or session scope \
                 with a resolvable workspace"
            )));
        }
        Ok(())
    };

    let operation = accept_domain_operation(
        &db,
        "tools.test",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        validation,
    )?;
    let operation_id = operation.operation_id.clone();
    let session_column = scope.session_column();

    // The controlled execution runs detached: the accepted operation record
    // (and its receipt) is durable before this task starts, so a dropped
    // connection neither cancels the test nor re-executes it on retry.
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&db, &operation_id, &session_column);
        let completion = if !test_executable {
            Err((
                CODE_TOOL_EXECUTION_UNAVAILABLE.to_owned(),
                format!(
                    "tool `{entry_id}` is not wired into the controlled test executor of \
                     this runtime assembly (effectful and turn-context tools need the \
                     runtime execution bridge)"
                ),
            ))
        } else {
            let tool = builtin_catalog()
                .iter()
                .find(|entry| entry.tool_id == entry_id)
                .and_then(|entry| entry.tool.clone());
            match tool {
                Some(tool) => match value_to_js(&arguments) {
                    Ok(js_input) => {
                        let context = controlled_context(workspace.as_deref());
                        match tool.execute(js_input, context).await {
                            Ok(output) => Ok(json!({
                                "invocationId": operation_id,
                                "toolId": entry_id,
                                "resultSchemaId": TEST_RESULT_SCHEMA_ID,
                                "value": tool_output_value(&output),
                                "effects": entry_effects,
                            })),
                            Err(error) => Err((
                                CODE_TOOL_EXECUTION_UNAVAILABLE.to_owned(),
                                format!(
                                    "tool `{entry_id}` failed its controlled test: {} {}",
                                    error.code,
                                    String::from_utf16_lossy(error.message.units())
                                ),
                            )),
                        }
                    }
                    Err(error) => Err((error.code, error.message)),
                },
                None => Err((
                    CODE_TOOL_EXECUTION_UNAVAILABLE.to_owned(),
                    format!("tool `{entry_id}` vanished from the catalog"),
                )),
            }
        };
        match completion {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &db,
                    &operation_id,
                    &session_column,
                    "succeeded",
                    &completion,
                );
            }
            Err((code, message)) => {
                let _ = finish_domain_operation(
                    &db,
                    &operation_id,
                    &session_column,
                    "failed",
                    &json!({"code": code, "message": message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// Tests (contract positive/negative cases against the real dispatch table)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::{InstanceMeta, V1Store, now_rfc3339};
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use rusqlite::params;
    use std::time::{Duration, Instant};

    fn fresh_home(tag: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        (tmp, root_dir)
    }

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

    /// 播种一个带真实授权根的项目（files.rs 测试同款）并返回工作区路径。
    fn seed_project(rt: &Arc<PublicV1Runtime>, root_dir: &Path, project_id: &str) -> PathBuf {
        let store = rt.store();
        let declared_root = root_dir.parent().unwrap().join("authorized-t017");
        std::fs::create_dir_all(&declared_root).unwrap();
        let workspace_path = declared_root.join("ws");
        std::fs::create_dir_all(&workspace_path).unwrap();
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-t017', ?1, ?2, ?3)",
                params![
                    store.meta().instance_id,
                    declared_root.display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        rt.store()
            .register_project(&json!({
                "projectId": project_id,
                "name": "T017 Tools",
                "environmentId": "local",
                "root": {"instanceId": store.meta().instance_id, "rootId": "root-t017", "relativePath": "ws"},
            }))
            .unwrap();
        workspace_path
    }

    async fn dispatch_err(
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

    fn global_scope() -> Value {
        json!({"kind": "global"})
    }

    fn tool_items(result: &Value) -> Vec<Value> {
        result["items"].as_array().cloned().unwrap_or_default()
    }

    fn find_tool<'a>(items: &'a [Value], tool_id: &str) -> &'a Value {
        items
            .iter()
            .find(|item| item["toolId"] == json!(tool_id))
            .unwrap_or_else(|| panic!("tool `{tool_id}` missing from items"))
    }

    /// Polls `operations.get` until the domain operation is terminal and
    /// returns its projected `Operation` object.
    async fn wait_operation(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
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
                Instant::now() < deadline,
                "operation `{operation_id}` never became terminal (state `{state}`)"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn tools_list_projects_the_builtin_catalog() {
        let (_tmp, root) = fresh_home("tools-list-catalog");
        let (rt, _iid) = open_runtime(&root);

        let (_, result) = rt
            .dispatch("tools.list", json!({"scope": global_scope()}), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("tools.list", &result)
            .unwrap();

        let items = tool_items(&result);
        assert!(
            items.len() >= 10,
            "the builtin catalog must list its families, got {}",
            items.len()
        );
        for required in [
            "read_file",
            "glob",
            "grep",
            "get_current_time",
            "write_file",
        ] {
            let entry = find_tool(&items, required);
            assert_eq!(entry["name"], json!(required));
            assert_eq!(entry["ownerId"], json!("builtin"));
            assert_eq!(entry["availability"]["state"], json!("ready"));
            assert_eq!(entry["availability"]["available"], json!(true));
            assert_eq!(entry["availability"]["reasonCode"], Value::Null);
            assert_eq!(entry["permissionRequired"], json!([]));
            assert!(entry["schemaId"].as_str().is_some_and(|id| !id.is_empty()));
        }
        // web_search has no provider assembly in v1 and must not be ready.
        let web_search = find_tool(&items, "web_search");
        assert_eq!(web_search["availability"]["state"], json!("unavailable"));
        assert_eq!(web_search["availability"]["available"], json!(false));
        assert_eq!(
            web_search["availability"]["reasonCode"],
            json!("SEARCH_PROVIDER_NOT_CONFIGURED")
        );
        assert!(result["snapshotRevision"].as_str().is_some());
    }

    #[tokio::test]
    async fn tools_list_pagination_is_lossless_and_duplicate_free() {
        let (_tmp, root) = fresh_home("tools-list-pages");
        let (rt, _iid) = open_runtime(&root);

        let (_, full) = rt
            .dispatch("tools.list", json!({"scope": global_scope()}), None, None)
            .await
            .unwrap();
        let full_ids: Vec<String> = tool_items(&full)
            .iter()
            .filter_map(|item| item["toolId"].as_str().map(ToOwned::to_owned))
            .collect();

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut input = json!({"scope": global_scope(), "limit": 3});
            if let Some(cursor) = &cursor {
                input["cursor"] = json!(cursor);
            }
            let (_, page) = rt.dispatch("tools.list", input, None, None).await.unwrap();
            let page_ids: Vec<String> = tool_items(&page)
                .iter()
                .filter_map(|item| item["toolId"].as_str().map(ToOwned::to_owned))
                .collect();
            assert!(page_ids.len() <= 3, "page larger than the limit");
            seen.extend(page_ids);
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        assert_eq!(seen, full_ids, "paginated walk must match the full list");

        // A malformed cursor is rejected before any listing.
        let error = dispatch_err(
            &rt,
            "tools.list",
            json!({"scope": global_scope(), "cursor": "bogus"}),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn tools_list_reflects_scoped_policy_rules() {
        let (_tmp, root) = fresh_home("tools-list-policy");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-policy");
        std::fs::write(workspace.join("seed.txt"), b"seed").unwrap();
        rt.store()
            .create_session(&json!({"sessionId": "sess-policy", "projectId": "proj-policy"}))
            .unwrap();
        let project_scope = json!({"kind": "project", "id": "proj-policy"});

        // No policy defined: both scopes show bash ready.
        let (_, base) = rt
            .dispatch("tools.list", json!({"scope": project_scope}), None, None)
            .await
            .unwrap();
        assert_eq!(
            find_tool(&tool_items(&base), "bash")["availability"]["state"],
            json!("ready")
        );

        // Project policy denies bash and asks for read_file.
        let (_, policy) = rt
            .dispatch(
                "policy.get",
                json!({"scope": {"kind": "project", "projectId": "proj-policy"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let rev = policy["policy"]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "policy.update",
            json!({
                "scope": {"kind": "project", "projectId": "proj-policy"},
                "expectedRevision": rev,
                "mode": "ask",
                "rules": [
                    {"ruleId": "deny-bash", "effect": "deny", "tool": "bash", "resourcePattern": "*"},
                    {"ruleId": "ask-read", "effect": "ask", "tool": "read_file", "resourcePattern": "*"},
                ],
            }),
            None,
            Some(&rev),
        )
        .await
        .unwrap();

        let (_, project_view) = rt
            .dispatch("tools.list", json!({"scope": project_scope}), None, None)
            .await
            .unwrap();
        let items = tool_items(&project_view);
        let bash = find_tool(&items, "bash");
        assert_eq!(bash["availability"]["state"], json!("blocked"));
        assert_eq!(bash["availability"]["available"], json!(false));
        assert_eq!(bash["availability"]["reasonCode"], json!("POLICY_DENIED"));
        let read_file = find_tool(&items, "read_file");
        assert_eq!(
            read_file["permissionRequired"],
            json!(["ask-read"]),
            "ask rules surface as the required permissions"
        );
        assert_eq!(read_file["availability"]["state"], json!("ready"));

        // Session inherits the project policy; the global view does not.
        let (_, session_view) = rt
            .dispatch(
                "tools.list",
                json!({"scope": {"kind": "session", "id": "sess-policy"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            find_tool(&tool_items(&session_view), "bash")["availability"]["state"],
            json!("blocked")
        );
        let (_, global_view) = rt
            .dispatch("tools.list", json!({"scope": global_scope()}), None, None)
            .await
            .unwrap();
        assert_eq!(
            find_tool(&tool_items(&global_view), "bash")["availability"]["state"],
            json!("ready")
        );

        // The query filter matches ids and descriptions.
        let (_, queried) = rt
            .dispatch(
                "tools.list",
                json!({"scope": project_scope, "query": "GET_CURRENT"}),
                None,
                None,
            )
            .await
            .unwrap();
        // Only `get_current_time` mentions the query (id or description).
        let queried_items = tool_items(&queried);
        assert_eq!(
            queried_items.len(),
            1,
            "the query matches exactly one catalog entry"
        );
        assert_eq!(
            queried_items[0]["toolId"],
            json!("get_current_time"),
            "query matched the tool id case-insensitively"
        );

        // Scope existence: unknown project/session is INVALID_ARGUMENT.
        let error = dispatch_err(
            &rt,
            "tools.list",
            json!({"scope": {"kind": "project", "id": "proj-missing"}}),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn tools_describe_returns_the_declared_contract() {
        let (_tmp, root) = fresh_home("tools-describe");
        let (rt, _iid) = open_runtime(&root);

        let (_, result) = rt
            .dispatch(
                "tools.describe",
                json!({"scope": global_scope(), "toolId": "read_file"}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("tools.describe", &result)
            .unwrap();
        assert_eq!(result["tool"]["toolId"], json!("read_file"));
        // The input schema is the ported tool's own declaration.
        assert_eq!(
            result["inputSchema"]["properties"]["file_path"]["type"],
            json!("string")
        );
        assert_eq!(result["sideEffects"], json!([]));
        assert_eq!(result["retryPolicy"], json!("readOnly"));

        let (_, bash) = rt
            .dispatch(
                "tools.describe",
                json!({"scope": global_scope(), "toolId": "bash"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(
            bash["sideEffects"]
                .as_array()
                .is_some_and(|effects| !effects.is_empty()),
            "bash declares its side effects"
        );
        assert_eq!(bash["retryPolicy"], json!("never"));

        let error = dispatch_err(
            &rt,
            "tools.describe",
            json!({"scope": global_scope(), "toolId": "no-such-tool"}),
            None,
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn tools_test_rejects_pre_accept_conditions() {
        let (_tmp, root) = fresh_home("tools-test-reject");
        let (rt, _iid) = open_runtime(&root);
        let scope = json!({"kind": "global"});

        // Unknown tool.
        let error = dispatch_err(
            &rt,
            "tools.test",
            json!({"scope": scope, "toolId": "no-such-tool", "schemaId": "x", "arguments": {}}),
            Some("key-unknown"),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // Stale schemaId.
        let error = dispatch_err(
            &rt,
            "tools.test",
            json!({
                "scope": scope,
                "toolId": "get_current_time",
                "schemaId": "agentcpm-pd.tool.get_current_time/2",
                "arguments": {},
            }),
            Some("key-stale-schema"),
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // Arguments that violate the tool's registered schema (unknown field).
        let error = dispatch_err(
            &rt,
            "tools.test",
            json!({
                "scope": scope,
                "toolId": "get_current_time",
                "schemaId": "agentcpm-pd.tool.get_current_time/1",
                "arguments": {"bogus": 1},
            }),
            Some("key-bad-args"),
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // A statically unavailable tool never accepts a test.
        let error = dispatch_err(
            &rt,
            "tools.test",
            json!({
                "scope": scope,
                "toolId": "web_search",
                "schemaId": "agentcpm-pd.tool.web_search/1",
                "arguments": {"query": "hello"},
            }),
            Some("key-web-search"),
        )
        .await;
        assert_eq!(error.code, "UNAVAILABLE");

        // Side-effecting tool without the explicit authorization.
        let error = dispatch_err(
            &rt,
            "tools.test",
            json!({
                "scope": scope,
                "toolId": "bash",
                "schemaId": "agentcpm-pd.tool.bash/1",
                "arguments": {"command": "true"},
                "allowEffects": false,
            }),
            Some("key-bash-effects"),
        )
        .await;
        assert_eq!(error.code, "EFFECT_APPROVAL_REQUIRED");
    }

    #[tokio::test]
    async fn tools_test_runs_the_read_only_family_in_the_workspace() {
        let (_tmp, root) = fresh_home("tools-test-exec");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-exec");
        std::fs::write(workspace.join("note.txt"), b"t017 controlled test").unwrap();
        let project_scope = json!({"kind": "project", "id": "proj-exec"});

        // The workspace-less scope refuses the fs-anchored tool pre-accept.
        let error = dispatch_err(
            &rt,
            "tools.test",
            json!({
                "scope": global_scope(),
                "toolId": "read_file",
                "schemaId": "agentcpm-pd.tool.read_file/1",
                "arguments": {"file_path": "note.txt"},
            }),
            Some("key-global-fs"),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // A real controlled execution of read_file inside the workspace.
        let (_, receipt) = rt
            .dispatch(
                "tools.test",
                json!({
                    "scope": project_scope,
                    "toolId": "read_file",
                    "schemaId": "agentcpm-pd.tool.read_file/1",
                    "arguments": {"file_path": "note.txt"},
                }),
                Some("key-read-ok"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], json!("operation"));
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_operation(&rt, &operation_id).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let result = &operation["result"];
        assert_eq!(result["toolId"], json!("read_file"));
        assert_eq!(
            result["resultSchemaId"],
            json!("agentcpm-pd.tool.testresult/1")
        );
        let text = serde_json::to_string(&result["value"]).unwrap_or_default();
        assert!(
            text.contains("t017 controlled test"),
            "the controlled read returned the file content: {text}"
        );

        // A path that escapes the workspace is refused by the tool's own
        // containment (the controlled context never lifts it).
        let (_, escape_receipt) = rt
            .dispatch(
                "tools.test",
                json!({
                    "scope": project_scope,
                    "toolId": "read_file",
                    "schemaId": "agentcpm-pd.tool.read_file/1",
                    "arguments": {"file_path": "../../../etc/passwd"},
                }),
                Some("key-read-escape"),
                None,
            )
            .await
            .unwrap();
        let escape_op = wait_operation(
            &rt,
            escape_receipt["operationId"].as_str().unwrap_or_default(),
        )
        .await;
        assert_eq!(
            escape_op["state"],
            json!("failed"),
            "an escaping read must fail, not leak the file"
        );
        let leaked = serde_json::to_string(&escape_op["result"]).unwrap_or_default();
        assert!(
            !leaked.contains("root:"),
            "the escaped read must not leak /etc/passwd content"
        );

        // A non-executable tool fails honestly instead of pretending.
        let (_, honest_receipt) = rt
            .dispatch(
                "tools.test",
                json!({
                    "scope": project_scope,
                    "toolId": "bash",
                    "schemaId": "agentcpm-pd.tool.bash/1",
                    "arguments": {"command": "true"},
                    "allowEffects": true,
                }),
                Some("key-bash-honest"),
                None,
            )
            .await
            .unwrap();
        let honest_op = wait_operation(
            &rt,
            honest_receipt["operationId"].as_str().unwrap_or_default(),
        )
        .await;
        assert_eq!(honest_op["state"], json!("failed"));
        assert!(
            honest_op["result"]["code"] == json!("TOOL_EXECUTION_UNAVAILABLE"),
            "the unwired executor fails honestly: {}",
            honest_op["result"]
        );
    }

    #[tokio::test]
    async fn tools_test_deduplicates_by_idempotency_key() {
        let (_tmp, root) = fresh_home("tools-test-dedup");
        let (rt, _iid) = open_runtime(&root);
        let input = json!({
            "scope": global_scope(),
            "toolId": "get_current_time",
            "schemaId": "agentcpm-pd.tool.get_current_time/1",
            "arguments": {},
        });

        let (_, first) = rt
            .dispatch("tools.test", input.clone(), Some("key-dedup"), None)
            .await
            .unwrap();
        assert_eq!(first["deduplicated"], json!(false));
        wait_operation(&rt, first["operationId"].as_str().unwrap_or_default()).await;

        let (_, replay) = rt
            .dispatch("tools.test", input.clone(), Some("key-dedup"), None)
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], json!(true), "the key replays once");

        // The same key with different input conflicts instead.
        let mut other = input.clone();
        other["arguments"] = json!({"timezone": "UTC"});
        let error = dispatch_err(&rt, "tools.test", other, Some("key-dedup")).await;
        assert_eq!(error.code, "IDEMPOTENCY_CONFLICT");
    }
}
