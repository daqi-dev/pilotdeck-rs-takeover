//! Project-generation context storage composition. As in
//! `ProjectContextStorageBundle.ts:20-35`, the generation shares storage
//! construction, not context/turn state. Each session composes its own runtime
//! and tool-result budget (`SessionContextRuntimeBundle.ts:83-89,155-187`).

use super::{
    Family, FamilyResource, Resource, ResourceFamilySource, RuntimeError, RuntimeResult,
    StageContext, StageSink,
};
use super::{factory::DeferredResource, memory_bundle::MemoryResolverResource};
use crate::runtime::context::tool_budget::{ToolResultBudget, ToolResultBudgetOptions};
use crate::runtime::context::{
    DefaultContextRuntime, InstructionDiscovery, Options as ContextOptions,
    RuntimeContextSurface as ContextRuntimeContextSurface,
};
use pilotdeck_protocol::JsValue;
use std::{any::Any, path::PathBuf, sync::Arc, time::UNIX_EPOCH};

pub struct ContextStorageFamilySource;

impl ResourceFamilySource for ContextStorageFamilySource {
    fn family(&self) -> Family {
        Family::ContextStorage
    }

    fn stage(&self, context: &StageContext, sink: &mut StageSink<'_>) -> RuntimeResult {
        let clock = context.clock();
        let resources = Arc::new(ContextStorageResources {
            snapshot: context.snapshot(),
            project_root: context.project_root(),
            pilot_home: context.pilot_home(),
            // Memory is staged after context storage. Resolve it only when
            // composing a session from the published project generation.
            memory: context.defer_resource(Resource::Memory),
            runtime_context_surface: convert_runtime_context_surface(
                context.profile().runtime_context_surface,
            ),
            now: Arc::new(move || {
                clock()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_secs_f64() * 1000.0)
                    .unwrap_or(0.0)
            }),
        });
        sink.register_resource(Resource::InstructionStorage, resources.clone());
        sink.register_resource(Resource::ToolResultSpill, resources);
        super::persistence::stage(context, sink)?;
        Ok(())
    }
}

/// Immutable native storage factory published in both context storage slots.
/// No runtime, replacement cache, or alias counter belongs to the generation.
pub struct ContextStorageResources {
    snapshot: Arc<JsValue>,
    project_root: PathBuf,
    pilot_home: PathBuf,
    memory: DeferredResource,
    runtime_context_surface: ContextRuntimeContextSurface,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
}

/// Options for composing a main or scoped context. The caller still owns
/// subagent definition routing; this factory does not implement `createContext`.
#[derive(Default)]
pub struct SessionContextOptions {
    /// Set for a scoped agent whose definition has `memory: "disabled"`.
    /// The default inherits this generation's resolver.
    pub memory_disabled: bool,
    /// `sessionExecution.handle.hostWorkspaceRoot`, when provided. Both memory
    /// retrieval and instruction discovery use it; otherwise the generation's
    /// project root is used (`ProjectSessionRuntimeBundle.ts:518,572`).
    pub host_workspace_root: Option<PathBuf>,
    /// `new PluginRuntimeExtensionResolver(contributions)`
    /// (`ProjectSessionRuntimeBundle.ts:264`): the session's plugin-derived
    /// commands, skills and MCP instructions for the system prompt. `None`
    /// contributes nothing.
    pub extension: Option<Arc<dyn crate::runtime::context::ExtensionResolver>>,
    /// The session-owned inputs of the full-summary stage. `None` composes
    /// the port without `CompactionEngine`, as a caller that supplies no
    /// model.
    pub compaction: Option<SessionCompaction>,
    /// The live session's context state, kept across its turns
    /// (`SessionContextState`). `None` starts fresh.
    pub session_state: Option<Arc<crate::runtime::context::SessionContextState>>,
}

/// What `SessionContextRuntimeBundle` hands its `CompactionEngine`
/// (`SessionContextRuntimeBundle.ts:91-135`) beyond the generation's own
/// config: the router stream, the session's lifecycle and event emitter, and
/// the engine's failure cooldown, which outlives one turn's runtime.
pub struct SessionCompaction {
    pub model: Arc<dyn crate::host::modules::model::ModelExecution>,
    pub lifecycle:
        Option<Arc<dyn crate::runtime::context::compaction::engine::CompactionLifecycle>>,
    pub events: Option<crate::runtime::context::compaction::engine::EventEmitter>,
    pub cooldown: crate::runtime::context::compaction::engine::SummaryCooldown,
}

