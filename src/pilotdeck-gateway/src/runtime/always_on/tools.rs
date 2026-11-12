//! Port of the four Always-On tools (`src/always-on/tool/*.ts`) and the
//! phase halves of `AlwaysOnRunContextRegistry` they read: each tool looks
//! up the calling session's run context by session key and acts only inside
//! the phase it belongs to. The manager owns one set and the host composes it
//! into every `always-on/…` session (`getAlwaysOnToolNames`,
//! SessionToolCompositionBundle.ts:71-79).
use std::sync::{Arc, Mutex};

use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};

use super::{
    AlwaysOnPaths, DiscoveryPlanRecord, DiscoveryPlanStore,
    manager::AlwaysOnRunContextRegistry,
    paths::report_markdown_path,
    plan::parse_plan_markdown,
    report::{ReportMetadata, parse_report_markdown},
    workspace::{Strategy, WorkspaceHandle, WorkspaceRegistry},
};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        cron::iso_from_epoch_ms,
        tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PhaseKind {
    Discovery,
    Workspace,
    Execution,
    Report,
}

/// The mutable half of a phase context: what the tools record.
#[derive(Default)]
pub(crate) struct PhaseState {
    /// Discovery: the stored plan record and its markdown.
    pub plan: Option<(JsObject, String)>,
    pub plan_call_count: u32,
    /// Discovery: chat alias → real session id.
    pub chat_aliases: Vec<(String, String)>,
    /// Workspace: the prepared handle.
    pub handle: Option<WorkspaceHandle>,
    /// Report: `(markdown, filePath)` of the first successful report.
    pub report: Option<(String, String)>,
    pub report_call_count: u32,
}

/// One phase context (AlwaysOnRunContextRegistry.ts:11-64).
pub(crate) struct PhaseContext {
    pub kind: PhaseKind,
    pub paths: AlwaysOnPaths,
    pub plan_store: Arc<DiscoveryPlanStore>,
    /// Workspace: the plan title and the provider registry.
    pub plan_title: String,
    pub workspace_registry: Option<WorkspaceRegistry>,
    /// Execution / report: the plan and the workspace (with `startedAt`).
    pub plan: Option<DiscoveryPlanRecord>,
    pub workspace: Option<WorkspaceHandle>,
    pub state: Mutex<PhaseState>,
}

impl std::fmt::Debug for PhaseContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhaseContext")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl PhaseContext {
    pub(crate) fn new(
        kind: PhaseKind,
        paths: AlwaysOnPaths,
        plan_store: Arc<DiscoveryPlanStore>,
    ) -> Self {
        Self {
            kind,
            paths,
            plan_store,
            plan_title: String::new(),
            workspace_registry: None,
            plan: None,
            workspace: None,
            state: Mutex::new(PhaseState::default()),
        }
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, PhaseState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Plan,
    Report,
    Workspace,
    ChatHistory,
}

const PLAN_SCHEMA: &str = r#"{"type":"object","required":["title","summary","rationale","dedupeKey","content"],"additionalProperties":false,"properties":{"title":{"type":"string"},"summary":{"type":"string"},"rationale":{"type":"string"},"dedupeKey":{"type":"string"},"content":{"type":"string","description":"Full plan markdown body."}}}"#;
const REPORT_SCHEMA: &str = r#"{"type":"object","required":["content"],"additionalProperties":false,"properties":{"content":{"type":"string","description":"Full work-report markdown body."}}}"#;
const WORKSPACE_SCHEMA: &str = r#"{"type":"object","required":["strategy"],"additionalProperties":false,"properties":{"strategy":{"type":"string","enum":["git-worktree","snapshot-copy","auto"],"description":"Workspace isolation strategy."}}}"#;
const CHAT_SCHEMA: &str = r#"{"type":"object","required":["sessionId"],"additionalProperties":false,"properties":{"sessionId":{"type":"string","description":"The sessionId of the chat session to read (from the chat digest)."}}}"#;

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Plan => "always_on_discovery_plan",
            Kind::Report => "always_on_report",
            Kind::Workspace => "always_on_prepare_workspace",
            Kind::ChatHistory => "always_on_read_chat_history",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Kind::Plan => {
                "Save the single discovery plan for this Always-On fire. Returns plan_quota_exhausted if called more than once per fire. Plan content must follow the PilotDeck Always-On plan markdown contract."
            }
            Kind::Report => {
                "Persist the work-report markdown for the current Always-On execution turn. Missing required sections are filled by the runtime fallback; do not fight the contract."
            }
            Kind::Workspace => {
                "Prepare an isolated workspace for the current Always-On plan execution. Use `auto` to let the runtime pick the best strategy, or specify `git-worktree` / `snapshot-copy` explicitly."
            }
            Kind::ChatHistory => {
                "Read the full conversation from a user chat session. Use the sessionId from the chat digest in the discovery prompt to expand a session of interest. Only available during Always-On discovery (Phase 1)."
            }
        }
    }
    fn schema(self) -> &'static str {
        match self {
            Kind::Plan => PLAN_SCHEMA,
            Kind::Report => REPORT_SCHEMA,
            Kind::Workspace => WORKSPACE_SCHEMA,
            Kind::ChatHistory => CHAT_SCHEMA,
        }
    }
}

