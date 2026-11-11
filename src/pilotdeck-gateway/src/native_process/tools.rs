use super::{
    NativeTurnEnvironment, Options,
    assembly::{env_object, value},
};
use crate::{
    host::{
        HostError, HostFuture, ModuleError,
        modules::{
            CompositionSwitches, capability, model_port::RouterModelPortOptions,
            permission::PermissionDecision, tool_context::*,
        },
    },
    runtime::{
        hooks::Cancellation,
        interaction::GatewayInteractionCoordinator,
        mcp::{
            bridge::{
                CreateToolDefinitionsOptions, McpBridgeTool,
                create_mcp_tool_definitions_from_runtime,
            },
            protocol::McpStatus,
            registry::{
                McpDiagnosticSink, ProjectMcpRuntimeLease, ProjectMcpRuntimeProvider,
                SessionMcpRuntimeRegistration, SessionMcpRuntimeRegistry,
            },
            sdk::SdkMcpConfig,
        },
        permission::{self, DefaultPermissionContextOptions, PermissionContext, PermissionTool},
        project::{
            RuntimeError, RuntimeProfile, RuntimeProfileOverrides,
            interaction_permission::PermissionTurnOptions,
        },
        session::factory::SessionBinding,
        steer::MutationRecorder,
        tools::{SchedulerMode, ToolRegistry, ToolRuntime, builtin},
        turn::{host_factory::SessionPorts, interaction::InteractionTurnInput},
    },
    storage::recorder::Recorder,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, admission::ExecutionIdentity};
use pilotdeck_transport::{server::ExecutionClock, system_clock::SystemExecutionClock};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// `ProjectSessionStorage.ts:413`: one session's tool-results directory under
/// the project root, with the path-safe session id. Shared by the per-turn
/// ports and the #897 session-composition preflight so the two cannot drift.
pub(super) fn session_tool_results_dir(project: &Path, session_id: &JsString) -> PathBuf {
    let dir = if crate::mutants::on("session-cwd-dir-layout") {
        project.join("tool-results")
    } else {
        project.join(".pilotdeck/tool-results")
    };
    if crate::mutants::on("session-cwd-dir-unsanitized") {
        dir.join(String::from_utf16_lossy(session_id.units()))
    } else {
        dir.join(crate::storage::sanitize_session_id(session_id))
    }
}

pub struct SessionTools {
    /// The session's project root: `resolve(projectKey ?? fallbackProjectRoot)`
    /// for the key the session was created under.
    project: PathBuf,
    env: JsObject,
    binding: SessionBinding,
    config: JsObject,
    interactions: Arc<GatewayInteractionCoordinator>,
    permission: Arc<Permissions>,
    lsp: Arc<super::families::DefaultLsp>,
    /// #1433: the session's composed MCP tools, registered into every turn's
    /// tool registry after the LSP entry (composed once at prepare, like
    /// `SessionMcpRuntimeBundle.compose`).
    mcp: SessionMcpTools,
    /// #1864 (b): re-read on every turn, like InProcessGateway.ts:1443 —
    /// the host-side permission context follows the stored selection so the
    /// same turn never runs one mode for the sidecar and another for native
    /// tool admission.
    permission_mode_source: Option<Arc<dyn Fn() -> Option<JsString> + Send + Sync>>,
    /// The host's `config.runMode` (agentLoopSidecarClient.ts:432-436): each
    /// turn's `input.runMode` overrides it, a turn without one keeps the
    /// previous value, and `"agent"` is the fallback. The tool runtime's
    /// ask-mode gate and a fork's read-only inheritance read it from the
    /// per-turn config.
    run_mode: Arc<Mutex<Option<JsString>>>,
    /// Session-scoped execution world shared by every turn's tool registry,
    /// so a background task started in one turn is still there in the next.
    world: ExecutionWorld,
    /// The goal runtime's session-long state (`SessionGoalBundle`).
    goal: Arc<crate::runtime::goal::GoalSessionState>,
    /// `planFileManager` (`SessionPlanTodoBundle`).
    plan_files: Arc<crate::runtime::plan_todo::SessionPlanFiles>,
    /// The generation's agent event buffer, for events the tool runtime
    /// raises itself.
    events: Option<crate::runtime::project::interaction_events::InteractionEvents>,
    /// The subagent family behind the `agent` tool. Without it the tool is
    /// not registered.
    subagents: Option<super::subagents::SubagentRuntime>,
    /// Set on the tools of a forked child session: its definition's scope.
    fork: Option<super::subagents::ForkScope>,
    /// The agent the `subagent` / `send_message` tools act for
    /// (`SessionSubagentContinuationBundle`). Without it, or without the
    /// subagent family, they are not registered.
    continuation: Option<super::continuations::ParentRef>,
    /// `LocalGatewayBootConfig.elicitationTimeoutMs`.
    elicitation_timeout: Duration,
    /// `SessionFileHistoryBundle`: the session's file-history store behind
    /// the write tools, recording its snapshots in the transcript.
    file_history: Arc<crate::storage::file_history::FileHistoryStore>,
    /// `sdkSessionConfig.userDialogKinds` and the session's gateway
    /// user-dialog channel: one `request_user_*` tool per enabled kind.
    user_dialog: Option<(
        Vec<&'static str>,
        crate::user_dialog::GatewayUserDialogChannel,
    )>,
    /// The session's hook settings (plugin contributions, then the SDK
    /// session config) and the observer projecting their execution.
    hooks: crate::hooks::protocol::HooksSettings,
    hook_observers: crate::runtime::hooks::runtime::HookObservers,
    /// The tool runtime's lifecycle: the current turn's bound hooks.
    tool_hooks: Arc<super::hooks::ToolHooks>,
}

/// `SessionFileHistoryBundle.compose`: backups under the session's
/// `file-history` directory, every recorded snapshot appended to the
/// transcript, and the store primed from the snapshots already there.
fn session_file_history(
    binding: &SessionBinding,
) -> crate::storage::file_history::FileHistoryStore {
    use crate::storage::file_history::{FileHistoryError, FileHistoryOptions, FileHistoryStore};
    // `<chats>/<session>.jsonl` → `<chats>/<session>/file-history`.
    let backup_dir = binding
        .recorder
        .writer()
        .path()
        .with_extension("")
        .join("file-history");
    let mut options = FileHistoryOptions::new(backup_dir);
    let recorder = binding.recorder.clone();
    let session = binding.context.session_key.clone();
    options.on_snapshot_recorded = Some(Arc::new(move |snapshot, kind| {
        let turn = snapshot
            .get("messageId")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_default();
        let mut draft = value([
            ("type", "file_snapshot_recorded".into()),
            ("snapshotKind", kind.into()),
        ]);
        for (key, field) in snapshot.entries() {
            draft.insert(key.clone(), field.clone());
        }
        recorder
            .writer()
            .record_session_event(&session, &turn, draft)
            .map_err(|error| FileHistoryError::Fs(error.to_string()))
    }));
    let store = FileHistoryStore::new(options);
    let mut entries = Vec::new();
    binding
        .projection
        .for_each_entry(|entry| entries.push(entry.clone()));
    if let Ok(snapshots) = crate::runtime::rewind::snapshot_entries(&entries) {
        store.replay_from_transcript(&snapshots);
    }
    store
}

/// What a forked child session takes over from the session that forks it
/// (`createSubagentRuntimeComposition`): the same project, environment and
/// services, under its own binding, config and tool scope.
#[derive(Clone)]
pub(super) struct ForkSeed {
    project: PathBuf,
    env: JsObject,
    /// The parent session's context: the child inherits its tool allow and
    /// deny lists (the child registry is a view of the parent's).
    pub(super) context: crate::runtime::session::SessionContext,
    interactions: Arc<GatewayInteractionCoordinator>,
    permission: Arc<Permissions>,
    lsp: Arc<super::families::DefaultLsp>,
    mcp: SessionMcpTools,
    world: ExecutionWorld,
    events: Option<crate::runtime::project::interaction_events::InteractionEvents>,
    subagents: Option<super::subagents::SubagentRuntime>,
    /// A one-shot child's registry view keeps the parent's continuation
    /// tools, still bound to the parent agent.
    continuation: Option<super::continuations::ParentRef>,
    /// The child registry is a scoped view of the parent's
    /// (SubagentRuntimeComposition.ts:57-66): its write tools are the
    /// parent's instances and record into the parent session's file history.
    file_history: Arc<crate::storage::file_history::FileHistoryStore>,
    elicitation_timeout: Duration,
}
impl ForkSeed {
    /// The child session's tools. The permission context starts as a copy of
    /// the parent's current one; goal and plan state are the child's own.
    pub(super) fn child(
        self,
        binding: SessionBinding,
        config: JsObject,
        scope: super::subagents::ForkScope,
    ) -> SessionTools {
        let permission = self
            .permission
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone();
        let run_mode = Arc::new(Mutex::new(config_run_mode(&config)));
        SessionTools {
            plan_files: Arc::new(crate::runtime::plan_todo::SessionPlanFiles::new(
                &self.project,
            )),
            project: self.project,
            env: self.env,
            binding,
            config,
            interactions: self.interactions,
            // The child tool runtime runs on the inherited permission runtime
            // (SubagentRuntimeComposition.ts:76-81), so it asks through the
            // parent's answerer. Mode transitions stay with the parent session.
            permission: Arc::new(Permissions(
                Mutex::new((permission, None)),
                self.permission.1.clone(),
                Default::default(),
                Default::default(),
            )),
            lsp: self.lsp,
            mcp: self.mcp,
            permission_mode_source: None,
            run_mode,
            world: self.world,
            goal: Arc::default(),
            events: self.events,
            subagents: self.subagents,
            fork: Some(scope),
            continuation: self.continuation,
            file_history: self.file_history,
            user_dialog: None,
            elicitation_timeout: self.elicitation_timeout,
            hooks: Vec::new(),
            hook_observers: Default::default(),
            tool_hooks: Arc::default(),
        }
    }
}
impl SessionTools {
    pub fn new(
        options: &Options,
        env: &crate::config::Env,
        binding: SessionBinding,
        config: JsObject,
        interactions: Arc<GatewayInteractionCoordinator>,
        lsp: Arc<super::families::DefaultLsp>,
        mcp: SessionMcpTools,
    ) -> Result<Self, RuntimeError> {
        let project = options.project_root_for(binding.context.project_key.as_ref());
        let context =
            permission::create_default_permission_context(DefaultPermissionContextOptions {
                cwd: project.to_string_lossy().into_owned().into(),
                // `canPrompt: override?.canPrompt ?? runtime.profile.interaction.canPrompt`
                // (`cli/ProjectSessionFactory.ts:270`); a turn's own
                // `canPrompt` still wins in the tool context.
                can_prompt: Some(
                    RuntimeProfile::resolve(
                        &JsValue::Undefined,
                        &RuntimeProfileOverrides::default(),
                    )
                    .interaction
                    .can_prompt,
                ),
                plan_directory_path: Some(
                    crate::runtime::plan_todo::SessionPlanFiles::directory_for(&project),
                ),
                ..Default::default()
            });
        let plan_files = Arc::new(crate::runtime::plan_todo::SessionPlanFiles::new(&project));
        let file_history = Arc::new(session_file_history(&binding));
        let run_mode = Arc::new(Mutex::new(config_run_mode(&config)));
        let mut world = ExecutionWorld::new();
        world.invocation = invocation_audit(options, env, &project);
        Ok(Self {
            project,
            env: env_object(env),
            binding,
            config,
            interactions,
            lsp,
            mcp,
            permission: Arc::new(Permissions(
                Mutex::new((context, None)),
                Default::default(),
                Default::default(),
                Default::default(),
            )),
            permission_mode_source: None,
            run_mode,
            world,
            goal: Arc::default(),
            plan_files,
            events: None,
            subagents: None,
            fork: None,
            continuation: None,
            file_history,
            user_dialog: None,
            hooks: Vec::new(),
            hook_observers: Default::default(),
            tool_hooks: Arc::default(),
            elicitation_timeout: super::permission_prompt::interaction_timeout(
                env.get("PILOTDECK_ELICITATION_TIMEOUT_MS")
                    .map(|value| value.as_str()),
            ),
        })
    }

