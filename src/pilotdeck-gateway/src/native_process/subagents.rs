//! The native host's one-shot subagent family: the fork port behind the
//! `agent` tool (`agent/sub/OneShotSubagentPort.ts`, `SubAgentSession.ts`,
//! `SubagentRuntimeComposition.ts`, `cli/SessionSubagentTranscriptBundle.ts`).
//!
//! A fork is a real child session: its own agent loop (one more sidecar), its
//! own sidechain transcript under `<chat>/<session>/subagents/<id>.jsonl`
//! with its own projection checkpoint, a tool registry scoped to the
//! subagent definition, and a runtime config derived from the parent's. The
//! parent transcript keeps only the `subagent_started` / `subagent_completed`
//! references; the child's activity reaches the parent's client through the
//! parent turn's event buffer.
use super::tools::{ForkSeed, SessionTools};
use crate::{
    host::{
        HostError, Limits,
        modules::{
            capability::RuntimeContext,
            tool_context::{
                EventSink, HostToolCheckpoint, OneShotSubagent, SeedState, SharedFileState,
            },
        },
    },
    runtime::{
        project::ProjectRuntimeRegistry,
        session::{
            LiveSession,
            factory::{FactoryStorage, SessionBinding},
        },
        tools::{
            ToolRunContext,
            builtin::agent::{
                ForkApi, ForkFuture, ForkReport, ForkRequest, SubagentDefinition, SubagentFork,
                definitions_from_config, parse_summary, subagent_system_prompt,
            },
            js_space,
            registry::ToolPolicy,
        },
        turn::host_factory::{Deployment, TurnHostFactory},
    },
    storage::{JsonlTranscriptStore, TranscriptKey, recorder::Recorder},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{path::PathBuf, sync::Arc, time::Instant};

/// `SUBAGENT_PROMPT_PREVIEW_BYTES` / `SUBAGENT_SUMMARY_PREVIEW_BYTES`
/// (`session/transcript/TranscriptEntry.ts:457-458`).
const PROMPT_PREVIEW_BYTES: usize = 1024;
const SUMMARY_PREVIEW_BYTES: usize = 4 * 1024;

/// What every session of one gateway needs to start a child loop.
#[derive(Clone)]
pub(super) struct SubagentRuntime {
    registry: Arc<ProjectRuntimeRegistry>,
    deployment: Deployment,
    limits: Limits,
    /// The pilot home, for the sidechain's storage key.
    home: PathBuf,
    /// The process's continuable children (`subagent` / `send_message`).
    pub(super) continuations: Arc<super::continuations::Continuations>,
    /// `runtime.backgroundSubagents`: the launcher behind `background: true`
    /// definitions.
    background: Option<crate::runtime::background::BackgroundSubagentRuntime>,
}

impl SubagentRuntime {
    pub(super) fn new(
        registry: Arc<ProjectRuntimeRegistry>,
        deployment: Deployment,
        limits: Limits,
        home: PathBuf,
        continuations: Arc<super::continuations::Continuations>,
    ) -> Self {
        Self {
            registry,
            deployment,
            limits,
            home,
            continuations,
            background: None,
        }
    }

    pub(super) fn with_background(
        mut self,
        background: Option<crate::runtime::background::BackgroundSubagentRuntime>,
    ) -> Self {
        self.background = background;
        self
    }

    /// `NativeSubagentContinuationHost.createChild`: a continuable child
    /// session over its own durable storage — restored from the transcript
    /// when `resume` — with its metadata tail, a tool registry scoped to its
    /// definition whose continuation tools act for the child itself
    /// (`parent`), and no inherited file state.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn materialize_continuable(
        &self,
        seed: ForkSeed,
        parent_config: &JsObject,
        config: JsObject,
        definition: Arc<SubagentDefinition>,
        child: &JsString,
        transcript_path: PathBuf,
        resume: bool,
        parent: super::continuations::ParentRef,
    ) -> Result<
        (
            Arc<crate::runtime::turn::live::LiveSessionAdapter>,
            Arc<FactoryStorage>,
        ),
        JsString,
    > {
        let fail = |error: &dyn std::fmt::Display| JsString::from(error.to_string());
        let cwd = PathBuf::from(
            parent_config
                .get("cwd")
                .and_then(JsValue::as_string)
                .map(text)
                .unwrap_or_default(),
        );
        let storage = Arc::new(FactoryStorage::new(
            Arc::new(JsonlTranscriptStore::new()),
            TranscriptKey {
                project_root: cwd.clone(),
                pilot_home: self.home.clone(),
                session_id: child.clone(),
                transcript_path,
            },
        ));
        if resume {
            storage
                .restore()
                .await
                .map_err(|error| fail(&error.message))?;
        }
        let tail = crate::runtime::turn::runner::tail::SessionTail::new(
            storage.recorder.clone(),
            child.clone(),
            crate::runtime::turn::runner::tail::TailConfig {
                cwd: Some(cwd.clone()),
                // `shouldCollectFileArtifacts`: none for the pilot home.
                collect_file_artifacts: cwd != self.home,
                ..Default::default()
            },
        );
        if resume {
            let state = storage
                .projection
                .snapshot()
                .map_err(|error| fail(&error.message))?;
            tail.restore(state.metadata);
        }
        let checkpoint =
            HostToolCheckpoint::new(None, &[]).map_err(|error| fail(&error.message))?;
        let tools: SessionTools = seed
            .clone()
            .child(
                SessionBinding {
                    context: seed.context.clone(),
                    recorder: storage.recorder.clone(),
                    checkpoint,
                    projection: storage.projection.clone(),
                },
                config.clone(),
                ForkScope {
                    definition,
                    root_recorder: storage.recorder.clone(),
                },
            )
            .with_continuation_parent(parent);
        let projection = storage.projection.clone();
        let session = TurnHostFactory::new_with_signal(
            self.registry.clone(),
            self.deployment.clone(),
            self.limits,
            Some("pilotdeck-agent-loop".into()),
            move |identity, recorder, signal| tools.turn(identity, recorder, signal),
        )
        .with_agent_config(config)
        .as_in_process_loop()
        .into_live_session(storage.recorder.clone(), move || {
            projection
                .snapshot()
                .map(|state| state.messages)
                .unwrap_or_default()
        })
        // A full session, not a one-shot sidechain: its TurnRunner records
        // the loop's status messages like any session's.
        .with_tail(tail);
        Ok((Arc::new(session), storage))
    }

    /// `createNativeOneShotSubagentPort({ config, dependencies })` for one
    /// parent turn.
    pub(super) fn port(&self, seed: ForkSeed, parent: ParentTurn) -> Arc<dyn OneShotSubagent> {
        Arc::new(OneShotPort {
            runtime: self.clone(),
            seed,
            parent,
        })
    }
}