/// One Always-On tool bound to the manager's shared registry.
pub(crate) struct AlwaysOnTool {
    kind: Kind,
    fields: JsObject,
    contexts: Arc<AlwaysOnRunContextRegistry>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
    uuid: Arc<dyn Fn() -> String + Send + Sync>,
}

/// `AlwaysOnManager` tools in their declaration order
/// (AlwaysOnManager.ts:89-108).
pub(crate) fn always_on_tools(
    contexts: Arc<AlwaysOnRunContextRegistry>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
    uuid: Arc<dyn Fn() -> String + Send + Sync>,
) -> Vec<Arc<dyn ExecutableTool>> {
    [Kind::Plan, Kind::Report, Kind::Workspace, Kind::ChatHistory]
        .into_iter()
        .map(|kind| {
            let mut fields = JsObject::new();
            fields.insert("name".into(), JsValue::from(kind.name()));
            fields.insert("description".into(), JsValue::from(kind.description()));
            fields.insert("kind".into(), JsValue::from("session"));
            fields.insert(
                "inputSchema".into(),
                parse_json(kind.schema()).expect("always-on tool schema is valid JSON"),
            );
            Arc::new(AlwaysOnTool {
                kind,
                fields,
                contexts: contexts.clone(),
                now: now.clone(),
                uuid: uuid.clone(),
            }) as Arc<dyn ExecutableTool>
        })
        .collect()
}

impl ToolDefinition for AlwaysOnTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(matches!(self.kind, Kind::ChatHistory)))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(matches!(self.kind, Kind::ChatHistory)))
    }
}

fn text_field(input: &JsValue, key: &str) -> String {
    match input.get(key) {
        Some(JsValue::String(text)) => String::from_utf16_lossy(text.units()),
        _ => String::new(),
    }
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

/// `{ content: [{type: "text", text}, {type: "json", value: data}], data,
/// metadata }`.
fn output(text: String, data: JsObject, metadata: JsObject) -> ToolOutput {
    let mut text_block = JsObject::new();
    text_block.insert("type".into(), JsValue::from("text"));
    text_block.insert("text".into(), JsValue::from(text.as_str()));
    let mut json_block = JsObject::new();
    json_block.insert("type".into(), JsValue::from("json"));
    json_block.insert("value".into(), JsValue::Object(data.clone()));
    ToolOutput {
        content: vec![JsValue::Object(text_block), JsValue::Object(json_block)],
        supplemental_messages: None,
        data: Some(JsValue::Object(data)),
        metadata: Some(metadata),
    }
}

fn run_metadata(run_id: &str) -> JsObject {
    let mut metadata = JsObject::new();
    metadata.insert("runId".into(), JsValue::from(run_id));
    metadata
}

impl ExecutableTool for AlwaysOnTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let kind = self.kind;
        let contexts = self.contexts.clone();
        let now = self.now.clone();
        let uuid = self.uuid.clone();
        Box::pin(async move {
            let session = match context.fields.get("sessionId") {
                Some(JsValue::String(key)) => String::from_utf16_lossy(key.units()),
                _ => String::new(),
            };
            let found = contexts
                .get(&session)
                .and_then(|context| context.phase.clone().map(|phase| (context, phase)));
            match kind {
                Kind::Plan => {
                    let Some((run, phase)) =
                        found.filter(|(_, phase)| phase.kind == PhaseKind::Discovery)
                    else {
                        return Err(ToolError::failed(
                            "always_on_discovery_plan called outside of an Always-On discovery turn.",
                        ));
                    };
                    execute_plan(&input, &run.run_id, &phase, &now, &uuid)
                }
                Kind::Report => {
                    let Some((run, phase)) = found.filter(|(_, phase)| {
                        matches!(phase.kind, PhaseKind::Report | PhaseKind::Execution)
                    }) else {
                        return Err(ToolError::failed(
                            "always_on_report called outside of an Always-On report or execution turn.",
                        ));
                    };
                    if phase.kind == PhaseKind::Execution {
                        return Err(ToolError::failed(
                            "always_on_report called in a context without a reportStore.",
                        ));
                    }
                    execute_report(&input, &run.run_id, &phase, &now)
                }
                Kind::Workspace => {
                    let Some((run, phase)) =
                        found.filter(|(_, phase)| phase.kind == PhaseKind::Workspace)
                    else {
                        return Err(ToolError::failed(
                            "always_on_prepare_workspace called outside of an Always-On workspace turn.",
                        ));
                    };
                    execute_workspace(&input, &run.run_id, &run.project_key, &phase).await
                }
                Kind::ChatHistory => {
                    let Some((run, phase)) =
                        found.filter(|(_, phase)| phase.kind == PhaseKind::Discovery)
                    else {
                        return Err(ToolError::failed(
                            "always_on_read_chat_history is only available during Always-On discovery (Phase 1).",
                        ));
                    };
                    execute_chat_history(&input, &run.run_id, &run.project_key, &phase)
                }
            }
        })
    }
}