    /// The SDK adapter's permission inputs (SessionAgentConfigBundle.ts:
    /// 167-180): `dontAsk` turns prompting off, `acceptEdits` is recorded on
    /// the context, and `additionalWorkingDirectories` widen the roots the
    /// context accepts.
    pub(super) fn with_sdk_permissions(self, config: Option<&JsValue>) -> Self {
        let Some(config) = config.and_then(JsValue::as_object) else {
            return self;
        };
        let mode = config.get("permissionMode").and_then(JsValue::as_string);
        {
            let mut state = self.permission.0.lock().unwrap_or_else(|e| e.into_inner());
            if mode.is_some_and(|mode| mode.equals_str("dontAsk")) {
                state.0.can_prompt = false;
            }
            state.0.accept_edits = Some(mode.is_some_and(|mode| mode.equals_str("acceptEdits")));
            // `managedPermissions` (SessionAgentConfigBundle.ts:169-191):
            // `canPrompt: false` disables prompting as policy, and the deny /
            // ask entries become `policy` rules ahead of the session's own.
            if let Some(managed) = config
                .get("managedPermissions")
                .and_then(JsValue::as_object)
            {
                if managed.get("canPrompt") == Some(&JsValue::Bool(false)) {
                    state.0.can_prompt = false;
                    state.0.policy_can_prompt = Some(false);
                }
                for (key, behavior) in [("deny", "deny"), ("ask", "ask")] {
                    let Some(JsValue::Array(entries)) = managed.get(key) else {
                        continue;
                    };
                    let policy: Vec<_> = entries
                        .iter()
                        .filter_map(JsValue::as_string)
                        .map(|entry| {
                            let mut rule =
                                crate::runtime::interaction::coordinator::entry_to_rule(entry);
                            rule.source = "policy".into();
                            rule.behavior = behavior.into();
                            rule
                        })
                        .collect();
                    let rules = if key == "deny" {
                        &mut state.0.rules.deny
                    } else {
                        &mut state.0.rules.ask
                    };
                    rules.splice(0..0, policy);
                }
            }
            if let Some(JsValue::Array(directories)) = config.get("additionalWorkingDirectories") {
                for directory in directories.iter().filter_map(JsValue::as_string) {
                    if !state.0.additional_working_directories.contains(directory) {
                        state
                            .0
                            .additional_working_directories
                            .push(directory.clone());
                    }
                }
            }
        }
        self
    }

    /// The session's user-dialog tools (ProjectSessionRuntimeBundle.ts:
    /// 328-338), bound to its gateway user-dialog channel.
    pub(super) fn with_user_dialog(
        mut self,
        kinds: Vec<&'static str>,
        channel: crate::user_dialog::GatewayUserDialogChannel,
    ) -> Self {
        if !kinds.is_empty() {
            self.user_dialog = Some((kinds, channel));
        }
        self
    }

    /// The subagent family (`oneShotSubagentPort` and the `agent` tool).
    pub(super) fn with_subagents(mut self, subagents: super::subagents::SubagentRuntime) -> Self {
        self.subagents = Some(subagents);
        self
    }

    /// The agent this session's continuation tools act for.
    pub(super) fn with_continuation_parent(
        mut self,
        parent: super::continuations::ParentRef,
    ) -> Self {
        self.continuation = Some(parent);
        self
    }

    fn fork_seed(&self) -> ForkSeed {
        ForkSeed {
            project: self.project.clone(),
            env: self.env.clone(),
            context: self.binding.context.clone(),
            interactions: self.interactions.clone(),
            permission: self.permission.clone(),
            lsp: self.lsp.clone(),
            mcp: self.mcp.clone(),
            world: self.world.clone(),
            events: self.events.clone(),
            subagents: self.subagents.clone(),
            continuation: self.continuation.clone(),
            file_history: self.file_history.clone(),
            elicitation_timeout: self.elicitation_timeout,
        }
    }

    /// The project generation's event buffer (`eventEmitter` of the tool
    /// runtime).
    pub fn with_events(
        mut self,
        events: crate::runtime::project::interaction_events::InteractionEvents,
    ) -> Self {
        self.events = Some(events);
        self
    }

    /// Where a tool-requested permission mode change is published for the
    /// session's next turn (the gateway's live permission-mode registry).
    pub fn with_permission_mode_transition(
        self,
        transition: Arc<dyn Fn(JsString, Option<JsString>) + Send + Sync>,
    ) -> Self {
        let _ = self.permission.2.set(transition);
        self
    }

    /// The session's gateway permission prompt (`SessionInteractionBundle`
    /// with a gateway answerer): `emit` reaches the active `submit_turn`
    /// stream, `rules` is the session rule set the decisions read.
    pub fn with_permission_prompt(
        self,
        emit: super::permission_prompt::GatewayEmitSlot,
        rules: crate::config::permission::RuleSetHandle,
        timeout: Duration,
    ) -> Self {
        let _ = self
            .permission
            .1
            .set(Arc::new(super::permission_prompt::PermissionPrompt {
                session: self.binding.context.session_key.clone(),
                coordinator: self.interactions.clone(),
                emit,
                rules,
                timeout,
            }));
        self
    }

    /// The session's hooks (`SessionInteractionBundle`): `settings` run in the
    /// session's lifecycle runtimes, the tool runtime dispatches the tool
    /// events through the same runtime, and every execution transition is
    /// projected onto the gateway stream (`sdk`: `includeHookEvents`). With
    /// SDK session hooks, an async output naming an invocation id is
    /// registered with the gateway's deferred-result registry
    /// (SessionInteractionBundle.ts:146-157).
    pub(super) fn with_hooks(
        mut self,
        settings: crate::hooks::protocol::HooksSettings,
        emit: super::permission_prompt::GatewayEmitSlot,
        sdk: bool,
        async_registry: Option<super::hooks::AsyncRegistrySlot>,
    ) -> Self {
        let session = self.binding.context.session_key.clone();
        self.tool_hooks.configure(&settings);
        self.hooks = settings;
        self.hook_observers = crate::runtime::hooks::runtime::HookObservers {
            execution: Some(Arc::new(super::hooks::GatewayHookProjection {
                session: session.clone(),
                emit,
                sdk,
            })),
            async_registered: async_registry.map(|registry| {
                Arc::new(super::hooks::GatewayAsyncHooks {
                    session,
                    registry,
                    include_hook_events: sdk,
                }) as Arc<dyn crate::runtime::hooks::runtime::AsyncHookRegistrationHandler>
            }),
        };
        let _ = self.permission.3.set(self.tool_hooks.clone());
        self
    }

    /// The project's `executionWorld.backgroundTasks`, behind the `task_*`
    /// tools: every session of the project shares one runtime, which the
    /// gateway's `background_tasks` / `background_task_stop` also read.
    #[cfg(unix)]
    pub fn with_background_tasks(
        mut self,
        tasks: crate::runtime::background::BackgroundTaskRuntime,
    ) -> Self {
        self.world.tasks = tasks;
        self
    }

    /// The project's plugin runtime, behind `read_skill`
    /// (`createBuiltinRegistry({ readSkill })`). Without it the tool is not
    /// registered, as in the baseline.
    pub fn with_skills(mut self, plugins: Arc<crate::runtime::plugins::PluginRuntime>) -> Self {
        self.world.skills = Some(plugins);
        self
    }

    /// `applyRunModeOverride` (agentLoopSidecarClient.ts:432-436): the sink
    /// the live session calls with each admitted turn's `runMode`.
    pub fn run_mode_override(&self) -> Arc<dyn Fn(Option<JsString>) + Send + Sync> {
        let slot = self.run_mode.clone();
        Arc::new(move |requested| {
            let mut current = slot.lock().unwrap_or_else(|e| e.into_inner());
            *current = Some(
                requested
                    .or_else(|| current.take())
                    .unwrap_or_else(|| "agent".into()),
            );
        })
    }

    /// #1864 (b): the live permission-mode source, consulted on every turn
    /// before the permission context is emitted. `None` keeps the context's
    /// current mode.
    pub fn with_permission_mode_source(
        mut self,
        source: Arc<dyn Fn() -> Option<JsString> + Send + Sync>,
    ) -> Self {
        self.permission_mode_source = Some(source);
        self
    }

    pub fn turn(
        &self,
        identity: &ExecutionIdentity,
        recorder: Arc<Recorder>,
        signal: Cancellation,
    ) -> SessionPorts {
        // #1864 (b): the effective mode — a non-empty
        // `managedPermissions.defaultMode` wins, else the stored selection —
        // is written back before the context is emitted, so `Permissions::
        // decide` (which clones the current value per call), the emitted
        // `permissionContext` and this turn's local `config.permissionMode`
        // (`tool_context` and `resolve_workspace_path` read that copy) all
        // agree with the sidecar's payload this turn. A `None` source leaves
        // the previous mode and the plan-round-trip stack below untouched.
        let (permission_context, permission_mode) = {
            let mut state = self.permission.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mode) = permission::managed_default_mode(&self.config).or_else(|| {
                self.permission_mode_source
                    .as_ref()
                    .and_then(|source| source())
            }) {
                state.0.mode = crate::config::permission::PermissionMode::from(mode);
            }
            (state.0.to_value(), state.0.mode.as_js_string().clone())
        };
        let mut config = self.config.clone();
        config.insert("env".into(), JsValue::Object(self.env.clone()));
        config.insert("permissionMode".into(), JsValue::String(permission_mode));
        config.insert("permissionContext".into(), permission_context);
        if let Some(run_mode) = self
            .run_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            config.insert("runMode".into(), JsValue::String(run_mode));
        }
        let tools = native_tools(
            self.binding.context.allowed_tools.clone(),
            self.binding.context.disallowed_tools.clone(),
            self.lsp.clone(),
            &self.mcp,
            &self.world,
        );
        if let Some((kinds, channel)) = &self.user_dialog {
            for tool in builtin::request_user::user_dialog_tools(kinds, channel) {
                tools
                    .register(tool)
                    .expect("native user-dialog tool names are unique");
            }
        }
        // `createAgentTool({ maxSubagentDepth, subagentDepth })`: the tool
        // exists wherever the host can fork; its description names what the
        // next child may do under the depth cap.
        let (subagent_depth, max_subagent_depth) = super::subagents::depths(&self.config);
        if self.subagents.is_some() {
            tools
                .register(Arc::new(builtin::agent::AgentTool::new(
                    max_subagent_depth,
                    subagent_depth,
                )))
                .expect("native agent tool name is unique");
        }
        // `SessionSubagentContinuationBundle.attach`: the two continuation
        // tools, bound to the agent this session acts for. Like `agent`, they
        // need the `subagent_fork` capability, so a child under the depth cap
        // does not see them.
        if let (Some(subagents), Some(parent)) = (&self.subagents, &self.continuation) {
            let port: Arc<dyn builtin::subagent_continuation::ContinuationPort> =
                Arc::new(super::continuations::SessionContinuationPort {
                    manager: subagents.continuations.clone(),
                    parent: parent.clone(),
                    runtime: subagents.clone(),
                    seed: self.fork_seed(),
                    config: config.clone(),
                });
            for tool in [
                Arc::new(builtin::subagent_continuation::SubagentTool::new(
                    port.clone(),
                )) as Arc<dyn crate::runtime::tools::ExecutableTool>,
                Arc::new(builtin::subagent_continuation::SendMessageTool::new(port)),
            ] {
                tools
                    .register(tool)
                    .expect("native continuation tool names are unique");
            }
        }
        // A forked child sees a scoped view of that registry.
        let tools = match &self.fork {
            Some(scope) => tools
                .scoped(scope.policy(subagent_depth < max_subagent_depth))
                .expect("a fresh registry is not disposed"),
            None => tools,
        };
        // `createDurableToolPort`: the batch's calls and results are session
        // events around the scheduler's execution.
        let runtime = ToolRuntime::new(tools, self.permission.clone(), SchedulerMode::Concurrent)
            .with_lifecycle(self.tool_hooks.clone());
        let runtime = match self.events.clone() {
            // A refused event (the run was superseded or closed) is dropped,
            // like an emitter whose buffer is gone.
            Some(events) => runtime.with_event_emitter(Arc::new(move |event| {
                let _ = events.emit_current(event);
            })),
            None => runtime,
        };
        // A root session's forks report to its turn buffer. A forked child's
        // own forks report to nobody: the baseline's child dependencies carry
        // no `eventEmitter` (`createSubagentRuntimeComposition`), so a nested
        // fork's lifecycle and activity never reach the client.
        let fork_events: Option<EventSink> = match (&self.fork, self.events.clone()) {
            (None, Some(events)) => {
                let session = identity.session_id.clone();
                Some(Arc::new(move |event| {
                    let _ = events.emit_forked(&session, event);
                }) as EventSink)
            }
            _ => None,
        };
        // The whole fork tree keeps its parent-side references and its
        // sidechains with the root session's storage (the children inherit
        // the root's `subagentTranscript` hooks).
        let fork_recorder = self
            .fork
            .as_ref()
            .map_or_else(|| recorder.clone(), |scope| scope.root_recorder.clone());
        let execution = Arc::new(DurableTools {
            inner: Arc::new(runtime),
            recorder: recorder.clone(),
            projection: self.binding.projection.clone(),
            session: identity.session_id.clone(),
            turn: identity.turn_id.clone(),
            signal: signal.clone(),
        });
        let binding = TurnBinding {
            config: config.clone(),
            input: value([
                ("sessionId", JsValue::String(identity.session_id.clone())),
                ("turnId", JsValue::String(identity.turn_id.clone())),
            ]),
            checkpoint: self.binding.checkpoint.clone(),
            abort_signal: Some(Arc::new(signal.clone())),
            environment: Arc::new(NativeTurnEnvironment(self.env.clone())),
        };
        // `SessionPlanTodoBundle` / `SessionGoalBundle`: both providers sit
        // on the session's transcript writer and its live entry list.
        let plan_todo = crate::runtime::plan_todo::PlanTodoRuntime::new(
            identity.session_id.clone(),
            recorder.clone(),
            self.binding.projection.clone(),
        );
        let goal = crate::runtime::goal::GoalRuntime::new(
            identity.session_id.clone(),
            recorder.clone(),
            self.binding.projection.clone(),
            Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
            }),
            self.goal.clone(),
        );
        let tool_ports = ToolContextPorts {
            clock: Some(Arc::new(|| JsValue::String(SystemExecutionClock.now_iso()))),
            goal: Some(Arc::new(crate::runtime::goal::GoalSessions(goal))),
            plan_files: Some(self.plan_files.clone()),
            // The gateway elicitation channel, inside its durable bracket.
            elicitation: self.permission.1.get().map(|prompt| {
                Arc::new(crate::runtime::tools::ToolElicitation(Arc::new(
                    super::elicitation::GatewayElicitation {
                        session: prompt.session.clone(),
                        coordinator: prompt.coordinator.clone(),
                        emit: prompt.emit.clone(),
                        recorder: recorder.clone(),
                        timeout: self.elicitation_timeout,
                    },
                ))) as capability::RuntimeContext
            }),
            file_history: Some(self.file_history.clone()),
            // The live mode: `enter_plan_mode` / `exit_plan_mode` switch it
            // between two tool batches of one turn.
            permission_mode: Some({
                let permission = self.permission.clone();
                Arc::new(move || {
                    JsValue::String(
                        permission
                            .0
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .0
                            .mode
                            .as_js_string()
                            .clone(),
                    )
                })
            }),
            // `createDurablePermissionAuditRecorder`.
            audit_recorder: Some(Arc::new(crate::runtime::tools::ToolAudit(Arc::new(
                DurablePermissionAudit {
                    recorder: recorder.clone(),
                    session: identity.session_id.clone(),
                    turn: identity.turn_id.clone(),
                },
            )))),
            // `oneShotSubagentPort`: the fork contract of the `agent` tool.
            subagent: self.subagents.as_ref().map(|subagents| {
                subagents.port(
                    self.fork_seed(),
                    super::subagents::ParentTurn {
                        recorder: fork_recorder,
                        config: config.clone(),
                        emit: fork_events,
                    },
                )
            }),
            ..Default::default()
        };
        let sink = Durable {
            recorder,
            session: identity.session_id.clone(),
            turn: identity.turn_id.clone(),
        };
        let message_sink = sink.clone();
        SessionPorts {
            model: RouterModelPortOptions {
                invocation: self.world.invocation.clone(),
                project_path: Some(self.project.to_string_lossy().into_owned().into()),
                agent_model: self
                    .config
                    .get("provider")
                    .and_then(JsValue::as_string)
                    .zip(self.config.get("model").and_then(JsValue::as_string))
                    .map(|(p, m)| (p.clone(), m.clone())),
                agent_max_context_tokens: self.config.get("maxContextTokens").cloned(),
                // `isMainAgent: !config.isSubagent`.
                is_main_agent: self.config.get("isSubagent") != Some(&JsValue::Bool(true)),
                ..RouterModelPortOptions::main_agent()
            },
            recorder: None,
            tool_results_dir: session_tool_results_dir(&self.project, &identity.session_id),
            context_runtime: None,
            context_identity: ContextRequestIdentity::new(binding.clone(), tool_ports.clone()),
            context_config: config,
            tool_execution: execution,
            tool_context: Arc::new(CapabilityContext {
                builder: ToolContextBuilder::new(binding, tool_ports, true),
                plan_todo: Some(Arc::new(crate::runtime::plan_todo::PlanTodoSessions(
                    plan_todo.clone(),
                ))),
            }),
            tool_observer: self.permission.clone(),
            plan_todo: Some(Arc::new(crate::runtime::plan_todo::PlanTodoTurnHandler {
                runtime: plan_todo,
                turn: identity.turn_id.clone(),
            })),
            turn: Default::default(),
            interaction: InteractionTurnInput {
                permission: PermissionTurnOptions {
                    profile: RuntimeProfile::resolve(
                        &JsValue::Undefined,
                        &RuntimeProfileOverrides::default(),
                    )
                    .interaction,
                    context: self
                        .permission
                        .0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                        .clone(),
                    rules: crate::config::permission::RuleSetHandle::empty(),
                    coordinator: self.interactions.clone(),
                    emit: Arc::new(|_| false),
                    timeout: Duration::from_secs(30),
                },
                hooks: self.hooks.clone(),
                contribution: None,
                base_env: Some(self.env.clone()),
                signal,
                steer_recorder: Some(Arc::new(sink)),
                record_message: Some(Arc::new(move |message| message_sink.message(message))),
                messages: vec![],
                switches: CompositionSwitches {
                    plan_todo: false,
                    elicitation_available: None,
                },
                turn_ports: Default::default(),
                hook_wiring: crate::runtime::turn::interaction::HookWiring {
                    observers: self.hook_observers.clone(),
                    turn_sink: Some({
                        let tool_hooks = self.tool_hooks.clone();
                        Arc::new(move |port| tool_hooks.bind(port))
                    }),
                    // `permission_prompt.rs` is this session's gateway
                    // permission callback.
                    host_permission_prompt: true,
                },
            },
        }
    }
}

