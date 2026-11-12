//! Per-turn host assembly from a real project lease, then the stdio sidecar.
//!
//! This is the Rust landing point for `AgentLoopDeploymentProfile.ts:20-110`,
//! `AgentSessionRuntimeBundle.ts:242-290`, `sidecarHostModulePorts.ts:240-322`
//! and `stdioAgentLoopSidecarConnection.ts:31-60`: the deployment selects one
//! stdio sidecar, every actual turn acquires the current project generation,
//! builds a fresh `ModuleRouter` from that lease, and only then spawns the
//! child. The child never supplies model, budget, context or capability ports.
//!
//! `capability` and `turn` are session-owned in TS (`dependencies.tools`,
//! session steer/persistence). They arrive through [`SessionPorts`]; model,
//! budget and context are rebuilt from the lease for every turn. No handler is
//! reused across turns or generations: `model_port::compose`, the lease's
//! token accounting and `ContextStorageResources::create_session` all return
//! turn-owned state.
//!
//! The lease is acquired during assembly; the returned connection is lazy.
//! LiveSession binds the handler and commits start/accepted input before polling
//! that connection. Failed gates therefore create no child; cancellation/drop
//! still retains the lease until any outstanding recorder write exits.
use crate::{
    config::Env,
    host::{
        HostClient, HostError, HostErrorKind, Limits,
        modules::{ModuleRouter, budget, capability, context, model::TurnContext, model_port},
    },
    runtime::{
        project::{
            ProjectResources, Resource, RuntimeLease,
            context_storage::ContextStorageResources,
            model::{RouterResources, TokenAccountingResources},
        },
        turn::{
            interaction::InteractionTurnInput,
            live::{InteractionHost, LiveSessionAdapter},
        },
    },
    storage::recorder::Recorder,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, admission::ExecutionIdentity};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::process::Command;

/// `AgentLoopDeploymentProfile` (TS lines 11-38). TCP is parsed for exact
/// error/selection parity, but the native host factory has no TCP transport.
#[derive(Clone, Debug, PartialEq)]
pub enum Deployment {
    Native,
    Stdio {
        command: String,
        args: Vec<String>,
        cwd: PathBuf,
        env: Env,
    },
    Tcp {
        host: String,
        port: u16,
        connect_timeout_ms: Option<u64>,
    },
}

/// `resolveAgentLoopDeploymentProfile` (`AgentLoopDeploymentProfile.ts:44-81`).
/// Error text is byte-for-byte the TS baseline; the tcp selection is kept so
/// `create` can reject it explicitly (#130) instead of inventing a fallback.
pub fn resolve_deployment(env: &Env, cwd: &Path) -> Result<Deployment, String> {
    let transport = env
        .get("PILOTDECK_AGENT_LOOP_TRANSPORT")
        .map(|value| value.trim().to_lowercase())
        .unwrap_or_default();
    match transport.as_str() {
        "" | "native" => Ok(Deployment::Native),
        "tcp" => Ok(Deployment::Tcp {
            host: required_text(
                env.get("PILOTDECK_AGENT_LOOP_TCP_HOST").map(String::as_str),
                "127.0.0.1",
                "PILOTDECK_AGENT_LOOP_TCP_HOST",
            )?,
            port: required_port(env.get("PILOTDECK_AGENT_LOOP_TCP_PORT").map(String::as_str))?,
            connect_timeout_ms: optional_positive_integer(
                env.get("PILOTDECK_AGENT_LOOP_CONNECT_TIMEOUT_MS")
                    .map(String::as_str),
            )?,
        }),
        "stdio" => {
            let command = required_text(
                env.get("PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND")
                    .map(String::as_str),
                &std::env::current_exe()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "pilotdeck".into()),
                "PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND",
            )?;
            let sidecar_path = match env
                .get("PILOTDECK_AGENT_LOOP_SIDECAR_PATH")
                .map(String::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(value) => crate::metadata::resolve(cwd, value),
                // TS resolves `new URL("./pilotdeck-agent-loop-sidecar.js",
                // import.meta.url)`. The native sibling is the sidecar binary
                // next to the gateway executable.
                None => std::env::current_exe()
                    .ok()
                    .and_then(|path| path.parent().map(Path::to_path_buf))
                    .unwrap_or_else(|| cwd.to_path_buf())
                    .join("pilotdeck-agent-loop-sidecar.js"),
            };
            // A stdio child is per-turn; it must not inherit the parent's TCP
            // deployment config (`AgentLoopDeploymentProfile.ts:70-73`).
            let mut child_env = env.clone();
            child_env.remove("PILOTDECK_AGENT_LOOP_TCP_HOST");
            child_env.remove("PILOTDECK_AGENT_LOOP_TCP_PORT");
            Ok(Deployment::Stdio {
                command,
                args: vec![sidecar_path.to_string_lossy().into_owned()],
                cwd: cwd.to_path_buf(),
                env: child_env,
            })
        }
        _ => Err("PILOTDECK_AGENT_LOOP_TRANSPORT must be native, stdio, or tcp.".into()),
    }
}

