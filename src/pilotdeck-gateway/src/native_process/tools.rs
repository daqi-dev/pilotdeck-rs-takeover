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
    /// The last admitted turn's `permissionMode` / `basePermissionMode`
    /// submit options (`applyAgentPermissionOverrides`,
    /// agentLoopSidecarClient.ts:328), applied to the host-side permission
    /// context when the turn's ports are built.
    turn_permission: TurnPermission,
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
    /// The session's `CompactionEngine` failure cooldown.
    compaction_cooldown: crate::runtime::context::compaction::engine::SummaryCooldown,
    /// The session's context-runtime state (prompt-cache generations and
    /// prompt-time anchors): one per live session, like the baseline's
    /// session-composed `DefaultContextRuntime`.
    context_state: Arc<crate::runtime::context::SessionContextState>,
    /// The pilot home whose `permissions.json` each turn reads.
    pilot_home: Option<PathBuf>,
    /// The generation's interaction profile with the session's prompt
    /// admission.
    interaction: super::permission_prompt::SessionInteraction,
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
    /// The parent session's hooks: a child loop shares its parent's
    /// `dependencies.lifecycle`.
    hooks: crate::hooks::protocol::HooksSettings,
    hook_observers: crate::runtime::hooks::runtime::HookObservers,
    interaction: super::permission_prompt::SessionInteraction,
    context_state: Arc<crate::runtime::context::SessionContextState>,
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
        // `mode: definition.permissionMode ?? parent.permissionContext.mode`
        // (SubagentRuntimeComposition.ts:199-202).
        let mut permission = permission;
        if let Some(mode) = &scope.definition.permission_mode {
            permission.mode = mode.as_str().into();
        }
        // The child loop has no plan file manager (its dependencies carry
        // none, SubagentRuntimeComposition.ts:102-130), so its tool context
        // names no plan directory (AgentLoop.ts:2731-2738).
        permission.plan_directory_path = None;
        // `createContext` (ProjectSessionRuntimeBundle.ts:567-581): a
        // definition with `memory: "disabled"` or any `skills` composes the
        // fork's own context runtime.
        let memory_disabled = scope.definition.memory.as_deref() == Some("disabled");
        let own_context = memory_disabled || scope.definition.skills.is_some();
        let definition_skills = scope.definition.skills.clone().flatten();
        let mut world = self.world.for_child_session();
        // `configureTools` (ProjectSessionRuntimeBundle.ts:601-614): a skill
        // list replaces `read_skill` with one over the session's skills that
        // the list names.
        if let Some(names) = &definition_skills {
            world.skills_filter = Some(match &world.skills_filter {
                Some(session) => names
                    .iter()
                    .filter(|name| session.contains(name))
                    .cloned()
                    .collect(),
                None => names.clone(),
            });
        }
        let run_mode = Arc::new(Mutex::new(config_run_mode(&config)));
        // The child runs its loop without an AgentSession or TurnRunner
        // (SubAgentSession.runNative): the session events and
        // UserPromptSubmit are never dispatched for it, the loop and tool
        // events are, through the parent's runtime. The parent's projection
        // drops them (they carry the child's session id).
        let hooks: crate::hooks::protocol::HooksSettings = self
            .hooks
            .into_iter()
            .filter(|(event, _)| {
                !["SessionStart", "Setup", "SessionEnd", "UserPromptSubmit"]
                    .iter()
                    .any(|session_event| event.equals_str(session_event))
            })
            .collect();
        let tool_hooks: Arc<super::hooks::ToolHooks> = Arc::default();
        tool_hooks.configure(&hooks);
        let child_permission_hooks = std::sync::OnceLock::new();
        if !hooks.is_empty() {
            let _ = child_permission_hooks.set(tool_hooks.clone());
        }
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
                child_permission_hooks,
            )),
            lsp: self.lsp,
            mcp: self.mcp,
            permission_mode_source: None,
            run_mode,
            turn_permission: Arc::default(),
            world,
            goal: Arc::default(),
            events: self.events,
            subagents: self.subagents,
            fork: Some(scope),
            continuation: self.continuation,
            file_history: self.file_history,
            user_dialog: None,
            elicitation_timeout: self.elicitation_timeout,
            hooks,
            hook_observers: self.hook_observers,
            tool_hooks,
            compaction_cooldown: Arc::default(),
            // A one-shot subagent without its own context definition runs on
            // the parent's context runtime (`createContext` returns
            // `undefined`), whose state is keyed by the child's session id.
            // `memory: "disabled"` or a `skills` field composes the fork's
            // own context (`definition.memory !== "disabled" &&
            // definition.skills === undefined` is the inheriting case).
            context_state: if own_context {
                Arc::new(
                    self.context_state
                        .scoped_child(crate::runtime::context::ScopedContext {
                            memory_disabled,
                            skills: definition_skills,
                        }),
                )
            } else {
                self.context_state
            },
            // The child's permission context is a copy of the parent's,
            // user rules included; only the root session's turns re-read the
            // settings file.
            pilot_home: None,
            interaction: self.interaction,
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
            turn_permission: Arc::default(),
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
            compaction_cooldown: Arc::default(),
            context_state: Arc::default(),
            pilot_home: Some(options.home.clone()),
            interaction: Default::default(),
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
            hooks: self.hooks.clone(),
            hook_observers: self.hook_observers.clone(),
            interaction: self.interaction,
            context_state: self.context_state.clone(),
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
                interaction: self.interaction,
            }));
        self
    }

    /// `runtime.profile.interaction` and `override?.canPrompt ??
    /// profile.canPrompt` (ProjectSessionFactory.ts:268-271): the session's
    /// permission context starts from that admission
    /// (SessionAgentConfigBundle.ts:175) and the interaction providers follow
    /// the profile. Set before [`Self::with_permission_prompt`].
    pub(super) fn with_interaction(
        mut self,
        profile: crate::runtime::project::InteractionProfile,
        can_prompt_override: Option<bool>,
    ) -> Self {
        let can_prompt = can_prompt_override.unwrap_or(profile.can_prompt);
        self.interaction = super::permission_prompt::SessionInteraction {
            profile,
            can_prompt,
        };
        self.permission
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .can_prompt = profile.can_prompt;
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
        emit_before_terminal: super::permission_prompt::GatewayEmitSlot,
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
                emit_before_terminal,
                sdk,
            })),
            async_registered: async_registry.map(|registry| {
                Arc::new(super::hooks::GatewayAsyncHooks {
                    session,
                    registry,
                    include_hook_events: sdk,
                })
                    as Arc<dyn crate::runtime::hooks::runtime::AsyncHookRegistrationHandler>
            }),
            // Command hooks run on the execution world's (sandboxed) shell.
            shell: self.world.sandbox.confines().then(|| {
                Arc::new(crate::runtime::hooks::exec::LocalShellPort::sandboxed(
                    self.world.sandbox,
                )) as Arc<dyn crate::runtime::hooks::exec::ShellPort>
            }),
        };
        let _ = self.permission.3.set(self.tool_hooks.clone());
        self.tool_hooks
            .configure_session(&self.hooks, &self.hook_observers);
        // `registerSessionHookHandler` (SessionInteractionBundle.ts:301-313):
        // gateway-side observers (ElicitationResult) dispatch on this
        // session's hooks. A replaced or dropped session runs nothing.
        let hooks = Arc::downgrade(&self.tool_hooks);
        let cwd: JsString = self.project.to_string_lossy().as_ref().into();
        let owner = self.binding.context.session_key.clone();
        self.interactions.register_session_hook_handler(
            owner.clone(),
            Arc::new(move |event, payload| {
                if let Some(hooks) = hooks.upgrade() {
                    hooks.dispatch_detached(event, &owner, &cwd, payload);
                }
            }),
        );
        self
    }

    /// `configChangeLifecycle` + `registerConfigChangeHandler`
    /// (SessionInteractionBundle.ts:170-180, 286-299): a session whose SDK
    /// hooks name `ConfigChange` registers a handler the gateway's config
    /// change dispatch calls. The handler holds the session's hooks weakly,
    /// so a dropped session runs nothing. Call after [`Self::with_hooks`],
    /// whose execution projection the runtime shares.
    pub(super) fn with_sdk_config_change(
        self,
        sdk_config: Option<&JsValue>,
        registry: &super::SessionOverrides,
    ) -> Self {
        // `fileChangedLifecycle` (SessionInteractionBundle.ts:181-192): the
        // same session-scoped composition for the SDK `FileChanged` hooks.
        let file_changed = super::hooks::sdk_file_changed_settings(sdk_config);
        if file_changed
            .iter()
            .any(|(_, matchers)| !matchers.is_empty())
        {
            self.tool_hooks
                .configure_file_changed(&file_changed, &self.hook_observers);
        }
        let settings = super::hooks::sdk_config_change_settings(sdk_config);
        if settings.iter().all(|(_, matchers)| matchers.is_empty()) {
            return self;
        }
        let observers = crate::runtime::hooks::runtime::HookObservers {
            execution: self.hook_observers.execution.clone(),
            async_registered: None,
            shell: self.hook_observers.shell.clone(),
        };
        self.tool_hooks
            .configure_config_change(&settings, &observers);
        let hooks = Arc::downgrade(&self.tool_hooks);
        let cwd: JsString = self.project.to_string_lossy().as_ref().into();
        let owner = self.binding.context.session_key.clone();
        registry.register_config_change(
            &owner.clone(),
            Arc::new(move |payload| {
                if let Some(hooks) = hooks.upgrade() {
                    hooks.dispatch_config_change(&owner, &cwd, payload);
                }
            }),
        );
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

    /// `resolveWebSearchOptions(snapshot)` (ProjectExecutionWorldBundle.ts:
    /// 120-133): the project's `tools.webSearch`, over the session network.
    pub fn with_web_search(mut self, web_search: &JsValue) -> Self {
        let env = JsValue::Object(self.env.clone());
        self.world.web_search = builtin::web::search::options_from_config(
            web_search,
            self.world.fetcher.transport.clone(),
        )
        .map(|options| {
            let availability = builtin::web::search::WebSearchTool::new(options.clone())
                .check_availability(Some(&env));
            (options, availability)
        });
        self
    }

    /// `sdkSessionConfig.deferredTools`: hidden from the schema behind
    /// `search_tools`. A name this session does not have fails the session
    /// (`SDK deferred tool <name> is not available in this session.`).
    pub fn with_deferred_tools(mut self, config: Option<&JsValue>) -> Result<Self, RuntimeError> {
        let Some(JsValue::Array(entries)) = config.and_then(|config| config.get("deferredTools"))
        else {
            return Ok(self);
        };
        let requested: Vec<(JsString, Option<JsString>)> = entries
            .iter()
            .filter_map(|entry| {
                let name = entry.get("name").and_then(JsValue::as_string)?.clone();
                let hint = entry
                    .get("searchHint")
                    .and_then(JsValue::as_string)
                    .cloned();
                Some((name, hint))
            })
            .collect();
        if requested.is_empty() {
            return Ok(self);
        }
        self.world.deferred = Some((requested, Default::default()));
        Ok(self)
    }

    /// The deferred-tool check against the session's composed registry.
    pub fn check_deferred_tools(&self) -> Result<(), RuntimeError> {
        let Some((requested, _)) = &self.world.deferred else {
            return Ok(());
        };
        let registry = self.session_registry(&self.config);
        builtin::search_tools::apply_deferred_tools(&registry, requested, &Default::default())
            .map_err(RuntimeError::new)
    }

    /// `composeToolPorts(external, native)` refuses a host tool that shares
    /// a name with the external catalog (runtimePorts.ts:34-40). The
    /// baseline composes when the session runtime is built, so the submit
    /// fails before a turn starts.
    pub fn check_module_tools(&self) -> Result<(), RuntimeError> {
        let Some(external) = &self.world.module_tools else {
            return Ok(());
        };
        let names: Vec<JsString> = external
            .tools()
            .iter()
            .filter_map(|tool| {
                use crate::host::modules::capability::ToolDefinition as _;
                tool.fields()
                    .get("name")
                    .and_then(JsValue::as_string)
                    .cloned()
            })
            .collect();
        let registry = self.session_registry(&self.config);
        let host = registry
            .list()
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let late = self.late_host_tools();
        if let Some(name) = host
            .iter()
            .filter_map(|tool| tool.fields().get("name").and_then(JsValue::as_string))
            .filter(|name| !late.contains(name))
            .find(|name| names.contains(name))
        {
            let mut message: Vec<u16> = "External tool binding conflicts with host tool '"
                .encode_utf16()
                .collect();
            message.extend_from_slice(name.units());
            message.extend("'.".encode_utf16());
            return Err(RuntimeError::new(String::from_utf16_lossy(&message)));
        }
        Ok(())
    }

    /// Host tools attached after the session runtime composed its tool port
    /// (`SessionSubagentContinuationBundle.attach`): the continuation tools.
    fn late_host_tools(&self) -> Vec<JsString> {
        if self.subagents.is_some() && self.continuation.is_some() {
            vec!["subagent".into(), "send_message".into()]
        } else {
            Vec::new()
        }
    }

    /// The project's plugin runtime, behind `read_skill`
    /// (`createBuiltinRegistry({ readSkill })`). Without it the tool is not
    /// registered, as in the baseline.
    pub fn with_skills(mut self, plugins: Arc<crate::runtime::plugins::PluginRuntime>) -> Self {
        self.world.skills = Some(plugins);
        self
    }

    /// `applyRunModeOverride` / `applyAgentPermissionOverrides`
    /// (agentLoopSidecarClient.ts:327-328, 432-436): the sink the live
    /// session calls with each admitted turn's submit options.
    pub fn turn_override(&self) -> crate::runtime::turn::live::TurnOverrideSink {
        let run_mode = self.run_mode.clone();
        let turn_permission = self.turn_permission.clone();
        let permission = self.permission.clone();
        Arc::new(move |options| {
            let requested = options.get("runMode").and_then(JsValue::as_string).cloned();
            {
                let mut current = run_mode.lock().unwrap_or_else(|e| e.into_inner());
                *current = Some(
                    requested
                        .or_else(|| current.take())
                        .unwrap_or_else(|| "agent".into()),
                );
            }
            let mode = |key: &str| {
                options
                    .get(key)
                    .filter(|mode| permission::is_permission_mode(mode))
                    .and_then(JsValue::as_string)
                    .cloned()
            };
            let requested =
                mode("permissionMode").map(|current| (current, mode("basePermissionMode")));
            // The `permissionModeBeforePlan` the turn's config will carry
            // (permissionOverrides.ts:13-16), read off the host state the
            // previous turn left.
            let before_plan = {
                let state = permission.0.lock().unwrap_or_else(|e| e.into_inner());
                match &requested {
                    Some((current, base))
                        if current.equals_str("plan")
                            && !state.0.mode.as_js_string().equals_str("plan") =>
                    {
                        Some(
                            base.clone()
                                .unwrap_or_else(|| state.0.mode.as_js_string().clone()),
                        )
                    }
                    _ => state.1.as_ref().map(|mode| mode.as_js_string().clone()),
                }
            };
            *turn_permission.lock().unwrap_or_else(|e| e.into_inner()) = TurnOptions {
                permission: requested,
                can_elicit: options.get("canElicit") == Some(&JsValue::Bool(true)),
                can_prompt: options
                    .get("canPrompt")
                    .filter(|value| !matches!(value, JsValue::Null | JsValue::Undefined))
                    .cloned(),
                operation_deadline: options
                    .get("execution")
                    .and_then(|execution| execution.get("operationDeadline"))
                    // The gateway sets it as an ISO string (submit.rs).
                    .filter(|value| value.as_string().is_some_and(|text| !text.is_empty()))
                    .cloned(),
            };
            before_plan
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

    /// The session's tool registry before the fork scope: the builtins and
    /// MCP tools, the user-dialog tools, `agent` and the continuation tools.
    fn session_registry(&self, config: &JsObject) -> ToolRegistry {
        let tools = native_tools(
            self.binding.context.allowed_tools.clone(),
            self.binding.context.disallowed_tools.clone(),
            self.lsp.clone(),
            &self.mcp,
            &self.world,
        );
        // `sessionTools.register(createKnowledgeQueryTool(...))`, after the
        // composed MCP / extension tools and before the user-dialog tools.
        if let Some(tool) = &self.world.knowledge {
            let _ = tools.register(tool.clone());
        }
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
        // `attachDefinitionMcpTools`: `registry.registerOrReplace(tool)`.
        if let Some(fork) = &self.fork {
            for tool in &fork.mcp_tools {
                let _ = tools.register_or_replace(tool.clone());
            }
        }
        tools
    }

    pub fn turn(
        &self,
        identity: &ExecutionIdentity,
        recorder: Arc<Recorder>,
        signal: Cancellation,
    ) -> SessionPorts {
        // The effective mode — the turn's own `permissionMode` submit option
        // when the gateway resolved one; otherwise (#1864 (b)) a non-empty
        // `managedPermissions.defaultMode`, else the stored selection —
        // is written back before the context is emitted, so `Permissions::
        // decide` (which clones the current value per call), the emitted
        // `permissionContext` and this turn's local `config.permissionMode`
        // (`tool_context` and `resolve_workspace_path` read that copy) all
        // agree with the sidecar's payload this turn. A `None` source leaves
        // the previous mode and the plan-round-trip stack below untouched.
        let (permission_context, permission_mode) = {
            let mut state = self.permission.0.lock().unwrap_or_else(|e| e.into_inner());
            let turn = self
                .turn_permission
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .permission
                .clone();
            if let Some((mode, base)) = turn {
                // `applyAgentPermissionOverrides` (permissionOverrides.ts:
                // 12-19): entering plan from another mode remembers
                // `basePermissionMode ?? config.permissionMode` as the mode
                // exit_plan_mode returns to.
                let mode = crate::config::permission::PermissionMode::from(mode);
                if mode.as_js_string().equals_str("plan")
                    && !state.0.mode.as_js_string().equals_str("plan")
                {
                    state.1 = Some(
                        base.map(crate::config::permission::PermissionMode::from)
                            .unwrap_or_else(|| state.0.mode.clone()),
                    );
                }
                state.0.mode = mode;
            } else if let Some(mode) =
                permission::managed_default_mode(&self.config).or_else(|| {
                    self.permission_mode_source
                        .as_ref()
                        .and_then(|source| source())
                })
            {
                state.0.mode = crate::config::permission::PermissionMode::from(mode);
            }
            // `permissionRules: permissionSettingsToRuleSet(readPermissionSettings())`
            // (InProcessGateway.ts:1441,1457,1566-1569), applied with
            // `replaceUserRules` before the loop starts.
            if let Some(home) = &self.pilot_home {
                let user = super::permission_settings::read_user_rules(home);
                super::permission_settings::replace_user_rules(&mut state.0.rules, &user);
            }
            (state.0.to_value(), state.0.mode.as_js_string().clone())
        };
        let mut config = self.config.clone();
        config.insert("env".into(), JsValue::Object(self.env.clone()));
        config.insert("permissionMode".into(), JsValue::String(permission_mode));
        // `canPrompt: input.canPrompt ?? config.permissionContext.canPrompt`
        // (sidecarToolContext.ts:192): the tool runtime's prompt gate reads
        // the turn's own choice.
        let mut permission_context = permission_context;
        let turn_can_prompt = self
            .turn_permission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .can_prompt
            .clone();
        if let Some(can_prompt) = turn_can_prompt
            && let Some(context) = permission_context.as_object()
        {
            let mut context = context.clone();
            context.insert("canPrompt".into(), can_prompt);
            permission_context = JsValue::Object(context);
        }
        config.insert("permissionContext".into(), permission_context);
        if let Some(run_mode) = self
            .run_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            config.insert("runMode".into(), JsValue::String(run_mode));
        }
        let (subagent_depth, max_subagent_depth) = super::subagents::depths(&self.config);
        let tools = self.session_registry(&config);
        // `collectDeferredTools` (ProjectSessionRuntimeBundle.ts:348-370):
        // checked when the session is prepared, so it cannot fail here.
        if let Some((requested, revealed)) = &self.world.deferred {
            let _ = builtin::search_tools::apply_deferred_tools(&tools, requested, revealed);
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
        // `context.fileUpdateNotifier`: the session's SDK `FileChanged`
        // hooks, ahead of the runtime's own `FileChanged` dispatch.
        let runtime = match self.tool_hooks.sdk_file_saved(
            self.binding.context.session_key.clone(),
            self.project.to_string_lossy().as_ref().into(),
        ) {
            Some(notifier) => runtime.with_file_update_notifier(notifier),
            None => runtime,
        };
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
        let external_loop = self.world.external_loop;
        // `composeToolPorts(externalPorts.tools, nativeToolPort)`
        // (AgentSessionRuntimeBundle.ts:156-206): an external `modules.tools`
        // binding is listed and routed ahead of the session's own tools.
        let inner: Arc<dyn capability::ToolExecution> = match &self.world.module_tools {
            Some(external) => Arc::new(super::module_tools::ComposedTools {
                external: external.clone(),
                native: Arc::new(runtime),
                // `createDefaultPermissionContext({ acceptEdits: sdk?.permissionMode
                // === "acceptEdits", bypassAvailable: sessionOverride?.bypassAvailable
                // ?? true })` (SessionAgentConfigBundle.ts:182-183): what the
                // baseline's in-process tool context carries.
                host_permission: Some(value([
                    (
                        "bypassAvailable",
                        JsValue::Bool(self.world.bypass_override.unwrap_or(true)),
                    ),
                    (
                        "acceptEdits",
                        JsValue::Bool(
                            config
                                .get("permissionContext")
                                .and_then(|context| context.get("acceptEdits"))
                                == Some(&JsValue::Bool(true)),
                        ),
                    ),
                ])),
                late: self.late_host_tools(),
                external_loop,
            }),
            _ => Arc::new(runtime),
        };
        let execution = Arc::new(DurableTools {
            inner,
            recorder: recorder.clone(),
            projection: self.binding.projection.clone(),
            session: identity.session_id.clone(),
            turn: identity.turn_id.clone(),
            signal: signal.clone(),
            external_loop,
        });
        let binding = TurnBinding {
            config: config.clone(),
            input: {
                let mut input = value([
                    ("sessionId", JsValue::String(identity.session_id.clone())),
                    ("turnId", JsValue::String(identity.turn_id.clone())),
                ]);
                let turn = self
                    .turn_permission
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                // `canElicit: input.canElicit` (sidecarToolContext.ts:105):
                // the tool runtime honours it only with an elicitation port.
                if turn.can_elicit {
                    input.insert("canElicit".into(), JsValue::Bool(true));
                }
                // The turn's `execution.operationDeadline` reaches the tool
                // scheduler's execution context (sidecarToolContext.ts:168,
                // toolSchedulerAdapter.ts), where it narrows the `agent`
                // tool's subagent budget (executionDeadline.ts). A child loop
                // runs without an execution context (SubAgentSession.ts:205).
                if self.fork.is_none()
                    && let Some(deadline) = &turn.operation_deadline
                {
                    input.insert(
                        "execution".into(),
                        JsValue::Object(value([("operationDeadline", deadline.clone())])),
                    );
                }
                drop(turn);
                input
            },
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
        // `capabilities.model.auxiliary.forTurn(...)` (AgentLoop.ts:2740-2790):
        // the tool context's `model`, routed through the session router once
        // the turn's handlers are assembled.
        let auxiliary_model = crate::host::modules::auxiliary_model::AuxiliaryModelSlot::default();
        let tool_ports = ToolContextPorts {
            model: Some(Arc::new(auxiliary_model.clone())
                as Arc<dyn crate::host::modules::tool_context::TurnModel>),
            clock: Some(Arc::new(|| JsValue::String(SystemExecutionClock.now_iso()))),
            goal: Some(Arc::new(crate::runtime::goal::GoalSessions(goal))),
            // A child loop has no plan file manager
            // (SubagentRuntimeComposition.ts:102-130, AgentLoop.ts:2731-2738).
            plan_files: self.fork.is_none().then(|| {
                self.plan_files.clone() as Arc<dyn crate::host::modules::tool_context::PlanFiles>
            }),
            // The gateway elicitation channel, inside its durable bracket.
            // `questionProvider` (SessionInteractionBundle.ts:228-262):
            // `gateway` asks over the session stream, `deterministic`
            // answers locally, `disabled` composes no channel.
            elicitation: self
                .permission
                .1
                .get()
                .filter(|prompt| prompt.interaction.profile.question_provider != "disabled")
                .map(|prompt| {
                    Arc::new(crate::runtime::tools::ToolElicitation(Arc::new(
                        super::elicitation::GatewayElicitation {
                            session: prompt.session.clone(),
                            coordinator: prompt.coordinator.clone(),
                            emit: prompt.emit.clone(),
                            recorder: recorder.clone(),
                            timeout: self.elicitation_timeout,
                            deterministic: (prompt.interaction.profile.question_provider
                                == "deterministic")
                                .then_some(prompt.interaction),
                            hooks: Some((
                                self.tool_hooks.clone(),
                                self.project.to_string_lossy().as_ref().into(),
                            )),
                        },
                    ))) as capability::RuntimeContext
                }),
            // Nor a file history sink: the child's dependencies omit
            // `fileHistory` (SubagentRuntimeComposition.ts:102-130), so a
            // child's writes record no file snapshots.
            file_history: self
                .fork
                .is_none()
                .then(|| self.file_history.clone() as capability::RuntimeContext),
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
                        lifecycle: Some(self.tool_hooks.clone()
                            as Arc<dyn crate::runtime::tools::ToolLifecycle>),
                        recorder: fork_recorder,
                        config: config.clone(),
                        emit: fork_events,
                    },
                )
            }),
            // AgentLoop.ts:2770-2784: with `includeToolProgress` the tool
            // context's `progress` emits `tool_progress` through the loop's
            // event emitter, the same buffer as the runtime's warnings.
            event_emitter: self.events.clone().map(|events| {
                Arc::new(move |event| {
                    let _ = events.emit_current(event);
                }) as EventSink
            }),
            ..Default::default()
        };
        // `TurnRunner`'s `onCompactPersisted` (TurnRunner.ts:338-374): the
        // replacement boundary and the deferred `compaction_completed` land in
        // this session's transcript; a failed write aborts the turn.
        let persist_compaction = crate::host::modules::turn::persist_compaction_to_recorder(
            recorder.clone(),
            identity.session_id.clone(),
            identity.turn_id.clone(),
            {
                let signal = signal.clone();
                Arc::new(move |_reason: &str| signal.abort())
            },
        );
        let sink = Durable {
            recorder,
            session: identity.session_id.clone(),
            turn: identity.turn_id.clone(),
        };
        let message_sink = sink.clone();
        let compaction = crate::runtime::turn::host_factory::CompactionPorts {
            lifecycle: Some(Arc::new(super::hooks::CompactionHooks {
                hooks: self.tool_hooks.clone(),
                session: identity.session_id.clone(),
                cwd: self.project.to_string_lossy().into_owned().into(),
            })),
            // The engine's `eventEmitter` is the session's event buffer; a
            // forked child's composition carries none.
            events: match (&self.fork, self.events.clone()) {
                (None, Some(events)) => Some(Arc::new(move |event: JsValue| {
                    if let Some(event) = event.as_object() {
                        let _ = events.emit_current(event.clone());
                    }
                })
                    as crate::runtime::context::compaction::engine::EventEmitter),
                _ => None,
            },
            cooldown: self.compaction_cooldown.clone(),
        };
        SessionPorts {
            skills: self.world.skills_filter.clone(),
            context_state: Some(self.context_state.clone()),
            auxiliary_model: Some(auxiliary_model),
            compaction,
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
                // `managedModelPolicy: config.managedModelPolicy`
                // (AgentSessionRuntimeBundle.ts:190).
                managed_model_policy: self.config.get("managedModelPolicy").and_then(|policy| {
                    let list = |label: &str| match policy.get(label) {
                        Some(JsValue::Array(entries)) => entries
                            .iter()
                            .filter_map(|entry| entry.as_string().cloned())
                            .collect(),
                        _ => Vec::new(),
                    };
                    policy
                        .as_object()
                        .map(|_| crate::runtime::model::router::ManagedModelPolicy {
                            allow: list("allow"),
                            deny: list("deny"),
                        })
                }),
                // `fallbackModels: config.fallbackModels`
                // (AgentSessionRuntimeBundle.ts:189).
                fallback_models: match self.config.get("fallbackModels") {
                    Some(JsValue::Array(models)) => models
                        .iter()
                        .filter_map(crate::runtime::model::router::RouterModelRef::from_value)
                        .collect(),
                    _ => Vec::new(),
                },
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
            turn: crate::host::modules::turn::Ports::default()
                .with_persist_compaction(persist_compaction),
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
                // `sessionOverride?.cwd` composed as given (SessionAgentConfigBundle.ts:57).
                non_string_cwd: self
                    .config
                    .get("cwd")
                    .filter(|cwd| {
                        !matches!(cwd, JsValue::String(_) | JsValue::Undefined | JsValue::Null)
                    })
                    .cloned(),
                base_env: Some(self.env.clone()),
                signal,
                steer_recorder: Some(Arc::new(sink)),
                record_message: Some(Arc::new(move |message| message_sink.message(message))),
                messages: vec![],
                switches: CompositionSwitches {
                    plan_todo: false,
                    // `interaction: { elicitationAvailable: true }` whenever
                    // the session has an elicitation port
                    // (sidecarHostModulePorts.ts:206-207).
                    elicitation_available: self
                        .permission
                        .1
                        .get()
                        .filter(|prompt| prompt.interaction.profile.question_provider != "disabled")
                        .map(|_| true),
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
                    advisory: self.tool_hooks.session_port(),
                    // Filled in by the live session with the turn's
                    // user-abort signal.
                    advisory_signal: None,
                },
            },
        }
    }
}