/// Owned by one session/context composition. Scoped callers use
/// [`ContextStorageResources::create_session_with_options`] for memory opt-out
/// and execution-world roots; automatic subagent context routing is not wired.
pub struct SessionContextStorage {
    pub context_runtime: DefaultContextRuntime,
    pub tool_result_budget: Arc<ToolResultBudget>,
}

impl ContextStorageResources {
    /// Build one session/context composition over `tool_results_dir`.
    /// Keep the corresponding `RuntimeLease` alive while creating and using
    /// the session: the resolver is owned by that generation's memory bundle.
    /// A retained storage handle alone does not prevent provider disposal.
    /// This default entry inherits memory and uses the generation's project
    /// root. Scoped/execution-world callers select overrides through
    /// [`Self::create_session_with_options`].
    ///
    /// The caller supplies the session storage's directory (normally
    /// `.pilotdeck/tool-results/<safeId>`) or execution-world spill root, as in
    /// `ProjectSessionRuntimeBundle.ts:520,574`. There is no project-flat
    /// default.
    ///
    /// **Isolation.** Each budget prefixes flat filenames with its own UUID under
    /// the supplied root; identical or sanitize-equal IDs in different contexts cannot
    /// alias a spill file. Read aliases remain exclusive-created under refs.
    /// Within one budget a sanitized key still identifies its cached record.
    /// Expired writes are retired after the bounded wait; their successors
    /// use different physical files, so an uncancellable blocking writer
    /// cannot corrupt or publish over its successor. See the unapproved
    /// difference A7.4-TOOLBUDGET-CONTEXT-ISOLATION-001.
    ///
    /// **Executor isolation.** [`ToolResultBudget`] offloads the spill write,
    /// alias copy and their path checks to `spawn_blocking`
    /// (`write_text_if_absent`/`copy_file_if_absent`), so `apply_tool_results`
    /// never holds a tokio worker through the blocking fs calls. This matches
    /// the TS spill port, whose `node:fs/promises` writes run off the event
    /// loop, and the storage rule in `runtime/context/instructions.rs` (#637).
    /// Per-session setup keeps one synchronous `current_dir` read on the
    /// caller thread (a single getcwd, no spill I/O; `ToolResultBudget::new`,
    /// which #885 turns into an error for a deleted cwd and a relative dir).
    /// Cancellation: one text result's whole persistence sequence — spill
    /// write, alias copy and replacement-cache registration — is a single
    /// blocking task behind one await, and a spawned blocking task is never
    /// aborted, so dropping the caller's future leaves the same state TS
    /// leaves when an abandoned async function still runs every step
    /// unobserved while its lease remains valid. Expiry prevents cache
    /// publication and releases the slot on the next claim or timed wait. Media spills
    /// complete their write the same way (without an alias). A caller dropped
    /// between two tool-result blocks simply stops the loop there: those
    /// blocks are not spilled yet, and a retry re-runs them.
    pub fn create_session(
        &self,
        tool_results_dir: impl Into<PathBuf>,
    ) -> Result<SessionContextStorage, RuntimeError> {
        self.create_session_with_options(tool_results_dir, SessionContextOptions::default())
    }