fn required_text(value: Option<&str>, default: &str, name: &str) -> Result<String, String> {
    let value = value.unwrap_or(default).trim();
    if value.is_empty() {
        return Err(format!("{name} must not be empty."));
    }
    Ok(value.to_owned())
}

/// JS `Number(value)` restricted to the integer forms the deployment profile
/// consumes. TS parses both numbers with `Number()` + `Number.isInteger`
/// (`AgentLoopDeploymentProfile.ts:127-141`), so `+80`, `80.0`, `1e3` and
/// `0x50` are accepted while `+0x50`, `0x+50` and `80.5` are not.
///
/// The grammar (JS whitespace, radix-prefix recognition and the digit gate that
/// rejects `0x+50`) lives in [`crate::js_value::string_to_number`]; keeping a
/// second copy here is what let the two drift apart before #744 fix-r2. Only
/// `Number.isInteger` remains: finite and without a fractional part.
fn js_integral_number(value: &str) -> Option<f64> {
    let number = crate::js_value::string_to_number(value);
    (number.is_finite() && number.fract() == 0.0).then_some(number)
}

fn required_port(value: Option<&str>) -> Result<u16, String> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Err("PILOTDECK_AGENT_LOOP_TCP_PORT is required when transport is tcp.".into());
    };
    match js_integral_number(value) {
        Some(port) if (1.0..=65_535.0).contains(&port) => Ok(port as u16),
        _ => Err("PILOTDECK_AGENT_LOOP_TCP_PORT must be an integer between 1 and 65535.".into()),
    }
}

fn optional_positive_integer(value: Option<&str>) -> Result<Option<u64>, String> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    match js_integral_number(value) {
        // TS accepts any positive integer `number`; the deployment field is a
        // `u64` millisecond count, so values above its range stay rejected.
        Some(number) if number > 0.0 && number <= u64::MAX as f64 => Ok(Some(number as u64)),
        _ => Err("PILOTDECK_AGENT_LOOP_CONNECT_TIMEOUT_MS must be a positive integer.".into()),
    }
}