/// How long a tool batch waits for its assistant message to be durable.
const DURABLE_MESSAGE_WAIT: Duration = Duration::from_secs(2);

/// `createDurableToolPort` (`agent/modules/capability/durableToolPort.ts`):
/// `tool_call` entries before the batch runs and `tool_result` entries (which
/// close the step) after it, unless the turn was aborted meanwhile.
struct DurableTools {
    inner: Arc<ToolRuntime>,
    recorder: Arc<Recorder>,
    projection: Arc<crate::runtime::session::factory::SessionProjection>,
    session: JsString,
    turn: JsString,
    signal: Cancellation,
}
impl DurableTools {
    /// The call as the capability wire carries it, for a batch whose
    /// assistant message never became durable.
    fn bare_call(call: &capability::ToolCall) -> JsValue {
        let mut durable = JsObject::new();
        durable.insert("id".into(), JsValue::String(call.id.clone()));
        durable.insert("name".into(), JsValue::String(call.name.clone()));
        durable.insert("input".into(), call.input.clone());
        JsValue::Object(durable)
    }
    /// The call as the loop holds it: the assistant message's `tool_call`
    /// block (which carries the timeline position) without its block type.
    /// The capability wire only carries id, name and arguments.
    fn durable_call_in(
        projection: &crate::runtime::session::factory::SessionProjection,
        turn: &JsString,
        call: &capability::ToolCall,
    ) -> Option<JsValue> {
        let mut found = None;
        projection.for_each_entry(|entry| {
            if entry.get("type") != Some(&"assistant_message".into())
                || entry.get("turnId") != Some(&JsValue::String(turn.clone()))
            {
                return;
            }
            let Some(JsValue::Array(blocks)) = entry.get("message").and_then(|m| m.get("content"))
            else {
                return;
            };
            for block in blocks {
                if block.get("type") == Some(&"tool_call".into())
                    && block.get("id") == Some(&JsValue::String(call.id.clone()))
                {
                    found = block.as_object().cloned();
                }
            }
        });
        let block = found?;
        let mut durable = JsObject::new();
        for (key, value) in block.entries() {
            if !key.equals_str("type") {
                durable.insert(key.clone(), value.clone());
            }
        }
        Some(JsValue::Object(durable))
    }
}
impl capability::ToolCatalog for DurableTools {
    fn list(&self) -> HostFuture<Vec<Arc<dyn capability::ToolDefinition>>> {
        self.inner.list()
    }
}
impl capability::ToolExecution for DurableTools {
    fn refresh(&self) -> HostFuture<()> {
        self.inner.refresh()
    }
    fn execute_all(
        &self,
        calls: Vec<capability::ToolCall>,
        context: capability::ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let inner = self.inner.clone();
        let signal = self.signal.clone();
        let recorder = self.recorder.clone();
        let (session, turn) = (self.session.clone(), self.turn.clone());
        let projection = self.projection.clone();
        Box::pin(async move {
            // The loop records the assistant message before it runs the
            // message's tool calls (in the baseline both happen in one
            // process, in that order). Here the message arrives as a loop
            // event and is appended on the storage queue while this call is
            // already in flight, so give the append a bounded moment to land:
            // the calls are recorded after it and take their timeline from it.
            let mut waited = Duration::ZERO;
            while waited < DURABLE_MESSAGE_WAIT
                && calls
                    .iter()
                    .any(|call| DurableTools::durable_call_in(&projection, &turn, call).is_none())
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
                waited += Duration::from_millis(2);
            }
            let durable_calls: Vec<JsValue> = calls
                .iter()
                .map(|call| {
                    DurableTools::durable_call_in(&projection, &turn, call)
                        .unwrap_or_else(|| DurableTools::bare_call(call))
                })
                .collect();
            {
                let recorder = recorder.clone();
                let (session, turn) = (session.clone(), turn.clone());
                tokio::task::spawn_blocking(move || {
                    recorder.record_tool_calls(&session, &turn, &durable_calls)
                })
                .await
                .map_err(|e| ModuleError::new(e.to_string()))?
                .map_err(|e| ModuleError::new(e.to_string()))?;
            }
            let results = inner.execute_all(calls, context).await?;
            // An aborted turn ends before its tool results are projected; a
            // late scheduler result is not a settled tool outcome.
            if signal.is_cancelled() {
                return Ok(results);
            }
            let durable: Vec<JsValue> = results.iter().cloned().map(JsValue::Object).collect();
            tokio::task::spawn_blocking(move || {
                recorder.record_tool_results(&session, &turn, &durable)
            })
            .await
            .map_err(|e| ModuleError::new(e.to_string()))?
            .map_err(|e| ModuleError::new(e.to_string()))?;
            Ok(results)
        })
    }
}

/// `createDurablePermissionAuditRecorder`: permission decisions as session
/// events at the turn's open step. The tool input is deliberately not kept.
struct DurablePermissionAudit {
    recorder: Arc<Recorder>,
    session: JsString,
    turn: JsString,
}
impl crate::runtime::tools::ToolAuditRecorder for DurablePermissionAudit {
    fn record_permission(&self, kind: &'static str, fields: JsObject) -> HostFuture<()> {
        let recorder = self.recorder.clone();
        let (session, turn) = (self.session.clone(), self.turn.clone());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut draft = value([
                    ("type", kind.into()),
                    (
                        "step",
                        JsValue::Number(recorder.current_step(&turn)? as f64),
                    ),
                ]);
                for (key, value) in fields.entries() {
                    draft.insert(key.clone(), value.clone());
                }
                Ok::<_, crate::storage::recorder::RecorderError>(
                    recorder
                        .writer()
                        .record_session_event(&session, &turn, draft)?,
                )
            })
            .await
            .map_err(|e| ModuleError::new(e.to_string()))?
            .map_err(|e| ModuleError::new(e.to_string()).into())
        })
    }
}

struct LspTool {
    service: Arc<super::families::DefaultLsp>,
    fields: JsObject,
}
impl LspTool {
    fn new(service: Arc<super::families::DefaultLsp>) -> Self {
        Self { service, fields: value([
            ("name", "lsp".into()), ("kind", "filesystem".into()),
            ("description", "Query a language server for precise definitions, references, implementations, or hover information. Coordinates are one-based UTF-16 positions.".into()),
            ("inputSchema", pilotdeck_protocol::parse_json(r#"{"type":"object","required":["operation","file_path","line","character"],"additionalProperties":false,"properties":{"operation":{"type":"string","enum":["goToDefinition","findReferences","goToImplementation","hover"]},"file_path":{"type":"string"},"line":{"type":"integer"},"character":{"type":"integer"}}}"#).expect("native LSP schema")),
        ]) }
    }
}
impl capability::ToolDefinition for LspTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}
impl crate::runtime::tools::ExecutableTool for LspTool {
    fn validate_input(
        &self,
        input: JsValue,
        _: crate::runtime::tools::ToolRunContext,
    ) -> crate::runtime::tools::ToolFuture<Option<Vec<JsObject>>> {
        Box::pin(async move {
            for key in ["line", "character"] {
                if !input
                    .get(key)
                    .and_then(JsValue::as_number)
                    .is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n >= 1.0)
                {
                    return Ok(Some(vec![value([
                        ("path", key.into()),
                        ("code", "invalid_schema".into()),
                        (
                            "message",
                            JsValue::String(
                                format!("{key} must be a positive one-based integer").into(),
                            ),
                        ),
                    ])]));
                }
            }
            if input
                .get("file_path")
                .and_then(JsValue::as_string)
                .is_none_or(|s| {
                    crate::metadata::js_trim(&String::from_utf16_lossy(s.units())).is_empty()
                })
            {
                return Ok(Some(vec![value([
                    ("path", "file_path".into()),
                    ("code", "invalid_schema".into()),
                    ("message", "file_path must not be empty".into()),
                ])]));
            }
            Ok(None)
        })
    }
    fn execute(
        &self,
        input: JsValue,
        context: crate::runtime::tools::ToolRunContext,
    ) -> crate::runtime::tools::ToolFuture<crate::runtime::tools::ToolOutput> {
        let service = self.service.clone();
        Box::pin(async move {
            let text = |v: Option<&JsValue>| {
                v.and_then(JsValue::as_string)
                    .map(|s| String::from_utf16_lossy(s.units()))
                    .unwrap_or_default()
            };
            let cwd = text(context.fields.get("cwd"));
            if crate::metadata::js_trim(&cwd).is_empty() {
                return Err(crate::runtime::tools::ToolError::new(
                    "LSP_WORKSPACE_REQUIRED",
                    "LSP tool requires a workspace cwd",
                ));
            }
            let result = service.query(
                &cwd,
                &text(input.get("file_path")),
                input
                    .get("line")
                    .and_then(JsValue::as_number)
                    .unwrap_or(f64::NAN)
                    - 1.0,
                input
                    .get("character")
                    .and_then(JsValue::as_number)
                    .unwrap_or(f64::NAN)
                    - 1.0,
            );
            match result {
                Err(error) => Err(crate::runtime::tools::ToolError::new(
                    error
                        .js_error()
                        .and_then(|e| e.code.clone())
                        .unwrap_or_else(|| "tool_execution_failed".into()),
                    error.message,
                )),
                Ok(_) => Err(crate::runtime::tools::ToolError::new(
                    "LSP_UNAVAILABLE",
                    "No native LSP provider is registered.",
                )),
            }
        })
    }
}