/// `createDurableToolPort` (`agent/modules/capability/durableToolPort.ts`):
/// `tool_call` entries before the batch runs and `tool_result` entries (which
/// close the step) after it, unless the turn was aborted meanwhile.
struct DurableTools {
    inner: Arc<dyn capability::ToolExecution>,
    recorder: Arc<Recorder>,
    projection: Arc<crate::runtime::session::factory::SessionProjection>,
    session: JsString,
    turn: JsString,
    signal: Cancellation,
    /// The loop is an explicitly deployed sidecar
    /// (`PILOTDECK_AGENT_LOOP_TRANSPORT=stdio` or `tcp`). The baseline's durable tool
    /// port then sees only what the capability wire carries, so its
    /// `tool_call` entries hold the bare call, without a timeline position.
    external_loop: bool,
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
        let external_loop = self.external_loop;
        Box::pin(async move {
            // The loop records the assistant message before it runs the
            // message's tool calls (in the baseline both happen in one
            // process, in that order). Here the message arrives as a loop
            // event ahead of this call on the same sidecar stream, and its
            // application ends with the durable append; wait for exactly the
            // events that preceded this call. The calls are then recorded
            // after the message and take their timeline from its latest
            // durable copy (a retried step may reuse the same call id).
            crate::runtime::turn::live::prior_wire_events_applied().await;
            let durable_calls: Vec<JsValue> = calls
                .iter()
                .map(|call| {
                    (!external_loop)
                        .then(|| DurableTools::durable_call_in(&projection, &turn, call))
                        .flatten()
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
                    "tool_execution_failed",
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
            // An `LspError` is no `PilotDeckToolRuntimeError`: its `LSP_*`
            // code stays on the error object and the runtime reports the
            // generic `tool_execution_failed` (errorRecovery.ts:176-182).
            match result {
                Err(error) => Err(crate::runtime::tools::ToolError::new(
                    "tool_execution_failed",
                    error.message,
                )),
                Ok(_) => Err(crate::runtime::tools::ToolError::new(
                    "tool_execution_failed",
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
type TurnPermission = Arc<Mutex<TurnOptions>>;
/// The last admitted turn's submit options the host-side config applies.
#[derive(Clone, Default)]
struct TurnOptions {
    /// `permissionMode` with its `basePermissionMode`.
    permission: Option<(JsString, Option<JsString>)>,
    /// `canElicit === true`.
    can_elicit: bool,
    /// `canPrompt`, when the turn gave one.
    can_prompt: Option<JsValue>,
    /// `execution.operationDeadline`, when the turn has one.
    operation_deadline: Option<JsValue>,
}
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
            // The turn's `permissions.json` rules (`source: "user"`) follow
            // the live set, as `replaceUserRules` left them.
            let user = context.rules.clone();
            context.rules = prompt.rules.snapshot();
            context.rules.deny.splice(0..0, deny);
            context.rules.ask.splice(0..0, ask);
            super::permission_settings::replace_user_rules(&mut context.rules, &user);
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
            if let Some(denied) = prompt
                .as_ref()
                .and_then(|prompt| prompt.interaction.admit_permission_ask())
            {
                return Ok(denied);
            }
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
                    super::permission_prompt::PromptOutcome::Deny(reason) => {
                        JsValue::Object(value([
                            ("behavior", "deny".into()),
                            ("message", JsValue::String(reason)),
                        ]))
                    }
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

/// `sdkSessionConfig.deferredTools` (name, searchHint) with the session's
/// revealed names.
type DeferredTools = (
    Vec<(JsString, Option<JsString>)>,
    builtin::search_tools::RevealedTools,
);

/// The session's share of the baseline's execution world
/// (`createNodeExecutionWorldBundle`): what the builtin tools close over.
#[derive(Clone)]
pub(super) struct ExecutionWorld {
    /// `executionWorld.backgroundTasks`, behind the `task_*` tools.
    #[cfg(unix)]
    tasks: crate::runtime::background::BackgroundTaskRuntime,
    /// `getURLMarkdownContent` over the process network, behind `web_fetch`.
    fetcher: builtin::web::fetcher::UrlFetcher,
    /// `webSearch` options (`resolveWebSearchOptions`) with the outcome of
    /// the tool's availability check against the session env; `None` is
    /// `webSearch: false`.
    web_search: Option<(
        builtin::web::search::WebSearchOptions,
        Option<(&'static str, &'static str)>,
    )>,
    /// `sdkSessionConfig.deferredTools` (name, searchHint) and the names this
    /// session's `search_tools` calls revealed.
    deferred: Option<DeferredTools>,
    /// `readSkill: { loader, lister }`: the project's plugin runtime.
    skills: Option<Arc<crate::runtime::plugins::PluginRuntime>>,
    /// `knowledge_query` over the external `modules.knowledge` binding
    /// (ProjectSessionRuntimeBundle.ts:316-326).
    knowledge: Option<Arc<builtin::knowledge_query::KnowledgeQueryTool>>,
    /// The external `modules.tools` capability port
    /// (`createRuntimeModulePorts(...).tools`, one per session).
    module_tools: Option<Arc<super::module_tools::ExternalToolPort>>,
    /// The session override's `bypassAvailable`, when it gives one.
    bypass_override: Option<bool>,
    /// `sdkSessionConfig.skills`: the session's skills restricted to these
    /// names (`filterSessionSkills`), for `read_skill` and the prompt.
    skills_filter: Option<Vec<String>>,
    /// `sdkSessionPlugins`: the session's own plugins, which the scoped
    /// `read_skill` reads with the project's.
    session_plugins: Vec<Arc<crate::runtime::plugins::LoadedPlugin>>,
    /// An external `modules.skills` binding's `read_skill` source.
    external_skills: Option<builtin::read_skill::ExternalReadSkill>,
    /// `sdkSessionConfig.managedTools`: the `{ allow, deny }` selector policy
    /// (`blockedToolNames`' `managed`).
    managed_tools: Option<JsValue>,
    /// `sdkSessionConfig.sandbox`: the session-local tool visibility policy
    /// (`blockedToolNames`' `sandbox`).
    sandbox_policy: Option<JsValue>,
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
    /// The project generation's `agent.sandboxMode` (the runtime profile's
    /// `sandboxMode`, `ProjectExecutionWorldBundle.ts:61-65`): the fs, shell
    /// and code-runtime providers behind the builtin tools.
    sandbox: crate::runtime::tools::sandbox::Sandbox,
    /// The session's loop runs as an external module: a `modules.agentLoop`
    /// binding (`createAgentLoopBindingFactory`, AgentLoopDeploymentProfile.ts:
    /// 107-127) or an explicit stdio / tcp deployment. The assembly's
    /// deployment decision, the same one that marks (or not) the sidecar as
    /// the in-process loop (`pilotdeck_transport::IN_PROCESS_LOOP_ENV`).
    external_loop: bool,
}
impl ExecutionWorld {
    /// The world of a child (subagent) session: shared services, but its own
    /// external `modules.tools` port, since the child composes its module
    /// ports for its own session (AgentSessionRuntimeBundle.ts:156, 200-206).
    fn for_child_session(&self) -> Self {
        let mut world = self.clone();
        world.module_tools = self
            .module_tools
            .as_ref()
            .and_then(|port| port.fresh())
            .map(Arc::new);
        world
    }

    pub(super) fn new() -> Self {
        Self {
            #[cfg(unix)]
            tasks: crate::runtime::background::BackgroundTaskRuntime::default(),
            fetcher: builtin::web::fetcher::UrlFetcher::new(Arc::new(
                crate::runtime::model::http::HyperModelTransport::new(),
            )),
            web_search: None,
            deferred: None,
            skills: None,
            knowledge: None,
            module_tools: None,
            bypass_override: None,
            skills_filter: None,
            session_plugins: Vec::new(),
            external_skills: None,
            managed_tools: None,
            sandbox_policy: None,
            extra_tools: Vec::new(),
            excluded_tools: Vec::new(),
            invocation: None,
            sandbox: Default::default(),
            external_loop: false,
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

    /// An external `modules.knowledge` binding: the session gets
    /// `knowledge_query`, one module port per session as in the baseline.
    pub(super) fn with_knowledge(mut self, binding: Option<&JsValue>) -> Self {
        self.world.knowledge = binding
            .filter(|binding| {
                binding
                    .as_object()
                    .is_some_and(|binding| binding.contains_key("implementationId"))
            })
            .and_then(builtin::knowledge_query::KnowledgeQueryTool::from_binding)
            .map(Arc::new);
        self
    }

    /// An external `modules.tools` binding: its catalog is composed ahead
    /// of the session's tools, one module client per session.
    /// The session's loop runs as an external module (see the field).
    pub(super) fn with_external_loop(mut self, external: bool) -> Self {
        self.world.external_loop = external;
        self
    }

    pub(super) fn with_module_tools(mut self, binding: Option<&JsValue>) -> Self {
        self.world.module_tools = binding
            .and_then(super::module_tools::ExternalToolPort::from_binding)
            .map(Arc::new);
        self
    }

    /// The session's plugin contributions, taken when the session is
    /// composed (ProjectSessionRuntimeBundle.ts:247-264) and kept by its
    /// context runtime until the session is recreated.
    pub(super) fn with_session_extension(
        self,
        extension: Option<Arc<dyn crate::runtime::context::ExtensionResolver>>,
    ) -> Self {
        self.context_state.freeze_session_extension(extension);
        self
    }

    /// The SDK config's `skills` scope and `managedTools` policy.
    pub(super) fn with_sdk_tool_scope(
        mut self,
        skills: Option<Vec<String>>,
        managed_tools: Option<JsValue>,
    ) -> Self {
        self.world.skills_filter = skills;
        self.world.managed_tools = managed_tools;
        self
    }

    /// An external `modules.skills` binding's session source.
    pub(super) fn with_external_skills(
        mut self,
        external: Option<builtin::read_skill::ExternalReadSkill>,
    ) -> Self {
        self.world.external_skills = external;
        self
    }

    /// The session's SDK plugins (`sdkSessionPlugins`).
    pub(super) fn with_sdk_session_plugins(
        mut self,
        plugins: Vec<Arc<crate::runtime::plugins::LoadedPlugin>>,
    ) -> Self {
        self.world.session_plugins = plugins;
        self
    }

    /// The SDK config's `sandbox` tool policy.
    pub(super) fn with_sdk_sandbox(mut self, policy: Option<JsValue>) -> Self {
        self.world.sandbox_policy = policy;
        self
    }

    /// The project generation's sandbox mode for every tool this session
    /// (and its forks) builds.
    pub(super) fn with_sandbox(mut self, mode: crate::runtime::project::SandboxMode) -> Self {
        self.world.sandbox = crate::runtime::tools::sandbox::Sandbox::new(mode);
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
        let mut this = self;
        this.world.bypass_override = bypass_available;
        this
    }
}

/// `blockedToolNames`' `restrict(managed)` (ProjectSessionRuntimeBundle.ts:
/// 783-794): a tool a `deny` selector matches is blocked; with a non-empty
/// `allow`, so is a tool no `allow` selector matches.
fn managed_tool_blocked(policy: &JsValue, name: &JsString) -> bool {
    let selectors = |key: &str| match policy.get(key) {
        Some(JsValue::Array(items)) => items
            .iter()
            .filter_map(JsValue::as_string)
            .map(|selector| String::from_utf16_lossy(selector.units()))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let name = String::from_utf16_lossy(name.units());
    // `matchesToolSelector`: `*`, the exact name, or a `prefix*`.
    let matches = |selector: &String| {
        selector == "*"
            || *selector == name
            || selector
                .strip_suffix('*')
                .is_some_and(|prefix| name.starts_with(prefix))
    };
    let allow = selectors("allow");
    selectors("deny").iter().any(matches) || (!allow.is_empty() && !allow.iter().any(matches))
}

/// `blockedToolNames`' `sandbox` (ProjectSessionRuntimeBundle.ts:807-826):
/// `process: deny` blocks the host-bridge tools (shell / mcp / custom kinds,
/// `execute_code`, `agent`, `task_*`); `filesystem: deny` also every
/// filesystem tool, `read_only` the three writers; `network: deny` also every
/// network tool.
fn sandbox_tool_blocked(
    policy: &JsValue,
    tool: &dyn crate::runtime::tools::ExecutableTool,
) -> bool {
    let fields = tool.fields();
    let Some(name) = fields.get("name").and_then(JsValue::as_string) else {
        return false;
    };
    let name = String::from_utf16_lossy(name.units());
    let kind = fields
        .get("kind")
        .and_then(JsValue::as_string)
        .map(|kind| String::from_utf16_lossy(kind.units()))
        .unwrap_or_default();
    let setting = |key: &str| {
        policy
            .get(key)
            .and_then(JsValue::as_string)
            .map(|value| String::from_utf16_lossy(value.units()))
    };
    let host_bridge = matches!(kind.as_str(), "shell" | "mcp" | "custom")
        || name == "execute_code"
        || name == "agent"
        || name.starts_with("task_");
    let deny_process = setting("process").as_deref() == Some("deny") && host_bridge;
    let deny_filesystem = match setting("filesystem").as_deref() {
        Some("deny") => kind == "filesystem" || host_bridge,
        Some("read_only") => {
            matches!(name.as_str(), "write_file" | "edit_file" | "edit_notebook") || host_bridge
        }
        _ => false,
    };
    let deny_network =
        setting("network").as_deref() == Some("deny") && (kind == "network" || host_bridge);
    deny_process || deny_filesystem || deny_network
}

pub(super) fn native_tools(
    allowed: Option<Vec<JsString>>,
    denied: Option<Vec<JsString>>,
    lsp: Arc<super::families::DefaultLsp>,
    mcp: &SessionMcpTools,
    world: &ExecutionWorld,
) -> ToolRegistry {
    let managed = world.managed_tools.clone();
    let sandbox = world.sandbox_policy.clone();
    let tools = ToolRegistry::new(
        None,
        Some(Arc::new(move |tool| {
            let Some(name) = tool.fields().get("name").and_then(JsValue::as_string) else {
                return false;
            };
            !sandbox
                .as_ref()
                .is_some_and(|policy| sandbox_tool_blocked(policy, tool))
                && !managed
                    .as_ref()
                    .is_some_and(|policy| managed_tool_blocked(policy, name))
                && allowed.as_ref().is_none_or(|names| names.contains(name))
                && !denied.as_ref().is_some_and(|names| names.contains(name))
        })),
    );
    // `createBuiltinRegistry` (tool/registry/createBuiltinRegistry.ts:128):
    // the catalog builtins. `agent`, `subagent` and `send_message` join per
    // turn in `SessionTools::turn`, bound to the session's subagent ports.
    let mut builtins = builtin::read_only_tools_with(world.sandbox);
    builtins.extend(builtin::write_tools_with(world.sandbox));
    #[cfg(unix)]
    {
        builtins.extend(builtin::shell_tools_with(world.sandbox));
        // `createExecuteCodeTool({ webSearch: options?.webSearch !== false })`.
        builtins.push(Arc::new(builtin::execute_code::ExecuteCodeTool::new(
            builtin::execute_code::ExecuteCodeToolOptions {
                web_search: world.web_search.is_some(),
                sandbox: world.sandbox,
            },
        )));
        builtins.extend(builtin::task::task_tools(Some(world.tasks.clone())));
    }
    let web_search = world.web_search.clone().map(|(options, availability)| {
        (
            Arc::new(builtin::web::search::WebSearchTool::new(options)),
            availability,
        )
    });
    if let Some((tool, _)) = &web_search {
        builtins.push(tool.clone());
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
        builtins.push(Arc::new(
            builtin::read_skill::ReadSkillTool::new(plugins.clone())
                .with_selected(world.skills_filter.clone())
                .with_session_plugins(world.session_plugins.clone())
                .with_external(world.external_skills.clone()),
        ));
    }
    for tool in builtins {
        tools
            .register(tool)
            .expect("native builtin names are unique");
    }
    for tool in &world.extra_tools {
        let _ = tools.register(tool.clone());
    }
    // `createBuiltinRegistry`: a disabled `web_search` is marked unavailable
    // under its alias.
    if web_search.is_none() {
        let _ = tools.mark_unavailable(
            crate::runtime::tools::registry::UnavailableTool {
                name: "web_search".into(),
                code: "unavailable".into(),
                reason: "web_search is disabled in this session.".into(),
            },
            &["WebSearch".into()],
        );
    }
    for name in &world.excluded_tools {
        let _ = tools.unregister(name);
    }
    // `filterAvailableTools(working, { cwd, env })`
    // (SessionToolCompositionBundle.ts:46): after the session's exclusions,
    // a registered `web_search` whose availability check fails is replaced
    // by its diagnostic.
    if let Some((_, Some((code, reason)))) = &web_search
        && tools.has(&"web_search".into())
    {
        let _ = tools.unregister(&"web_search".into());
        let _ = tools.mark_unavailable(
            crate::runtime::tools::registry::UnavailableTool {
                name: "web_search".into(),
                code: (*code).into(),
                reason: (*reason).into(),
            },
            &["WebSearch".into()],
        );
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
    /// `preparePerSessionSpecs` (ProjectRuntimeRegistry.ts:345-351): the
    /// browser-use output directory and flags. `None` passes the specs
    /// through unchanged.
    pub browser_use: Option<crate::runtime::mcp::browser_use::BrowserUseSessionContext<'a>>,
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
    // SessionMcpRuntimeBundle.ts:79-81: prepared after the cap check, before
    // the runtime exists; a failure (the output directory cannot be made)
    // rejects the whole composition.
    // The shared lease was handed to the session's resources before this
    // point, so the failure releases it.
    let specs = match &options.browser_use {
        Some(context) => {
            match crate::runtime::mcp::browser_use::prepare_browser_use_specs(specs, context) {
                Ok(specs) => specs,
                Err(error) => {
                    let _ = release.release().await;
                    return Err(error);
                }
            }
        }
        None => specs,
    };
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
            browser_use: None,
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