/// Session-owned ports that are not rebuilt from the project generation.
/// Every field except `model`/`recorder` is turn state and must be fresh.
pub struct SessionPorts {
    /// `options` for the router model adapter (`RouterModelPortOptions`).
    pub model: crate::host::modules::model_port::RouterModelPortOptions,
    /// Durable model recorder; `None` keeps the model port non-durable.
    pub recorder: Option<crate::host::modules::model_port::DurableRecorder>,
    /// Session tool-results directory (`ProjectSessionRuntimeBundle.ts:520`).
    pub tool_results_dir: PathBuf,
    /// A caller-supplied context runtime; production leaves this `None` and the
    /// lease's `ContextStorageResources` composes the session runtime.
    pub context_runtime: Option<Arc<dyn context::ContextRuntime>>,
    /// `modules.context.requestIdentity`; turn-bound by the session.
    pub context_identity: crate::host::modules::tool_context::ContextRequestIdentity,
    pub context_config: JsObject,
    pub tool_execution: Arc<dyn capability::ToolExecution>,
    pub tool_context: Arc<dyn capability::ToolContext>,
    pub tool_observer: Arc<dyn capability::CapabilityResultObserver>,
    /// `modules.planTodo` resolved handle for this session.
    pub plan_todo: Option<Arc<dyn capability::PlanTodoHandler>>,
    /// Session steer/persistence callbacks (`sidecarTurnComposition.ts:83-87`).
    pub turn: crate::host::modules::turn::Ports,
    /// Trusted per-turn interaction input; `InteractionTurn::bind` still
    /// replaces the profile and the four interaction ports from the lease.
    pub interaction: InteractionTurnInput,
    /// Session-owned inputs of the context's `CompactionEngine`.
    pub compaction: CompactionPorts,
    /// `sdkSessionConfig.skills`: the prompt lists only these skills.
    pub skills: Option<Vec<String>>,
    /// The session's context-runtime state (cache generations, prompt-time
    /// anchors) that the baseline keeps for the live session's lifetime.
    /// `None` composes a context with fresh state, as a new session would.
    pub context_state: Option<Arc<crate::runtime::context::SessionContextState>>,
}

/// The session's side of `CompactionEngine`'s composition: its lifecycle
/// dispatcher (PreCompact / PostCompact), its event emitter, and the summary
/// failure cooldown that the baseline engine keeps for the session's life.
/// The model comes from the leased generation's router.
#[derive(Clone, Default)]
pub struct CompactionPorts {
    pub lifecycle:
        Option<Arc<dyn crate::runtime::context::compaction::engine::CompactionLifecycle>>,
    pub events: Option<crate::runtime::context::compaction::engine::EventEmitter>,
    pub cooldown: crate::runtime::context::compaction::engine::SummaryCooldown,
}

type PortsFactory = dyn Fn(&ExecutionIdentity, Arc<Recorder>, crate::runtime::hooks::Cancellation) -> SessionPorts
    + Send
    + Sync;

pub struct TurnHostFactory {
    registry: Arc<crate::runtime::project::ProjectRuntimeRegistry>,
    deployment: Deployment,
    limits: Limits,
    expected_module_id: Option<JsString>,
    ports: Arc<PortsFactory>,
    agent_config: Option<JsObject>,
    permission_mode_source: Option<Arc<dyn Fn() -> Option<JsString> + Send + Sync>>,
    turn_override_sink: Option<super::live::TurnOverrideSink>,
    /// See [`LiveSessionAdapter::as_in_process_loop`].
    in_process_loop: bool,
    /// The session's project root; `None` is the registry's default project.
    project_root: Option<PathBuf>,
}

impl TurnHostFactory {
    pub fn new(
        registry: Arc<crate::runtime::project::ProjectRuntimeRegistry>,
        deployment: Deployment,
        limits: Limits,
        expected_module_id: Option<JsString>,
        ports: impl Fn(&ExecutionIdentity, Arc<Recorder>) -> SessionPorts + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_signal(
            registry,
            deployment,
            limits,
            expected_module_id,
            move |identity, recorder, _signal| ports(identity, recorder),
        )
    }