struct Permissions(
    Mutex<(
        PermissionContext,
        Option<crate::config::permission::PermissionMode>,
    )>,
    /// The gateway answerer, when the session has one.
    std::sync::OnceLock<Arc<super::permission_prompt::PermissionPrompt>>,
    /// `permissionModes.transition(sessionKey, mode, fallbackBase)`: a tool's
    /// mode change is carried to the session's next turn.
    std::sync::OnceLock<ModeTransition>,
    /// The session's hooks: an `ask` dispatches `PermissionRequest` before the
    /// gateway callback (SessionInteractionBundle.ts:136-145).
    std::sync::OnceLock<Arc<super::hooks::ToolHooks>>,
);
type ModeTransition = Arc<dyn Fn(JsString, Option<JsString>) + Send + Sync>;
struct Tool(
    Arc<dyn capability::ToolDefinition>,
    JsString,
    Option<JsString>,
    /// The call's tool runtime context, for `check_runtime_permissions`.
    capability::RuntimeContext,
);
impl PermissionTool for Tool {
    fn name(&self) -> &JsString {
        &self.1
    }
    fn kind(&self) -> Option<&JsString> {
        self.2.as_ref()
    }
    fn is_read_only(&self, input: &JsValue) -> Result<JsValue, HostError> {
        self.0.is_read_only(input)
    }
    fn check_permissions<'a>(
        &'a self,
        input: &'a JsValue,
        context: &'a PermissionContext,
    ) -> permission::PermissionFuture<'a, Option<JsValue>> {
        match self.0.check_runtime_permissions(input, &self.3) {
            Some(check) => check,
            None => self.0.check_permissions(input, context),
        }
    }
}
impl PermissionDecision for Permissions {
    fn decide(
        &self,
        tool: Arc<dyn capability::ToolDefinition>,
        input: JsValue,
        runtime: capability::RuntimeContext,
        id: JsString,
    ) -> HostFuture<JsValue> {
        let mut context = self.0.lock().unwrap_or_else(|e| e.into_inner()).0.clone();
        let name = tool
            .fields()
            .get("name")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_default();
        let kind = tool
            .fields()
            .get("kind")
            .and_then(JsValue::as_string)
            .cloned();
        let prompt = self.1.get().cloned();
        let hooks = self.3.get().cloned();
        let run = runtime
            .clone()
            .downcast::<crate::runtime::tools::ToolRunContext>()
            .ok();
        // The call's own permission context decides whether a prompt may be
        // shown (`input.canPrompt ?? config.permissionContext.canPrompt`).
        if let Some(can_prompt) = run
            .as_ref()
            .and_then(|run| run.fields.get("permissionContext"))
            .and_then(|permission| permission.get("canPrompt"))
            .and_then(JsValue::as_bool)
        {
            context.can_prompt = can_prompt;
        }
        // The session rule set is live: a remembered approval or a session
        // grant applies to the very next decision.
        // The SDK's `policy` rules stay ahead of it (SessionAgentConfigBundle
        // .ts:181-190).
        if let Some(prompt) = &prompt {
            let policy = |rules: &[crate::config::permission::PermissionRule]| {
                rules
                    .iter()
                    .filter(|rule| rule.source.equals_str("policy"))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            let (deny, ask) = (policy(&context.rules.deny), policy(&context.rules.ask));
            context.rules = prompt.rules.snapshot();
            context.rules.deny.splice(0..0, deny);
            context.rules.ask.splice(0..0, ask);
        }
        Box::pin(async move {
            let decision = permission::decide(
                &context,
                &Tool(tool, name.clone(), kind, runtime),
                &input,
                &id,
            )
            .await?;
            let asks = decision
                .get("type")
                .and_then(JsValue::as_string)
                .is_some_and(|kind| kind.equals_str("ask"));
            let (Some(run), true) = (run, asks) else {
                return Ok(decision);
            };
            // `ToolRuntime.runTool`: an `ask` goes through the
            // `PermissionRequest` hooks — the configured ones first, the
            // gateway callback (`permission_prompt.rs`) last — and the first
            // hook decision replaces the ask.
            let suggestions = decision
                .get("request")
                .and_then(|request| request.get("options"))
                .cloned();
            let mut answer = None;
            if let Some(hooks) = &hooks {
                let mut extra = JsObject::new();
                extra.insert(
                    "permissionSuggestions".into(),
                    suggestions.clone().unwrap_or(JsValue::Undefined),
                );
                let effects = crate::runtime::tools::ToolLifecycle::dispatch(
                    hooks.as_ref(),
                    "PermissionRequest",
                    &name,
                    crate::runtime::tools::tool_event_payload(&name, &id, &input, extra),
                    &run,
                )
                .await?
                .map(|result| result.effects)
                .unwrap_or_default();
                answer = effects.into_iter().find_map(|effect| match effect {
                    crate::runtime::hooks::HookEffect::PermissionRequestResult { result } => {
                        Some(result)
                    }
                    _ => None,
                });
            }
            if let Some(prompt) = prompt {
                let outcome = prompt
                    .request(&name, &id, &input, suggestions.as_ref(), &run)
                    .await;
                answer.get_or_insert(match outcome {
                    super::permission_prompt::PromptOutcome::Allow => {
                        JsValue::Object(value([("behavior", "allow".into())]))
                    }
                    super::permission_prompt::PromptOutcome::Deny(reason) => JsValue::Object(
                        value([("behavior", "deny".into()), ("message", JsValue::String(reason))]),
                    ),
                });
            }
            let Some(answer) = answer else {
                return Ok(decision);
            };
            let message = |text: JsString| {
                JsValue::Object(value([
                    ("type", "runtime".into()),
                    ("message", JsValue::String(text)),
                ]))
            };
            let named = |prefix: &str| {
                JsString::from_utf16(
                    prefix
                        .encode_utf16()
                        .chain(name.units().iter().copied())
                        .chain(".".encode_utf16())
                        .collect(),
                )
            };
            let behavior = answer.get("behavior").and_then(JsValue::as_string);
            Ok(if behavior.is_some_and(|b| b.equals_str("allow")) {
                let mut allowed = value([
                    ("type", "allow".into()),
                    ("reason", message(named("PermissionRequest hook allowed "))),
                ]);
                if let Some(updated) = answer
                    .get("updatedInput")
                    .filter(|updated| !matches!(updated, JsValue::Undefined))
                {
                    allowed.insert("updatedInput".into(), updated.clone());
                }
                JsValue::Object(allowed)
            } else if behavior.is_some_and(|b| b.equals_str("deny")) {
                let reason = answer
                    .get("message")
                    .and_then(JsValue::as_string)
                    .cloned()
                    .unwrap_or_else(|| named("PermissionRequest hook denied "));
                JsValue::Object(value([
                    ("type", "deny".into()),
                    ("reason", message(reason.clone())),
                    ("message", JsValue::String(reason)),
                ]))
            } else {
                decision
            })
        })
    }
}
impl capability::CapabilityResultObserver for Permissions {
    fn on_capability_results(&self, results: Vec<JsObject>) -> HostFuture<()> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for result in results {
            if result.get("type") != Some(&"success".into()) {
                continue;
            }
            let Some(mode) = result
                .get("data")
                .and_then(|v| v.get("requestedMode"))
                .filter(|v| permission::is_permission_mode(v))
                .and_then(JsValue::as_string)
            else {
                continue;
            };
            let mut mode = crate::config::permission::PermissionMode::from(mode.clone());
            if mode.as_js_string().equals_str("plan")
                && !state.0.mode.as_js_string().equals_str("plan")
            {
                state.1 = Some(state.0.mode.clone());
            } else if state.0.mode.as_js_string().equals_str("plan")
                && !mode.as_js_string().equals_str("plan")
                && let Some(previous) = state.1.take()
            {
                mode = previous;
            }
            if let Some(transition) = self.2.get() {
                transition(
                    mode.as_js_string().clone(),
                    state
                        .1
                        .as_ref()
                        .map(|base| base.as_js_string().clone())
                        .or_else(|| Some(state.0.mode.as_js_string().clone())),
                );
            }
            state.0.mode = mode;
        }
        Box::pin(async { Ok(()) })
    }
}
#[derive(Clone)]
struct Durable {
    recorder: Arc<Recorder>,
    session: JsString,
    turn: JsString,
}
impl Durable {
    fn append(&self, turn: JsString, draft: JsObject) -> HostFuture<()> {
        let recorder = self.recorder.clone();
        let session = self.session.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                recorder
                    .writer()
                    .record_session_event(&session, &turn, draft)
            })
            .await
            .map_err(|e| ModuleError::new(e.to_string()))?
            .map_err(|e| ModuleError::new(e.to_string()).into())
        })
    }
    fn message(&self, message: JsValue) -> HostFuture<()> {
        self.append(
            self.turn.clone(),
            value([("type", "durable_message".into()), ("message", message)]),
        )
    }
}
impl MutationRecorder for Durable {
    fn record(&self, turn: JsString, mutation: JsObject) -> HostFuture<()> {
        let mut draft = value([("type", "inbox_mutation".into())]);
        for (key, value) in mutation.entries() {
            draft.insert(key.clone(), value.clone());
        }
        self.append(turn, draft)
    }
}

/// The session's share of the baseline's execution world
/// (`createNodeExecutionWorldBundle`): what the builtin tools close over.
#[derive(Clone)]
pub(super) struct ExecutionWorld {
    /// `executionWorld.backgroundTasks`, behind the `task_*` tools.
    #[cfg(unix)]
    tasks: crate::runtime::background::BackgroundTaskRuntime,
    /// `getURLMarkdownContent` over the process network, behind `web_fetch`.
    fetcher: builtin::web::fetcher::UrlFetcher,
    /// `readSkill: { loader, lister }`: the project's plugin runtime.
    skills: Option<Arc<crate::runtime::plugins::PluginRuntime>>,
    /// The gateway's `extraTools` (the automation providers' tools,
    /// `ProjectExecutionWorldBundle.ts:107-111`), registered after the
    /// builtins.
    extra_tools: Vec<Arc<dyn crate::runtime::tools::ExecutableTool>>,
    /// The session override's `excludeTools`, unregistered from the
    /// composed registry (`SessionToolCompositionBundle.applySessionPolicy`).
    excluded_tools: Vec<JsString>,
    /// The raw provider audit of the session's model calls
    /// (`invocationLogSink` + `workspaceId`), when `PILOTDECK_STORAGE_ROOT`
    /// is set. A fork inherits it (`OneShotSubagentPort.ts:167-169`).
    invocation: Option<crate::host::modules::model_port::InvocationPortAudit>,
}
impl ExecutionWorld {
    pub(super) fn new() -> Self {
        Self {
            #[cfg(unix)]
            tasks: crate::runtime::background::BackgroundTaskRuntime::default(),
            fetcher: builtin::web::fetcher::UrlFetcher::new(Arc::new(
                crate::runtime::model::http::HyperModelTransport::new(),
            )),
            skills: None,
            extra_tools: Vec::new(),
            excluded_tools: Vec::new(),
            invocation: None,
        }
    }
}

/// `resolveInvocationStorageConfig(env)` + `workspaceIdResolver(projectKey)`
/// (createLocalGateway.ts:612-614,1026-1028): the session's audit, keyed by
/// the same project storage id the turn's snapshots use.
fn invocation_audit(
    options: &Options,
    env: &crate::config::Env,
    project: &std::path::Path,
) -> Option<crate::host::modules::model_port::InvocationPortAudit> {
    use crate::runtime::snapshot::SnapshotProjectResolver;
    let config = crate::storage::invocation_log::resolve_invocation_storage_config(|key| {
        env.get(key).cloned()
    })?;
    let projects =
        crate::runtime::snapshot::SnapshotProjects::new(options.home.clone(), PathBuf::new());
    Some(crate::host::modules::model_port::InvocationPortAudit {
        sink: Arc::new(crate::storage::invocation_log::JsonlInvocationLogSink::new(
            config,
        )),
        workspace_id: projects
            .workspace_id(&project.to_string_lossy())
            .as_str()
            .into(),
    })
}

impl SessionTools {
    /// The gateway's extra tools and the session override's exclusions
    /// (`ProjectSessionRuntimeBundle.ts:294`).
    pub(super) fn with_extra_tools(
        mut self,
        extra: Vec<Arc<dyn crate::runtime::tools::ExecutableTool>>,
        excluded: Vec<JsString>,
    ) -> Self {
        self.world.extra_tools = extra;
        self.world.excluded_tools = excluded;
        self
    }