/// The calling tool's context with the background task's own abort signal
/// in place of the parent turn's (`api.fork({ ..., abortSignal })`).
fn detached_context(
    context: &ToolRunContext,
    signal: crate::runtime::tools::ToolCancellation,
) -> ToolRunContext {
    let host = &context.host;
    let host = crate::host::modules::tool_context::ToolRuntimeContext {
        fields: host.fields.clone(),
        files: host.files.clone(),
        abort_signal: Some(Arc::new(signal)),
        model: host.model.clone(),
        elicitation: host.elicitation.clone(),
        user_dialog: host.user_dialog.clone(),
        audit_recorder: host.audit_recorder.clone(),
        file_history: host.file_history.clone(),
        file_update_notifier: host.file_update_notifier.clone(),
        now: host.now.clone(),
        progress: host.progress.clone(),
        subagent: host.subagent.clone(),
        plan_todo: host.plan_todo.clone(),
        goal: host.goal.clone(),
        plan_directory: host.plan_directory.clone(),
    };
    ToolRunContext {
        host: Arc::new(host),
        fields: context.fields.clone(),
        execute_tool: context.execute_tool.clone(),
        file_saved: context.file_saved.clone(),
    }
}

/// `config.subagentDepth ?? 0` and `config.maxSubagentDepth ?? 1`.
pub(super) fn depths(config: &JsObject) -> (f64, f64) {
    let number = |key: &str, default: f64| {
        config
            .get(key)
            .and_then(JsValue::as_number)
            .unwrap_or(default)
    };
    (
        number("subagentDepth", 0.0),
        number("maxSubagentDepth", 1.0),
    )
}

/// The `agent.subagents` config section as the session's runtime config
/// carries it (`SessionAgentConfigBundle.compose`, lines 88-151):
/// `subagentModel` from `default` with that model's own token limits,
/// `subagentTimeoutMs` from `timeoutMs` and `maxSubagentDepth` from
/// `maxDepth`. The section itself is not a runtime config key.
///
/// Not carried: `modelMultimodal` of the subagent model (the session config
/// composed here carries none for the main model either).
pub(super) fn compose_subagents_section(
    config: &JsObject,
    capabilities: impl Fn(&JsString, &JsString) -> Option<JsValue>,
    max_output_tokens_override: Option<f64>,
) -> JsObject {
    let mut composed = JsObject::new();
    for (key, value) in config.entries() {
        if !key.equals_str("subagents") {
            composed.insert(key.clone(), value.clone());
        }
    }
    let Some(section) = config.get("subagents") else {
        return composed;
    };
    let default = section.get("default");
    if let Some((provider, model)) = default
        .and_then(|default| default.get("provider"))
        .and_then(JsValue::as_string)
        .zip(
            default
                .and_then(|default| default.get("model"))
                .and_then(JsValue::as_string),
        )
    {
        let mut selected = object([
            ("provider", JsValue::String(provider.clone())),
            ("model", JsValue::String(model.clone())),
        ]);
        let limits = capabilities(provider, model);
        let limit = |key: &str| {
            limits
                .as_ref()
                .and_then(|limits| limits.get(key))
                .filter(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
                .cloned()
        };
        if let Some(tokens) = limit("maxContextTokens") {
            selected.insert("maxContextTokens".into(), tokens);
        }
        if let Some(tokens) = limit("maxOutputTokens") {
            selected.insert(
                "maxOutputTokens".into(),
                max_output_tokens_override.map_or(tokens, JsValue::Number),
            );
        }
        composed.insert("subagentModel".into(), JsValue::Object(selected));
    }
    for (key, source) in [
        ("subagentTimeoutMs", "timeoutMs"),
        ("maxSubagentDepth", "maxDepth"),
    ] {
        if let Some(value) = section
            .get(source)
            .filter(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
        {
            composed.insert(key.into(), value.clone());
        }
    }
    composed
}

/// The scope of a forked child's tools (`createToolCapabilityPolicy` over the
/// parent registry's scoped view).
#[derive(Clone)]
pub(super) struct ForkScope {
    pub(super) definition: Arc<SubagentDefinition>,
    /// The root session's recorder: where a nested fork's parent-side
    /// references and sidechain go.
    pub(super) root_recorder: Arc<Recorder>,
}

impl ForkScope {
    /// The definition's allow list, and no tool that needs a runtime
    /// capability the child lacks: `subagent_fork` is granted only while the
    /// depth cap permits another descendant, user interaction and the plan
    /// workflow never.
    pub(super) fn policy(&self, can_fork: bool) -> ToolPolicy {
        let definition = self.definition.clone();
        Arc::new(move |tool| {
            let fields = tool.fields();
            let Some(name) = fields.get("name").and_then(JsValue::as_string) else {
                return false;
            };
            if definition
                .disallowed_tools
                .iter()
                .any(|entry| name.equals_str(entry))
            {
                return false;
            }
            let allowed = &definition.allowed_tools;
            if !allowed.iter().any(|entry| entry == "*")
                && !allowed.iter().any(|entry| name.equals_str(entry))
            {
                return false;
            }
            let mut needed = tool.required_runtime_capabilities().unwrap_or_default();
            if let Some(JsValue::Array(declared)) = fields.get("requiredRuntimeCapabilities") {
                needed.extend(declared.iter().filter_map(JsValue::as_string).cloned());
            }
            needed
                .iter()
                .all(|capability| can_fork && capability.equals_str("subagent_fork"))
        })
    }
}

/// The parent turn a fork reports to.
#[derive(Clone)]
pub(super) struct ParentTurn {
    /// The recorder of the fork tree's root session (`subagentTranscript`):
    /// it takes the two reference entries, stamped with the forking
    /// session's own ids, and resolves the sidechain path.
    pub(super) recorder: Arc<Recorder>,
    /// The parent turn's runtime config (`AgentRuntimeConfig`).
    pub(super) config: JsObject,
    /// The parent's `eventEmitter`, when it has one.
    pub(super) emit: Option<EventSink>,
    /// The parent's `dependencies.lifecycle`: `SubagentStart` /
    /// `SubagentStop` are the parent session's hook events.
    pub(super) lifecycle: Option<Arc<dyn crate::runtime::tools::ToolLifecycle>>,
}

struct OneShotPort {
    runtime: SubagentRuntime,
    seed: ForkSeed,
    parent: ParentTurn,
}

impl OneShotSubagent for OneShotPort {
    fn create_fork(
        &self,
        identity: JsObject,
        read: SharedFileState,
        write: SharedFileState,
    ) -> RuntimeContext {
        let id = |key: &str| {
            identity
                .get(key)
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default()
        };
        Arc::new(ForkApi(Arc::new(Fork {
            runtime: self.runtime.clone(),
            seed: self.seed.clone(),
            parent: self.parent.clone(),
            session: id("sessionId"),
            turn: id("turnId"),
            read,
            write,
        })))
    }
}

/// `createNativeForkApi` bound to one parent turn.
#[derive(Clone)]
struct Fork {
    runtime: SubagentRuntime,
    seed: ForkSeed,
    parent: ParentTurn,
    session: JsString,
    turn: JsString,
    read: SharedFileState,
    write: SharedFileState,
}

fn text(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in entries {
        result.insert(key.into(), value);
    }
    result
}

fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(number) => *number != 0.0 && !number.is_nan(),
        JsValue::String(text) => !text.is_empty(),
        _ => true,
    }
}