    /// Production entry for ports sharing the LiveSession cancellation source.
    pub fn new_with_signal(
        registry: Arc<crate::runtime::project::ProjectRuntimeRegistry>,
        deployment: Deployment,
        limits: Limits,
        expected_module_id: Option<JsString>,
        ports: impl Fn(
            &ExecutionIdentity,
            Arc<Recorder>,
            crate::runtime::hooks::Cancellation,
        ) -> SessionPorts
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            registry,
            deployment,
            limits,
            expected_module_id,
            ports: Arc::new(ports),
            agent_config: None,
            permission_mode_source: None,
            turn_override_sink: None,
            in_process_loop: false,
            project_root: None,
        }
    }

    /// Lease each turn's runtime from the session's own project, so the
    /// turn's interaction resources are the ones the session was composed
    /// from (`registry.resolve(projectKey ?? fallbackProjectRoot)`).
    pub fn for_project(mut self, project_root: PathBuf) -> Self {
        self.project_root = Some(project_root);
        self
    }

    /// Bind the session's explicit serialized AgentRuntimeConfig.
    pub fn with_agent_config(mut self, config: JsObject) -> Self {
        self.agent_config = Some(config);
        self
    }

    /// #1864 (b): the live permission-mode source the adapter consults on
    /// every submit.
    pub fn with_permission_mode_source(
        mut self,
        source: Arc<dyn Fn() -> Option<JsString> + Send + Sync>,
    ) -> Self {
        self.permission_mode_source = Some(source);
        self
    }

    /// See [`LiveSessionAdapter::with_turn_override_sink`].
    pub fn with_turn_override_sink(mut self, sink: super::live::TurnOverrideSink) -> Self {
        self.turn_override_sink = Some(sink);
        self
    }

    /// The deployment is the gateway's own built-in loop; see
    /// [`LiveSessionAdapter::as_in_process_loop`].
    pub fn as_in_process_loop(mut self) -> Self {
        self.in_process_loop = true;
        self
    }

    /// Build the production `LiveSession` over this factory. The registry must
    /// outlive the session and be disposed by its owner after the session.
    /// The ports factory receives the same recorder on every turn; use it for
    /// any durable model/tool ports. `history` reads the current session view.
    pub fn into_live_session(
        mut self,
        recorder: Arc<Recorder>,
        history: impl Fn() -> Vec<JsValue> + Send + Sync + 'static,
    ) -> LiveSessionAdapter {
        let ports = self.ports.clone();
        self.ports = Arc::new(move |identity, recorder, signal| {
            let mut ports = ports(identity, recorder.clone(), signal);
            ports.recorder = Some(model_port::DurableRecorder {
                recorder,
                recording_session_id: ports.recorder.and_then(|r| r.recording_session_id),
            });
            ports
        });
        let close_timeout = self.limits.close_timeout;
        let agent_config = self.agent_config.clone();
        let permission_mode_source = self.permission_mode_source.take();
        let turn_override_sink = self.turn_override_sink.take();
        let in_process_loop = self.in_process_loop;
        let factory = Arc::new(self);
        let maintenance_factory = factory.clone();
        let maintenance: Arc<super::live::MaintenanceFactory> =
            Arc::new(move |identity, recorder, signal| {
                let factory = maintenance_factory.clone();
                Box::pin(async move {
                    factory
                        .maintenance_context(&identity, recorder, signal)
                        .await
                })
            });
        let session = LiveSessionAdapter::with_interaction_signal(
            recorder,
            history,
            move |identity, recorder, signal| {
                let factory = factory.clone();
                Box::pin(async move {
                    factory
                        .create_with_signal(&identity, recorder, signal)
                        .await
                })
            },
        )
        .with_close_timeout(if crate::mutants::on("gates-factory-close-timeout") {
            Limits::default().close_timeout
        } else {
            close_timeout
        })
        .with_maintenance(maintenance);
        let session = match agent_config {
            Some(config) => session.with_agent_config(config),
            None => session,
        };
        let session = match permission_mode_source {
            Some(source) => session.with_permission_mode_source(source),
            None => session,
        };
        let session = match turn_override_sink {
            Some(sink) => session.with_turn_override_sink(sink),
            None => session,
        };
        if in_process_loop {
            session.as_in_process_loop()
        } else {
            session
        }
    }

    /// Acquire one generation lease and prepare a lazy sidecar connection. Unsupported
    /// transports fail before any lease exists, so they cannot leak.
    pub async fn create(
        &self,
        identity: &ExecutionIdentity,
        recorder: Arc<Recorder>,
    ) -> Result<InteractionHost, HostError> {
        self.create_with_signal(
            identity,
            recorder,
            crate::runtime::hooks::Cancellation::new(),
        )
        .await
    }

    pub async fn create_with_signal(
        &self,
        identity: &ExecutionIdentity,
        recorder: Arc<Recorder>,
        signal: crate::runtime::hooks::Cancellation,
    ) -> Result<InteractionHost, HostError> {
        let (command, args, cwd, env) = match &self.deployment {
            Deployment::Stdio {
                command,
                args,
                cwd,
                env,
            } => (command, args, cwd, env),
            Deployment::Native => {
                return Err(HostError::new(
                    HostErrorKind::Protocol,
                    "Agent loop transport 'native' has no sidecar host factory.",
                ));
            }
            Deployment::Tcp { .. } => {
                return Err(HostError::new(
                    HostErrorKind::Protocol,
                    "Agent loop transport 'tcp' is not supported; see #130.",
                ));
            }
        };
        let lease = self
            .registry
            .acquire(self.project_root.as_deref())
            .map_err(|error| HostError::new(HostErrorKind::Protocol, error.message))?;
        let ports = (self.ports)(identity, recorder, signal);
        let (router, input, lease) = match Self::assemble(lease, identity, ports).await {
            Ok(value) => value,
            // `assemble` owns (and therefore drops) the lease on error.
            Err(error) => return Err(error),
        };
        let mut command = Command::new(command);
        command.args(args).current_dir(cwd).env_clear().envs(env);
        let expected = self.expected_module_id.clone();
        let limits = self.limits;
        Ok(InteractionHost {
            client: Box::pin(async move {
                HostClient::spawn(&mut command, expected.as_ref(), limits).await
            }),
            lease,
            router,
            input,
        })
    }

    /// Build the non-interaction handlers from the leased generation. Public
    /// to the crate so the manifest test can bind without spawning a child.
    pub(crate) async fn assemble(
        lease: RuntimeLease,
        identity: &ExecutionIdentity,
        ports: SessionPorts,
    ) -> Result<(ModuleRouter, InteractionTurnInput, RuntimeLease), HostError> {
        let SessionPorts {
            model,
            recorder,
            tool_results_dir,
            context_runtime,
            context_identity,
            context_config,
            tool_execution,
            tool_context,
            tool_observer,
            plan_todo,
            turn,
            mut interaction,
            compaction,
            skills,
            context_state,
        } = ports;
        let context = lease
            .runtime()
            .resources()
            .get::<ProjectResources>()
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::Protocol,
                    "Leased project resources are missing.",
                )
            })?
            .context();
        let signal = interaction.signal.clone();
        let turn_context = TurnContext {
            session_id: identity.session_id.clone(),
            turn_id: identity.turn_id.clone(),
            abort: Some(signal.clone()),
            ..Default::default()
        };
        let router = context.resource(Resource::Router).ok_or_else(|| {
            HostError::new(
                HostErrorKind::Protocol,
                "Leased project router resource is missing.",
            )
        })?;
        let router = router
            .as_any()
            .downcast_ref::<RouterResources>()
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::Protocol,
                    "Leased project router resource is not a native router family.",
                )
            })?;
        let context_recorder = recorder.as_ref().map(|durable| durable.recorder.clone());
        let compaction_model = model_port::compaction_execution(
            router.router.clone(),
            router.model_config.clone(),
            model.project_path.clone(),
            identity.session_id.clone(),
        );
        let mut result = ModuleRouter {
            model: model_port::compose(
                router.router.clone(),
                router.model_config.clone(),
                recorder,
                model,
                turn_context,
            ),
            ..Default::default()
        };
        if let Some(resource) = context.resource(Resource::TokenAccounting) {
            let accounting = resource
                .as_any()
                .downcast_ref::<TokenAccountingResources>()
                .ok_or_else(|| {
                    HostError::new(
                        HostErrorKind::Protocol,
                        "Leased token accounting resource is not native.",
                    )
                })?;
            // TS `AgentTurnCapabilities.model.budget.estimateUsageCost` calls
            // `router.estimateUsageCost`, which reads the router's
            // `stats.modelPricing` (`RouterRuntime.ts:1065-1067`,
            // `TokenStatsCollector.ts:90-96,183-190`). The lease's router owns
            // that parsed table, so the budget port must use the same rates.
            let pricing = router.router.config().stats.model_pricing.clone();
            result.budget = budget::Handler {
                ports: Some(budget::Ports::for_accounting(
                    accounting.accounting.clone(),
                    pricing,
                )),
                signal: Some(signal.clone()),
            };
        }
        let context_runtime = match context_runtime {
            Some(runtime) => Some(runtime),
            None => Some(
                session_context_runtime(
                    &context,
                    tool_results_dir,
                    compaction,
                    compaction_model,
                    skills.as_deref(),
                    context_state,
                )
                .await?,
            ),
        };
        if let Some(runtime) = context_runtime {
            let handler = context::Handler::new(
                runtime,
                context_identity,
                context_config,
                Some(signal.clone()),
            );
            // `createDurableContextRuntime(context, recorder)`: a session
            // with a durable recorder records context materializations and
            // compaction brackets in its transcript.
            result.context = match context_recorder {
                Some(recorder) => handler.with_recorder(recorder),
                None => handler,
            };
        }
        let has_plan_todo = plan_todo.is_some();
        let mut capability = capability::Handler::new(tool_execution, tool_context, tool_observer);
        if let Some(plan) = plan_todo {
            capability = capability.with_plan_todo(plan);
        }
        result.capability = capability;
        // `InteractionTurn::bind` revokes stale authorizations and then installs
        // these session-owned callbacks for the current turn.
        interaction.turn_ports = turn;
        // `modules.planTodo` advertisement is independent of the handler
        // (#570), so record the same selection the session composed.
        interaction.switches.plan_todo = has_plan_todo;
        Ok((result, interaction, lease))
    }
}