    /// The session override's `canPrompt` / `bypassAvailable`
    /// (`ProjectSessionFactory.ts:270`, `SessionAgentConfigBundle.ts:183`).
    pub(super) fn with_prompt_policy(
        self,
        can_prompt: Option<bool>,
        bypass_available: Option<bool>,
    ) -> Self {
        {
            let mut state = self.permission.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(can_prompt) = can_prompt {
                state.0.can_prompt = can_prompt;
            }
            if let Some(bypass_available) = bypass_available {
                state.0.bypass_available = bypass_available;
            }
        }
        self
    }
}

pub(super) fn native_tools(
    allowed: Option<Vec<JsString>>,
    denied: Option<Vec<JsString>>,
    lsp: Arc<super::families::DefaultLsp>,
    mcp: &SessionMcpTools,
    world: &ExecutionWorld,
) -> ToolRegistry {
    let tools = ToolRegistry::new(
        None,
        Some(Arc::new(move |tool| {
            let Some(name) = tool.fields().get("name").and_then(JsValue::as_string) else {
                return false;
            };
            allowed.as_ref().is_none_or(|names| names.contains(name))
                && !denied.as_ref().is_some_and(|names| names.contains(name))
        })),
    );
    // `createBuiltinRegistry` (tool/registry/createBuiltinRegistry.ts:128):
    // the catalog builtins. `agent`, `subagent` and `send_message` join per
    // turn in `SessionTools::turn`, bound to the session's subagent ports.
    let mut builtins = builtin::read_only_tools();
    builtins.extend(builtin::write_tools());
    #[cfg(unix)]
    {
        builtins.extend(builtin::shell_tools());
        // `createExecuteCodeTool({ webSearch })`: `web_search` is not
        // registered by this process, so its helper is not offered either.
        builtins.push(Arc::new(builtin::execute_code::ExecuteCodeTool::new(
            builtin::execute_code::ExecuteCodeToolOptions { web_search: false },
        )));
        builtins.extend(builtin::task::task_tools(Some(world.tasks.clone())));
    }
    builtins.push(Arc::new(builtin::web::fetch_tool::WebFetchTool::new(
        builtin::web::fetch_tool::WebFetchToolOptions {
            model: None,
            provider: None,
            model_id: None,
            max_output_tokens: None,
            fetcher: world.fetcher.clone(),
        },
    )));
    builtins.push(Arc::new(
        builtin::structured_output::StructuredOutputTool::new(),
    ));
    builtins.push(Arc::new(
        builtin::ask_user_question::AskUserQuestionTool::new(),
    ));
    // `planMode` defaults on; the loop offers them only to a turn with
    // `allowPlanModeTools: true`.
    builtins.extend(builtin::plan_mode::plan_mode_tools());
    builtins.push(Arc::new(builtin::todo_write::TodoWriteTool::new()));
    builtins.push(Arc::new(builtin::send_attachment::SendAttachmentTool::new()));
    builtins.extend(builtin::goal::goal_tools());
    if let Some(plugins) = &world.skills {
        builtins.push(Arc::new(builtin::read_skill::ReadSkillTool::new(
            plugins.clone(),
        )));
    }
    for tool in builtins {
        tools
            .register(tool)
            .expect("native builtin names are unique");
    }
    for tool in &world.extra_tools {
        let _ = tools.register(tool.clone());
    }
    for name in &world.excluded_tools {
        let _ = tools.unregister(name);
    }
    tools
        .register(Arc::new(LspTool::new(lsp)))
        .expect("native LSP name is unique");
    // #1433: the session's MCP tools join the same registry (and therefore
    // the same allowed/denied policy and permission layer) after the local
    // entries.
    mcp.register(&tools);
    tools
}

// ---------------------------------------------------------------------------
// #1433: session MCP tools in the turn composition
// ---------------------------------------------------------------------------

/// `getSdkMcpServers(sessionKey)` (ProjectRuntimeRegistry.ts:322-325): the
/// session's enabled servers as a `name -> config` map. The disabled entries
/// and the `{ config, enabled }` projection stay behind the read, so the
/// composition below only ever sees what a turn may use.
pub fn enabled_session_mcp_servers(mcp_config: &SdkMcpConfig, session_key: &JsString) -> JsObject {
    let mut servers = JsObject::new();
    for (name, value) in mcp_config.session_mcp_servers(&JsValue::String(session_key.clone())) {
        let enabled = value.get("enabled").cloned().unwrap_or(JsValue::Undefined);
        if !crate::runtime::mcp::sdk::js_truthy(&enabled) {
            continue;
        }
        servers.insert(
            name,
            value.get("config").cloned().unwrap_or(JsValue::Undefined),
        );
    }
    servers
}

/// #1433 (`SessionMcpRuntimeBundle.compose`, the tool half): one session's
/// composed MCP tool set. `shared` holds the shared-runtime lease's tools —
/// registration keeps existing names; `per_session` holds the dedicated
/// runtime's tools — registration replaces same-name entries
/// (SessionMcpRuntimeBundle.ts:61-64, 138-144).
#[derive(Clone, Default)]
pub struct SessionMcpTools {
    shared: Vec<Arc<McpBridgeTool>>,
    per_session: Vec<Arc<McpBridgeTool>>,
}

impl SessionMcpTools {
    /// The empty composition: no session servers, nothing registered.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn shared(&self) -> &[Arc<McpBridgeTool>] {
        &self.shared
    }

    pub fn per_session(&self) -> &[Arc<McpBridgeTool>] {
        &self.per_session
    }

    pub fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.per_session.is_empty()
    }

    /// The registration tail of `compose`: shared-lease tools keep existing
    /// names, per-session tools replace them.
    pub fn register(&self, tools: &ToolRegistry) {
        for tool in &self.shared {
            let name = tool.spec().wire_name.clone();
            if !tools.has(&name) {
                tools
                    .register(tool.clone())
                    .expect("#1433 shared session MCP tool name is unique");
            }
        }
        for tool in &self.per_session {
            let name = tool.spec().wire_name.clone();
            if tools.has(&name) {
                tools
                    .replace(tool.clone())
                    .expect("#1433 per-session MCP tool replaces its own name");
            } else {
                tools
                    .register(tool.clone())
                    .expect("#1433 per-session MCP tool name is unique");
            }
        }
    }
}