fn defined(value: Option<&JsValue>) -> Option<&JsValue> {
    value.filter(|value| !matches!(value, JsValue::Undefined))
}

/// `truncatePreview`: at most `cap` UTF-8 bytes, cut between code points.
fn truncate_preview(input: &JsString, cap: usize) -> (JsString, bool) {
    let full = text(input);
    if full.len() <= cap {
        return (input.clone(), false);
    }
    let mut end = 0;
    for (index, ch) in full.char_indices() {
        if index + ch.len_utf8() > cap {
            break;
        }
        end = index + ch.len_utf8();
    }
    (full[..end].into(), true)
}

/// `applySystemPromptFilters`: drop the named XML-like blocks, then trim the
/// end. A block starts at the beginning of the text or after a line break.
fn filter_parent_system(prompt: &str, definition: &SubagentDefinition) -> String {
    let mut next = prompt.to_owned();
    if definition.omit_project_instructions {
        next = strip_block(&next, "agents-md");
        next = strip_block(&next, "project-instructions");
    }
    if definition.omit_git_status {
        next = strip_block(&next, "git-status");
        next = strip_block(&next, "gitStatus");
    }
    next.trim_end_matches(js_space).to_owned()
}

/// `input.replace(/(?:^|\n)\s*<tag(?:\s[^>]*)?>[\s\S]*?<\/tag>\s*/g, "\n")`.
fn strip_block(input: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut rest = input;
    loop {
        let mut found = None;
        let mut from = 0;
        while let Some(at) = rest[from..].find(&open).map(|at| at + from) {
            let after = &rest[at + open.len()..];
            let opens = after.starts_with('>')
                || after.chars().next().is_some_and(js_space) && after.contains('>');
            // The whitespace run before the tag must reach back to a line
            // break or the start of the text.
            let before = rest[..at].trim_end_matches(js_space);
            let lead = &rest[before.len()..at];
            let start = if before.is_empty() {
                Some(0)
            } else {
                lead.find('\n').map(|offset| before.len() + offset)
            };
            if let (true, Some(start)) = (opens, start)
                && let Some(end) = after.find(&close)
            {
                let tail = &after[end + close.len()..];
                let trimmed = tail.trim_start_matches(js_space);
                found = Some((start, rest.len() - trimmed.len()));
                break;
            }
            from = at + open.len();
        }
        let Some((start, end)) = found else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        out.push('\n');
        rest = &rest[end..];
    }
}

/// `buildSubagentRuntimeConfig`: the child's runtime config from the parent's.
fn child_config(
    parent: &JsObject,
    definition: &SubagentDefinition,
    subagent_id: &JsString,
) -> JsObject {
    // The baseline counts a one-shot fork twice: the fork API hands the child
    // composition a parent config already at `depth + 1`
    // (OneShotSubagentPort.ts:155-159), and the composition adds one more
    // (SubagentRuntimeComposition.ts:59,186). A first-level child therefore
    // runs at depth 2, and gets the nested `agent` tool only under a cap of 3
    // or more.
    build_child_config(parent, definition, subagent_id, None, 2.0)
}