use crate::runtime::context as context_runtime;

/// The session's context runtime composed from the leased generation's
/// context storage (`SessionContextRuntimeBundle.compose`), with the
/// session's compaction ports.
async fn session_context_runtime(
    context: &crate::runtime::project::StageContext,
    tool_results_dir: PathBuf,
    compaction: CompactionPorts,
    compaction_model: Arc<dyn crate::host::modules::model::ModelExecution>,
    skills: Option<&[String]>,
    session_state: Option<Arc<crate::runtime::context::SessionContextState>>,
) -> Result<Arc<dyn context::ContextRuntime>, HostError> {
    let storage = context
        .resource(Resource::InstructionStorage)
        .ok_or_else(|| {
            HostError::new(
                HostErrorKind::Protocol,
                "Leased context storage resource is missing.",
            )
        })?;
    let storage = storage
        .as_any()
        .downcast_ref::<ContextStorageResources>()
        .ok_or_else(|| {
            HostError::new(
                HostErrorKind::Protocol,
                "Leased context storage resource is not native.",
            )
        })?;
    // A session-composition failure is not a host execution
    // failure: TS's factory rethrows the constructor error from
    // session creation unchanged (`ProjectSessionFactory.ts:160-163`),
    // so this boundary keeps the original message and kind.
    let session_error = |message: String| {
        HostError::new(
            if crate::mutants::on("budget-session-kind") {
                HostErrorKind::Protocol
            } else {
                HostErrorKind::Session
            },
            message,
        )
    };
    let extension = session_extension(context, skills)
        .await
        .map_err(|error| session_error(error.to_string()))?;
    Ok(Arc::new(
        storage
            .create_session_with_options(
                tool_results_dir,
                crate::runtime::project::context_storage::SessionContextOptions {
                    extension,
                    compaction: Some(
                        crate::runtime::project::context_storage::SessionCompaction {
                            model: compaction_model,
                            lifecycle: compaction.lifecycle,
                            events: compaction.events,
                            cooldown: compaction.cooldown,
                        },
                    ),
                    session_state,
                    ..Default::default()
                },
            )
            .map_err(|error| session_error(error.message))?
            .context_runtime,
    ) as Arc<dyn context::ContextRuntime>)
}