/// #1433 (`GatewaySessionResourceLeaseBundle`, the release half): the
/// close-time obligations of one composed session — the per-session runtime
/// registration first, then the shared-runtime lease (the reverse of the
/// composition order).
#[derive(Default)]
pub struct SessionMcpRelease {
    per_session: Option<SessionMcpRuntimeRegistration>,
    shared: Option<ProjectMcpRuntimeLease>,
}
impl SessionMcpRelease {
    /// Release both obligations. The first failure still releases the second,
    /// and both failures surface together.
    pub async fn release(self) -> Result<(), RuntimeError> {
        let mut failures = Vec::new();
        if let Some(registration) = self.per_session
            && let Err(error) = registration.dispose().await
        {
            failures.push(error);
        }
        if let Some(lease) = self.shared
            && let Err(error) = lease.release().await
        {
            failures.push(error);
        }
        match failures.len() {
            0 => Ok(()),
            1 => Err(failures
                .pop()
                .expect("#1433 exactly one MCP release failure")),
            _ => Err(RuntimeError {
                message: format!(
                    "Failed to release session MCP resources: {}",
                    failures
                        .iter()
                        .map(|error| error.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                causes: failures,
                js_error: None,
            }),
        }
    }
}

/// #1433: inputs of [`session_mcp_tools`] (`SessionMcpRuntimeBundleOptions`).
pub struct SessionMcpToolsOptions<'a> {
    /// The SDK-owned session server table (`set_mcp_servers` commits here).
    pub mcp_config: &'a SdkMcpConfig,
    /// `contributions.mcpServers`: the servers the session's plugin
    /// generation declares. The SDK-set servers are spread over them
    /// (`{ ...contributions.mcpServers, ...sdkMcpServers }`,
    /// ProjectSessionRuntimeBundle.ts:279).
    pub plugin_servers: JsObject,
    pub provider: &'a ProjectMcpRuntimeProvider,
    /// The session factory's per-session registry; gateway disposal drains
    /// it (`ProjectSessionFactory.mcp`).
    pub per_session_runtimes: &'a SessionMcpRuntimeRegistry,
    pub session_key: &'a JsString,
    /// `runtime.snapshot.config.gateway.maxPerSessionMcpInstances ?? 5` —
    /// the snapshot's already-normalized f64 (finite, floored at 1 by
    /// `config/snapshot.rs`), compared against the registry size without
    /// truncation: the TS check is `size >= maxPerSessionInstances`
    /// (SessionMcpRuntimeBundle.ts:70), where a cap of 2.5 still admits the
    /// third registration (`2 >= 2.5` is false).
    pub max_per_session_instances: f64,
    /// `onDiagnostic`; production writes to stderr.
    pub on_diagnostic: Option<McpDiagnosticSink>,
}

/// #1433: `SessionMcpRuntimeBundle.compose()` — build one session's MCP tool
/// set from the enabled session server table. This is the production read
/// face of `SdkMcpConfig::session_mcp_servers` (#1433): shared servers lease
/// the project runtime (`acquireSharedRuntime`), per-session stdio servers
/// start a dedicated runtime under the session registry. Per-session start
/// failures degrade to diagnostics (the session keeps the shared tools); only
/// a non-active provider or registry fails the caller.
pub async fn session_mcp_tools(
    options: SessionMcpToolsOptions<'_>,
) -> Result<(SessionMcpTools, SessionMcpRelease), RuntimeError> {
    let session_key = String::from_utf16_lossy(options.session_key.units());
    let servers = enabled_session_mcp_servers(options.mcp_config, options.session_key);
    // `{ ...contributions.mcpServers, ...sdkMcpServers }`: an SDK-set server
    // replaces a plugin server of the same name in place.
    let servers = {
        let mut merged = options.plugin_servers;
        for (name, config) in servers.own_entries() {
            merged.insert(name.clone(), config.clone());
        }
        merged
    };
    let diagnostic = |message: String, error: Option<RuntimeError>| {
        if let Some(sink) = &options.on_diagnostic {
            sink(message, error);
        }
    };
    // `compose()` acquires the shared runtime before anything per-session;
    // the lease travels with the release half whatever happens next.
    let shared_lease = options.provider.acquire_shared_runtime(&servers).await?;
    let tools = SessionMcpTools {
        shared: shared_lease.tools().to_vec(),
        per_session: Vec::new(),
    };
    let mut release = SessionMcpRelease {
        per_session: None,
        shared: Some(shared_lease),
    };
    let Some(specs) = options.provider.per_session_server_specs(&servers, false)? else {
        return Ok((tools, release));
    };
    let server_list = specs
        .iter()
        .map(|spec| spec.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let limit_diagnostic = || {
        format!(
            "Per-session MCP limit reached ({}). Session {session_key} will not start: {}.",
            options.max_per_session_instances, server_list,
        )
    };
    // The TS check (SessionMcpRuntimeBundle.ts:70) refuses before the runtime
    // factory runs, so the pre-check stays ahead of `create_runtime`.
    if options.per_session_runtimes.size() as f64 >= options.max_per_session_instances {
        diagnostic(limit_diagnostic(), None);
        return Ok((tools, release));
    }
    let runtime = options.provider.create_runtime(specs);
    // #2442: the pre-check above and the insertion below are two separate
    // lock acquisitions; another session's concurrent prepare can register
    // between them, so the cap is re-checked and the entry inserted in one
    // critical section (`register_below_cap`).
    // Registration precedes the start (SessionMcpRuntimeBundle.ts:119-125):
    // even a failed start is disposed at close, never leaked.
    let registration = match options.per_session_runtimes.register_below_cap(
        &session_key,
        runtime.clone(),
        options.max_per_session_instances,
    ) {
        Ok(Some(registration)) => registration,
        Ok(None) => {
            diagnostic(limit_diagnostic(), None);
            return Ok((tools, release));
        }
        Err(error) => return Err(error),
    };
    release.per_session = Some(registration);
    match runtime.start().await {
        Ok(statuses) => {
            for status in statuses {
                if status.status == McpStatus::Error {
                    diagnostic(
                        format!(
                            "{} (server={}, session={session_key}): {}",
                            if status.server_id == "funasr" {
                                "ASR unavailable"
                            } else {
                                "Per-session MCP unavailable"
                            },
                            status.server_id,
                            status.error.as_deref().unwrap_or("unknown error"),
                        ),
                        None,
                    );
                }
            }
        }
        Err(error) => {
            diagnostic(
                format!("Per-session MCP startup failed for {session_key}"),
                Some(error),
            );
            return Ok((tools, release));
        }
    }
    match create_mcp_tool_definitions_from_runtime(runtime, CreateToolDefinitionsOptions::default())
        .await
    {
        Ok(per_session) => Ok((
            SessionMcpTools {
                per_session,
                ..tools
            },
            release,
        )),
        Err(error) => {
            diagnostic(
                format!("Per-session MCP startup failed for {session_key}"),
                Some(error),
            );
            Ok((tools, release))
        }
    }
}

/// A configured string `runMode`, the initial host run mode.
fn config_run_mode(config: &JsObject) -> Option<JsString> {
    config.get("runMode").and_then(JsValue::as_string).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::mcp::{
        client::{McpCallOptions, McpClientError, McpToolCallResult},
        protocol::{McpServerSpec, McpToolSpec},
        registry::{BuiltinMcpPaths, ProjectMcpRuntimeProviderOptions},
        runtime::{McpClientStatusEntry, McpFuture, McpRuntimePort, McpRuntimeServerInfo},
        sdk::{
            SdkMcpConfigOptions, SdkMcpEnvironment, SdkMcpError, SdkMcpFuture, SdkMcpProject,
            SdkMcpProjectResolver, SdkMcpSessionPlugins, SystemEnvironment,
        },
    };

    fn wire_name(tool: &McpBridgeTool) -> String {
        String::from_utf16_lossy(tool.spec().wire_name.units())
    }

    // ── SdkMcpConfig harness (the sdk_mcp.rs external-test shape, in-crate)

    struct EmptyProject;
    impl SdkMcpProject for EmptyProject {
        fn project_root(&self) -> JsString {
            "/pdrs-fixture/project".into()
        }
        fn refresh<'a>(&'a self) -> SdkMcpFuture<'a, Result<(), SdkMcpError>> {
            Box::pin(async { Ok(()) })
        }
        fn snapshot<'a>(
            &'a self,
        ) -> SdkMcpFuture<'a, Vec<Arc<crate::runtime::plugins::LoadedPlugin>>> {
            Box::pin(async { Vec::new() })
        }
    }
    struct StaticResolver;
    impl SdkMcpProjectResolver for StaticResolver {
        fn resolve<'a>(
            &'a self,
            _: Option<&'a JsString>,
        ) -> SdkMcpFuture<'a, Result<Arc<dyn SdkMcpProject>, SdkMcpError>> {
            let project: Arc<dyn SdkMcpProject> = Arc::new(EmptyProject);
            Box::pin(async { Ok(project) })
        }
    }
    struct NoPlugins;
    impl SdkMcpSessionPlugins for NoPlugins {
        fn plugins(&self, _: &JsValue) -> Vec<Arc<crate::runtime::plugins::LoadedPlugin>> {
            Vec::new()
        }
    }
    struct FixtureEnvironment;
    impl SdkMcpEnvironment for FixtureEnvironment {
        fn env(&self, _: &JsString) -> Option<JsString> {
            None
        }
        fn home(&self) -> JsString {
            "/pdrs-fixture/home".into()
        }
    }

    fn test_config() -> Arc<SdkMcpConfig> {
        Arc::new(SdkMcpConfig::new(SdkMcpConfigOptions {
            resolver: Arc::new(StaticResolver),
            close_session: Arc::new(|_, _| Box::pin(async { Ok(()) })),
            session_plugins: Arc::new(NoPlugins),
            permission_rule_sets: Arc::new(
                crate::config::permission::GatewaySessionPermissionRuleSetRegistry::new(),
            ),
            environment: Arc::new(FixtureEnvironment),
            projects: crate::runtime::sdk_session::SdkSessionProjects::shared(),
        }))
    }

    /// `setMcpServersForSdk` input: `(name, per_session)` stdio servers.
    fn set_input(session_key: &str, servers: &[(&str, bool)]) -> JsValue {
        let mut map = JsObject::new();
        for (name, per_session) in servers {
            let mut config = JsObject::new();
            config.insert("type".into(), JsValue::String("stdio".into()));
            config.insert("command".into(), JsValue::String("node".into()));
            if *per_session {
                config.insert("perSession".into(), JsValue::Bool(true));
            }
            map.insert((*name).into(), JsValue::Object(config));
        }
        let mut input = JsObject::new();
        input.insert("sessionKey".into(), JsValue::String(session_key.into()));
        input.insert("servers".into(), JsValue::Object(map));
        JsValue::Object(input)
    }

    fn toggle_input(session_key: &str, server_name: &str, enabled: bool) -> JsValue {
        let mut input = JsObject::new();
        input.insert("sessionKey".into(), JsValue::String(session_key.into()));
        input.insert("serverName".into(), JsValue::String(server_name.into()));
        input.insert("enabled".into(), JsValue::Bool(enabled));
        JsValue::Object(input)
    }

    // ── provider fake (`create_runtime` seam, like the TS bundle's factory)

    struct FakeMcpRuntime {
        label: &'static str,
        tools: Vec<McpToolSpec>,
        start_fails: bool,
        events: Arc<Mutex<Vec<String>>>,
    }
    impl crate::runtime::mcp::runtime::SessionMcpRuntime for FakeMcpRuntime {
        fn stop(&self) -> McpFuture<'_, Result<(), RuntimeError>> {
            let events = self.events.clone();
            let label = self.label;
            Box::pin(async move {
                events
                    .lock()
                    .expect("#1433 events lock")
                    .push(format!("stop:{label}"));
                Ok(())
            })
        }
    }
    impl McpRuntimePort for FakeMcpRuntime {
        fn start(&self) -> McpFuture<'_, Result<Vec<McpClientStatusEntry>, RuntimeError>> {
            if self.start_fails {
                return Box::pin(async { Err(RuntimeError::new("injected start failure")) });
            }
            let statuses = self
                .tools
                .iter()
                .map(|tool| McpClientStatusEntry {
                    server_id: String::from_utf16_lossy(tool.server_id.units()),
                    status: McpStatus::Ready,
                    error: None,
                })
                .collect();
            Box::pin(async { Ok(statuses) })
        }
        fn list_all_tools(&self) -> McpFuture<'_, Result<Vec<McpToolSpec>, RuntimeError>> {
            let tools = self.tools.clone();
            Box::pin(async { Ok(tools) })
        }
        fn server_info(&self, _: &str) -> Option<McpRuntimeServerInfo> {
            Some(McpRuntimeServerInfo {
                transport: "stdio",
                cwd: None,
            })
        }
        fn call_tool<'a>(
            &'a self,
            _: &'a str,
            _: &'a JsString,
            _: &'a JsValue,
            _: McpCallOptions,
        ) -> McpFuture<'a, Result<McpToolCallResult, McpClientError>> {
            Box::pin(async {
                Err(McpClientError::new(
                    "fake",
                    None,
                    "the fake runtime does not call tools",
                ))
            })
        }
    }

    /// #1433 fix-r1 (review P1-2) probe runtime: `stop()` parks on a
    /// one-shot gate, standing in for a slow child shutdown. The label is
    /// pushed before the gate is awaited, so a test can wait for the stop to
    /// have parked.
    struct GatedRuntime {
        label: &'static str,
        gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        events: Arc<Mutex<Vec<String>>>,
    }
    impl GatedRuntime {
        fn new(
            label: &'static str,
            gate: tokio::sync::oneshot::Receiver<()>,
            events: Arc<Mutex<Vec<String>>>,
        ) -> Self {
            Self {
                label,
                gate: Mutex::new(Some(gate)),
                events,
            }
        }
    }
    impl crate::runtime::mcp::runtime::SessionMcpRuntime for GatedRuntime {
        fn stop(&self) -> McpFuture<'_, Result<(), RuntimeError>> {
            let gate = self.gate.lock().expect("#1433 gate lock").take();
            let events = self.events.clone();
            let label = self.label;
            Box::pin(async move {
                events
                    .lock()
                    .expect("#1433 events lock")
                    .push(format!("stop:{label}"));
                if let Some(receiver) = gate {
                    let _ = receiver.await;
                }
                Ok(())
            })
        }
    }
    impl McpRuntimePort for GatedRuntime {
        fn start(&self) -> McpFuture<'_, Result<Vec<McpClientStatusEntry>, RuntimeError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn list_all_tools(&self) -> McpFuture<'_, Result<Vec<McpToolSpec>, RuntimeError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn server_info(&self, _: &str) -> Option<McpRuntimeServerInfo> {
            None
        }
        fn call_tool<'a>(
            &'a self,
            _: &'a str,
            _: &'a JsString,
            _: &'a JsValue,
            _: McpCallOptions,
        ) -> McpFuture<'a, Result<McpToolCallResult, McpClientError>> {
            Box::pin(async {
                Err(McpClientError::new(
                    "gated",
                    None,
                    "the gated runtime does not call tools",
                ))
            })
        }
    }

    fn fake_provider(
        events: Arc<Mutex<Vec<String>>>,
        start_fails: bool,
    ) -> Arc<ProjectMcpRuntimeProvider> {
        let create_runtime = Arc::new(move |specs: Vec<McpServerSpec>| {
            let per_session = specs.iter().any(|spec| {
                matches!(
                    spec.transport,
                    crate::runtime::mcp::protocol::McpTransportSpec::Stdio {
                        per_session: true,
                        ..
                    }
                )
            });
            let runtime: Arc<dyn McpRuntimePort> = Arc::new(FakeMcpRuntime {
                label: if per_session { "per-session" } else { "shared" },
                tools: specs
                    .iter()
                    .map(|spec| McpToolSpec {
                        server_id: spec.id.as_str().into(),
                        tool_name: "echo".into(),
                        wire_name: format!("mcp__{}__echo", spec.id).into(),
                        description: "fake echo".into(),
                        input_schema: pilotdeck_protocol::parse_json("{}")
                            .expect("#1433 fake tool schema"),
                        annotations: None,
                        meta: None,
                    })
                    .collect(),
                start_fails,
                events: events.clone(),
            });
            runtime
        });
        Arc::new(ProjectMcpRuntimeProvider::new(
            ProjectMcpRuntimeProviderOptions {
                project_root: "/pdrs-fixture/project".into(),
                pilot_home: "/pdrs-fixture/home".into(),
                create_runtime: Some(create_runtime),
                proxy: None,
                on_diagnostic: None,
                environment: Arc::new(SystemEnvironment),
                builtin_paths: BuiltinMcpPaths::default(),
            },
        ))
    }

    async fn compose(
        config: &Arc<SdkMcpConfig>,
        provider: &Arc<ProjectMcpRuntimeProvider>,
        runtimes: &SessionMcpRuntimeRegistry,
        session_key: &str,
        max_per_session_instances: f64,
        on_diagnostic: Option<crate::runtime::mcp::registry::McpDiagnosticSink>,
    ) -> Result<(SessionMcpTools, SessionMcpRelease), RuntimeError> {
        session_mcp_tools(SessionMcpToolsOptions {
            mcp_config: config,
            plugin_servers: JsObject::new(),
            provider,
            per_session_runtimes: runtimes,
            session_key: &JsString::from(session_key),
            max_per_session_instances,
            on_diagnostic,
        })
        .await
    }

    /// `getSdkMcpServers` (#1433): only enabled entries, projected to their
    /// configs, in insertion order.
    #[tokio::test]
    async fn enabled_session_mcp_servers_projects_only_enabled_configs() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input("s", &[("a", false), ("b", false)]))
            .await
            .expect("#1433 fixture servers commit");
        config
            .toggle_mcp_server(&toggle_input("s", "b", false))
            .await
            .expect("#1433 fixture server b disabled");
        let servers = enabled_session_mcp_servers(&config, &"s".into());
        let projected = pilotdeck_protocol::stringify_json(&JsValue::Object(servers))
            .expect("#1433 servers serialize")
            .expect("#1433 servers are not undefined");
        crate::negative_assert_eq!(
            projected,
            r#"{"a":{"type":"stdio","command":"node"}}"#,
            "#1433 enabled session servers project to their configs"
        );
    }

    /// `SessionMcpRuntimeBundle.compose` (#1433): shared servers lease the
    /// project runtime, per-session stdio servers start under the session
    /// registry, and the release disposes the registration.
    #[tokio::test]
    async fn session_mcp_tools_composes_shared_and_per_session_tools() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input(
                "s",
                &[("shared-echo", false), ("private-echo", true)],
            ))
            .await
            .expect("#1433 fixture servers commit");
        let provider = fake_provider(Arc::new(Mutex::new(Vec::new())), false);
        let runtimes = SessionMcpRuntimeRegistry::new();
        let (tools, release) = crate::test_negative_receipt::expect_ok(
            compose(&config, &provider, &runtimes, "s", 5.0, None).await,
            "#1433 composition succeeds",
        );
        crate::negative_assert!(
            tools
                .shared()
                .iter()
                .any(|tool| wire_name(tool) == "mcp__shared-echo__echo"),
            "#1433 shared session server tool composed"
        );
        crate::negative_assert!(
            tools
                .per_session()
                .iter()
                .any(|tool| wire_name(tool) == "mcp__private-echo__echo"),
            "#1433 per-session server tool composed"
        );
        crate::negative_assert_eq!(runtimes.size(), 1, "#1433 per-session runtime registered");
        release.release().await.expect("#1433 composition release");
        crate::negative_assert_eq!(
            runtimes.size(),
            0,
            "#1433 per-session runtime disposed on release"
        );
    }

    /// `GatewaySessionResourceLeaseBundle` (#1433): the per-session runtime
    /// stops before the shared lease on release.
    #[tokio::test]
    async fn session_mcp_release_stops_per_session_before_shared() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input(
                "s",
                &[("shared-echo", false), ("private-echo", true)],
            ))
            .await
            .expect("#1433 fixture servers commit");
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let provider = fake_provider(events.clone(), false);
        let (_, release) = crate::test_negative_receipt::expect_ok(
            compose(
                &config,
                &provider,
                &SessionMcpRuntimeRegistry::new(),
                "s",
                5.0,
                None,
            )
            .await,
            "#1433 composition succeeds",
        );
        release.release().await.expect("#1433 composition release");
        crate::negative_assert_eq!(
            events.lock().expect("#1433 events lock").clone(),
            vec!["stop:per-session".to_owned(), "stop:shared".to_owned()],
            "#1433 release order: per-session MCP before shared MCP"
        );
    }

    /// #1433 fix-r1 (review P1-2): a slow per-session stop (waiting on its
    /// child) must not hold any lock the concurrent registry and provider
    /// disposal needs. The `released: Mutex<bool>` guards, the registry's
    /// inner lock and the provider's state lock are all released before their
    /// awaited stops, so a concurrent `dispose()` reaches its own settle wait
    /// instead of parking on a mutex held across the release's await.
    /// #2444: both halves are actually observed. The registry dispose is
    /// awaited while both stops are still gated, and the shared gate only
    /// opens after the shared stop has parked on it — so the provider's
    /// dispose is provably parked on an awaited stop, stays unfinished until
    /// the gate opens, and the provider's state lock is probed during that
    /// parked stop (a lock held across the stop's await surfaces as the
    /// probe's timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_mcp_release_stays_concurrent_with_registry_and_provider_dispose() {
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let runtimes = SessionMcpRuntimeRegistry::new();
        let (open_per_session, per_session_gate) = tokio::sync::oneshot::channel::<()>();
        let registration = runtimes
            .register(
                "s",
                Arc::new(GatedRuntime::new(
                    "per-session",
                    per_session_gate,
                    events.clone(),
                )),
            )
            .expect("#1433 gated per-session registration");
        runtimes
            .register(
                "t",
                Arc::new(FakeMcpRuntime {
                    label: "draining",
                    tools: Vec::new(),
                    start_fails: false,
                    events: events.clone(),
                }),
            )
            .expect("#1433 concurrent per-session registration");
        let (open_shared, shared_gate) = tokio::sync::oneshot::channel::<()>();
        // The factory must stay `Fn`: the one-shot receiver leaves through an
        // interior-mutability slot, not by moving out of the capture.
        let shared_gate_slot = Arc::new(Mutex::new(Some(shared_gate)));
        let gated_shared = {
            let slot = shared_gate_slot.clone();
            let events = events.clone();
            move |_specs: Vec<McpServerSpec>| {
                Arc::new(GatedRuntime::new(
                    "shared",
                    slot.lock()
                        .expect("#1433 gate slot lock")
                        .take()
                        .expect("#1433 exactly one shared generation"),
                    events.clone(),
                )) as Arc<dyn McpRuntimePort>
            }
        };
        let provider = Arc::new(ProjectMcpRuntimeProvider::new(
            ProjectMcpRuntimeProviderOptions {
                project_root: "/pdrs-fixture/project".into(),
                pilot_home: "/pdrs-fixture/home".into(),
                create_runtime: Some(Arc::new(gated_shared)),
                proxy: None,
                on_diagnostic: None,
                environment: Arc::new(SystemEnvironment),
                builtin_paths: BuiltinMcpPaths::default(),
            },
        ));
        let config = test_config();
        config
            .set_mcp_servers(&set_input("s", &[("shared-echo", false)]))
            .await
            .expect("#1433 fixture servers commit");
        let servers = enabled_session_mcp_servers(&config, &"s".into());
        let lease = crate::test_negative_receipt::expect_ok(
            provider.acquire_shared_runtime(&servers).await,
            "#1433 gated shared lease",
        );
        let release = SessionMcpRelease {
            per_session: Some(registration),
            shared: Some(lease),
        };
        let release_task = tokio::spawn(async move { release.release().await });
        // Wait until the release is parked on the gated per-session stop: the
        // spawned stop pushes its label before awaiting the gate.
        let mut parked = false;
        for _ in 0..400 {
            if events
                .lock()
                .expect("#1433 events lock")
                .iter()
                .any(|event| event == "stop:per-session")
            {
                parked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        crate::negative_assert!(
            parked,
            "#1433 the release parks on the gated per-session stop"
        );
        // While the release is parked, the locks a concurrent dispose needs
        // stay available (the probes run as their own tasks, so a held lock
        // would surface as a timeout, not a blocked worker).
        let probe = {
            let runtimes = runtimes.clone();
            let provider = provider.clone();
            tokio::spawn(async move { (runtimes.size(), provider.shared_generation_count()) })
        };
        let (size, generations) = tokio::time::timeout(std::time::Duration::from_secs(2), probe)
            .await
            .expect("#1433 the registry and provider stay lockable during release")
            .expect("#1433 the lockability probe joins");
        crate::negative_assert_eq!(
            size,
            1,
            "#1433 the parked release has unlinked its own entry already"
        );
        crate::negative_assert_eq!(
            generations,
            1,
            "#1433 the parked release keeps the shared entry for its lease"
        );
        let registry_dispose = {
            let runtimes = runtimes.clone();
            tokio::spawn(async move { runtimes.dispose().await })
        };
        let mut provider_dispose = {
            let provider = provider.clone();
            tokio::spawn(async move { provider.dispose().await })
        };
        // #2444: the registry dispose drains only the "t" entry (the release
        // already unlinked its own), so it must finish while both stops are
        // still gated — awaited before the gates open, making the "not
        // blocked by the parked release" claim an actual observation.
        let registry_result =
            tokio::time::timeout(std::time::Duration::from_secs(2), registry_dispose)
                .await
                .expect("#2444 registry dispose completes while both stops are gated")
                .expect("#2444 the registry dispose task joins");
        crate::test_negative_receipt::expect_ok(registry_result, "#1433 registry dispose returns");
        open_per_session
            .send(())
            .expect("#2444 open the per-session stop gate");
        // #2444: wait for the release to hand its shared lease back — the
        // shared stop pushes its label before awaiting the gate, so from here
        // the provider's dispose settle is parked on an actually-awaited stop.
        let mut shared_parked = false;
        for _ in 0..400 {
            if events
                .lock()
                .expect("#2444 events lock")
                .iter()
                .any(|event| event == "stop:shared")
            {
                shared_parked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        crate::negative_assert!(
            shared_parked,
            "#2444 the shared stop parks on its gate before the provider settle waits"
        );
        // #2444: with the shared stop parked on its gate, the provider dispose
        // cannot have settled — assert it stays unfinished BEFORE opening the
        // gate (300 ms budget).
        crate::negative_assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), &mut provider_dispose)
                .await
                .is_err(),
            "#2444 the provider dispose stays parked at the gated shared stop"
        );
        // #2444: the provider half's observation window — the parked shared
        // stop must not hold the provider's state lock (the probe runs as
        // its own task, so a held lock surfaces as a timeout, not a blocked
        // worker).
        let shared_probe = {
            let provider = provider.clone();
            tokio::spawn(async move { provider.shared_generation_count() })
        };
        let shared_generations =
            tokio::time::timeout(std::time::Duration::from_secs(2), shared_probe)
                .await
                .expect("#2444 the provider stays lockable during the gated shared stop")
                .expect("#2444 the shared-stop lockability probe joins");
        crate::negative_assert_eq!(
            shared_generations,
            1,
            "#2444 the parked shared stop keeps its entry until the stop settles"
        );
        open_shared
            .send(())
            .expect("#2444 open the shared stop gate");
        let released = tokio::time::timeout(std::time::Duration::from_secs(2), release_task)
            .await
            .expect("#1433 the release finishes once the gates open")
            .expect("#1433 the release task joins");
        crate::test_negative_receipt::expect_ok(released, "#1433 gated release returns");
        let provider_result =
            tokio::time::timeout(std::time::Duration::from_secs(2), provider_dispose)
                .await
                .expect("#2444 the provider dispose finishes once the gate opens")
                .expect("#2444 the provider dispose task joins");
        crate::test_negative_receipt::expect_ok(provider_result, "#1433 provider dispose returns");
        crate::negative_assert_eq!(
            events
                .lock()
                .expect("#1433 events lock")
                .iter()
                .filter(|event| event.as_str() == "stop:per-session")
                .count(),
            1,
            "#1433 the per-session stop ran exactly once"
        );
        crate::negative_assert_eq!(
            events
                .lock()
                .expect("#1433 events lock")
                .iter()
                .filter(|event| event.as_str() == "stop:shared")
                .count(),
            1,
            "#1433 the shared stop ran exactly once"
        );
    }

    /// `compose` (#1433): a disabled server never reaches the composition.
    #[tokio::test]
    async fn session_mcp_tools_skips_disabled_servers() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input("s", &[("a", true)]))
            .await
            .expect("#1433 fixture servers commit");
        config
            .toggle_mcp_server(&toggle_input("s", "a", false))
            .await
            .expect("#1433 fixture server disabled");
        let provider = fake_provider(Arc::new(Mutex::new(Vec::new())), false);
        let (tools, release) = crate::test_negative_receipt::expect_ok(
            compose(
                &config,
                &provider,
                &SessionMcpRuntimeRegistry::new(),
                "s",
                5.0,
                None,
            )
            .await,
            "#1433 composition succeeds",
        );
        crate::negative_assert!(
            tools.is_empty(),
            "#1433 a disabled session server composes no tools"
        );
        release.release().await.expect("#1433 composition release");
    }

    /// `compose` (#1433): the per-session limit degrades to a diagnostic and
    /// keeps the shared tools; no runtime is registered past the limit.
    #[tokio::test]
    async fn session_mcp_tools_respects_the_per_session_limit() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input(
                "s",
                &[("shared-echo", false), ("private-echo", true)],
            ))
            .await
            .expect("#1433 fixture servers commit");
        let provider = fake_provider(Arc::new(Mutex::new(Vec::new())), false);
        let runtimes = SessionMcpRuntimeRegistry::new();
        runtimes
            .register(
                "other",
                Arc::new(FakeMcpRuntime {
                    label: "per-session",
                    tools: Vec::new(),
                    start_fails: false,
                    events: Arc::new(Mutex::new(Vec::new())),
                }),
            )
            .expect("#1433 pre-existing per-session runtime");
        let diagnostics: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = diagnostics.clone();
        let (tools, release) = crate::test_negative_receipt::expect_ok(
            compose(
                &config,
                &provider,
                &runtimes,
                "s",
                1.0,
                Some(Arc::new(move |message: String, _: Option<RuntimeError>| {
                    sink.lock().expect("#1433 diagnostics lock").push(message);
                })),
            )
            .await,
            "#1433 composition succeeds",
        );
        crate::negative_assert!(
            tools
                .shared()
                .iter()
                .any(|tool| wire_name(tool) == "mcp__shared-echo__echo"),
            "#1433 the limit keeps the shared tools"
        );
        crate::negative_assert!(
            tools.per_session().is_empty(),
            "#1433 the limit skips the per-session server"
        );
        crate::negative_assert_eq!(
            runtimes.size(),
            1,
            "#1433 the limit registers no per-session runtime"
        );
        crate::negative_assert!(
            diagnostics
                .lock()
                .expect("#1433 diagnostics lock")
                .iter()
                .any(|message| message.starts_with(
                    "Per-session MCP limit reached (1). Session s will not start: private-echo."
                )),
            "#1433 the limit reports the TS diagnostic"
        );
        release.release().await.expect("#1433 composition release");
    }

    /// `compose` (#1433 fix-r1 review P2#4): the limit is the snapshot's f64,
    /// compared without truncation. The TS check is
    /// `perSessionRuntimes.size >= maxPerSessionInstances`
    /// (SessionMcpRuntimeBundle.ts:70), where a cap of 2.5 still admits the
    /// third registration (`2 >= 2.5` is false in Node v22.22.0; the
    /// `as usize` cast used to floor 2.5 to 2 and refuse it).
    #[tokio::test]
    async fn session_mcp_tools_allows_the_third_runtime_under_a_fractional_limit() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input(
                "s",
                &[("shared-echo", false), ("private-echo", true)],
            ))
            .await
            .expect("#1433 fixture servers commit");
        let provider = fake_provider(Arc::new(Mutex::new(Vec::new())), false);
        let runtimes = SessionMcpRuntimeRegistry::new();
        for key in ["other-a", "other-b"] {
            runtimes
                .register(
                    key,
                    Arc::new(FakeMcpRuntime {
                        label: "per-session",
                        tools: Vec::new(),
                        start_fails: false,
                        events: Arc::new(Mutex::new(Vec::new())),
                    }),
                )
                .expect("#1433 pre-existing per-session runtime");
        }
        let (tools, release) = crate::test_negative_receipt::expect_ok(
            compose(&config, &provider, &runtimes, "s", 2.5, None).await,
            "#1433 composition succeeds under a fractional cap",
        );
        crate::negative_assert!(
            tools
                .per_session()
                .iter()
                .any(|tool| wire_name(tool) == "mcp__private-echo__echo"),
            "#1433 a 2.5 cap still admits the third per-session runtime"
        );
        crate::negative_assert_eq!(
            runtimes.size(),
            3,
            "#1433 the fractional cap registers the third runtime"
        );
        // At size 3 the cap binds again (`3 >= 2.5`), reported with the f64.
        let diagnostics: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = diagnostics.clone();
        let (tools, limited_release) = crate::test_negative_receipt::expect_ok(
            compose(
                &config,
                &provider,
                &runtimes,
                "s",
                2.5,
                Some(Arc::new(move |message: String, _: Option<RuntimeError>| {
                    sink.lock().expect("#1433 diagnostics lock").push(message);
                })),
            )
            .await,
            "#1433 composition succeeds at the fractional cap",
        );
        crate::negative_assert!(
            tools.per_session().is_empty(),
            "#1433 the fractional cap skips the per-session server at size 3"
        );
        crate::negative_assert!(
            diagnostics
                .lock()
                .expect("#1433 diagnostics lock")
                .iter()
                .any(|message| message.starts_with(
                    "Per-session MCP limit reached (2.5). Session s will not start: private-echo."
                )),
            "#1433 the fractional cap reports the TS diagnostic with the f64 limit"
        );
        release.release().await.expect("#1433 composition release");
        limited_release
            .release()
            .await
            .expect("#1433 composition release");
        crate::negative_assert_eq!(
            runtimes.size(),
            2,
            "#1433 both releases dispose their own obligations"
        );
    }

    /// #2442: the size pre-check and the registry insertion are separate
    /// lock acquisitions, so a concurrent session's compose can register
    /// between them. The gated factory parks session-a's compose after its
    /// pre-check (size 4 < cap 5) and before its registration; session-b's
    /// compose registers the fifth runtime in the meantime; the sixth must
    /// still be refused (`register_below_cap` re-checks the cap under the
    /// registry's own lock). Unfixed, both register and the size ends at 6.
    /// The owner runs creations for different session keys as independent
    /// tasks (see the factory overlap probe), so the interleave is reachable
    /// from the production path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_mcp_tools_refuses_the_sixth_runtime_when_a_racer_registers_after_the_check() {
        use std::{
            sync::atomic::{AtomicBool, Ordering},
            time::{Duration, Instant},
        };
        let config = test_config();
        for key in ["session-a", "session-b"] {
            config
                .set_mcp_servers(&set_input(
                    key,
                    &[("shared-echo", false), ("private-echo", true)],
                ))
                .await
                .expect("#2442 fixture servers commit");
        }
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let runtimes = SessionMcpRuntimeRegistry::new();
        for key in ["p1", "p2", "p3", "p4"] {
            runtimes
                .register(
                    key,
                    Arc::new(FakeMcpRuntime {
                        label: "per-session",
                        tools: Vec::new(),
                        start_fails: false,
                        events: events.clone(),
                    }),
                )
                .expect("#2442 pre-existing per-session runtime");
        }
        // Session-a's provider: the per-session factory call (private-echo)
        // parks until the racing registration has landed (size reaches 5),
        // holding the compose between its pre-check and its register; the
        // shared call (shared-echo) returns immediately. In production both
        // composes share one provider; only the registry is the shared race
        // surface, so the racer below uses its own provider.
        let entered = Arc::new(AtomicBool::new(false));
        let racing_registry = runtimes.clone();
        let entered_flag = entered.clone();
        let gated_events = events.clone();
        let gated_provider = Arc::new(ProjectMcpRuntimeProvider::new(
            ProjectMcpRuntimeProviderOptions {
                project_root: "/pdrs-fixture/project".into(),
                pilot_home: "/pdrs-fixture/home".into(),
                create_runtime: Some(Arc::new(move |specs: Vec<McpServerSpec>| {
                    let per_session = specs.iter().any(|spec| spec.id == "private-echo");
                    let runtime: Arc<dyn McpRuntimePort> = Arc::new(FakeMcpRuntime {
                        label: if per_session { "per-session" } else { "shared" },
                        tools: specs
                            .iter()
                            .map(|spec| McpToolSpec {
                                server_id: spec.id.as_str().into(),
                                tool_name: "echo".into(),
                                wire_name: format!("mcp__{}__echo", spec.id).into(),
                                description: "fake echo".into(),
                                input_schema: pilotdeck_protocol::parse_json("{}")
                                    .expect("#2442 fake tool schema"),
                                annotations: None,
                                meta: None,
                            })
                            .collect(),
                        start_fails: false,
                        events: gated_events.clone(),
                    });
                    if !per_session {
                        return runtime;
                    }
                    entered_flag.store(true, Ordering::SeqCst);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while racing_registry.size() < 5 {
                        if Instant::now() > deadline {
                            panic!("#2442 the racing registration never landed");
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    runtime
                })),
                proxy: None,
                on_diagnostic: None,
                environment: Arc::new(SystemEnvironment),
                builtin_paths: BuiltinMcpPaths::default(),
            },
        ));
        let diagnostics: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = diagnostics.clone();
        let gated_config = config.clone();
        let gated_runtimes = runtimes.clone();
        let compose_a = tokio::spawn(async move {
            compose(
                &gated_config,
                &gated_provider,
                &gated_runtimes,
                "session-a",
                5.0,
                Some(Arc::new(move |message: String, _: Option<RuntimeError>| {
                    sink.lock().expect("#2442 diagnostics lock").push(message);
                })),
            )
            .await
        });
        // Wait until session-a is parked inside its per-session factory call —
        // past the pre-check (it read size 4) and before its registration.
        let mut parked = false;
        for _ in 0..400 {
            if entered.load(Ordering::SeqCst) {
                parked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        crate::negative_assert!(
            parked,
            "#2442 the gated compose parks between its pre-check and its registration"
        );
        // The racer: a full compose for another session, whose registration
        // is the fifth runtime (its own pre-check still reads 4).
        let racing_provider = fake_provider(events.clone(), false);
        let (racing_tools, racing_release) = crate::test_negative_receipt::expect_ok(
            compose(&config, &racing_provider, &runtimes, "session-b", 5.0, None).await,
            "#2442 the racing compose succeeds",
        );
        crate::negative_assert!(
            racing_tools
                .per_session()
                .iter()
                .any(|tool| wire_name(tool) == "mcp__private-echo__echo"),
            "#2442 the racing compose registers the fifth runtime"
        );
        // The gated compose resumes and must be refused past the cap.
        let gated = tokio::time::timeout(Duration::from_secs(10), compose_a)
            .await
            .expect("#2442 the gated compose finishes once the racer registered")
            .expect("#2442 the gated compose task joins");
        let (gated_tools, gated_release) =
            crate::test_negative_receipt::expect_ok(gated, "#2442 the gated compose returns");
        crate::negative_assert!(
            gated_tools.per_session().is_empty(),
            "#2442 the over-cap compose starts no per-session runtime"
        );
        crate::negative_assert!(
            gated_tools
                .shared()
                .iter()
                .any(|tool| wire_name(tool) == "mcp__shared-echo__echo"),
            "#2442 the refusal keeps the shared tools"
        );
        crate::negative_assert!(
            diagnostics
                .lock()
                .expect("#2442 diagnostics lock")
                .iter()
                .any(|message| message.starts_with(
                    "Per-session MCP limit reached (5). Session session-a will not start: private-echo."
                )),
            "#2442 the refusal reports the TS diagnostic"
        );
        crate::negative_assert_eq!(
            runtimes.size(),
            5,
            "#2442 the racing registration stands and the sixth runtime is refused"
        );
        racing_release
            .release()
            .await
            .expect("#2442 racing release");
        gated_release
            .release()
            .await
            .expect("#2442 the refused compose releases its shared lease");
        crate::negative_assert_eq!(
            runtimes.size(),
            4,
            "#2442 only the four pre-existing runtimes remain"
        );
    }

    /// `compose` (#1433): a per-session start failure degrades to a
    /// diagnostic; the registration is still released at close, never leaked.
    #[tokio::test]
    async fn session_mcp_tools_degrades_per_session_start_failures() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input("s", &[("private-echo", true)]))
            .await
            .expect("#1433 fixture servers commit");
        let provider = fake_provider(Arc::new(Mutex::new(Vec::new())), true);
        let runtimes = SessionMcpRuntimeRegistry::new();
        let diagnostics: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = diagnostics.clone();
        let (tools, release) = crate::test_negative_receipt::expect_ok(
            compose(
                &config,
                &provider,
                &runtimes,
                "s",
                5.0,
                Some(Arc::new(move |message: String, _: Option<RuntimeError>| {
                    sink.lock().expect("#1433 diagnostics lock").push(message);
                })),
            )
            .await,
            "#1433 composition survives the start failure",
        );
        crate::negative_assert!(
            tools.is_empty(),
            "#1433 a failed per-session start composes no tools"
        );
        crate::negative_assert_eq!(
            runtimes.size(),
            1,
            "#1433 the failed start keeps its registration for close"
        );
        crate::negative_assert!(
            diagnostics
                .lock()
                .expect("#1433 diagnostics lock")
                .iter()
                .any(|message| message == "Per-session MCP startup failed for s"),
            "#1433 the start failure reports the TS diagnostic"
        );
        release.release().await.expect("#1433 composition release");
        crate::negative_assert_eq!(
            runtimes.size(),
            0,
            "#1433 the failed start is disposed on release"
        );
    }

    /// `SessionTools::turn` (#1433): the composed MCP tools reach the turn's
    /// tool registry through `native_tools`, next to the builtins and LSP,
    /// under the same allowed/denied policy.
    #[tokio::test]
    async fn native_turn_registry_carries_the_session_mcp_tools() {
        let config = test_config();
        config
            .set_mcp_servers(&set_input("s", &[("a", true)]))
            .await
            .expect("#1433 fixture servers commit");
        let provider = fake_provider(Arc::new(Mutex::new(Vec::new())), false);
        let (tools, release) = crate::test_negative_receipt::expect_ok(
            compose(
                &config,
                &provider,
                &SessionMcpRuntimeRegistry::new(),
                "s",
                5.0,
                None,
            )
            .await,
            "#1433 composition succeeds",
        );
        let name: JsString = "mcp__a__echo".into();
        let registry = native_tools(
            None,
            None,
            Arc::new(super::super::families::DefaultLsp::default()),
            &tools,
            &ExecutionWorld::new(),
        );
        crate::negative_assert!(
            registry.has(&name),
            "#1433 the turn registry carries the session MCP tool"
        );
        crate::negative_assert!(
            registry.has(&"read_file".into()),
            "#1433 the turn registry keeps the read-only builtins"
        );
        let restricted = native_tools(
            Some(vec![name.clone()]),
            None,
            Arc::new(super::super::families::DefaultLsp::default()),
            &tools,
            &ExecutionWorld::new(),
        );
        let listed = restricted
            .list()
            .expect("#1433 restricted tool catalog")
            .len();
        crate::negative_assert_eq!(
            listed,
            1,
            "#1433 the allowlist admits only the session MCP tool"
        );
        crate::negative_assert!(
            restricted.has(&name),
            "#1433 the allowlist admits the session MCP tool"
        );
        let denied = native_tools(
            None,
            Some(vec![name]),
            Arc::new(super::super::families::DefaultLsp::default()),
            &tools,
            &ExecutionWorld::new(),
        );
        crate::negative_assert!(
            !denied.has(&"mcp__a__echo".into()),
            "#1433 the denylist excludes the session MCP tool"
        );
        crate::negative_assert!(
            denied.has(&"read_file".into()),
            "#1433 the denylist keeps the unrelated builtins"
        );
        release.release().await.expect("#1433 composition release");
    }

    fn bridge_tool(wire_name: &str) -> Arc<McpBridgeTool> {
        Arc::new(McpBridgeTool::new(
            McpToolSpec {
                server_id: "a".into(),
                tool_name: "t".into(),
                wire_name: wire_name.into(),
                description: "fixture".into(),
                input_schema: pilotdeck_protocol::parse_json("{}").expect("#1433 fake tool schema"),
                annotations: None,
                meta: None,
            },
            Arc::new(FakeMcpRuntime {
                label: "per-session",
                tools: Vec::new(),
                start_fails: false,
                events: Arc::new(Mutex::new(Vec::new())),
            }),
            CreateToolDefinitionsOptions::default(),
        ))
    }

    /// The registration tail (#1433): shared-lease tools keep existing names,
    /// per-session tools replace them (SessionMcpRuntimeBundle.ts:61-64,
    /// 138-144). `kind: "mcp"` marks a bridge tool; read_file carries
    /// `kind: "filesystem"`, so the field separates the two even though both
    /// definitions also set `maxResultBytes`.
    #[test]
    fn register_keeps_shared_names_and_replaces_per_session_names() {
        let base = native_tools(
            None,
            None,
            Arc::new(super::super::families::DefaultLsp::default()),
            &SessionMcpTools::empty(),
            &ExecutionWorld::new(),
        );
        let is_mcp = |name: &str| {
            base.get(&name.into())
                .expect("#1433 fixture tool present")
                .fields()
                .get("kind")
                .and_then(pilotdeck_protocol::JsValue::as_string)
                .is_some_and(|kind| kind.equals_str("mcp"))
        };
        crate::negative_assert!(
            !is_mcp("read_file"),
            "#1433 the fixture starts on the builtin read tool"
        );
        SessionMcpTools {
            shared: vec![bridge_tool("read_file")],
            per_session: Vec::new(),
        }
        .register(&base);
        crate::negative_assert!(
            !is_mcp("read_file"),
            "#1433 a shared session MCP tool keeps the existing builtin name"
        );
        SessionMcpTools {
            shared: Vec::new(),
            per_session: vec![bridge_tool("read_file")],
        }
        .register(&base);
        crate::negative_assert!(
            is_mcp("read_file"),
            "#1433 a per-session MCP tool replaces the same-name builtin"
        );
    }
}