    /// Compose with scoped memory and execution-world options. The lease and
    /// spill-directory contracts of [`Self::create_session`] apply here too.
    /// For a scoped agent, translate `definition.memory === "disabled"` to
    /// `memory_disabled`; the caller decides whether a separate context is
    /// needed (`ProjectSessionRuntimeBundle.ts:568-595`).
    pub fn create_session_with_options(
        &self,
        tool_results_dir: impl Into<PathBuf>,
        options: SessionContextOptions,
    ) -> Result<SessionContextStorage, RuntimeError> {
        let max_context_tokens = self.max_context_tokens()?;
        let summary = options
            .compaction
            .map(|compaction| self.compaction_engine(compaction))
            .transpose()?;
        // Even an opted-out context must respect the generation's lifetime.
        let memory_resolver = self
            .memory
            .resolve()?
            .filter(|_| !options.memory_disabled)
            .map(|resource| {
                resource
                    .as_any()
                    .downcast_ref::<MemoryResolverResource>()
                    .map(|memory| memory.resolver().clone())
                    .ok_or_else(|| {
                        RuntimeError::new("Project memory resource is not a memory resolver.")
                    })
            })
            .transpose()?;
        let project_root = options
            .host_workspace_root
            .unwrap_or_else(|| self.project_root.clone());
        // #885: the constructor's `resolve` may reject a relative spill dir
        // against a deleted cwd; TS's throw surfaces as a session-creation
        // failure. This composition boundary keeps the message verbatim, so the
        // host chain reports it unchanged instead of as a host execution
        // failure (see `TurnHostFactory::assemble`).
        let tool_result_budget = Arc::new(
            ToolResultBudget::new(ToolResultBudgetOptions::new(tool_results_dir))
                .map_err(|error| RuntimeError::new(error.to_string()))?,
        );
        let memory_retrieval_timeout_ms = self
            .snapshot
            .get("config")
            .and_then(|config| config.get("memory"))
            .and_then(|memory| memory.get("retrievalTimeoutMs"))
            .and_then(JsValue::as_number)
            .unwrap_or(30_000.0);
        let context_runtime = DefaultContextRuntime::new(ContextOptions {
            max_context_tokens,
            runtime_context_surface: self.runtime_context_surface,
            instruction_discovery: Some(InstructionDiscovery::from_environment(
                project_root.clone(),
                project_root.clone(),
                self.pilot_home.clone(),
            )),
            tool_result_budget: Some(tool_result_budget.clone()),
            project_root: Some(project_root.to_string_lossy().into_owned()),
            memory_resolver,
            memory_retrieval_timeout_ms,
            now: Some(self.now.clone()),
            extension: options.extension,
            compaction: Some(Arc::new(native_compaction_port(summary))),
            session_state: options.session_state,
            // `new PromptContributionRegistry({ name: `agent:${sessionKey}` })`
            // (`SessionContextRuntimeBundle.ts:164-166`): the session always
            // owns a registry, so every materialization carries its
            // generation.
            prompt_contributions: Some(crate::runtime::context::PromptContributionRegistry::new(
                None,
            )),
            ..ContextOptions::default()
        });
        Ok(SessionContextStorage {
            context_runtime,
            tool_result_budget,
        })
    }

    /// `new CompactionEngine({ model, tokenBudget, tokenAccounting, lifecycle,
    /// provider, model_, protectedToolNames, now, eventEmitter })` with the
    /// agent model of this generation (`ProjectSessionRuntimeBundle.ts:531-532`).
    fn compaction_engine(
        &self,
        compaction: SessionCompaction,
    ) -> Result<crate::runtime::context::compaction::engine::CompactionEngine, RuntimeError> {
        use crate::runtime::context::compaction::engine::{
            CompactionEngine, CompactionEngineOptions,
        };
        let route = self
            .snapshot
            .get("config")
            .and_then(|config| config.get("agent"))
            .and_then(|agent| agent.get("model"));
        let field = |key: &str| {
            route
                .and_then(|route| route.get(key))
                .and_then(JsValue::as_string)
                .cloned()
                .ok_or_else(|| RuntimeError::new("Agent model provider is missing."))
        };
        let mut options =
            CompactionEngineOptions::new(compaction.model, field("provider")?, field("model")?);
        options.lifecycle = compaction.lifecycle;
        options.event_emitter = compaction.events;
        options.cooldown = Some(compaction.cooldown);
        Ok(CompactionEngine::new(options))
    }