/// What a maintenance operation outside a turn holds: the session's context
/// runtime composed from one generation lease, kept alive with it.
pub struct MaintenanceContext {
    pub runtime: Arc<dyn context::ContextRuntime>,
    /// The session's hooks bound for the maintenance run (PreCompact /
    /// PostCompact dispatch through them); released with the context.
    hooks: Option<(
        crate::runtime::project::interaction_hooks::TurnHooks,
        Option<super::interaction::TurnHooksSink>,
    )>,
    pub lease: RuntimeLease,
}
impl Drop for MaintenanceContext {
    fn drop(&mut self) {
        if let Some((hooks, sink)) = self.hooks.take() {
            if let Some(sink) = sink {
                sink(None);
            }
            hooks.close();
        }
    }
}

impl TurnHostFactory {
    /// `ManualCompactionController`'s context (createAgentSession.ts:169-177):
    /// the same session context runtime a turn composes — compaction engine,
    /// lifecycle hooks, event emitter and summary cooldown — from the
    /// current generation, without a sidecar or a module router.
    pub async fn maintenance_context(
        &self,
        identity: &ExecutionIdentity,
        recorder: Arc<Recorder>,
        signal: crate::runtime::hooks::Cancellation,
    ) -> Result<MaintenanceContext, HostError> {
        let lease = self
            .registry
            .acquire(self.project_root.as_deref())
            .map_err(|error| HostError::new(HostErrorKind::Protocol, error.message))?;
        let mut ports = (self.ports)(identity, recorder, signal);
        let context = lease
            .runtime()
            .resources()
            .get::<ProjectResources>()
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::Protocol,
                    "Leased project resources are missing.",
                )
            })?
            .context();
        // The session's event buffer outlives the maintenance turn: the
        // compaction's events wait there for the next turn to drain them.
        if ports.compaction.events.is_some() {
            ports.compaction.events = context
                .resource(Resource::InteractionEvents)
                .and_then(|resource| {
                    resource
                        .as_any()
                        .downcast_ref::<crate::runtime::project::interaction_events::InteractionEvents>()
                        .cloned()
                })
                .map(|events| {
                    Arc::new(move |event: JsValue| {
                        if let Some(event) = event.as_object() {
                            let _ = events.carry(event.clone());
                        }
                    }) as crate::runtime::context::compaction::engine::EventEmitter
                });
        }
        if let Some(runtime) = ports.context_runtime {
            return Ok(MaintenanceContext {
                runtime,
                hooks: None,
                lease,
            });
        }
        // The session lifecycle the compaction engine dispatches PreCompact /
        // PostCompact through: bound for this maintenance run, as a turn
        // binds it (`InteractionTurn::bind_owned`).
        let hooks = match context
            .resource(Resource::InteractionHooks)
            .and_then(|resource| {
                resource
                    .as_any()
                    .downcast_ref::<crate::runtime::project::interaction_hooks::InteractionHooks>()
                    .cloned()
            }) {
            Some(resource) => {
                let interaction = &mut ports.interaction;
                let hooks = resource.bind_turn_observed(
                    crate::host::modules::lifecycle::Binding {
                        session_id: identity.session_id.clone(),
                        turn_id: identity.turn_id.clone(),
                        cwd: interaction.permission.context.cwd.clone(),
                        permission_mode: Some(
                            interaction.permission.context.mode.as_js_string().clone(),
                        ),
                        base_env: interaction.base_env.take(),
                        signal: Some(interaction.signal.clone()),
                    },
                    identity.run_id.clone(),
                    std::mem::take(&mut interaction.hooks),
                    None,
                    &mut crate::host::modules::ModuleRouter::default(),
                    interaction.hook_wiring.observers.clone(),
                )?;
                hooks.wait_ready().await?;
                let sink = interaction.hook_wiring.turn_sink.clone();
                if let Some(sink) = &sink {
                    sink(Some(Arc::new(hooks.clone())));
                }
                Some((hooks, sink))
            }
            None => None,
        };
        let router = context.resource(Resource::Router).ok_or_else(|| {
            HostError::new(
                HostErrorKind::Protocol,
                "Leased project router resource is missing.",
            )
        })?;
        let router = router
            .as_any()
            .downcast_ref::<RouterResources>()
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::Protocol,
                    "Leased project router resource is not a native router family.",
                )
            })?;
        let compaction_model = model_port::compaction_execution(
            router.router.clone(),
            router.model_config.clone(),
            ports.model.project_path.clone(),
            identity.session_id.clone(),
        );
        let runtime = session_context_runtime(
            &context,
            ports.tool_results_dir,
            ports.compaction,
            compaction_model,
            ports.skills.as_deref(),
            ports.context_state,
        )
        .await?;
        Ok(MaintenanceContext {
            runtime,
            hooks,
            lease,
        })
    }
}