/// `ASSISTANT_TEXT_LIMIT` (AlwaysOnChatHistoryTool.ts:35), in UTF-16 units.
const ASSISTANT_TEXT_LIMIT: usize = 300;

/// AlwaysOnChatHistoryTool.ts:63-132: resolve a digest alias, replay the
/// session transcript and return its user / assistant text.
fn execute_chat_history(
    input: &JsValue,
    run_id: &str,
    project_key: &str,
    phase: &PhaseContext,
) -> Result<ToolOutput, ToolError> {
    let requested = text_field(input, "sessionId");
    let real = phase
        .state()
        .chat_aliases
        .iter()
        .find(|(alias, _)| *alias == requested)
        .map(|(_, id)| id.clone())
        .unwrap_or_else(|| requested.clone());
    let project_id = phase
        .paths
        .project_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let chat_dir = crate::metadata::resolve(
        &crate::metadata::resolve(
            &crate::metadata::resolve(&phase.paths.pilot_home, "projects"),
            &project_id,
        ),
        "chats",
    );
    let real_js = JsString::from(real.as_str());
    let transcript = chat_dir.join(format!(
        "{}.jsonl",
        crate::storage::sanitize_session_id(&real_js)
    ));
    let read = crate::storage::read_transcript(&transcript)
        .map_err(|error| ToolError::failed(error.to_string()))?;
    if read.entries.is_empty() {
        let reason = read
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.message.clone())
            .unwrap_or_else(|| "No transcript entries found.".into());
        return Err(ToolError::failed(format!(
            "Could not read session {requested}: {reason}"
        )));
    }
    let replay = crate::runtime::session::factory::project_transcript(&read.entries)
        .map_err(|error| ToolError::failed(error.message.to_string()))?;
    let nullish = |value: Option<&JsValue>| {
        matches!(value, None | Some(JsValue::Undefined | JsValue::Null))
    };
    let summary = || {
        crate::runtime::manager::file_catalog::list_sessions(
            &chat_dir,
            std::path::Path::new(project_key),
            false,
        )
        .unwrap_or_default()
        .into_iter()
        .map(|(_, info)| info)
        .find(|info| {
            info.get("sessionId")
                .and_then(JsValue::as_string)
                .is_some_and(|id| id.equals_str(&real))
        })
        .and_then(|info| info.get("summary").cloned())
    };
    let title_value = [
        replay.metadata.get("title").cloned(),
        replay.metadata.get("aiTitle").cloned(),
    ]
    .into_iter()
    .find(|value| !nullish(value.as_ref()))
    .flatten()
    .or_else(|| summary().filter(|value| !nullish(Some(value))));
    let title = match title_value {
        Some(value) => String::from_utf16_lossy(
            &crate::js_value::to_string_units(&value).unwrap_or_default(),
        ),
        None => real.clone(),
    };
    let mut conversation = Vec::new();
    for message in &replay.messages {
        let role = match message.get("role").and_then(JsValue::as_string) {
            Some(role) if role.equals_str("user") => "user",
            Some(role) if role.equals_str("assistant") => "assistant",
            _ => continue,
        };
        let Some(JsValue::Array(blocks)) = message.get("content") else {
            continue;
        };
        let parts: Vec<String> = blocks
            .iter()
            .filter(|block| {
                block
                    .get("type")
                    .and_then(JsValue::as_string)
                    .is_some_and(|kind| kind.equals_str("text"))
            })
            .filter_map(|block| match block.get("text") {
                Some(JsValue::String(text)) => {
                    Some(js_trim(&String::from_utf16_lossy(text.units())).to_string())
                }
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let full = parts.join("\n\n");
        let units: Vec<u16> = full.encode_utf16().collect();
        let text = if role == "assistant" && units.len() > ASSISTANT_TEXT_LIMIT {
            format!(
                "{}...",
                String::from_utf16_lossy(&units[..ASSISTANT_TEXT_LIMIT])
            )
        } else {
            full
        };
        let mut entry = JsObject::new();
        entry.insert("role".into(), JsValue::from(role));
        entry.insert("text".into(), JsValue::from(text.as_str()));
        entry.insert("createdAt".into(), JsValue::from(""));
        conversation.push(JsValue::Object(entry));
    }
    let count = conversation.len();
    let mut data = JsObject::new();
    data.insert("sessionId".into(), JsValue::from(real.as_str()));
    data.insert("title".into(), JsValue::from(title.as_str()));
    data.insert("messageCount".into(), JsValue::Number(count as f64));
    data.insert("conversation".into(), JsValue::Array(conversation));
    Ok(output(
        format!("Session \"{title}\" \u{2014} {count} messages."),
        data,
        run_metadata(run_id),
    ))
}

/// AlwaysOnDiscoveryPlanTool.ts:60-120.
fn execute_plan(
    input: &JsValue,
    run_id: &str,
    phase: &PhaseContext,
    now: &Arc<dyn Fn() -> f64 + Send + Sync>,
    uuid: &Arc<dyn Fn() -> String + Send + Sync>,
) -> Result<ToolOutput, ToolError> {
    {
        let mut state = phase.state();
        state.plan_call_count += 1;
        if state.plan.is_some() {
            return Err(ToolError::failed(
                "plan_quota_exhausted: Always-On discovery permits at most one plan per fire.",
            ));
        }
    }
    let content = text_field(input, "content");
    let parsed = parse_plan_markdown(&content)
        .map_err(|message| ToolError::failed(format!("plan_invalid: {message}")))?;
    let plan_id = match parsed.meta("id") {
        "" => format!("plan_{}", uuid()),
        id => id.to_string(),
    };
    let io = |error: std::io::Error| ToolError::failed(error.to_string());
    let file_path = phase
        .plan_store
        .write_plan_markdown(&plan_id, &parsed.raw_content)
        .map_err(io)?;
    let or = |given: String, fallback: &str| {
        if given.is_empty() {
            fallback.to_string()
        } else {
            given
        }
    };
    let mut record = JsObject::new();
    record.insert("id".into(), JsValue::from(plan_id.as_str()));
    record.insert(
        "title".into(),
        JsValue::from(or(js_trim(&text_field(input, "title")).into(), &parsed.title).as_str()),
    );
    record.insert(
        "createdAt".into(),
        JsValue::from(iso_from_epoch_ms(now()).as_str()),
    );
    record.insert("status".into(), JsValue::from("ready"));
    record.insert(
        "summary".into(),
        JsValue::from(js_trim(&text_field(input, "summary"))),
    );
    record.insert(
        "rationale".into(),
        JsValue::from(js_trim(&text_field(input, "rationale"))),
    );
    record.insert(
        "dedupeKey".into(),
        JsValue::from(
            or(
                js_trim(&text_field(input, "dedupeKey")).into(),
                parsed.meta("dedupeKey"),
            )
            .as_str(),
        ),
    );
    record.insert(
        "sourceRunId".into(),
        JsValue::from(or(parsed.meta("sourceRunId").into(), run_id).as_str()),
    );
    record.insert("planFilePath".into(), JsValue::from(file_path.as_str()));
    let stored = phase.plan_store.upsert(&record).map_err(io)?;
    phase.state().plan = Some((stored.clone(), parsed.raw_content.clone()));
    let field = |key: &str| stored.get(key).cloned().unwrap_or(JsValue::Undefined);
    let mut data = JsObject::new();
    data.insert("ok".into(), JsValue::Bool(true));
    data.insert("planId".into(), field("id"));
    data.insert("planFilePath".into(), field("planFilePath"));
    data.insert("dedupeKey".into(), field("dedupeKey"));
    let show = |value: JsValue| match &value {
        JsValue::String(text) => String::from_utf16_lossy(text.units()),
        _ => String::new(),
    };
    Ok(output(
        format!(
            "Plan saved as {} ({}).",
            show(field("id")),
            show(field("dedupeKey"))
        ),
        data,
        run_metadata(run_id),
    ))
}

/// AlwaysOnReportTool.ts:45-91.
fn execute_report(
    input: &JsValue,
    run_id: &str,
    phase: &PhaseContext,
    now: &Arc<dyn Fn() -> f64 + Send + Sync>,
) -> Result<ToolOutput, ToolError> {
    phase.state().report_call_count += 1;
    let finished_at = iso_from_epoch_ms(now());
    let (Some(plan), Some(workspace)) = (&phase.plan, &phase.workspace) else {
        return Err(ToolError::failed(
            "always_on_report called in a context without a reportStore.",
        ));
    };
    let started_at = workspace
        .metadata
        .iter()
        .find(|(key, _)| key == "startedAt")
        .map_or(finished_at.clone(), |(_, value)| value.clone());
    let parsed = parse_report_markdown(
        &text_field(input, "content"),
        &ReportMetadata {
            run_id,
            plan_id: &plan.id,
            started_at: &started_at,
            finished_at: &finished_at,
            outcome: "executed",
            workspace_strategy: &workspace.strategy,
            workspace_handle: &workspace.cwd,
        },
    );
    let file_path = write_report(&phase.paths, run_id, &parsed.raw_content)
        .map_err(|error| ToolError::failed(error.to_string()))?;
    phase.state().report = Some((parsed.raw_content.clone(), file_path.clone()));
    let mut data = JsObject::new();
    data.insert("ok".into(), JsValue::Bool(true));
    data.insert("reportFilePath".into(), JsValue::from(file_path.as_str()));
    data.insert(
        "fallbacks".into(),
        JsValue::Array(
            parsed
                .fallbacks
                .iter()
                .map(|fallback| JsValue::from(fallback.as_str()))
                .collect(),
        ),
    );
    let mut metadata = run_metadata(run_id);
    metadata.insert("planId".into(), JsValue::from(plan.id.as_str()));
    Ok(output(
        format!("Work report saved at {file_path}."),
        data,
        metadata,
    ))
}

/// `DiscoveryReportStore.writeReport` (DiscoveryReportStore.ts:9-14).
pub(crate) fn write_report(
    paths: &AlwaysOnPaths,
    run_id: &str,
    markdown: &str,
) -> std::io::Result<String> {
    let path = report_markdown_path(paths, &JsString::from(run_id));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, markdown)?;
    Ok(path.to_string_lossy().into_owned())
}