    fn max_context_tokens(&self) -> Result<f64, RuntimeError> {
        let config = self
            .snapshot
            .get("config")
            .ok_or_else(|| RuntimeError::new("Context storage config is missing."))?;
        let agent = config
            .get("agent")
            .ok_or_else(|| RuntimeError::new("Context storage agent config is missing."))?;
        let route = agent.get("model");
        let provider = route
            .and_then(|route| route.get("provider"))
            .and_then(JsValue::as_string)
            .ok_or_else(|| RuntimeError::new("Agent model provider is missing."))?;
        let model = route
            .and_then(|route| route.get("model"))
            .and_then(JsValue::as_string)
            .ok_or_else(|| RuntimeError::new("Agent model name is missing."))?;
        // ModelRuntime.ts:27-47 reads this generation's normalized catalog.
        // Resolve the model even with an override, matching the unconditional
        // getCapabilities before `agent.maxContextTokens ?? caps.maxContextTokens`
        // in ProjectSessionRuntimeBundle.ts:513-533.
        let provider_config = config
            .get("model")
            .and_then(|config| config.get("providers"))
            .and_then(JsValue::as_object)
            .and_then(|providers| providers.get_key(provider))
            .ok_or_else(|| {
                RuntimeError::new(format!(
                    "Provider {} does not exist.",
                    String::from_utf16_lossy(provider.units())
                ))
            })?;
        let model_config = provider_config
            .get("models")
            .and_then(JsValue::as_object)
            .and_then(|models| models.get_key(model))
            .ok_or_else(|| {
                RuntimeError::new(format!(
                    "Model {} does not exist in provider {}.",
                    String::from_utf16_lossy(model.units()),
                    String::from_utf16_lossy(provider.units())
                ))
            })?;
        agent
            .get("maxContextTokens")
            .filter(|value| !matches!(value, JsValue::Null | JsValue::Undefined))
            .or_else(|| {
                model_config
                    .get("capabilities")
                    .and_then(|caps| caps.get("maxContextTokens"))
            })
            .and_then(JsValue::as_number)
            .ok_or_else(|| RuntimeError::new("Model maxContextTokens must be a number."))
    }
}

impl FamilyResource for ContextStorageResources {
    fn dispose(&self) -> super::DisposeFuture<'_> {
        // The baseline storage bundle has no dispose; sessions own their state.
        Box::pin(async { Ok(()) })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn convert_runtime_context_surface(
    value: super::RuntimeContextSurface,
) -> ContextRuntimeContextSurface {
    match value {
        super::RuntimeContextSurface::SystemPrompt => ContextRuntimeContextSurface::SystemPrompt,
        super::RuntimeContextSurface::UserMessage => ContextRuntimeContextSurface::UserMessage,
    }
}

/// `createNativeCompactionPort` as `SessionContextRuntimeBundle.compose`
/// builds it (`SessionContextRuntimeBundle.ts:84-161`): token budget, auto
/// policy, micro compaction, snip and overflow recovery, all with the
/// baseline's default protected tool names.
///
/// `summary` is the session's `CompactionEngine`; without it the orchestrator
/// answers `skipped` where the full summary would run.
fn native_compaction_port(
    summary: Option<crate::runtime::context::compaction::engine::CompactionEngine>,
) -> crate::runtime::context::compaction::engine::port::CompactionPort {
    use crate::runtime::context::compaction::{
        MicroCompactionEngine, MicroCompactionEngineOptions, SnipEngine, SnipEngineOptions,
        engine::port::{CompactionBudgetPort, CompactionPort},
        policy::AutoCompactionPolicy,
        recovery::ContextOverflowRecovery,
    };
    let budget: Arc<dyn CompactionBudgetPort> =
        Arc::new(crate::runtime::model::token_accounting::TokenBudgetManager::default());
    let port = CompactionPort {
        budget: Some(budget.clone()),
        policy: Some(Arc::new(AutoCompactionPolicy::new(budget))),
        micro: Some(Arc::new(MicroCompactionEngine::new(
            MicroCompactionEngineOptions::default(),
        ))),
        snip: Some(Arc::new(SnipEngine::new(SnipEngineOptions::default()))),
        recovery: Some(Arc::new(ContextOverflowRecovery::default())),
        summary: summary.map(|engine| {
            Arc::new(engine)
                as Arc<dyn crate::runtime::context::compaction::engine::port::CompactionSummaryPort>
        }),
        ..CompactionPort::default()
    };
    // `createCompactionOrchestrator(port, { maxContextTokens:
    // options.maxContextTokens ?? 8192 })` (NativeCompactionPort.ts:56-60):
    // the session bundle passes no maxContextTokens, so an input without one
    // (the manual `/compact` command's) budgets against 8192.
    let orchestrator =
        crate::runtime::context::compaction::orchestrator::CompactionOrchestrator::new(
            port.clone(),
            8192.0,
        );
    CompactionPort {
        auto_compact: Some(Arc::new(orchestrator)),
        ..port
    }
}