/// `await runtime.pluginRuntime.refresh()` followed by
/// `new PluginRuntimeExtensionResolver(contributions)`
/// (`ProjectSessionRuntimeBundle.ts:247-264`): the generation's plugin
/// contributions as the prompt assembler reads them. A generation without a
/// plugin runtime contributes nothing.
async fn session_extension(
    context: &crate::runtime::project::StageContext,
    selected: Option<&[String]>,
) -> Result<
    Option<Arc<dyn context_runtime::ExtensionResolver>>,
    crate::runtime::plugins::PluginRuntimeError,
> {
    let Some(resource) = context.resource(Resource::PluginRuntime) else {
        return Ok(None);
    };
    let Some(plugins) = resource
        .as_any()
        .downcast_ref::<crate::runtime::project::plugins::PluginResources>()
    else {
        return Ok(None);
    };
    plugins.runtime.refresh().await?;
    // `replaceSkillContributions(contributions, [])` for a disabled skill
    // module (ProjectSessionRuntimeBundle.ts:250-262).
    let skills_disabled = crate::runtime::project::skills_module_disabled(&context.snapshot());
    let text = |value: &pilotdeck_protocol::JsString| String::from_utf16_lossy(value.units());
    Ok(Some(Arc::new(PluginExtension {
        commands: plugins
            .runtime
            .all_commands()
            .await
            .into_iter()
            .map(|command| context_runtime::ContributedCommand {
                name: command.name,
                description: command.description,
                argument_hint: command.argument_hint,
            })
            .collect(),
        skills: plugins
            .runtime
            .all_skills()
            .await
            .into_iter()
            .filter(|_| !skills_disabled)
            // `filterSessionSkills(contributions, sdkSessionConfig.skills)`.
            .filter(|skill| selected.is_none_or(|names| names.contains(&skill.name)))
            .map(|skill| context_runtime::ContributedSkill {
                name: skill.name,
                description: skill.description,
                path: skill.path.to_string_lossy().into_owned(),
            })
            .collect(),
        mcp_instructions: plugins
            .runtime
            .all_mcp_instructions()
            .await
            .into_iter()
            .map(|entry| context_runtime::McpServerInstruction {
                server_name: text(&entry.server_name),
                instructions: Some(text(&entry.instructions)),
            })
            .collect(),
    })))
}

/// The frozen contribution lists of one session composition.
struct PluginExtension {
    commands: Vec<context_runtime::ContributedCommand>,
    skills: Vec<context_runtime::ContributedSkill>,
    mcp_instructions: Vec<context_runtime::McpServerInstruction>,
}
impl context_runtime::ExtensionResolver for PluginExtension {
    fn list_commands(&self) -> Vec<context_runtime::ContributedCommand> {
        self.commands.clone()
    }
    fn list_skills(&self) -> Vec<context_runtime::ContributedSkill> {
        self.skills.clone()
    }
    fn list_mcp_instructions(&self) -> Vec<context_runtime::McpServerInstruction> {
        self.mcp_instructions.clone()
    }
}

#[cfg(test)]
#[path = "host_factory_tests.rs"]
mod tests;