/// `buildSubagentRuntimeConfig` for a continuable child: the model is the one
/// its descriptor names (`resolvedModel`), and the composition adds one level
/// of depth (`NativeSubagentContinuationHost.createChild`).
pub(super) fn continuable_child_config(
    parent: &JsObject,
    definition: &SubagentDefinition,
    subagent_id: &JsString,
    model: (JsString, JsString),
) -> JsObject {
    build_child_config(parent, definition, subagent_id, Some(model), 1.0)
}

fn build_child_config(
    parent: &JsObject,
    definition: &SubagentDefinition,
    subagent_id: &JsString,
    resolved: Option<(JsString, JsString)>,
    depth_increment: f64,
) -> JsObject {
    let configured = parent.get("subagentModel").and_then(JsValue::as_object);
    // `options.resolvedModel ?? options.definition.modelOverride`.
    let resolved = resolved.or_else(|| {
        definition
            .model_override
            .as_ref()
            .map(|(provider, model)| (provider.as_str().into(), model.as_str().into()))
    });
    // An explicit model keeps the configured subagent model's other fields
    // only when it names that same provider and model.
    let selected = match resolved {
        Some((provider, model)) => {
            let mut selected = configured
                .filter(|configured| {
                    configured.get("provider") == Some(&JsValue::String(provider.clone()))
                        && configured.get("model") == Some(&JsValue::String(model.clone()))
                })
                .cloned()
                .unwrap_or_default();
            selected.insert("provider".into(), JsValue::String(provider));
            selected.insert("model".into(), JsValue::String(model));
            Some(selected)
        }
        None => configured.cloned(),
    };
    let selected = selected.as_ref();
    // The host rebuilds the permission context and the environment per turn
    // from the child's own state. A configured subagent model replaces the
    // parent's and drops the parent's token caps (they describe the parent's
    // model).
    let mut dropped = vec!["permissionContext", "env"];
    if selected.is_some() {
        dropped.extend(["maxContextTokens", "maxOutputTokens"]);
    }
    let mut config = JsObject::new();
    for (key, value) in parent.entries() {
        if !dropped.iter().any(|name| key.equals_str(name)) {
            config.insert(key.clone(), value.clone());
        }
    }
    if let Some(selected) = selected {
        for key in ["provider", "model", "modelMultimodal"] {
            if let Some(value) = selected.get(key).filter(|value| truthy(value)) {
                config.insert(key.into(), value.clone());
            }
        }
    }
    let read_only = definition.is_read_only
        || parent.get("permissionMode") == Some(&"plan".into())
        || parent.get("runMode") == Some(&"ask".into());
    if read_only {
        config.insert("runMode".into(), "ask".into());
    }
    if let Some(mode) = &definition.permission_mode {
        config.insert("permissionMode".into(), mode.as_str().into());
    }
    if let Some(turns) = definition.max_turns {
        config.insert("maxTurns".into(), JsValue::Number(turns));
    }
    config.insert("isSubagent".into(), JsValue::Bool(true));
    // `[buildSubagentSystemPrompt(definition), definition.criticalSystemReminder]`
    // without blank parts, joined by a blank line.
    let subagent_system = [
        Some(subagent_system_prompt(definition)),
        definition.critical_system_reminder.clone(),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.trim_matches(js_space).is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let parent_system = parent
        .get("systemPrompt")
        .and_then(JsValue::as_string)
        .map(|prompt| filter_parent_system(&text(prompt), definition))
        .unwrap_or_default();
    config.insert(
        "systemPrompt".into(),
        JsValue::String(
            if parent_system.is_empty() {
                subagent_system
            } else {
                format!("{subagent_system}\n\n{parent_system}")
            }
            .into(),
        ),
    );
    config.insert("stopOnStructuredOutput".into(), JsValue::Bool(false));
    config.insert(
        "subagentDepth".into(),
        JsValue::Number(depths(parent).0 + depth_increment),
    );
    let mut metadata = parent
        .get("metadata")
        .and_then(JsValue::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert("subagentId".into(), JsValue::String(subagent_id.clone()));
    metadata.insert("subagentType".into(), definition.id.as_str().into());
    config.insert("metadata".into(), JsValue::Object(metadata));
    config
}

/// `extractFinalAssistantText`: the text blocks of the last assistant message
/// that has any, joined by line breaks and trimmed.
fn final_assistant_text(messages: &[JsValue]) -> String {
    for message in messages.iter().rev() {
        let Some(JsValue::Array(blocks)) = message.get("content") else {
            continue;
        };
        let parts: Vec<String> = blocks
            .iter()
            .filter(|block| block.get("type") == Some(&"text".into()))
            .filter_map(|block| block.get("text").and_then(JsValue::as_string).map(text))
            .collect();
        if !parts.is_empty() {
            return parts.join("\n").trim_matches(js_space).to_owned();
        }
    }
    String::new()
}

enum Step {
    ParentAborted,
    TimedOut,
    Next(Result<Option<JsValue>, crate::runtime::project::RuntimeError>),
}

struct ChildOutcome {
    markdown: String,
    usage: JsValue,
    turns: JsValue,
    duration_ms: f64,
}

impl Fork {
    fn emit(&self, event: JsObject) {
        if let Some(emit) = &self.parent.emit {
            emit(event);
        }
    }

    /// `{ sessionId, turnId, subagentId, subagentType }` of a forwarded event.
    fn base(
        &self,
        kind: &str,
        subagent_id: &JsString,
        definition: &SubagentDefinition,
    ) -> JsObject {
        object([
            ("type", kind.into()),
            ("sessionId", JsValue::String(self.session.clone())),
            ("turnId", JsValue::String(self.turn.clone())),
            ("subagentId", JsValue::String(subagent_id.clone())),
            ("subagentType", definition.id.as_str().into()),
        ])
    }

    /// `SubAgentSession.forwardActivity`: the child loop's events as the
    /// parent's client sees them.
    fn forward(&self, event: &JsValue, subagent_id: &JsString, definition: &SubagentDefinition) {
        let Some(kind) = event.get("type").and_then(JsValue::as_string).map(text) else {
            return;
        };
        let copy = |target: &mut JsObject, keys: &[&str]| {
            for key in keys {
                if let Some(value) = defined(event.get(key)) {
                    target.insert((*key).into(), value.clone());
                }
            }
        };
        match kind.as_str() {
            "model_event" => {
                let mut forwarded = self.base("subagent_model_event", subagent_id, definition);
                copy(
                    &mut forwarded,
                    &["event", "timeline", "blockId", "streamBoundary"],
                );
                self.emit(forwarded);
            }
            "tool_calls_detected" => {
                let mut forwarded =
                    self.base("subagent_tool_calls_detected", subagent_id, definition);
                copy(&mut forwarded, &["calls"]);
                self.emit(forwarded);
            }
            "compact_started" | "compact_completed" => {
                let mut forwarded = self.base("agent_status", subagent_id, definition);
                forwarded.insert("event".into(), format!("subagent_{kind}").as_str().into());
                copy(&mut forwarded, &["timeline"]);
                let mut detail = event.as_object().cloned().unwrap_or_default();
                detail.insert("subagentId".into(), JsValue::String(subagent_id.clone()));
                forwarded.insert("detail".into(), JsValue::Object(detail));
                self.emit(forwarded);
            }
            "assistant_message" => {
                let Some(JsValue::Array(blocks)) = event
                    .get("message")
                    .and_then(|message| message.get("content"))
                else {
                    return;
                };
                for block in blocks {
                    let kind = block.get("type");
                    let shown = kind == Some(&"text".into()) || kind == Some(&"thinking".into());
                    let Some(timeline) = block
                        .get("timeline")
                        .filter(|timeline| shown && truthy(timeline))
                    else {
                        continue;
                    };
                    let mut forwarded = self.base("agent_status", subagent_id, definition);
                    forwarded.insert("event".into(), "subagent_assistant_block".into());
                    forwarded.insert("timeline".into(), timeline.clone());
                    let mut detail = object([
                        ("subagentId", JsValue::String(subagent_id.clone())),
                        ("kind", kind.cloned().unwrap_or(JsValue::Undefined)),
                    ]);
                    for key in ["text", "blockId"] {
                        if let Some(value) = defined(block.get(key)) {
                            detail.insert(key.into(), value.clone());
                        }
                    }
                    forwarded.insert("detail".into(), JsValue::Object(detail));
                    self.emit(forwarded);
                }
            }
            "tool_result" => {
                let mut forwarded = self.base("subagent_tool_result", subagent_id, definition);
                copy(&mut forwarded, &["result", "timeline"]);
                self.emit(forwarded);
            }
            // The child's tool runtime shares the parent's emitter in the
            // baseline; here its lifecycle pair surfaces in the child's own
            // stream and is handed on unchanged (child session id included).
            "pre_tool_execute" | "post_tool_execute" => {
                if let Some(fields) = event.as_object() {
                    self.emit(fields.clone());
                }
            }
            _ => {}
        }
    }

    async fn record_parent(&self, draft: JsObject) -> Result<(), JsString> {
        let recorder = self.parent.recorder.clone();
        let (session, turn) = (self.session.clone(), self.turn.clone());
        tokio::task::spawn_blocking(move || {
            recorder
                .writer()
                .record_session_event(&session, &turn, draft)
        })
        .await
        .map_err(|error| JsString::from(error.to_string()))?
        .map_err(|error| JsString::from(error.to_string()))
    }

    async fn record_completed(
        &self,
        subagent_id: &JsString,
        definition: &SubagentDefinition,
        summary: &JsString,
        outcome: Option<&ChildOutcome>,
    ) -> Result<(), JsString> {
        let (preview, truncated) = truncate_preview(summary, SUMMARY_PREVIEW_BYTES);
        let mut draft = object([
            ("type", "subagent_completed".into()),
            ("subagentId", JsValue::String(subagent_id.clone())),
            ("subagentType", definition.id.as_str().into()),
            ("summaryPreview", JsValue::String(preview)),
            ("summaryTruncated", JsValue::Bool(truncated)),
        ]);
        if let Some(usage) = outcome
            .map(|outcome| &outcome.usage)
            .filter(|usage| truthy(usage))
        {
            draft.insert("usage".into(), usage.clone());
        }
        draft.insert(
            "turns".into(),
            outcome.map_or(JsValue::Number(0.0), |outcome| outcome.turns.clone()),
        );
        draft.insert(
            "durationMs".into(),
            JsValue::Number(outcome.map_or(0.0, |outcome| outcome.duration_ms)),
        );
        draft.insert("errored".into(), JsValue::Bool(outcome.is_none()));
        self.record_parent(draft).await
    }

    /// `SubAgentSession.runNative`: run the child loop to its terminal.
    async fn run_child(
        &self,
        request: &ForkRequest,
        definition: Arc<SubagentDefinition>,
        storage: &FactoryStorage,
        child_session: &JsString,
        context: &ToolRunContext,
    ) -> Result<ChildOutcome, JsString> {
        let started = Instant::now();
        let fail = |error: &dyn std::fmt::Display| JsString::from(error.to_string());
        let checkpoint = HostToolCheckpoint::new(
            Some(&SeedState {
                allowed_read_files: None,
                read_file_state: Some(self.read.lock().unwrap_or_else(|e| e.into_inner()).clone()),
                write_snapshots: Some(self.write.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            }),
            &[],
        )
        .map_err(|error: HostError| fail(&error.message))?;
        let config = child_config(&self.parent.config, &definition, &request.subagent_id);
        let tools: SessionTools = self.seed.clone().child(
            SessionBinding {
                context: self.seed.context.clone(),
                recorder: storage.recorder.clone(),
                checkpoint,
                projection: storage.projection.clone(),
            },
            config.clone(),
            ForkScope {
                definition: definition.clone(),
                root_recorder: self.parent.recorder.clone(),
            },
        );
        let session = TurnHostFactory::new_with_signal(
            self.runtime.registry.clone(),
            self.runtime.deployment.clone(),
            self.runtime.limits,
            Some("pilotdeck-agent-loop".into()),
            move |identity, recorder, signal| tools.turn(identity, recorder, signal),
        )
        .with_agent_config(config)
        .as_in_process_loop()
        .into_live_session(storage.recorder.clone(), Vec::new)
        .as_sidechain();
        let turn_id = JsString::from(format!("{}-t0", text(&request.subagent_id)));
        let mut turn = session
            .submit(
                JsValue::Object(object([
                    ("type", "text".into()),
                    ("text", JsValue::String(request.directive.clone())),
                ])),
                JsValue::Object(object([
                    ("sessionId", JsValue::String(child_session.clone())),
                    ("turnId", JsValue::String(turn_id.clone())),
                ])),
            )
            .map_err(|error| fail(&error.message))?;
        let timeout = tokio::time::sleep(std::time::Duration::from_millis(
            request.timeout_ms.max(0.0) as u64,
        ));
        tokio::pin!(timeout);
        let mut timed_out = false;
        let mut aborted = false;
        let mut messages: Vec<JsValue> = Vec::new();
        let mut result = None;
        let failure = loop {
            // `composeAbortSignal`: the parent's abort and the fork's own
            // timeout both abort the child, which then runs to its terminal.
            let step = tokio::select! {
                biased;
                _ = context.cancelled(), if !aborted && !timed_out => Step::ParentAborted,
                _ = &mut timeout, if !aborted && !timed_out => Step::TimedOut,
                next = turn.next() => Step::Next(next),
            };
            let event: JsValue = match step {
                Step::ParentAborted => {
                    aborted = true;
                    let _ = session.abort(&"subagent_parent_aborted".into());
                    continue;
                }
                Step::TimedOut => {
                    timed_out = true;
                    let _ = session.abort(&"subagent_timeout".into());
                    continue;
                }
                Step::Next(Ok(Some(event))) => event,
                Step::Next(Ok(None)) => break None,
                Step::Next(Err(error)) => break Some(fail(&error.message)),
            };
            let kind = event.get("type").and_then(JsValue::as_string).map(text);
            match kind.as_deref() {
                // The descriptor is the entry right after `turn_started`,
                // before the first visible child input
                // (`recordSubagentAcceptedInputWithDescriptor`).
                Some("turn_started") => {
                    let recorder = storage.recorder.clone();
                    let (session_id, turn_id) = (child_session.clone(), turn_id.clone());
                    let draft = object([
                        ("type", "subagent_descriptor".into()),
                        (
                            "descriptor",
                            JsValue::Object(object([
                                ("version", JsValue::Number(1.0)),
                                ("mode", "one-shot".into()),
                                ("provider", "pilotdeck-native".into()),
                                ("definitionId", definition.id.as_str().into()),
                            ])),
                        ),
                    ]);
                    let written = tokio::task::spawn_blocking(move || {
                        recorder
                            .writer()
                            .record_session_event(&session_id, &turn_id, draft)
                    })
                    .await;
                    match written {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            let _ = session.abort(&"subagent_descriptor_failed".into());
                            break Some(fail(&error));
                        }
                        Err(error) => {
                            let _ = session.abort(&"subagent_descriptor_failed".into());
                            break Some(fail(&error));
                        }
                    }
                }
                Some("assistant_message") => {
                    if let Some(message) = event.get("message") {
                        messages.push(message.clone());
                    }
                }
                Some("turn_completed") => result = event.get("result").cloned(),
                _ => {}
            }
            self.forward(&event, &request.subagent_id, &definition);
        };
        drop(turn);
        let _ = session.dispose().await;
        if timed_out {
            return Err(format!("Subagent timed out after {}ms.", request.timeout_ms).into());
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        let Some(result) = result else {
            return Err("SubAgentSession: AgentLoop returned no result".into());
        };
        let stop_reason = result
            .get("stopReason")
            .and_then(JsValue::as_string)
            .map(text)
            .unwrap_or_else(|| "undefined".into());
        match result
            .get("type")
            .and_then(JsValue::as_string)
            .map(text)
            .as_deref()
        {
            Some("aborted") => {
                return Err(
                    format!("SubAgentSession: subagent turn aborted ({stop_reason})").into(),
                );
            }
            Some("error") => {
                let details = match result.get("errors") {
                    Some(JsValue::Array(errors)) => errors
                        .iter()
                        .map(|error| {
                            error
                                .get("message")
                                .and_then(JsValue::as_string)
                                .map(text)
                                .unwrap_or_else(|| "undefined".into())
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                    _ => String::new(),
                };
                return Err(if details.is_empty() {
                    format!("SubAgentSession: subagent turn failed ({stop_reason})")
                } else {
                    format!("SubAgentSession: subagent turn failed ({stop_reason}): {details}")
                }
                .into());
            }
            _ => {}
        }
        if aborted {
            return Err("Subagent aborted before completion.".into());
        }
        Ok(ChildOutcome {
            markdown: final_assistant_text(&messages),
            usage: result.get("usage").cloned().unwrap_or(JsValue::Undefined),
            turns: result.get("turns").cloned().unwrap_or(JsValue::Undefined),
            duration_ms: started.elapsed().as_millis() as f64,
        })
    }

    /// `dispatchSubagentLifecycle` (OneShotSubagentPort.ts:494-518): the
    /// parent's session, cwd, permission mode and turn environment.
    async fn lifecycle(
        &self,
        event: &'static str,
        context: &ToolRunContext,
        payload: JsObject,
    ) -> Result<(), JsString> {
        let Some(lifecycle) = &self.parent.lifecycle else {
            return Ok(());
        };
        lifecycle
            .dispatch(event, &event.into(), payload, context)
            .await
            .map(|_| ())
            .map_err(|error| JsString::from(error.message))
    }

    /// `api.fork`: the parent-side bookkeeping around one child run.
    async fn run(
        self,
        request: ForkRequest,
        context: ToolRunContext,
    ) -> Result<ForkReport, JsString> {
        self.run_reporting_start(request, context, None).await
    }

    /// [`Self::run`], signalling `started` once the parent-side
    /// `subagent_started` record is durable.
    async fn run_reporting_start(
        self,
        request: ForkRequest,
        context: ToolRunContext,
        started: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<ForkReport, JsString> {
        let definition = definitions_from_config(&self.parent.config)
            .into_iter()
            .find(|definition| definition.id == text(&request.definition_id))
            .ok_or_else(|| {
                JsString::from(format!(
                    "Unknown subagent type: {}",
                    text(&request.definition_id)
                ))
            })?;
        let cwd = self
            .parent
            .config
            .get("cwd")
            .and_then(JsValue::as_string)
            .map(text)
            .unwrap_or_default();
        let child_session = JsString::from(format!("{cwd}::sub::{}", text(&request.subagent_id)));
        let parent_writer = self.parent.recorder.writer().clone();
        let sidechain = parent_writer.for_subagent(&request.subagent_id);
        let relative: JsString = parent_writer
            .relative_subagent_path(&request.subagent_id)
            .into();
        let storage = FactoryStorage::new(
            Arc::new(JsonlTranscriptStore::new()),
            TranscriptKey {
                project_root: PathBuf::from(&cwd),
                pilot_home: self.runtime.home.clone(),
                session_id: child_session.clone(),
                transcript_path: sidechain.transcript_path,
            },
        );
        let (preview, truncated) = truncate_preview(&request.directive, PROMPT_PREVIEW_BYTES);
        self.record_parent(object([
            ("type", "subagent_started".into()),
            ("subagentId", JsValue::String(request.subagent_id.clone())),
            ("subagentType", definition.id.as_str().into()),
            ("promptPreview", JsValue::String(preview)),
            ("promptTruncated", JsValue::Bool(truncated)),
            ("transcriptRelativePath", JsValue::String(relative.clone())),
            ("subagentSessionId", JsValue::String(child_session.clone())),
        ]))
        .await?;
        if let Some(started) = started {
            let _ = started.send(());
        }
        // `dispatchSubagentLifecycle("SubagentStart")` between the durable
        // start and the start event (OneShotSubagentPort.ts:129-147).
        self.lifecycle(
            "SubagentStart",
            &context,
            object([
                ("subagentId", JsValue::String(request.subagent_id.clone())),
                ("subagentType", definition.id.as_str().into()),
            ]),
        )
        .await?;
        let mut started = self.base("subagent_started", &request.subagent_id, &definition);
        if let Some(call) = &request.tool_call_id {
            started.insert("toolCallId".into(), JsValue::String(call.clone()));
        }
        self.emit(started);
        let completed = |success: bool, aborted: Option<bool>, duration_ms: f64| {
            let mut event = self.base("subagent_completed", &request.subagent_id, &definition);
            event.insert("success".into(), JsValue::Bool(success));
            if let Some(aborted) = aborted {
                event.insert("aborted".into(), JsValue::Bool(aborted));
            }
            event.insert("durationMs".into(), JsValue::Number(duration_ms));
            event
        };
        let outcome = self
            .run_child(
                &request,
                definition.clone(),
                &storage,
                &child_session,
                &context,
            )
            .await;
        // A parent completion never advertises a child whose storage has not
        // finished its durable teardown.
        let closed = storage
            .close_sidechain()
            .await
            .map_err(|error| JsString::from(error.message));
        let outcome = match (outcome, closed) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
            (Err(error), Err(_)) => Err(error),
        };
        match outcome {
            Ok(outcome) => {
                let markdown = JsString::from(outcome.markdown.as_str());
                self.record_completed(&request.subagent_id, &definition, &markdown, Some(&outcome))
                    .await?;
                self.lifecycle(
                    "SubagentStop",
                    &context,
                    object([
                        ("subagentId", JsValue::String(request.subagent_id.clone())),
                        ("subagentType", definition.id.as_str().into()),
                        ("success", JsValue::Bool(true)),
                    ]),
                )
                .await?;
                self.emit(completed(true, None, outcome.duration_ms));
                Ok(ForkReport {
                    parsed: parse_summary(&outcome.markdown),
                    markdown,
                    usage: outcome.usage,
                    turns: outcome.turns,
                    duration_ms: outcome.duration_ms,
                    subagent_session_id: child_session,
                    transcript_relative_path: Some(relative),
                })
            }
            Err(failure) => {
                let timed_out = text(&failure).starts_with("Subagent timed out after ");
                let aborted = context.is_aborted() && !timed_out;
                let failure = match self
                    .record_completed(&request.subagent_id, &definition, &failure, None)
                    .await
                {
                    Ok(()) => failure,
                    Err(_) => {
                        "Subagent failed and its parent completion could not be recorded.".into()
                    }
                };
                let failure = match self
                    .lifecycle(
                        "SubagentStop",
                        &context,
                        object([
                            ("subagentId", JsValue::String(request.subagent_id.clone())),
                            ("subagentType", definition.id.as_str().into()),
                            ("success", JsValue::Bool(false)),
                        ]),
                    )
                    .await
                {
                    Ok(()) => failure,
                    Err(_) => {
                        "Subagent failed and its stop lifecycle hook could not be dispatched."
                            .into()
                    }
                };
                self.emit(completed(false, Some(aborted), 0.0));
                Err(failure)
            }
        }
    }
}

impl SubagentFork for Fork {
    fn depth(&self) -> f64 {
        depths(&self.parent.config).0
    }
    fn max_subagent_depth(&self) -> f64 {
        depths(&self.parent.config).1
    }
    fn definition_ids(&self) -> Vec<JsString> {
        definitions_from_config(&self.parent.config)
            .iter()
            .map(|definition| definition.id.as_str().into())
            .collect()
    }
    fn fork(&self, request: ForkRequest, context: ToolRunContext) -> ForkFuture {
        Box::pin(self.clone().run(request, context))
    }
    fn is_background_definition(&self, id: &JsString) -> bool {
        definitions_from_config(&self.parent.config)
            .iter()
            .any(|definition| definition.id == text(id) && definition.background)
    }
    /// `launchBackground` (OneShotSubagentPort.ts:77-93): the fork runs in
    /// the background-subagent runtime under that task's own abort signal.
    fn launch_background(
        &self,
        request: ForkRequest,
        context: ToolRunContext,
    ) -> Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsString, JsString>> + Send>>,
    > {
        let background = self.runtime.background.clone()?;
        let fork = self.clone();
        Some(Box::pin(async move {
            let definition = definitions_from_config(&fork.parent.config)
                .into_iter()
                .find(|definition| definition.id == text(&request.definition_id))
                .filter(|definition| definition.background)
                .ok_or_else(|| {
                    JsString::from(format!(
                        "Subagent type {} is not configured for background execution.",
                        text(&request.definition_id)
                    ))
                })?;
            // `BackgroundSubagentRuntime.start` runs the fork from a
            // microtask queued before `launchBackground` resolves, and the
            // fork's synchronous prefix (`OneShotSubagentPort.ts:94-128`)
            // records `subagent_started` in the parent transcript before
            // the parent's tool runtime regains control: the start record
            // lands ahead of the `agent` call's `tool_result`. The task id
            // is therefore returned only once that record is durable (or the
            // fork failed before writing it).
            let (started, recorded) = tokio::sync::oneshot::channel();
            let task = background.start(crate::runtime::background::StartBackgroundSubagentInput {
                kind: Some(crate::runtime::background::SubagentKind::Background),
                session_id: fork.session.clone(),
                parent_turn_id: fork.turn.clone(),
                subagent_id: request.subagent_id.clone(),
                subagent_type: definition.id.as_str().into(),
                observed_subagent_id: None,
                run: Box::new(move |signal| {
                    Box::pin(async move {
                        let context = detached_context(&context, signal);
                        fork.run_reporting_start(request, context, Some(started))
                            .await
                            .map(|_| ())
                    })
                }),
            });
            let _ = recorded.await;
            Ok(task.task_id)
        }))
    }
    fn subagent_id(&self) -> JsString {
        uuid::Uuid::new_v4().to_string().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tools::builtin::agent::subagent_definition;

    #[test]
    fn preview_cuts_between_code_points_at_the_byte_cap() {
        assert_eq!(truncate_preview(&"abc".into(), 3), ("abc".into(), false));
        assert_eq!(truncate_preview(&"abcd".into(), 3), ("abc".into(), true));
        // `é` is two bytes: a cap of 2 keeps `a` only.
        assert_eq!(truncate_preview(&"aéb".into(), 2), ("a".into(), true));
    }

    #[test]
    fn parent_system_blocks_are_dropped_for_read_only_definitions() {
        let explore = subagent_definition("explore").expect("explore");
        let general = subagent_definition("general-purpose").expect("general");
        let prompt = "Intro\n  <project-instructions>\nrules\n</project-instructions>\n\nMiddle\n<git-status a=\"1\">x</git-status>\nEnd  \n";
        assert_eq!(filter_parent_system(prompt, &explore), "Intro\nMiddle\nEnd");
        assert_eq!(filter_parent_system(prompt, &general), prompt.trim_end());
        // A tag in the middle of a line is not a block.
        assert_eq!(
            filter_parent_system("see <git-status>x</git-status> here", &explore),
            "see <git-status>x</git-status> here"
        );
    }

    #[test]
    fn child_config_follows_the_definition_and_the_depth() {
        let parent = object([
            ("provider", "p".into()),
            ("model", "m".into()),
            ("cwd", "/w".into()),
            ("maxContextTokens", JsValue::Number(10.0)),
            ("permissionContext", JsValue::Object(JsObject::new())),
        ]);
        let explore = subagent_definition("explore").expect("explore");
        let child = child_config(&parent, &explore, &"id-1".into());
        assert_eq!(child.get("runMode"), Some(&"ask".into()));
        assert_eq!(child.get("isSubagent"), Some(&JsValue::Bool(true)));
        assert_eq!(child.get("subagentDepth"), Some(&JsValue::Number(2.0)));
        assert_eq!(child.get("maxContextTokens"), Some(&JsValue::Number(10.0)));
        assert!(child.get("permissionContext").is_none());
        assert_eq!(
            child.get("metadata").and_then(|m| m.get("subagentType")),
            Some(&"explore".into())
        );
        let general = subagent_definition("general-purpose").expect("general");
        let mut with_model = parent.clone();
        with_model.insert(
            "subagentModel".into(),
            JsValue::Object(object([("provider", "q".into()), ("model", "n".into())])),
        );
        let child = child_config(&with_model, &general, &"id-2".into());
        assert_eq!(child.get("runMode"), None);
        assert_eq!(child.get("provider"), Some(&"q".into()));
        assert_eq!(child.get("model"), Some(&"n".into()));
        assert!(child.get("maxContextTokens").is_none());
    }
}