/// AlwaysOnWorkspaceTool.ts:48-101.
async fn execute_workspace(
    input: &JsValue,
    run_id: &str,
    project_key: &str,
    phase: &PhaseContext,
) -> Result<ToolOutput, ToolError> {
    if phase.state().handle.is_some() {
        return Err(ToolError::failed(
            "workspace_already_prepared: a workspace was already created for this run.",
        ));
    }
    let Some(registry) = &phase.workspace_registry else {
        return Err(ToolError::failed(
            "always_on_prepare_workspace called outside of an Always-On workspace turn.",
        ));
    };
    let requested = text_field(input, "strategy");
    let prepared = if requested == "auto" {
        registry
            .prepare(project_key, run_id, &phase.plan_title)
            .await
    } else {
        let Some(strategy) = Strategy::from_id(&requested) else {
            return Err(ToolError::failed(format!(
                "workspace strategy \"{requested}\" is not available."
            )));
        };
        registry
            .prepare_with(strategy, project_key, run_id, &phase.plan_title)
            .await
    };
    let handle = prepared.map_err(|error| ToolError::new(error.code(), error.message()))?;
    phase.state().handle = Some(handle.clone());
    let mut data = JsObject::new();
    data.insert("ok".into(), JsValue::Bool(true));
    data.insert("strategy".into(), JsValue::from(handle.strategy.as_str()));
    data.insert("cwd".into(), JsValue::from(handle.cwd.as_str()));
    data.insert("reused".into(), JsValue::Bool(false));
    Ok(output(
        format!(
            "Workspace prepared at {} ({}).",
            handle.cwd, handle.strategy
        ),
        data,
        run_metadata(run_id),
    ))
}
