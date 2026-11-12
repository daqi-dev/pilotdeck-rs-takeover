use super::{
    Options,
    families::{LspFamily, TitleFamily, TitleResource},
};
use crate::runtime::permission::effective_permission_mode;
use crate::{
    ProxyError,
    config::{Env, permission::GatewaySessionPermissionRuleSetRegistry},
    metadata::object,
    runtime::{
        async_hooks::{AsyncHookRegistry, AsyncHookTurn, TokioHookClock},
        interaction::{GatewayInteractionCoordinator, InteractionOptions},
        mcp::{
            gateway::{McpGateway, McpGatewayOptions},
            sdk::{
                PluginProjectRuntime, SdkMcpCloseSession, SdkMcpConfig, SdkMcpConfigOptions,
                SdkMcpError, SdkMcpFuture, SdkMcpProject, SdkMcpProjectResolver,
                SdkMcpSessionPlugins, SystemEnvironment,
            },
        },
        memory::{
            MemoryManagementService, MemoryProjection, MemoryProviderError, ProjectMemoryRegistry,
            gateway::MemoryGateway,
        },
        project::{
            Families, Family, ProjectResources, ProjectResourcesFactory, ProjectResourcesOptions,
            ProjectRuntimeRegistry, Resource, RuntimeError, RuntimeProfileOverrides, StageContext,
            memory_bundle::MemoryManagementResource, persistence::PersistenceResources,
            plugins::PluginResources, tools_mcp::McpProviderResource,
        },
        replacement::ReplacementCoordinator,
        sdk_session::SdkSessionProjects,
        session::{
            SessionOwner, SessionOwnerOptions,
            factory::{
                FactoryOptions, PreparedResources, PreparedSession, ProjectSessionFactory,
                RecordingPlan, SessionStorage,
            },
        },
        turn::{
            admission::{GatewaySubmitAdmission, SubmitAdmissionOptions},
            gateway::{GatewayServices, TurnGateway},
            host_factory::{Deployment, TurnHostFactory},
            projector::{
                GatewayAgentEventProjector, GatewayAgentEventProjectorOptions,
                GatewayToolResultArtifactStore,
            },
            publish::{FileRegistry, TurnRunRegistry},
            replay::TurnReplayOptions,
            runner::tail::TailConfig,
            submit::{TurnSubmitter, WorkspaceAdmissions},
            submit_host::{GatewaySubmitHost, NativeSubmitReplacement},
        },
    },
    storage::{FileRunRegistry, recorder::Recorder},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, SystemTime},
};

mod always_on;
mod automation;
mod cron;
mod mcp;
mod memory;
pub(crate) mod snapshot;
mod steer;
mod user_dialog;

pub type Recorders = Arc<Mutex<HashMap<JsString, Weak<Recorder>>>>;

/// The cached live writer this request shares a transcript file with, if the
/// session is loaded, together with the key it is registered under. Callers
/// append *through* it instead of building a second writer: the recorder
/// numbers its appends from an in-memory sequence, so a fresh writer that
/// restores the tail and appends `sequence + 1` would either duplicate that
/// number or leave the recorder replaying a stale one. The owner key is
/// returned because `session_model_set`/`clear` must close *that* key for the
/// new selection to be read back — the request spelling can be an alias.
///
/// The lookup uses the stored-transcript identity, not the request spelling --
/// `/p` and `/p/.`, `a/b` and `a-b` name one file and therefore one writer.
///
/// #1860: lives next to [`Recorders`] because it is the map's reader; the
/// transcript fence it pairs with moved to `runtime::session::transcript_locks`
/// so the session factory can take it without a `runtime` → `native_process`
/// edge.
pub(crate) fn live_recorder(
    recorders: &Recorders,
    session_key: &JsString,
) -> Option<(JsString, Arc<Recorder>)> {
    let wanted = crate::storage::sanitize_session_id(session_key);
    let map = recorders.lock().unwrap_or_else(|error| error.into_inner());
    // Prefer the exact spelling when both it and an alias are live; either way
    // both names belong to one transcript, so any live writer is the writer.
    let mut alias = None;
    for (key, weak) in map.iter() {
        if crate::storage::sanitize_session_id(key) != wanted {
            continue;
        }
        let Some(recorder) = weak.upgrade() else {
            continue;
        };
        // A closed session's writer can outlive the close in a stray Arc; it
        // drops every append, so it is not the live writer (the baseline's
        // router forgets the session on close, SessionRouter.ts:412-413).
        if recorder.writer().is_closed() {
            continue;
        }
        if key == session_key {
            return Some((key.clone(), recorder));
        }
        alias = Some((key.clone(), recorder));
    }
    alias
}

/// [`live_recorder`] restricted to the writer of one transcript file. A
/// request that names a project (`tag_session` / `rename_session` with an
/// explicit `projectKey`) writes that project's transcript in the baseline
/// (`createLocalGateway.ts:1116-1140` builds a storage for the resolved
/// project), so a session loaded under another project is not its writer.
pub(crate) fn live_recorder_at(
    recorders: &Recorders,
    session_key: &JsString,
    transcript: &std::path::Path,
) -> Option<(JsString, Arc<Recorder>)> {
    let (owner, recorder) = live_recorder(recorders, session_key)?;
    // Read-only identity: unlike `canonical_transcript_path` this never
    // creates the chat directory of a project nothing has written to yet.
    let identity = |path: &std::path::Path| {
        path.canonicalize().unwrap_or_else(|_| {
            match (
                path.parent().map(std::path::Path::canonicalize),
                path.file_name(),
            ) {
                (Some(Ok(parent)), Some(name)) => parent.join(name),
                _ => path.to_path_buf(),
            }
        })
    };
    (identity(recorder.writer().path()) == identity(transcript)).then_some((owner, recorder))
}

/// The controlled registry switch for collection and testing (#1369).
/// Unset keeps the production default (registry on, no receipt); "on"/"off"
/// state the switch explicitly and publish a construction receipt; every
/// other value, including the empty string, fails closed at startup.
pub(super) enum RunRegistrySwitch {
    On,
    Off,
}

pub(super) fn run_registry_switch(env: &Env) -> Result<Option<RunRegistrySwitch>, ProxyError> {
    let Some(value) = env.get("PILOTDECK_NATIVE_RUN_REGISTRY") else {
        return Ok(None);
    };
    match value.as_str() {
        "on" => Ok(Some(RunRegistrySwitch::On)),
        "off" => Ok(Some(RunRegistrySwitch::Off)),
        _ => Err(
            "native run registry switch (PILOTDECK_NATIVE_RUN_REGISTRY) must be exactly on or off."
                .into(),
        ),
    }
}

/// The controlled storage stimulus entry for collection and testing (#1370,
/// `PILOTDECK_NATIVE_STORAGE_STIMULUS`). Unset keeps the production default —
/// no control files are read and no receipts are published. A set value names
/// the control directory and publishes a construction receipt; a value that
/// is not an existing directory fails closed at startup.
pub(super) fn storage_stimulus_switch(env: &Env) -> Result<Option<PathBuf>, ProxyError> {
    let Some(value) = env.get(crate::storage::stimulus::STIMULUS_ENV) else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Err(
            "native storage stimulus (PILOTDECK_NATIVE_STORAGE_STIMULUS) must be a control directory."
                .into(),
        );
    }
    let path = PathBuf::from(value);
    if !path.is_dir() {
        return Err(
            "native storage stimulus (PILOTDECK_NATIVE_STORAGE_STIMULUS) must name an existing directory."
                .into(),
        );
    }
    Ok(Some(path))
}

/// The controlled loop/fence stimulus entry for collection and testing
/// (#1371, `PILOTDECK_NATIVE_LOOP_STIMULUS`). Unset keeps the production
/// default — no control files are read and no receipts are published. A set
/// value names the control directory and publishes a construction receipt; a
/// value that is not an existing directory (or carries a malformed plan) fails
/// closed at startup.
pub(super) fn loop_stimulus_switch(env: &Env) -> Result<Option<PathBuf>, ProxyError> {
    let Some(value) = env.get(pilotdeck_transport::loop_stimulus::STIMULUS_ENV) else {
        return Ok(None);
    };
    pilotdeck_transport::loop_stimulus::validate_value(value)
        .map(Some)
        .map_err(Into::into)
}

/// Test/embedded entry: the production path is [`start_with_webhook`], which
/// also hands the `/feishu/webhook` resolver to the listener.
#[cfg(test)]
pub async fn start(options: Options, env: Env) -> Result<TurnGateway, ProxyError> {
    start_observed(
        options,
        env,
        Arc::new(|_, result| Box::pin(async move { result })),
        Arc::new(|error| eprintln!("native project cleanup: {error}")),
    )
    .await
}

type CleanupResult =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;
pub(super) type CleanupObserver =
    Arc<dyn Fn(&'static str, Result<(), String>) -> CleanupResult + Send + Sync>;

/// `ProjectRuntimeRegistry.ts:305-308`: when a session lease is acquired, the
/// wiring layer pushes the MCP-generated ask rules onto `lease.rules().ask`
/// through `syncSdkMcpPermissionRules`.
///
/// Returns `false` when the config was never published into the forward
/// reference. That state used to be a silent skip — the acquire path left the
/// ask array as it found it, and no ruler could tell "there was nothing to
/// sync" from "the publication regressed" (#1429). The caller reports the
/// miss instead of swallowing it.
pub(super) fn sync_acquired_ask_rules(
    config: Option<&Arc<SdkMcpConfig>>,
    rules: &crate::config::permission::RuleSetHandle,
    session_key: &JsString,
) -> bool {
    let Some(config) = config else {
        return false;
    };
    rules
        .member("ask")
        .update_array(|ask| config.sync_permission_rules(session_key, ask));
    true
}

/// Where the assembly reports a non-fatal wiring miss during a session acquire
/// (#1429). The process writes it to stderr; the `#[cfg(test)]` seam injects a
/// sink so the report itself is assertable — the miss branch needs an
/// unpublished MCP config, which the real wiring never produces.
pub(super) type AcquireMissReport = Arc<dyn Fn(&str) + Send + Sync>;

const MCP_ASK_SYNC_SKIPPED: &str = "native session: MCP ask rule sync skipped \
     (the config was not published before the session acquire)";

/// Inputs the assembly normally creates itself and only tests vary. Production
/// always uses `Default`: the rule-set registry is created in place, the MCP
/// config is published, and an acquire miss goes to stderr.
pub(super) struct AssemblySeams {
    pub permission_rule_sets: Option<Arc<GatewaySessionPermissionRuleSetRegistry>>,
    pub acquire_miss_report: Option<AcquireMissReport>,
    pub publish_mcp_config: bool,
}

impl Default for AssemblySeams {
    fn default() -> Self {
        Self {
            permission_rule_sets: None,
            acquire_miss_report: None,
            publish_mcp_config: true,
        }
    }
}

// An observer runs only after the actual production disposal. Tests can inject
// a failure or a deterministic completion gate without substituting resources.
#[cfg(test)]
pub(super) async fn start_observed(
    options: Options,
    env: Env,
    cleanup_observer: CleanupObserver,
    cleanup_diagnostic: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<TurnGateway, ProxyError> {
    start_observed_with_upload_cleanup(
        options,
        env,
        crate::upload::CLEANUP_INTERVAL,
        cleanup_observer,
        cleanup_diagnostic,
    )
    .await
}

#[cfg(test)]
pub(super) async fn start_observed_with_upload_cleanup(
    options: Options,
    env: Env,
    upload_cleanup_interval: Duration,
    cleanup_observer: CleanupObserver,
    cleanup_diagnostic: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<TurnGateway, ProxyError> {
    start_assembly(
        options,
        env,
        upload_cleanup_interval,
        cleanup_observer,
        cleanup_diagnostic,
        None,
        None,
        AssemblySeams::default(),
    )
    .await
}

/// Production entry: the host plus the `/feishu/webhook` resolver the native
/// listener mounts (`activeFeishu`, resolved per request so a stop or reload
/// changes the answer), and the #1353 switch handles the embedding's machine
/// drives (the cron stop handle, the channel assembly).
pub(super) async fn start_with_webhook(
    options: Options,
    env: Env,
) -> Result<
    (
        TurnGateway,
        crate::native_http::WebhookResolver,
        super::switch_wiring::AssemblySwitchHandles,
    ),
    ProxyError,
> {
    let slot = Arc::new(std::sync::OnceLock::new());
    let switch_slots = super::switch_wiring::AssemblySwitchSlots::default();
    let host = start_assembly(
        options,
        env,
        crate::upload::CLEANUP_INTERVAL,
        Arc::new(|_, result| Box::pin(async move { result })),
        Arc::new(|error| eprintln!("native project cleanup: {error}")),
        Some(slot.clone()),
        Some(switch_slots.clone()),
        AssemblySeams::default(),
    )
    .await?;
    let slot = slot.get().cloned().ok_or_else(|| {
        ProxyError::from("feishu webhook slot was not constructed during assembly")
    })?;
    // The cron slot is filled by the builder on the gateway thread when a
    // `cron:` section exists; `start_assembly` returning means the builder
    // ran, so reading it here is deterministic (the feishu slot above reads
    // the same way).
    Ok((
        host,
        crate::channel::feishu::FeishuWebhookSlot::resolver(&slot),
        super::switch_wiring::AssemblySwitchHandles {
            cron: switch_slots.cron.get().cloned(),
            channels: switch_slots.channels.get().cloned().ok_or_else(|| {
                ProxyError::from("channel assembly slot was not constructed during assembly")
            })?,
            release_interactions: switch_slots.release_interactions.get().cloned(),
            module_host: switch_slots.module_host.get().cloned(),
        },
    ))
}

/// #1429: the session permission rule set registry is normally created inside
/// the assembly, which leaves the acquire-time MCP ask sync unobservable from a
/// test. Supplying the registry keeps the assertion on the production entry
/// point (real routes, real prepare closure) instead of a copy of it. The miss
/// sink and the publication switch exist only for tests: production always
/// publishes the MCP config and writes a miss to stderr.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn start_observed_with_permission_rule_sets(
    options: Options,
    env: Env,
    permission_rule_sets: Arc<GatewaySessionPermissionRuleSetRegistry>,
    miss_report: AcquireMissReport,
    publish_mcp_config: bool,
    cleanup_observer: CleanupObserver,
    cleanup_diagnostic: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<TurnGateway, ProxyError> {
    start_assembly(
        options,
        env,
        crate::upload::CLEANUP_INTERVAL,
        cleanup_observer,
        cleanup_diagnostic,
        None,
        None,
        AssemblySeams {
            permission_rule_sets: Some(permission_rule_sets),
            acquire_miss_report: Some(miss_report),
            publish_mcp_config,
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_assembly(
    options: Options,
    env: Env,
    upload_cleanup_interval: Duration,
    cleanup_observer: CleanupObserver,
    cleanup_diagnostic: Arc<dyn Fn(&str) + Send + Sync>,
    webhook_slot: Option<Arc<std::sync::OnceLock<Arc<crate::channel::feishu::FeishuWebhookSlot>>>>,
    switch_slots: Option<super::switch_wiring::AssemblySwitchSlots>,
    seams: AssemblySeams,
) -> Result<TurnGateway, ProxyError> {
    let AssemblySeams {
        permission_rule_sets,
        acquire_miss_report,
        publish_mcp_config,
    } = seams;
    let in_process_cli = options.in_process_cli;
    let registry_switch = run_registry_switch(&env)?;
    let stimulus_switch = storage_stimulus_switch(&env)?;
    if let Some(dir) = &stimulus_switch {
        // The #1368/#1370 storage stimulus boundary witness: the controlled
        // entry states its resolved directory at construction, before any turn
        // is driven. The receipt format follows the #1369 construction style.
        eprintln!("native storage stimulus on {}", dir.display());
    }
    let loop_switch = loop_stimulus_switch(&env)?;
    // #1371: only a resolved *plan* replaces the turn. An armed directory
    // without a plan is inert, so it must keep the production behaviour byte
    // for byte, automatic title request included. The plan is resolved here,
    // at gateway startup; the sidecar resolves the same file when it starts,
    // so the collector must write `loop.json` before launching the gateway
    // (see the `loop_stimulus` module docs) or the two processes can disagree.
    let loop_planned = pilotdeck_transport::loop_stimulus::LoopStimulus::global()
        .is_some_and(|stimulus| stimulus.is_planned());
    if let Some(dir) = &loop_switch {
        // #1371: the controlled loop/fence entry states its directory before
        // any turn is driven, in the same construction style as #1370.
        eprintln!("native loop stimulus on {}", dir.display());
    }
    if let Some(switch) = &registry_switch {
        // The #1368 registry boundary witness: the switch itself states its
        // resolved state at construction, before any turn is driven.
        let state = match switch {
            RunRegistrySwitch::On => "on",
            RunRegistrySwitch::Off => "off",
        };
        eprintln!("native run registry {state}");
    }
    validate_profile(&options, &env).await?;
    // `sessionDataPlane.replacement.recover({ pilotHome })`
    // (createLocalGateway.ts:656): settle a previous gateway's last-turn
    // replacements before any writer can open those transcripts.
    super::session_replace::recover_and_report(&options.home);
    // The baseline gateway never registers a project (the `.cwd` markers are
    // written by its web server and cron storage, src/web/server/
    // listProjects.ts and src/cron/runtime/CronProjectStorageProvider.ts:65),
    // and `pilotdeck server` needs none: its default project is the pilot
    // home, the General workspace every dialog lookup accepts unregistered.
    // The `--native` entry is `createLocalGateway({ projectRoot })` with the
    // default `fallbackProjectRoot ?? projectRoot`: there the default project
    // is an ordinary one, and the dialog registry refuses it until something
    // registered it. That entry has no web server to do so, so it registers
    // its own project here.
    // The CLI channel's gateway is that same baseline gateway and registers
    // nothing.
    if !options.default_project_is_general() && !options.in_process_cli {
        register_project(&options)?;
    }
    // The gateway event sink of the session interaction facade
    // (`GatewaySessionInteractionFacade.emit`): the host's coordinator is
    // built after the session factory, so it is published into this slot.
    // The project runtimes' background task completions use the same sink.
    let gateway_emit: super::permission_prompt::GatewayEmitSlot = Arc::default();
    // `router.activeTurnRunId(sessionKey)`, published once the session owner
    // exists, for the router's retry-progress projection.
    let active_run: ActiveRunSlot = Arc::default();
    // The deferred async-hook registry, likewise built with the host.
    let async_hooks: super::hooks::AsyncRegistrySlot = Arc::default();
    let prepare_async_hooks = async_hooks.clone();
    let project = project_registry_with(
        &options,
        &env,
        Some(background_completion_projection(gateway_emit.clone())),
        Some(Arc::new(RouterEventLog {
            path: options.home.join("router").join("events.jsonl"),
            emit: gateway_emit.clone(),
            active_run: active_run.clone(),
        })),
    );
    // The public host provider reads the same project runtimes the sessions
    // run on (createLocalGateway.ts:1524-1556).
    if let Some(slots) = &switch_slots {
        let _ = slots
            .module_host
            .set(Arc::new(crate::module_host::runtime::RuntimeHost {
                home: options.home.clone(),
                project: options.project.clone(),
                builtin_skills_root: options.builtin_skills_root.clone(),
                env: env.clone(),
                registry: project.clone(),
            }));
    }
    let snapshot_enabled = snapshot::snapshot_enabled(&options, &project).await?;
    let snapshot_recorder =
        snapshot::snapshot_recorder(&options, &project, snapshot_enabled).await?;
    let memory = memory::build_memory(options.clone(), project.clone()).await?;
    let recorders: Recorders = Arc::default();
    let permission_rule_sets = permission_rule_sets.unwrap_or_default();
    let interactions = Arc::new(GatewayInteractionCoordinator::new(InteractionOptions {
        permission_grants: Some(permission_rule_sets.clone()),
        ..InteractionOptions::default()
    }));
    let user_dialog_bus = user_dialog::build_user_dialog_bus();
    // #1864: the per-gateway session selection store. One instance serves
    // both writers (the three selector methods on `Requests`) and the reader
    // (the prepare closure composing each session's agent config).
    let session_overrides: Arc<super::SessionOverrides> = Arc::default();
    let dialog_project_keys = session_overrides.dialog_project_keys().clone();
    let prepare_overrides = session_overrides.clone();
    let evict_session_overrides = session_overrides.clone();
    let prepare_user_dialog_bus = user_dialog_bus.clone();
    let prepare_project = project.clone();
    let prepare_options = options.clone();
    let prepare_env = env.clone();
    let prepare_recorders = recorders.clone();
    let prepare_interactions = interactions.clone();
    let prepare_gateway_emit = gateway_emit.clone();
    let prepare_permission_rule_sets = permission_rule_sets.clone();
    // Forward reference so the prepare closure can call sync_permission_rules
    // after lease acquire (mirrors ProjectRuntimeRegistry.ts:305-308).
    let prepare_mcp_config: Arc<std::sync::OnceLock<Arc<SdkMcpConfig>>> = Arc::default();
    // #1429: report at most once per assembly — a per-acquire stderr write would
    // be unbounded if the publication regressed for good.
    // The baseline has no such report, and the server's stderr is output a
    // user sees: production stays silent (tests inject their own observer).
    let acquire_miss_report: AcquireMissReport =
        acquire_miss_report.unwrap_or_else(|| Arc::new(|_: &str| ()));
    let prepare_acquire_miss_report = acquire_miss_report.clone();
    let prepare_acquire_miss_reported: Arc<std::sync::OnceLock<()>> = Arc::default();
    // #1433: the prepare closure's MCP composition diagnostics, written like
    // the session bundle's `onDiagnostic` (ProjectRuntimeRegistry.ts:366-369):
    // `console.warn(\`[pilotdeck] ${message}\`, error?.message ?? "")`, two
    // arguments joined by one space on stderr.
    let prepare_mcp_diagnostic: Option<crate::runtime::mcp::registry::McpDiagnosticSink> =
        Some(Arc::new(|message: String, error: Option<RuntimeError>| {
            use std::io::Write;
            let detail = error.map(|error| error.message).unwrap_or_default();
            let _ = writeln!(std::io::stderr(), "[pilotdeck] {message} {detail}");
        }));
    // The `#[cfg(test)]` seam passes `false` to drive the published-miss branch;
    // production always publishes into the handle the prepare closure reads.
    let build_mcp_config = if publish_mcp_config {
        prepare_mcp_config.clone()
    } else {
        Arc::new(std::sync::OnceLock::new())
    };
    // `GatewaySubagentRuntimeBundle`: the process's continuable children. The
    // session owner is bound once the host has built it.
    let continuations: Arc<super::continuations::Continuations> = Arc::default();
    let prepare_continuations = continuations.clone();
    // The automation bundle's cron manager, published once the builder has
    // constructed it: its tools are the gateway's `extraTools`.
    let cron_manager_slot: Arc<std::sync::OnceLock<Arc<crate::runtime::cron::CronManager>>> =
        Arc::default();
    let prepare_cron_manager = cron_manager_slot.clone();
    // The automation bundle's always-on manager: its tools join every
    // `always-on/…` session (SessionToolCompositionBundle.ts:71-79).
    let always_on_manager_slot: Arc<
        std::sync::OnceLock<Arc<crate::runtime::always_on::AlwaysOnManager>>,
    > = Arc::default();
    let prepare_always_on_manager = always_on_manager_slot.clone();
    let storage_project = project.clone();
    let storage_options = options.clone();
    let storage_overrides = session_overrides.clone();
    // `ephemeralSessions` (ProjectRuntimeRegistry.ts:255,877-892): one
    // temporary storage root per `persistSession: false` session.
    let ephemeral_roots: Arc<Mutex<HashMap<JsString, PathBuf>>> = Arc::default();
    let storage_ephemeral_roots = ephemeral_roots.clone();
    // #897: the composition-time budget preflight resolves the same directory
    // the per-turn ports use (`SessionTools::turn`), so the two cannot drift.
    let budget_options = options.clone();
    let factory = Arc::new(ProjectSessionFactory::new(FactoryOptions {
        writer_busy_wait_timeout: options.drain_budget,
        prepare: Arc::new(move |context, session_mcp_runtimes| {
            let project = prepare_project.clone();
            let options = prepare_options.clone();
            let env = prepare_env.clone();
            let recorders = prepare_recorders.clone();
            let interactions = prepare_interactions.clone();
            let gateway_emit = prepare_gateway_emit.clone();
            let async_hooks = prepare_async_hooks.clone();
            let permission_rule_sets = prepare_permission_rule_sets.clone();
            let mcp_config_handle = prepare_mcp_config.clone();
            let acquire_miss_report = prepare_acquire_miss_report.clone();
            let acquire_miss_reported = prepare_acquire_miss_reported.clone();
            let mcp_diagnostic = prepare_mcp_diagnostic.clone();
            let session_overrides = prepare_overrides.clone();
            let continuations = prepare_continuations.clone();
            let cron_tools_slot = prepare_cron_manager.clone();
            let always_on_tools_slot = prepare_always_on_manager.clone();
            let user_dialog_bus = prepare_user_dialog_bus.clone();
            Box::pin(async move {
                // `registry.resolve(projectKey ?? fallbackProjectRoot)`
                // (ProjectRuntimeRegistry.ts:1540-1560): the session runs in
                // the runtime, the working directory and the storage of the
                // project its first turn named.
                let project_root = options.project_root_for(context.project_key.as_ref());
                let lease = project.acquire(Some(&project_root))?;
                let resources = lease
                    .runtime()
                    .resources()
                    .get::<ProjectResources>()
                    .ok_or_else(|| RuntimeError::new("Native project resources are missing."))?;
                let staged = resources.context();
                // The generation's `PilotDeckRuntimeProfile`
                // (`resolvePilotDeckRuntimeProfile(snapshot.config.agent)`).
                let profile = staged.profile();
                let title = staged
                    .owner(Family::SessionTitleProvider)
                    .ok_or_else(|| RuntimeError::new("Native title provider is missing."))?;
                let title = title
                    .as_any()
                    .downcast_ref::<TitleResource>()
                    .ok_or_else(|| RuntimeError::new("Native title provider has the wrong type."))?
                    .0
                    .clone();
                let lsp = staged
                    .owner(Family::LspService)
                    .ok_or_else(|| RuntimeError::new("Native LSP service is missing."))?;
                let lsp = Arc::new(
                    lsp.as_any()
                        .downcast_ref::<super::families::DefaultLsp>()
                        .ok_or_else(|| RuntimeError::new("Native LSP service has the wrong type."))?
                        .bind()?,
                );
                // `readSkill: { loader, lister }` over the project's plugin
                // runtime (ProjectRuntimeResourcesBundle.ts:265).
                let plugin_runtime =
                    staged
                        .resource(Resource::PluginRuntime)
                        .and_then(|resource| {
                            resource
                                .as_any()
                                .downcast_ref::<crate::runtime::project::plugins::PluginResources>()
                                .map(|plugins| plugins.runtime.clone())
                        });
                // `backgroundTasks: { runtime: executionWorld.backgroundTasks }`
                // (ProjectExecutionWorldBundle.ts:85): the project's runtime.
                #[cfg(unix)]
                let background_tasks = staged.resource(Resource::ExecutionWorld).and_then(|world| {
                    world
                        .as_any()
                        .downcast_ref::<crate::runtime::project::tools_mcp::ExecutionWorldResource>()
                        .map(|world| world.background().tasks.clone())
                });
                let background_subagents = staged
                    .resource(Resource::ExecutionWorld)
                    .and_then(|world| {
                        world
                            .as_any()
                            .downcast_ref::<crate::runtime::project::tools_mcp::ExecutionWorldResource>()
                            .map(|world| world.background().subagents.clone())
                    });
                let interaction_events = staged
                    .resource(Resource::InteractionEvents)
                    .and_then(|resource| {
                        resource
                            .as_any()
                            .downcast_ref::<crate::runtime::project::interaction_events::InteractionEvents>()
                            .cloned()
                    });
                let snapshot = staged.snapshot();
                // `modules.skills.enabled: false` (ProjectSessionRuntimeBundle.ts:250-300):
                // no `read_skill`, and the session contributes no skills.
                let skills_disabled = crate::runtime::project::skills_module_disabled(&snapshot);
                // `snapshot.config.modules?.knowledge` (ProjectSessionRuntimeBundle.ts:316).
                let knowledge_binding = snapshot
                    .get("config")
                    .and_then(|config| config.get("modules"))
                    .and_then(|modules| modules.get("knowledge"))
                    .cloned();
                // `snapshot.config.modules?.tools` (SessionAgentConfigBundle.ts:159-163).
                let tools_binding = snapshot
                    .get("config")
                    .and_then(|config| config.get("modules"))
                    .and_then(|modules| modules.get("tools"))
                    .cloned();
                // `snapshot.config.tools?.webSearch` (ProjectExecutionWorldBundle.ts:124).
                let web_search_config = snapshot
                    .get("config")
                    .and_then(|config| config.get("tools"))
                    .and_then(|tools| tools.get("webSearch"))
                    .cloned()
                    .unwrap_or(JsValue::Undefined);
                let mut agent = snapshot
                    .get("config")
                    .and_then(|v| v.get("agent"))
                    .cloned()
                    .unwrap_or(JsValue::Undefined);
                // `getSdkSessionConfig(sessionKey, runtime)`
                // (ProjectRuntimeRegistry.ts:1025-1036): the stored config's
                // settings are resolved again from the selected files now.
                let sdk_settings = super::sdk_config::resolve_settings(
                    &options.home,
                    options.os_home.as_deref(),
                    &project_root,
                    session_overrides.sdk_config(&context.session_key).as_ref(),
                )
                .map_err(|error| RuntimeError::new(error.message))?;
                super::sdk_config::apply_settings_overlay(&mut agent, sdk_settings.as_ref());
                let mut config = agent.as_object().cloned().unwrap_or_default();
                let selected = agent.get("model").cloned().unwrap_or(JsValue::Undefined);
                for key in ["provider", "model"] {
                    config.insert(
                        key.into(),
                        selected.get(key).cloned().unwrap_or(JsValue::Undefined),
                    );
                }
                let model = staged
                    .resource(Resource::Model)
                    .ok_or_else(|| RuntimeError::new("Native model resource is missing."))?;
                let model = model
                    .as_any()
                    .downcast_ref::<crate::runtime::project::model::ModelResources>()
                    .ok_or_else(|| {
                        RuntimeError::new("Native model resource has the wrong type.")
                    })?;
                use crate::runtime::model::router::RouterModelInvocation;
                let capabilities = selected
                    .get("provider")
                    .and_then(JsValue::as_string)
                    .zip(selected.get("model").and_then(JsValue::as_string))
                    .and_then(|(p, m)| model.providers.get_capabilities(p, m).ok())
                    .unwrap_or(JsValue::Undefined);
                for key in ["maxContextTokens", "maxOutputTokens"] {
                    if config
                        .get(key)
                        .is_none_or(|v| matches!(v, JsValue::Undefined | JsValue::Null))
                    {
                        config.insert(
                            key.into(),
                            capabilities.get(key).cloned().unwrap_or(JsValue::Undefined),
                        );
                    }
                }
                // `modelMultimodal = runtime.model.getMultimodal(...)`
                // (SessionAgentConfigBundle.ts:64-69): the tools read it from
                // their context (`read_file` images and PDFs).
                if let Some(multimodal) = selected
                    .get("provider")
                    .and_then(JsValue::as_string)
                    .zip(selected.get("model").and_then(JsValue::as_string))
                    .and_then(|(p, m)| model.providers.get_multimodal(p, m).ok())
                    .filter(|value| !value.is_undefined())
                {
                    config.insert("modelMultimodal".into(), multimodal);
                }
                let max_output_tokens_env = env
                    .get("PILOTDECK_MAX_OUTPUT_TOKENS")
                    .map(|v| crate::js_value::string_to_number(v))
                    .filter(|v| v.is_finite() && *v > 0.0 && v.fract() == 0.0);
                if let Some(limit) = max_output_tokens_env {
                    config.insert("maxOutputTokens".into(), JsValue::Number(limit));
                }
                // `agent.subagents`: the model, timeout and depth cap of the
                // session's forks.
                let mut config = super::subagents::compose_subagents_section(
                    &config,
                    |provider, id| model.providers.get_capabilities(provider, id).ok(),
                    max_output_tokens_env,
                );
                config.insert("jsonSelfCorrect".into(), JsValue::Bool(true));
                // #1864: compose the session selections the three selector
                // methods stored — the native counterpart of
                // SessionAgentConfigBundle.compose over the registry maps.
                // The permission-mode source for the per-turn override (b)
                // reads the same store live, mirroring
                // InProcessGateway.ts:1443.
                // `getSessionOverride(sessionKey)` (ProjectSessionFactory.ts:222):
                // the unattended runtimes' entry for this session.
                let unattended = session_overrides
                    .config_override(&context.session_key)
                    .map(|value| value.to_value())
                    .unwrap_or(JsValue::Undefined);
                // `sessionOverride?.cwd ?? runtime.projectRoot`
                // (SessionAgentConfigBundle.ts:57): a `workspaceCwd` or an
                // unattended runtime's workspace (an always-on phase runs in
                // its isolated workspace), as stored.
                let session_cwd = match unattended.get("cwd") {
                    None | Some(JsValue::Undefined | JsValue::Null) => {
                        JsValue::String(project_root.to_string_lossy().into_owned().into())
                    }
                    Some(cwd) => cwd.clone(),
                };
                let session_cwd_path = match &session_cwd {
                    JsValue::String(cwd) => {
                        std::path::PathBuf::from(String::from_utf16_lossy(cwd.units()))
                    }
                    _ => std::path::PathBuf::from(project_root.to_string_lossy().into_owned()),
                };
                config.insert("cwd".into(), session_cwd);
                let override_mode = unattended
                    .get("permissionMode")
                    .and_then(JsValue::as_string)
                    .cloned();
                let override_flag = |name: &str| match unattended.get(name) {
                    Some(JsValue::Bool(flag)) => Some(*flag),
                    _ => None,
                };
                let can_prompt_override = override_flag("canPrompt");
                let bypass_available_override = override_flag("bypassAvailable");
                let excluded_tools: Vec<JsString> = match unattended.get("excludeTools") {
                    Some(JsValue::Array(names)) => names
                        .iter()
                        .filter_map(|name| name.as_string().cloned())
                        .collect(),
                    _ => Vec::new(),
                };
                // `[...alwaysOn.getTools(), ...cron.getTools()]`
                // (ProjectAutomationBundle.ts:393-398); the always-on names
                // only survive in an always-on session.
                let mut extra_tools = if context
                    .session_key
                    .units()
                    .starts_with(&"always-on/".encode_utf16().collect::<Vec<u16>>())
                {
                    always_on_tools_slot
                        .get()
                        .map(|manager| manager.tools())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                extra_tools.extend(
                    cron_tools_slot
                        .get()
                        .filter(|manager| manager.enabled())
                        .map(crate::runtime::cron::cron_tools)
                        .unwrap_or_default(),
                );
                // `sessionOverride?.permissionMode ?? options.permissionMode`
                // (SessionAgentConfigBundle.ts:52-56).
                let stored_permission_mode = override_mode
                    .clone()
                    .or_else(|| session_overrides.permission_mode(&context.session_key));
                let stored_thinking = session_overrides.thinking(&context.session_key);
                // `sdkSessionConfig.userDialogKinds` of the config stored
                // before this session was created.
                let sdk_config = session_overrides.sdk_config(&context.session_key);
                let user_dialog_kinds = super::sdk_config::user_dialog_kinds(sdk_config.as_ref());
                // `await runtime.pluginRuntime.refresh()` (ProjectSessionRuntimeBundle
                // .ts:247-264): the session's plugin contributions are taken
                // once, when it is created. Their hooks come first, then the
                // SDK session config's (SessionInteractionBundle.ts:126-140).
                let plugin_runtime_resource = staged.resource(Resource::PluginRuntime);
                let session_plugins = plugin_runtime_resource
                    .as_ref()
                    .and_then(|resource| resource.as_any().downcast_ref::<PluginResources>());
                let plugin_hooks = match session_plugins {
                    Some(plugins) => crate::runtime::plugins::collect_plugin_hooks(
                        &plugins
                            .runtime
                            .refresh()
                            .await
                            .map_err(|error| RuntimeError::new(error.to_string()))?,
                    ),
                    None => Vec::new(),
                };
                let hook_settings =
                    super::hooks::session_hook_settings(plugin_hooks, sdk_config.as_ref());
                let include_hook_events = sdk_config
                    .as_ref()
                    .and_then(|config| config.get("includeHookEvents"))
                    == Some(&JsValue::Bool(true));
                // `options.sdkHooks`: only an SDK hook configuration routes
                // async invocations to the gateway registry.
                let sdk_hooks = sdk_config
                    .as_ref()
                    .and_then(|config| config.get("hooks"))
                    .is_some_and(|hooks| !hooks.is_undefined());
                let stored_style_content =
                    match session_overrides.output_style(&context.session_key) {
                        Some(name) => output_style_content(&staged, &name).await,
                        None => None,
                    };
                super::sdk_config::apply_prompt_fields(
                    &mut config,
                    session_overrides.sdk_config(&context.session_key).as_ref(),
                );
                super::sdk_config::apply_model_fields(
                    &mut config,
                    sdk_config.as_ref(),
                    sdk_settings.as_ref(),
                );
                super::sdk_config::apply_agent_fields(&mut config, sdk_config.as_ref());
                apply_session_overrides(
                    &mut config,
                    stored_permission_mode.as_ref(),
                    stored_thinking.as_ref(),
                    stored_style_content.as_deref(),
                );
                let mut retained = PreparedResources::default();
                retained.retain_lease(lease);
                // `acquirePermissionRuleSet({ permissionRules:
                // override?.permissionRules })` (ProjectSessionFactory.ts:
                // 262-265): the always-on execute phase's deny rules.
                let configured_rules = session_overrides
                    .config_overrides()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .permission_rules(&context.session_key);
                let permission_lease =
                    permission_rule_sets.acquire(&context.session_key, configured_rules.as_ref());
                let session_rules = permission_lease.rules();
                // `LocalGatewayBootConfig.permissionTimeoutMs`.
                let permission_timeout = super::permission_prompt::interaction_timeout(
                    env.get("PILOTDECK_PERMISSION_TIMEOUT_MS")
                        .map(|v| v.as_str()),
                );
                // Sync MCP ask rules on acquire, mirroring
                // ProjectRuntimeRegistry.ts:305-308 where the wiring layer
                // calls `lease.rules().ask` via syncSdkMcpPermissionRules.
                if !sync_acquired_ask_rules(
                    mcp_config_handle.get(),
                    &permission_lease.rules(),
                    &context.session_key,
                ) {
                    // #1429: this used to be a silent skip, so no ruler could
                    // tell a broken publication from an empty ask array.
                    // `build_mcp_gateway` publishes the config before the host
                    // serves any request, so reaching here means the startup
                    // wiring regressed; say it out loud (once per assembly).
                    if acquire_miss_reported.set(()).is_ok() {
                        acquire_miss_report(MCP_ASK_SYNC_SKIPPED);
                    }
                }
                retained.push(move || {
                    Box::pin(async move {
                        permission_lease.release();
                        Ok(())
                    })
                });
                // `SessionSubagentContinuationBundle.attach`: this session as
                // the parent of continuable children, released (children
                // drained, queued follow-ups discarded) with the session.
                let continuation_parent = continuations.bind_session(context.session_key.clone());
                let released_parent = continuation_parent.clone();
                retained.push(move || {
                    Box::pin(async move {
                        released_parent.release().await;
                        Ok(())
                    })
                });
                // #1433: compose the session's MCP tools
                // (`SessionMcpRuntimeBundle.compose`) from the enabled session
                // server table — the production read face of
                // `SdkMcpConfig::session_mcp_servers`. Shared servers lease the
                // project runtime; per-session stdio servers start under the
                // factory's per-session registry. An unpublished config keeps
                // the empty composition: the ask-rule sync above already
                // reported that wiring miss for this assembly (#1429).
                let (session_mcp, session_mcp_release) = match mcp_config_handle.get() {
                    Some(mcp_config) => {
                        let provider = staged
                            .owner(Family::McpProvider)
                            .ok_or_else(|| RuntimeError::new("Native MCP provider is missing."))?;
                        let provider = provider
                            .as_any()
                            .downcast_ref::<McpProviderResource>()
                            .ok_or_else(|| {
                                RuntimeError::new("Native MCP provider has the wrong type.")
                            })?
                            .provider()
                            .clone();
                        // #1433 fix-r1 (review P2#4): read the snapshot's
                        // already-normalized key — `config/snapshot.rs`
                        // normalizes it to a finite number with a floor of 1
                        // (the TS `numberField(..., 5)` + `Math.max(1, …)`),
                        // so re-clamping here would be a second
                        // normalization, and the old `as usize` cast
                        // truncated a fractional cap (TS `size >= 2.5` still
                        // admits the third registration).
                        let max_per_session_instances = snapshot
                            .get("config")
                            .and_then(|config| config.get("gateway"))
                            .and_then(|gateway| gateway.get("maxPerSessionMcpInstances"))
                            .and_then(JsValue::as_number)
                            .unwrap_or(5.0);
                        // The refreshed generation's `contributions.mcpServers`
                        // (ProjectSessionRuntimeBundle.ts:247-279): plugin
                        // servers — the builtin ones included — are composed
                        // when the session is created.
                        let plugin_servers = match session_plugins {
                            Some(plugins) => plugins.runtime.mcp_servers().await,
                            None => JsObject::new(),
                        };
                        super::tools::session_mcp_tools(super::tools::SessionMcpToolsOptions {
                            mcp_config,
                            plugin_servers,
                            provider: &provider,
                            per_session_runtimes: &session_mcp_runtimes,
                            session_key: &context.session_key,
                            max_per_session_instances,
                            on_diagnostic: mcp_diagnostic.clone(),
                        })
                        .await?
                    }
                    None => (
                        super::tools::SessionMcpTools::empty(),
                        super::tools::SessionMcpRelease::default(),
                    ),
                };
                retained.push(move || Box::pin(async move { session_mcp_release.release().await }));
                let tail = TailConfig {
                    cwd: Some(session_cwd_path.clone()),
                    // `shouldCollectFileArtifacts` (ProjectRuntimeRegistry.ts:365):
                    // a session whose project root is the pilot home reports
                    // no file artifacts.
                    collect_file_artifacts: project_root != options.home,
                    // #1371: the controlled loop emits no assistant text, so
                    // the automatic title request is a stimulus difference
                    // (RESULT.md TEXT row). An armed loop *with a plan*
                    // removes that side effect instead of leaving a
                    // non-comparable extra provider request in the recorded
                    // cells; an inert entry keeps the production request.
                    auto_generate_title: !loop_planned,
                    title_provider: (!loop_planned).then_some(title),
                    // `promptSuggestionGenerator` only for a session whose SDK
                    // config sets `promptSuggestions: true`
                    // (ProjectSessionFactory.ts:151,195).
                    suggestion_generator: sdk_config
                        .as_ref()
                        .is_some_and(|config| {
                            config.get("promptSuggestions") == Some(&JsValue::Bool(true))
                        })
                        .then(|| {
                            super::families::prompt_suggestion_generator(
                                model.providers.clone(),
                                selected.clone(),
                                Duration::from_millis(15_000),
                            )
                        }),
                    ..Default::default()
                };
                let session = context.session_key;
                Ok(PreparedSession {
                    resources: retained,
                    tail,
                    host: Box::new(move |binding| {
                        recorders
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(session.clone(), Arc::downgrade(&binding.recorder));
                        // #1864 (b): one source closure feeds both the session
                        // tools and the live adapter — the same store and key
                        // the prepare closure read back.
                        let permission_source = {
                            let session_overrides = session_overrides.clone();
                            let session = session.clone();
                            // The live registry (the last selection or
                            // plan-mode transition) is what the baseline's
                            // next run starts in: `permissionMode = inputMode
                            // ?? livePermissionMode ?? defaultPermissionMode`
                            // (InProcessGateway.ts:1442-1450). A session whose
                            // live entry was cleared (`closeSession`) runs in
                            // the server default, not the stored selection.
                            //
                            // An unattended session's turn is submitted
                            // with its override's mode (CronFire.ts:101
                            // `mode: "bypassPermissions"`, the same mode
                            // `registerTaskSession` stores), so that mode
                            // stands in for the run's requested mode.
                            let override_mode = override_mode.clone();
                            Arc::new(move || {
                                Some(
                                    session_overrides
                                        .live_mode(&session)
                                        .or_else(|| override_mode.clone())
                                        .unwrap_or_else(|| "default".into()),
                                )
                            })
                        };
                        let turn_project_root = project_root.clone();
                        // An explicit `PILOTDECK_AGENT_LOOP_TRANSPORT=stdio`
                        // runs the loop as the baseline's external module;
                        // otherwise this child is the gateway's own loop,
                        // which the baseline runs in process.
                        let explicit =
                            explicit_stdio_deployment(&env, &options.sidecar, &project_root);
                        let deployment = explicit.clone().unwrap_or_else(|| Deployment::Stdio {
                            command: options.sidecar.to_string_lossy().into_owned(),
                            args: options.sidecar_args.clone(),
                            cwd: project_root.clone(),
                            env: env.clone(),
                        });
                        continuation_parent.bind_recorder(binding.recorder.clone());
                        let tool_checkpoint = binding.checkpoint.clone();
                        // `GatewayUserDialogChannel` (ProjectSessionRuntimeBundle.ts:
                        // 638-650): only for a session that enabled a dialog kind,
                        // journaled next to its transcript.
                        let user_dialog = (!user_dialog_kinds.is_empty()).then(|| {
                            user_dialog::session_channel(
                                session.clone(),
                                user_dialog_bus.clone(),
                                gateway_emit.clone(),
                                binding.recorder.writer().path(),
                            )
                        });
                        let ports = super::tools::SessionTools::new(
                            &options,
                            &env,
                            binding,
                            config.clone(),
                            interactions,
                            lsp,
                            session_mcp,
                        )?
                        .with_extra_tools(extra_tools, excluded_tools)
                        .with_knowledge(knowledge_binding.as_ref())
                        .with_module_tools(tools_binding.as_ref())
                        .with_sandbox(profile.sandbox_mode)
                        .with_interaction(profile.interaction, can_prompt_override)
                        .with_web_search(&web_search_config)
                        .with_deferred_tools(sdk_config.as_ref())?
                        .with_prompt_policy(can_prompt_override, bypass_available_override)
                        .with_sdk_permissions(sdk_config.as_ref())
                        .with_sdk_tool_scope(
                            super::sdk_config::session_skills(sdk_config.as_ref()),
                            sdk_config
                                .as_ref()
                                .and_then(|config| config.get("managedTools"))
                                .filter(|policy| {
                                    !matches!(policy, JsValue::Undefined | JsValue::Null)
                                })
                                .cloned(),
                        )
                        .with_permission_mode_source(permission_source.clone())
                        .with_continuation_parent(continuation_parent.parent_ref())
                        .with_hooks(
                            hook_settings.clone(),
                            gateway_emit.clone(),
                            include_hook_events,
                            sdk_hooks.then(|| async_hooks.clone()),
                        )
                        .with_sdk_config_change(sdk_config.as_ref(), &session_overrides)
                        .with_permission_prompt(gateway_emit, session_rules, permission_timeout)
                        .with_permission_mode_transition({
                            let session_overrides = session_overrides.clone();
                            let session = session.clone();
                            Arc::new(move |mode, fallback_base| {
                                session_overrides.transition_live_mode(
                                    &session,
                                    mode,
                                    fallback_base,
                                )
                            })
                        });
                        let ports = match user_dialog {
                            Some(channel) => {
                                ports.with_user_dialog(user_dialog_kinds.clone(), channel)
                            }
                            None => ports,
                        };
                        let ports = match plugin_runtime {
                            Some(plugins) if !skills_disabled => ports.with_skills(plugins),
                            Some(_) => ports,
                            None => ports,
                        };
                        let ports = match interaction_events {
                            Some(events) => ports.with_events(events),
                            None => ports,
                        };
                        #[cfg(unix)]
                        let ports = match background_tasks {
                            Some(tasks) => ports.with_background_tasks(tasks),
                            None => ports,
                        };
                        let limits = crate::host::Limits {
                            close_timeout: options.drain_budget,
                            ..Default::default()
                        };
                        // A forked subagent runs the same loop deployment
                        // against the same project registry.
                        let ports = ports.with_subagents(
                            super::subagents::SubagentRuntime::new(
                                project.clone(),
                                deployment.clone(),
                                limits,
                                options.home.clone(),
                                continuations.clone(),
                            )
                            .with_background(background_subagents.clone()),
                        );
                        ports.check_deferred_tools()?;
                        ports.check_module_tools()?;
                        let turn_override_sink = ports.turn_override();
                        // The baseline's env deployment names no expected
                        // module (`createAgentLoopDeploymentFactory(profile,
                        // { transportObserver })`, createLocalGateway.ts:624).
                        let expected_module =
                            explicit.is_none().then(|| "pilotdeck-agent-loop".into());
                        let factory = TurnHostFactory::new_with_signal(
                            project,
                            deployment,
                            limits,
                            expected_module,
                            move |identity, recorder, signal| {
                                turn_ports(&ports, identity, recorder, signal)
                            },
                        )
                        .with_agent_config(config)
                        .with_turn_override_sink(turn_override_sink)
                        .for_project(turn_project_root)
                        // #1864 (b): TS re-reads the permission-mode override
                        // on every run (InProcessGateway.ts:1443), so an
                        // already-live session picks the change up without a
                        // close. The source reads the same store the selector
                        // method writes.
                        .with_permission_mode_source(permission_source);
                        Ok(if explicit.is_some() {
                            // An external loop's transport attempts go to the
                            // session operation ledger, and its messages carry
                            // the module stamp.
                            factory.with_operation_ledger(session.clone(), tool_checkpoint)
                        } else {
                            // The sidecar is this gateway's own loop, which the
                            // baseline runs in-process: no module stamp on its
                            // messages.
                            factory.as_in_process_loop()
                        })
                    }),
                })
            })
        }),
        create_storage: Arc::new(move |context| {
            let project = storage_project.clone();
            let options = storage_options.clone();
            // `isEphemeralSession(sessionKey)`: `persistSession: false`.
            let ephemeral = storage_overrides
                .sdk_config(&context.session_key)
                .is_some_and(|config| config.get("persistSession") == Some(&JsValue::Bool(false)));
            let ephemeral_roots = storage_ephemeral_roots.clone();
            Box::pin(async move {
                let project_root = options.project_root_for(context.project_key.as_ref());
                let lease = Arc::new(project.acquire(Some(&project_root))?);
                // `getOrCreateEphemeralStorage`: the session's storage lives
                // under `mkdtemp(tmpdir()/pilotdeck-sdk-ephemeral-)`, which is
                // both its project root and its pilot home, so nothing reaches
                // the gateway's own storage.
                let ephemeral_root = if ephemeral {
                    let mut roots = ephemeral_roots.lock().unwrap_or_else(|e| e.into_inner());
                    match roots.get(&context.session_key) {
                        Some(root) => Some(root.clone()),
                        None => {
                            let root = make_ephemeral_root()
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                            roots.insert(context.session_key.clone(), root.clone());
                            Some(root)
                        }
                    }
                } else {
                    None
                };
                let resource = lease
                    .runtime()
                    .resources()
                    .get::<ProjectResources>()
                    .ok_or_else(|| RuntimeError::new("Native persistence project is missing."))?
                    .context()
                    .resource(Resource::Persistence)
                    .ok_or_else(|| RuntimeError::new("Native persistence is missing."))?;
                let path = match &ephemeral_root {
                    Some(root) => {
                        crate::runtime::session::transcript_locks::session_transcript_path(
                            root,
                            root,
                            &context.session_key,
                        )
                    }
                    None => crate::runtime::session::transcript_locks::session_transcript_path(
                        &options.home,
                        &project_root,
                        &context.session_key,
                    ),
                };
                let open_path = path.clone();
                Ok(SessionStorage::Recording(RecordingPlan {
                    transcript_path: path,
                    open: Arc::new(move || {
                        let _lease = &lease;
                        let persistence = resource
                            .as_any()
                            .downcast_ref::<PersistenceResources>()
                            .ok_or_else(|| {
                                crate::storage::recorder::RecorderError::Type(
                                    "Native persistence has the wrong type.".into(),
                                )
                            })?;
                        let opened = persistence
                            .create_session(context.session_key.clone(), Some(open_path.clone()));
                        // A log that fails restore validation: the baseline
                        // disposes that storage, and its dispose flushes an
                        // empty projection checkpoint next to the transcript
                        // (robust-restart-truncated-session).
                        if let Err(
                            crate::storage::recorder::RecorderError::Domain { .. }
                            | crate::storage::recorder::RecorderError::Envelope { .. },
                        ) = &opened
                        {
                            let mut checkpoint = open_path.clone().into_os_string();
                            checkpoint.push(".projections.json");
                            let _ =
                                crate::runtime::session::factory::save_empty_projection_checkpoint(
                                    std::path::Path::new(&checkpoint),
                                    &context.session_key,
                                );
                        }
                        opened
                    }),
                }))
            })
        }),
        admission: None,
        tool_results_dir: Some(Arc::new(move |context| {
            super::tools::session_tool_results_dir(
                &budget_options.project_root_for(context.project_key.as_ref()),
                &context.session_key,
            )
        })),
        // TS returns the transcript read diagnostics from `resumeAgentSession`
        // (resumeAgentSession.ts:118) and `ProjectSessionFactory.createSession`
        // drops them with the rest of the result: nothing reaches the server's
        // output, not even the `transcript_missing` warning every new session
        // has.
        diagnostic: Arc::new(|_| {}),
    }));
    let built = (|| -> Result<_, ProxyError> {
        // TS parity: an unconfigured runRegistry is simply absent, so the file
        // registry is never opened and no run persistence happens at all.
        // `new FileRunRegistry(joinPath(pilotHome, "runs", "registry.json"))`
        // (createLocalGateway.ts:996).
        let runs_path = options.home.join("runs").join("registry.json");
        let runs = match registry_switch {
            Some(RunRegistrySwitch::Off) => None,
            Some(RunRegistrySwitch::On) | None => Some(Arc::new(FileRegistry::new(
                FileRunRegistry::open(runs_path)?,
            )?)),
        };
        // `resolve(tmpdir(), "pilotdeck-tool-output", process.pid.toString())`
        // (createLocalGateway.ts:924-926).
        let artifacts = Arc::new(GatewayToolResultArtifactStore::new(
            std::path::Path::new(&crate::node_io::node_tmpdir(|key| std::env::var_os(key)))
                .join("pilotdeck-tool-output")
                .join(std::process::id().to_string()),
            4096,
        )?);
        let projector = Arc::new(GatewayAgentEventProjector::new(
            GatewayAgentEventProjectorOptions {
                tool_result_artifacts: Some(artifacts),
                ..Default::default()
            },
        )?);
        // U1 upload store over the dialog registry, like the baseline's
        // `new UploadStore({ listProjects: projects.listProjectKeys,
        // resolveProject: projects.resolveProjectKey })`
        // (GatewayDialogBundle.ts:66-69).
        let (resolve_options, list_options) = (options.clone(), options.clone());
        let upload_store = Arc::new(crate::upload::UploadStore::new(
            crate::upload::UploadStoreOptions::new(
                move |key: &pilotdeck_protocol::JsString| {
                    resolve_options
                        .dialog_project(Some(key))
                        .map_err(|_| crate::upload::unknown_project_key(key))
                },
                move || {
                    Ok(list_options
                        .project_catalog()
                        .dialog_project_keys()
                        .into_iter()
                        .map(PathBuf::from)
                        .collect())
                },
            ),
        ));
        let upload_gateway = Arc::new(crate::upload::gateway::UploadGateway::new(
            upload_store.clone(),
        ));
        Ok((runs, projector, upload_store, upload_gateway))
    })();
    let (runs, projector, upload_store, upload_gateway) = match built {
        Ok(value) => value,
        Err(error) => {
            let _ = tokio::time::timeout(options.drain_budget, project.dispose()).await;
            return Err(error);
        }
    };
    // TS ui/server/routes/uploads.js:73-76: the store sweeps its own TTLs on a
    // detached schedule (#1439); the JoinHandle is dropped, not awaited. An
    // idle tick never blocks shutdown, but a sweep already running inside
    // `spawn_blocking` is not cancelled by the drop — see `spawn_cleanup`.
    crate::upload::spawn_cleanup(&upload_store, upload_cleanup_interval);
    let cleanup_factory = factory.clone();
    let cleanup_ephemeral = (
        ephemeral_roots.clone(),
        session_overrides.clone(),
        options.home.clone(),
    );
    let cleanup_project = project.clone();
    // #1746: exactly one teardown may run. The host polls this cleanup itself
    // when initialization fails — `start_with_cleanup_budget` below hands it
    // this embedding's `budget`, so the wait is never shorter than the timeout
    // inside this closure — and the fallback below makes up, per resource,
    // whatever this closure never finished: whether the host thread never
    // started (runtime build or spawn failure, the future dropped unpolled)
    // or the inner budget expired mid-teardown (#1959).
    //
    // #1959: "one teardown" records *entry*, which is the wrong denomination
    // for the fallback: a budget that expires mid-teardown (the channels
    // close/flush stages never reaching factory or project) used to suppress
    // the fallback too, so both leaked. The gate is now per resource — each
    // flag flips when that disposal *completes* — and the fallback makes up
    // exactly the resources the cleanup never finished, so a completed one is
    // never disposed twice (#1746 r1's double-run fix, restated per resource).
    let cleanup_entered: Arc<std::sync::atomic::AtomicBool> = Arc::default();
    let factory_disposed: Arc<std::sync::atomic::AtomicBool> = Arc::default();
    let project_disposed: Arc<std::sync::atomic::AtomicBool> = Arc::default();
    let build_factory = factory.clone();
    let build_project = project.clone();
    // #1328 A8.3: the channel assembly (ChannelAdapterBundle weixin subset
    // on the #1327 ChannelServer). The same config snapshot drives the
    // `adapters.weixin.enabled` startup gate and the ChannelStartDeps config;
    // #1326 takes the `cron` section off the same load; #2294 takes the
    // `alwaysOn` section with it.
    let (channel_assembly, cron_section, always_on_section, idle_options) = {
        use crate::runtime::project::ConfigSource;
        let config = crate::runtime::project::NativeConfigSource::new(
            options.project.clone(),
            env.clone(),
            Some(options.homedir()),
            None,
        )
        .load(&options.project);
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                let _ = tokio::time::timeout(options.drain_budget, project.dispose()).await;
                return Err(error.into());
            }
        };
        let config = config
            .get("config")
            .cloned()
            .unwrap_or_else(|| JsValue::Object(JsObject::new()));
        // #1302 B / #1386 / #1694 §7: a config that enables a channel Rust
        // does not implement refuses startup here, before `NativeChannels` is
        // constructed and therefore before any adapter construction or
        // `console.error` line. The error propagates out of `start` (and so
        // out of `native_process::run`), which is the observable output: the
        // process exits nonzero naming the key instead of starting with a
        // channel that will never respond. The second check covers the
        // bundle keys outside `UNMIGRATED_CHANNEL_KEYS` (`weixin` is built,
        // `feishu` is not yet — #1329).
        // Without the server composition no adapter is ever started, so
        // there is nothing to refuse.
        let rejected = if options.in_process_cli {
            Ok(())
        } else {
            crate::channel::loader::reject_unmigrated_channels(config.get("adapters")).and_then(
                |()| {
                    crate::channel::loader::reject_unimplemented_bundle_channels(
                        config.get("adapters"),
                        super::channels::IMPLEMENTED_BUNDLE_CHANNEL_KEYS,
                    )
                },
            )
        };
        if let Err(error) = rejected {
            // Same teardown order as the failure branch below (#1328): the
            // session factory first, then the project registry.
            let _ = tokio::time::timeout(options.drain_budget, async {
                let _ = factory.dispose().await;
                project.dispose().await
            })
            .await;
            return Err(error.into());
        }
        // #1326 A8 U8: the `cron:` section travels to the builder below, which
        // is the only place that constructs `CronManager`.
        // `Options::in_process_cli`: no section reaches the bundle, so it
        // stages neither manager (and adds none of their tools).
        let automation_section = |name: &str| {
            (!options.in_process_cli)
                .then(|| config.get(name).cloned())
                .flatten()
        };
        let cron_section = automation_section("cron");
        // #2294: the `alwaysOn:` section travels with it — the automation
        // bundle's `createAlwaysOn` gate reads the section itself (absent
        // section, or no project with `enabled: true`, constructs nothing).
        let always_on_section = automation_section("alwaysOn");
        let idle_options = session_idle_options(config.get("gateway"));
        let channel_assembly = Arc::new(super::channels::NativeChannels::new(
            super::channels::NativeChannelsOptions {
                pilot_home: options.home.clone(),
                config: Arc::new(config),
                weixin: crate::channel::weixin::WeixinChannelOptions::default(),
                weixin_maker: None,
                feishu: crate::channel::feishu::FeishuOptions::default(),
                // #2301: TS `consoleLogger` (pilotdeckServer.ts:67-72) — the
                // channel log the loader-table adapters' visible start lines
                // (`api-server: listening on …`, `sms: twilio not installed…`)
                // and the lifecycle core's startup lines go through.
                log: Some(super::channels::console_channel_log()),
                report_status: None,
                close_host: None,
            },
        ));
        // #1329: hand the listener the slot before any adapter starts, so the
        // route is mounted with the process rather than with the channel.
        if let Some(slot) = webhook_slot {
            let _ = slot.set(channel_assembly.feishu_webhook_slot());
        }
        // #1353 A8.5-pr2: the channel assembly is the machine's E4 face
        // (close, the admission stop) and E3 face (flush, the durable state) —
        // handed out here, where it is constructed, so the embedding can
        // drive the switch without knowing the assembly's build order.
        if let Some(slots) = &switch_slots {
            let _ = slots.channels.set(channel_assembly.clone());
        }
        (
            channel_assembly,
            cron_section,
            always_on_section,
            idle_options,
        )
    };
    let build_channels = channel_assembly.clone();
    let cleanup_channels = channel_assembly.clone();
    // #2294: the builder constructs and starts the one automation bundle (the
    // one `CronManager` and the one `AlwaysOnManager` behind its providers) on
    // the gateway thread, but this cleanup closure is built before the builder
    // runs — the shared slot carries the started bundle from the builder to
    // the cleanup, which stops it before the channels (automation precedes
    // closeServer, PilotDeckServerShutdownBundle.ts:33-37).
    let build_automation_slot: Arc<OnceLock<Arc<automation::ProjectAutomationBundle>>> =
        Arc::default();
    let cleanup_automation_slot = build_automation_slot.clone();
    // #1353 A8.5-pr2: the switch machine's S1 face stops the bundle's cron
    // manager through this shared handle (reading the abort rejections the
    // recording gateway below records). The host teardown stops the whole
    // bundle through the slot above; after a machine-driven stop the bundle's
    // own cron stop repeats benignly — no active runs remain and the release
    // pairs are idempotent — while always-on still stops there.
    let build_cron_slot: Arc<OnceLock<Arc<crate::switch::CronStopHandle>>> = match &switch_slots {
        Some(slots) => slots.cron.clone(),
        None => Arc::default(),
    };
    let build_release_slot = switch_slots
        .as_ref()
        .map(|slots| slots.release_interactions.clone());
    let budget = options.drain_budget;
    // #1959: the fallback's own read of the per-resource completion flags —
    // the cleanup closure below moves the originals.
    let fallback_factory_disposed = factory_disposed.clone();
    let fallback_project_disposed = project_disposed.clone();
    let fallback_observer = cleanup_observer.clone();
    // `bindServer(server)` (src/cli/pilotdeck.ts:248): the refresh bundle
    // broadcasts `config_changed` to every authenticated connection of the
    // gateway server. The set exists before the host so the builder can bind
    // it; the host registers each accepted connection into it.
    let connection_broadcast = Arc::new(crate::runtime::connection::ConnectionBroadcast::default());
    let build_connection_broadcast = connection_broadcast.clone();
    // `onUserDialogChange` (createLocalGateway.ts:1053-1058): the bus's
    // renderer notifications go to every connection of the server.
    user_dialog::bind_broadcast(
        &user_dialog_bus,
        connection_broadcast.clone(),
        dialog_project_keys.clone(),
    );
    let host = TurnGateway::start_with_cleanup_budget(
        TurnReplayOptions::new(),
        // #1746: the failure path waits out this embedding's own drain budget
        // (the cleanup below bounds itself by the same value), so a slow but
        // legal `--drain-ms` teardown is not cut by the host's default bound.
        budget,
        move |coordinator| {
            let emit_coordinator = coordinator.clone();
            // `turnEventCoordinator.dispose()` comes first in the gateway's
            // disposal: once the stop has begun, the facade delivers nothing.
            let emit_disposed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let release_disposed = emit_disposed.clone();
            let _ = gateway_emit.set(Arc::new(move |session, event| {
                !emit_disposed.load(std::sync::atomic::Ordering::SeqCst)
                    && emit_coordinator.emit(session, &event)
            }));
            let mut owner_options = SessionOwnerOptions::new(build_factory);
            (
                owner_options.idle_timeout_ms,
                owner_options.idle_sweep_interval_ms,
            ) = idle_options;
            // `onSessionEvict` (createLocalGateway.ts:846-848): every eviction
            // except a dirty recreate drops the session's live permission
            // mode, so the next turn starts from the stored selection or the
            // server default.
            let evict_overrides = evict_session_overrides.clone();
            owner_options.on_evict = Some(Arc::new(move |key, reason| {
                if reason != crate::runtime::session::EvictionReason::DirtyRecreate {
                    evict_overrides.clear_live_mode(key);
                }
            }));
            let owner = SessionOwner::new(owner_options);
            let _ = active_run.set({
                let owner = owner.clone();
                Arc::new(move |session: &JsString| owner.active_turn_run_id(session))
            });
            continuations.bind_owner(owner.clone());
            if let Some(slot) = &build_release_slot {
                // `gateway.dispose("local_gateway_disposed")` followed by the
                // router shutdown (LocalGatewayLifecycleBundle.ts:45-60), for
                // the turns that are parked on a prompt.
                let (owner, interactions) = (owner.clone(), interactions.clone());
                let _ = slot.set(Arc::new(move || {
                    release_disposed.store(true, std::sync::atomic::Ordering::SeqCst);
                    let reason: JsString = "local_gateway_disposed".into();
                    for session in owner.active_turn_sessions() {
                        if interactions.permission_bus().has_pending_session(&session)
                            || interactions.elicitation_bus().has_pending_session(&session)
                        {
                            interactions.reject_pending_turn(&session, &reason);
                            let _ = owner.abort(&session, None, &reason);
                        }
                    }
                }));
            }
            let replacement = Arc::new(ReplacementCoordinator::new(
                super::session_fork::storage(&options),
                Arc::new(owner.clone()),
                budget.as_millis() as u64,
                None,
            ));
            let project_admission = Arc::new(super::projects::SubmitProjectAdmission {
                options: options.clone(),
                registry: build_project.clone(),
                overrides: session_overrides.clone(),
            });
            let admission = SubmitAdmissionOptions {
                interactions: interactions.clone(),
                sdk_config: Some(Arc::new(super::sdk_config::NativeSdkSessionConfig {
                    options: options.clone(),
                    overrides: session_overrides.clone(),
                    project: build_project.clone(),
                })),
                compaction: Some(Arc::new(super::manual_compaction::OwnerCompaction {
                    owner: owner.clone(),
                })),
                model_assertion: Some(project_admission.clone()),
                dialog_recovery: Some(project_admission),
                ..SubmitAdmissionOptions::new(replacement.clone())
            };
            // U7 global settings (#1320): `createPilotConfigStoreSync` +
            // `runtimeRefresh` + the settings gateway
            // (createLocalGateway.ts:795-835, 1383-1410), composed in
            // `services::native_settings_for_process`; a store load failure
            // fails assembly, like the TS boot.
            let settings = super::services::native_settings_for_process(
                owner.clone(),
                build_project.clone(),
                &options.homedir(),
                // `createPilotConfigStoreSync({ projectRoot, env })`
                // (createLocalGateway.ts:800): the process root, not the
                // default project.
                &options.project_root,
                &env,
                session_overrides.clone(),
                Some(Arc::new(super::services::NativeOutputStylesReload {
                    options: options.clone(),
                    project: build_project.clone(),
                    owner: owner.clone(),
                    interactions: interactions.clone(),
                    overrides: session_overrides.clone(),
                })),
            )?;
            let cron_hooks = cron::NativeCronHooks {
                session_overrides: session_overrides.config_overrides(),
                on_turn_event: Some(cron::broadcast_turn_events(
                    build_connection_broadcast.clone(),
                )),
                on_result_delivery: Some(cron::deliver_to_channels(build_channels.clone())),
            };
            // `onAlwaysOnTurnEvent` (src/cli/pilotdeck.ts:82-84): the same
            // `always-on:turn-event` broadcast as cron's.
            let worktree_broadcast = build_connection_broadcast.clone();
            let always_on_hooks = always_on::NativeAlwaysOnHooks {
                session_overrides: session_overrides.config_overrides(),
                on_turn_event: Some(cron::broadcast_turn_events(
                    build_connection_broadcast.clone(),
                )),
                // `onWorktreeCreated` (src/cli/pilotdeck.ts:76-78).
                on_worktree_created: Some(Arc::new(move |run_id: &str, cwd: &str| {
                    let mut payload = pilotdeck_protocol::JsObject::new();
                    payload.insert("runId".into(), JsValue::from(run_id));
                    payload.insert("cwd".into(), JsValue::from(cwd));
                    worktree_broadcast.broadcast("worktree_created", &JsValue::Object(payload));
                })),
            };
            settings.bundle.bind_server(build_connection_broadcast);
            // `configStore.startWatching()` (createLocalGateway.ts:801): a
            // config file change reloads the store, which the refresh bundle
            // turns into runtime refresh + `config_changed`.
            let config_watch = settings
                .store
                .start_watching(crate::config::store::WatchOptions::default())
                .ok();
            let services = Arc::new(super::services::Services {
                options: options.clone(),
                project: build_project.clone(),
                env: env.clone(),
                recorders: recorders.clone(),
                owner: owner.clone(),
                snapshot_recorder: snapshot_recorder.clone(),
                upload_store: upload_store.clone(),
                session_overrides: session_overrides.clone(),
                config_store: Some(super::services::NativeConfigStore {
                    store: settings.store.clone(),
                    _watch: config_watch,
                }),
                memory_maintenance: Some(super::services::native_memory_maintenance(
                    build_project.clone(),
                )),
            });
            let mcp_gateway = mcp::build_mcp_gateway(
                &owner,
                &build_project,
                &permission_rule_sets,
                &build_mcp_config,
                &interactions,
                &session_overrides,
            );
            let async_hook_registry =
                steer::build_async_hook_registry(&build_project, &owner, &coordinator);
            if let Some(registry) = &async_hook_registry {
                let _ = async_hooks.set(registry.clone());
            }
            // #2294 A7.8: the one automation bundle of the native process —
            // the port of `ProjectAutomationBundle`, which now constructs the
            // one `CronManager` (the #1326 lane's manager behind the bundle's
            // cron provider) and the one `AlwaysOnManager`. A factory failure
            // fails the whole assembly through the same `project.dispose()`
            // fallback below, with the factory's own `io::Error` verbatim.
            let automation = Arc::new(automation::ProjectAutomationBundle::new(
                automation::ProjectAutomationBundleOptions {
                    config: automation::ProjectAutomationConfig {
                        always_on: always_on_section
                            .as_ref()
                            .map(crate::runtime::always_on::AlwaysOnConfig::from_section),
                        cron: cron_section.clone(),
                    },
                    pilot_home: options.home.clone(),
                    make_always_on: always_on::native_factory(always_on_hooks.clone()),
                    make_cron: cron::native_factory(cron_hooks),
                },
            )?);
            if let Some(manager) = automation.cron_manager() {
                let _ = cron_manager_slot.set(manager);
            }
            if let Some(manager) = automation.always_on_manager() {
                let _ = always_on_manager_slot.set(manager);
            }
            // #1326: the cron port's own volatile fence. It must NOT be the
            // owner's `SessionAdmission`: `SessionAdmissionCronGateway` holds
            // its fence for the whole of `run_turn`, and that call ends in
            // `TurnSubmitter::submit`, which admits the very same sessionKey
            // through `SessionOwner::begin_turn`. Sharing one instance puts
            // the outer fence in the inner admission's way — the submit is
            // rejected `session_busy` and the turn never runs (review r1,
            // both channels). A dedicated instance still rejects a second
            // concurrent cron trigger for one sessionKey (cell 11), while the
            // real per-session admission stays where it belongs, inside the
            // submit path.
            let cron_admission = Arc::new(crate::runtime::session::SessionAdmission::default());
            let admission_dialog_project_keys = session_overrides.dialog_project_keys().clone();
            let cron_owner = owner.clone();
            let requests = Arc::new(super::requests::Requests {
                project: build_project,
                memory: memory.clone(),
                options,
                env,
                coordinator: coordinator.clone(),
                runs: runs.clone(),
                owner: owner.clone(),
                replacement: replacement.clone(),
                recorders,
                interactions: interactions.clone(),
                mcp: mcp_gateway,
                async_hook_registry,
                user_dialog_bus: user_dialog_bus.clone(),
                upload_gateway: upload_gateway.clone(),
                // The CLI `prepareWeixinLogin` callback (pilotdeck.ts:240-247):
                // hot-start the weixin channel on request, after the same
                // durable-state restore as a config-enabled start.
                prepare_weixin_login: Some(build_channels.prepare_login_callback()),
                cron: automation.cron_manager(),
                // #2294: TS `getInitialGatewayOptions` reads the generation's
                // managers the same way (`generation.cron`,
                // `generation.alwaysOn`); the composition face is the
                // bundle's, so the `#1313` not_configured envelopes stay the
                // answer exactly when no always-on provider was staged.
                always_on: automation.always_on_manager(),
                always_on_standalone: Default::default(),
                settings: settings.gateway,
                session_overrides,
            });
            let submitter = Arc::new(TurnSubmitter {
                owner,
                coordinator,
                registry: runs.map(|runs| runs as Arc<dyn TurnRunRegistry>),
                workspace_admissions: WorkspaceAdmissions::default(),
                host: Arc::new(GatewaySubmitHost::new(
                    services,
                    Arc::new(NativeSubmitReplacement((*replacement).clone())),
                    Arc::new(GatewaySubmitAdmission::with_dialog_project_keys(
                        admission,
                        admission_dialog_project_keys,
                    )),
                )),
                projector,
            });
            // #2294: attach the live gateway facade and start the bundle.
            // The whole start chain never awaits anything pending
            // (`AlwaysOnManager::start` → the scheduler's sync spawn;
            // `CronManager::start` is synchronous), so `now_or_never` drives
            // it to completion inside the synchronous builder — which itself
            // runs inside the host's `block_on`, where `Handle::block_on`
            // would panic. A `None` means some start step unexpectedly
            // yielded; failing assembly is the honest answer, never a
            // deadlock. `update_subsystems` is the TS hot-reload publication
            // (`setCronController` / `setAlwaysOnControl`); this embedding has
            // no hot reload, so the consumer is a documented no-op. The
            // bundle's own bind gate enforces the attach-before-start order.
            {
                use futures_util::FutureExt;
                let busy_owner = cron_owner.clone();
                let is_project_busy: automation::IsProjectBusy =
                    Arc::new(move |project_key: &str| {
                        busy_owner.has_active_user_turn(&JsString::from(project_key))
                    });
                // #1353 A7: the recording wrapper the switch machine's S1
                // face reads. `CronManager::stop` aborts in-flight runs
                // through the bound gateway and swallows every rejection
                // (the `let _ =` in manager.rs stop), so the rejection has
                // to be recorded where the face can report it. Delegation is
                // behavior-preserving.
                let cron_gateway = Arc::new(crate::switch::RecordingCronGateway::wrap(Arc::new(
                    crate::runtime::cron::SessionAdmissionCronGateway::new(
                        cron_admission.clone(),
                        Arc::new(cron::NativeCronGatewayBody::new(
                            cron_owner.clone(),
                            submitter.clone(),
                            tokio::runtime::Handle::current(),
                        )),
                    ),
                )));
                let always_on_gateway: Arc<dyn crate::runtime::always_on::AlwaysOnAgentGateway> =
                    Arc::new(always_on::NativeAlwaysOnGatewayBody::new(
                        cron_owner.clone(),
                        submitter.clone(),
                    ));
                // The standalone control's turn facade (createApplyHandler.ts:
                // 142-150) — the same gateway the bundle's providers bind.
                let _ = requests.always_on_standalone.set(Arc::new(
                    crate::runtime::always_on::StandaloneAlwaysOn {
                        gateway: always_on_gateway.clone(),
                        session_overrides: always_on_hooks.session_overrides.clone(),
                        on_turn_event: always_on_hooks.on_turn_event.clone(),
                        language: always_on_section
                            .as_ref()
                            .map(crate::runtime::always_on::AlwaysOnConfig::from_section)
                            .and_then(|config| config.language),
                    },
                ));
                automation.attach(automation::ProjectAutomationAttachment {
                    agent_gateway: automation::ProjectAutomationAgentGateway {
                        always_on: always_on_gateway,
                        cron: cron_gateway.clone(),
                    },
                    is_project_busy: Some(is_project_busy),
                    update_subsystems: Arc::new(|_| ()),
                })?;
                match std::pin::pin!(automation.start()).now_or_never() {
                    Some(result) => {
                        // The started status is the bundle's own bookkeeping;
                        // the assembly only needs the failure to cross.
                        result.map_err(|error| std::io::Error::other(error.to_string()))?;
                    }
                    None => {
                        return Err(std::io::Error::other(
                            "automation start unexpectedly yielded inside the synchronous builder",
                        ));
                    }
                }
                build_automation_slot.set(automation.clone()).map_err(|_| {
                    std::io::Error::other("automation bundle slot already occupied")
                })?;
                // #1353 A8.5-pr2: the machine's S1 face stops the bundle's
                // cron manager through the shared handle, reading the abort
                // rejections the recording wrapper above accumulated. Without
                // a `cron:` section there is no manager and no handle — the
                // S1 face is a no-op, exactly as before the bundle existed.
                if let Some(cron) = automation.cron_manager() {
                    build_cron_slot
                        .set(Arc::new(crate::switch::CronStopHandle::new(
                            cron,
                            cron_gateway.failures(),
                        )))
                        .map_err(|_| std::io::Error::other("cron manager slot already occupied"))?;
                }
            }
            // #1328: publish the weixin gateway port (the TS `baseDeps.gateway`
            // every channel start receives) before any channel can construct.
            // The builder runs exactly once per host, so a second publish is
            // a wiring bug, not a value to ignore.
            if !build_channels.set_gateway_port(Arc::new(
                crate::channel::weixin::NativeWeixinGatewayPort::new(submitter.clone()),
            )) {
                return Err(std::io::Error::other(
                    "weixin gateway port already published",
                ));
            }
            // #1329: the same submitter answers `/feishu/webhook` turns; the
            // adapter holds the handle from construction, so this only fills
            // the slot behind it.
            if !build_channels.set_turn_submitter(submitter.clone()) {
                return Err(std::io::Error::other(
                    "feishu turn gateway already published",
                ));
            }
            Ok(GatewayServices {
                interactions,
                requests,
                submitter,
            })
        },
        async move {
            if cleanup_entered.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            // #1746: the host awaits this future on its own failure path before
            // reporting the error, so the deadline lives here; the shutdown
            // drain still bounds it by the embedding's shutdown budget.
            let teardown = async move {
                // #2294 A7.8: stop the automation bundle first — automation
                // precedes closeServer (PilotDeckServerShutdownBundle.ts:33-37).
                // The bundle's stop order is cron first, then always-on, so
                // producer dependencies drain in reverse startup order; a stop
                // failure is observed but never blocks the remaining teardown.
                // #1353: the machine's S1 face may have stopped the bundle's
                // cron manager earlier through the shared handle; the
                // bundle's own cron stop then repeats benignly (no active
                // runs remain, the release pairs are idempotent), and
                // always-on still gets its stop here.
                let automation_stop = match cleanup_automation_slot.get() {
                    Some(automation) => {
                        let outcome = automation.stop().await.map_err(|error| error.to_string());
                        cleanup_observer("automation", outcome).await
                    }
                    None => Ok(()),
                };
                // #1328 A8.3: stop the channel assembly and flush its durable
                // mapper state — the closeServer + flushChannelState pair
                // (PilotDeckServerShutdownBundle.ts:33-37: channels stop before
                // the gateway disposes). The WS server has already drained when
                // this cleanup runs; the close-host hook is the pr2 WS-ordering
                // follow-up.
                let channel_stop = cleanup_observer(
                    "channels",
                    cleanup_channels
                        .close()
                        .await
                        .map_err(|e| e.message().to_string()),
                )
                .await;
                let channel_flush = cleanup_channels.flush().await;
                // #1959: the completion flags flip before the observers run, so
                // an observer that stalls cannot un-complete a finished
                // resource (and the fallback below will not re-run it).
                let sessions = {
                    let result = cleanup_factory.dispose().await.map_err(|e| e.to_string());
                    factory_disposed.store(true, std::sync::atomic::Ordering::SeqCst);
                    cleanup_observer("session", result).await
                };
                let project = {
                    let result = cleanup_project.dispose().await.map_err(|e| e.to_string());
                    project_disposed.store(true, std::sync::atomic::Ordering::SeqCst);
                    cleanup_observer("project", result).await
                };
                if let Err(error) = &project {
                    cleanup_diagnostic(error);
                }
                dispose_ephemeral_sessions(
                    &cleanup_ephemeral.0,
                    &cleanup_ephemeral.1,
                    &cleanup_ephemeral.2,
                );
                let mut outcome = sessions.and(project);
                for failure in [
                    automation_stop.err(),
                    channel_stop.err(),
                    channel_flush.err(),
                ]
                .into_iter()
                .flatten()
                {
                    outcome = outcome.and(Err(failure));
                }
                outcome
            };
            match tokio::time::timeout(budget, teardown).await {
                Ok(outcome) => outcome,
                Err(_) => Err("gateway host cleanup deadline exceeded".to_string()),
            }
        },
    );
    match host {
        Ok(host) => {
            let host = host.with_connection_broadcast(connection_broadcast);
            // #1328: start config-enabled channels (pilotdeckServer.ts:288-296)
            // once the builder has published the gateway port. #2301: the
            // configured half — `loadEnabledChannels(config.adapters)` — joins
            // the loop after the bundle keys, in the TS startup order
            // (feishu, weixin, qq, then `channels`;
            // `ChannelAdapterBundle.createStartupAdapters`).
            let mut startup = Vec::new();
            if in_process_cli {
                return Ok(host);
            }
            if let Some(feishu) = channel_assembly.create_startup_feishu().await {
                startup.push(feishu);
            }
            if let Some(weixin) = channel_assembly.create_startup_weixin().await {
                startup.push(weixin);
            }
            startup.extend(channel_assembly.create_startup_configured());
            if !startup.is_empty() {
                channel_assembly.server().start_channels(startup);
            }
            Ok(host)
        }
        Err(error) => {
            // #1746: the host disposes the coordinator and polls the cleanup
            // above before reporting a builder failure. This fallback owns the
            // project/factory teardown the cleanup did not finish — #1959:
            // per resource, not by entry. The host either never started (the
            // future was dropped unpolled, both flags clear) or finished
            // awaiting the cleanup, so the flags are final when read: a flag
            // left clear means that disposal never completed — the host thread
            // never started, or the inner budget cut the teardown mid-flight,
            // which is exactly the case the makeup below exists for — and the
            // fallback makes that resource up; a resource the cleanup
            // completed is never disposed twice.
            if !fallback_factory_disposed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = tokio::time::timeout(budget, async {
                    let result = factory.dispose().await.map_err(|e| e.to_string());
                    let _ = fallback_observer("session", result).await;
                })
                .await;
            }
            if !fallback_project_disposed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = tokio::time::timeout(budget, async {
                    let result = project.dispose().await.map_err(|e| e.to_string());
                    let _ = fallback_observer("project", result).await;
                })
                .await;
            }
            Err(error.into())
        }
    }
}
/// #1864 (a): the pure compose step for the three session selections —
/// extracted from the prepare closure so the read-back tests exercise the
/// same logic the next session runs. Mirrors
/// SessionAgentConfigBundle.compose: `managedPermissions.defaultMode` (a
/// non-empty string) wins over the stored permission mode, else "default"
/// (shared: `runtime::permission::effective_permission_mode`);
/// a stored thinking value is written as-is (JS null included — presence is
/// the override, TS deletes only on undefined); the output style's content
/// merges into `appendSystemPrompt` as
/// `[existing, content].filter(non-blank).join("\n\n")`, an empty join
/// writes undefined, and a style that resolved to no content leaves the
/// field untouched.
pub(super) fn apply_session_overrides(
    config: &mut JsObject,
    stored_permission_mode: Option<&JsString>,
    stored_thinking: Option<&JsValue>,
    style_content: Option<&str>,
) {
    let mode = effective_permission_mode(config, stored_permission_mode.cloned());
    config.insert("permissionMode".into(), JsValue::String(mode));
    if let Some(thinking) = stored_thinking {
        config.insert("thinking".into(), thinking.clone());
    }
    if let Some(content) = style_content {
        let existing = config
            .get("appendSystemPrompt")
            .and_then(JsValue::as_string)
            .map(|value| String::from_utf16_lossy(value.units()));
        // `sdkOutputStyleContent?.trim()`.
        let joined = [existing.as_deref(), Some(crate::metadata::js_trim(content))]
            .into_iter()
            .flatten()
            .filter(|part| !crate::metadata::js_trim(part).is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        config.insert(
            "appendSystemPrompt".into(),
            if joined.is_empty() {
                JsValue::Undefined
            } else {
                JsValue::String(joined.into())
            },
        );
    }
}

/// #1864: the stored output style's content through the same view
/// `output_styles_list` lists (ProjectResources → PluginRuntime →
/// PluginResources.runtime). Like ProjectRuntimeRegistry's
/// getSdkOutputStyleContent the view is read without a refresh and a
/// missing style is not an error: `None` leaves the field alone.
async fn output_style_content(staged: &StageContext, name: &JsString) -> Option<String> {
    let plugin_runtime = staged
        .resource(Resource::PluginRuntime)?
        .as_any()
        .downcast_ref::<PluginResources>()?
        .runtime
        .clone();
    plugin_runtime
        .create_view(vec![])
        .await
        .list_output_styles()
        .into_iter()
        .find(|style| JsString::from(style.name.as_str()) == *name)
        .map(|style| style.content)
}
pub(super) fn register_project(options: &Options) -> Result<(), ProxyError> {
    // resolve_process_path reads current_dir only for a relative marker.
    register_project_at(options, std::path::Path::new(""))
}

pub(super) fn register_project_at(
    options: &Options,
    cwd: &std::path::Path,
) -> Result<(), ProxyError> {
    let chats = crate::session_chat_dir(
        &options.home,
        &options.project,
        &options.project.to_string_lossy(),
    );
    let directory = chats
        .parent()
        .ok_or("Native project storage has no parent.")?;
    std::fs::create_dir_all(directory)?;
    let marker = directory.join(".cwd");
    if marker.exists() {
        let existing = std::fs::read_to_string(&marker)?;
        let existing = crate::metadata::js_trim(&existing);
        // TS registration lookup rejects blank markers before resolving paths.
        // Resolving "" when cwd is the project would otherwise manufacture ownership.
        if existing.is_empty() {
            return Err("Native project storage marker is empty.".into());
        }
        // TS stat(marker) / resolve(marker) interpret relative markers against
        // the process cwd, not the project requested by the caller.
        if crate::metadata::resolve_process_path(cwd, std::path::Path::new(existing))?
            != options.project
        {
            return Err("Native project storage marker belongs to a different project.".into());
        }
        return Ok(());
    }
    // Publish all bytes atomically, without replacing another writer's marker.
    let temporary = directory.join(format!(".cwd-{}", uuid::Uuid::new_v4()));
    let publish = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(options.project.to_string_lossy().as_bytes())?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, &marker)
    })();
    let _ = std::fs::remove_file(&temporary);
    publish?;
    Ok(())
}
/// This entry's currently supported profile is a text main agent with native
/// read tools. Refuse configured integrations before admitting a turn instead
/// of silently dropping their tools/hooks or launching an implicit helper.
/// What the native host still refuses to start with: configured hooks and
/// module bindings, neither of which it serves yet (the baseline has no such
/// refusal). MCP servers, plugins, skills and the `agent.subagents` section
/// are served — see `session_mcp_tools`, the plugin runtime and
/// `subagents::compose_subagents_section` — and are no longer refused.
/// Plugins that contribute hooks stay refused with the config-level hooks.
pub(super) async fn validate_profile(options: &Options, env: &Env) -> Result<(), ProxyError> {
    use crate::runtime::project::ConfigSource;
    let snapshot = crate::runtime::project::NativeConfigSource::new(
        options.project.clone(),
        env.clone(),
        Some(options.homedir()),
        None,
    )
    .load(&options.project)?;
    // A top-level `hooks` section is no hook source: the baseline's loader
    // reports it as an unknown field and starts without it
    // (loadPilotConfig.ts:343-375). Plugin and SDK session hooks are served
    // (`native_process/hooks.rs`).
    // `modules`: a built-in binding (`provider: pilotdeck`) and a disabled
    // `skills` / `knowledge` slot need no outside service and are served; an
    // external `knowledge` module is reached only by `knowledge_query`, an
    // external `tools` module is composed ahead of the session's tools.
    // A protocol binding (an external module process or endpoint) and the
    // StaffDeck `sop` module are not served yet.
    let external = |binding: &JsValue| {
        binding
            .as_object()
            .is_some_and(|binding| binding.contains_key("implementationId"))
    };
    if snapshot
        .get("config")
        .and_then(|v| v.get("modules"))
        .and_then(JsValue::as_object)
        .is_some_and(|modules| {
            modules.own_entries().iter().any(|(slot, binding)| {
                slot.equals_str("sop")
                    || (external(binding)
                        && !slot.equals_str("knowledge")
                        && !slot.equals_str("tools"))
            })
        })
    {
        return Err(
            "Native isolated profile does not yet support configured module bindings.".into(),
        );
    }
    Ok(())
}
pub fn env_object(env: &Env) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in env {
        result.insert(key.as_str().into(), JsValue::String(value.as_str().into()));
    }
    result
}
pub fn value<const N: usize>(entries: [(&'static str, JsValue); N]) -> JsObject {
    object(entries)
        .as_object()
        .expect("object constructor")
        .clone()
}

pub(super) fn turn_ports(
    ports: &super::tools::SessionTools,
    identity: &pilotdeck_protocol::admission::ExecutionIdentity,
    recorder: Arc<Recorder>,
    signal: crate::runtime::hooks::Cancellation,
) -> crate::runtime::turn::host_factory::SessionPorts {
    ports.turn(identity, recorder, signal)
}

#[cfg(test)]
pub(super) fn project_registry(options: &Options, env: &Env) -> Arc<ProjectRuntimeRegistry> {
    project_registry_with(options, env, None, None)
}

/// `toGatewayBackgroundTaskCompletionStatus`
/// (gateway/tasks/GatewayBackgroundTaskCompletionProjection.ts:20-37).
fn background_task_completion_status(event: &JsObject) -> JsValue {
    let field = |key: &str| event.get(key).cloned().unwrap_or(JsValue::Undefined);
    let mut detail = JsObject::new();
    detail.insert("taskId".into(), field("taskId"));
    detail.insert("status".into(), field("status"));
    detail.insert(
        "exitCode".into(),
        match field("exitCode") {
            JsValue::Undefined => JsValue::Null,
            code => code,
        },
    );
    detail.insert("totalBytes".into(), field("totalBytes"));
    detail.insert("startedAt".into(), field("startedAt"));
    detail.insert("endedAt".into(), field("endedAt"));
    if let Some(JsValue::String(preview)) = event.get("outputPreview") {
        let units = preview.units();
        let end = units
            .iter()
            .rposition(|unit| !crate::runtime::replacement::js_whitespace(*unit))
            .map_or(0, |index| index + 1);
        if end > 0 {
            detail.insert(
                "outputPreview".into(),
                JsValue::String(JsString::from_utf16(units[..end].to_vec())),
            );
        }
    }
    let mut status = JsObject::new();
    status.insert("type".into(), "agent_status".into());
    status.insert("event".into(), "background_task_completed".into());
    status.insert("detail".into(), JsValue::Object(detail));
    JsValue::Object(status)
}

/// The session live projection of background task completions
/// (`GatewaySessionLiveProjectionBundle`, ProjectSessionRuntimeBundle.ts:
/// 668-677): a completion owned by a session is emitted into that session's
/// active turn; with no active turn the gateway drops it, as the TS emit does.
fn background_completion_projection(
    emit: super::permission_prompt::GatewayEmitSlot,
) -> crate::runtime::background::CompletionHandler {
    Arc::new(move |event: JsObject| {
        let (Some(emit), Some(JsValue::String(session))) = (emit.get(), event.get("sessionId"))
        else {
            return;
        };
        emit(session, background_task_completion_status(&event));
    })
}

type ActiveRunSlot =
    Arc<std::sync::OnceLock<Arc<dyn Fn(&JsString) -> Option<JsString> + Send + Sync>>>;

/// `ProjectRouterEventBusProvider` (src/cli/ProjectRouterEventBusProvider.ts):
/// every router event is appended to `<pilot home>/router/events.jsonl` as one
/// `JSON.stringify` line (best effort), and a retry-progress event is projected
/// into the session's active turn as `agent_status` / `retry_progress`
/// (`InProcessGateway.broadcastRetryProgress`, InProcessGateway.ts:732-756)
/// when the event's turn is the session's active run.
struct RouterEventLog {
    path: std::path::PathBuf,
    emit: super::permission_prompt::GatewayEmitSlot,
    active_run: ActiveRunSlot,
}

impl crate::runtime::model::router::RouterEventSink for RouterEventLog {
    fn emit(&self, event: JsValue) {
        if let Ok(Some(line)) = pilotdeck_protocol::stringify_json(&event) {
            use std::io::Write;
            if let Some(directory) = self.path.parent() {
                let _ = std::fs::create_dir_all(directory);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = file.write_all(format!("{line}\n").as_bytes());
            }
        }
        if event.get("type").and_then(JsValue::as_string)
            != Some(&JsString::from("pilotdeck_router_retry_progress"))
        {
            return;
        }
        let (Some(JsValue::String(session)), Some(JsValue::String(turn))) =
            (event.get("sessionId"), event.get("turnId"))
        else {
            return;
        };
        let Some(active_run) = self.active_run.get() else {
            return;
        };
        if active_run(session).as_ref() != Some(turn) {
            return;
        }
        let mut detail = JsObject::new();
        for key in [
            "attempt",
            "maxAttempts",
            "delayMs",
            "reason",
            "provider",
            "model",
        ] {
            detail.insert(
                key.into(),
                event.get(key).cloned().unwrap_or(JsValue::Undefined),
            );
        }
        let mut status = JsObject::new();
        status.insert("type".into(), "agent_status".into());
        status.insert("event".into(), "retry_progress".into());
        status.insert("detail".into(), JsValue::Object(detail));
        if let Some(emit) = self.emit.get() {
            emit(session, JsValue::Object(status));
        }
    }
}

/// `SessionRouter`'s idle options from the default runtime's `gateway`
/// section (createLocalGateway.ts:827-831): `(idleSessionTimeoutMinutes ??
/// 30) * 60_000` and `Math.max(0, idleSweepIntervalSeconds ?? 60) * 1_000`.
/// The owner compares integer idle milliseconds with `>`, so the timeout is
/// floored; a positive sweep interval follows Node's `setInterval` coercion
/// (below 1 ms or above `TIMEOUT_MAX` runs every 1 ms, fractions truncate).
fn session_idle_options(gateway: Option<&JsValue>) -> (i64, u64) {
    let number = |key: &str, fallback: f64| {
        gateway
            .and_then(|gateway| gateway.get(key))
            .and_then(JsValue::as_number)
            .filter(|value| value.is_finite())
            .unwrap_or(fallback)
    };
    let timeout = (number("idleSessionTimeoutMinutes", 30.0) * 60_000.0).floor();
    let interval = number("idleSweepIntervalSeconds", 60.0).max(0.0) * 1_000.0;
    let interval = if interval <= 0.0 {
        0
    } else if !(1.0..=2_147_483_647.0).contains(&interval) {
        1
    } else {
        interval.trunc() as u64
    };
    (timeout as i64, interval)
}

fn project_registry_with(
    options: &Options,
    env: &Env,
    on_completion: Option<crate::runtime::background::CompletionHandler>,
    router_events: Option<Arc<dyn crate::runtime::model::router::RouterEventSink>>,
) -> Arc<ProjectRuntimeRegistry> {
    // `builtinPlugins: options.builtinPlugins ?? loadBuiltinPlugins()`
    // (ProjectRuntimeRegistry.ts:1609): one scan for the process, shared by
    // every project runtime. The MCP placeholders of those plugins resolve
    // against the same directory (projectMcpSpec.ts).
    let builtin_plugins = options
        .builtin_plugins_root
        .as_deref()
        .map(crate::runtime::plugins::loader::load_builtin_plugins)
        .unwrap_or_default();
    let builtin_mcp_paths = options
        .builtin_plugins_root
        .as_deref()
        .and_then(|root| {
            crate::runtime::mcp::registry::BuiltinMcpPaths::for_builtin_plugins(root, env).ok()
        })
        .unwrap_or_default();
    Arc::new(ProjectRuntimeRegistry::new(
        options.project.clone(),
        Arc::new(ProjectResourcesFactory::new(ProjectResourcesOptions {
            pilot_home: options.home.clone(),
            cwd: options.project.clone(),
            env: env.clone(),
            homedir: Some(options.homedir()),
            builtin_skills_root: options.builtin_skills_root.clone(),
            now: Arc::new(SystemTime::now),
            telemetry: None,
            config_version: None,
            runtime_profile_overrides: RuntimeProfileOverrides::default(),
            families: Families {
                lsp: Some(Arc::new(LspFamily)),
                session_title: Some(Arc::new(TitleFamily)),
                ..Families::unwired()
            }
            .with_native_models()
            .with_router_events(router_events)
            .with_context_memory()
            .with_tools_mcp_completions(on_completion)
            .with_builtin_mcp_paths(builtin_mcp_paths)
            .with_plugins(crate::runtime::plugins::PluginRuntimeOptions {
                project_root: options.project.clone(),
                pilot_home: options.home.clone(),
                builtin_skills_root: options.builtin_skills_root.clone(),
                builtin_plugins,
                builtin_plugins_enabled: HashMap::new(),
            })
            .with_interaction(),
        })),
    ))
}

/// `mkdtemp(join(tmpdir(), "pilotdeck-sdk-ephemeral-"))`.
fn make_ephemeral_root() -> std::io::Result<PathBuf> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let base = std::env::temp_dir();
    loop {
        let suffix: String = uuid::Uuid::new_v4()
            .as_bytes()
            .iter()
            .take(6)
            .map(|byte| CHARS[usize::from(*byte) % CHARS.len()] as char)
            .collect();
        let root = base.join(format!("pilotdeck-sdk-ephemeral-{suffix}"));
        match std::fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

/// `disposeEphemeralSession` for every `persistSession: false` session when
/// the registry is disposed (ProjectRuntimeRegistry.ts:389-390,894-904): the
/// temporary storage root is removed and the session's SDK state cleared,
/// which appends a `cleared` record to the task-budget ledger when the
/// session's SDK config recorded a project.
fn dispose_ephemeral_sessions(
    roots: &Mutex<HashMap<JsString, PathBuf>>,
    overrides: &super::SessionOverrides,
    pilot_home: &std::path::Path,
) {
    let roots: Vec<(JsString, PathBuf)> = roots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .collect();
    if roots.is_empty() {
        return;
    }
    let projects = overrides.sdk_config_sessions();
    let mut ledger = crate::storage::TaskBudgetStore::new(pilot_home, None);
    for (session, root) in roots {
        let _ = std::fs::remove_dir_all(&root);
        if let Some((_, project)) = projects.iter().find(|(key, _)| *key == session) {
            let project: JsString = project.to_string_lossy().as_ref().into();
            let _ = ledger.clear_session(&project, &session);
        }
    }
}

/// `resolveAgentLoopDeploymentProfile` when it selects stdio
/// (`AgentLoopDeploymentProfile.ts:62-80`): the configured command (by
/// default this gateway's sidecar binary, which stands in for `node` running
/// the bundled sidecar script) with the sidecar path as its one argument, the
/// gateway's environment without the TCP deployment variables, started in the
/// gateway's working directory (the baseline passes no `cwd` to `spawn`). A
/// relative sidecar path resolves against the project root
/// (createLocalGateway.ts:623). `None` for the default deployment; the server
/// rejected invalid values at startup.
fn explicit_stdio_deployment(
    env: &crate::config::Env,
    sidecar: &std::path::Path,
    project_root: &std::path::Path,
) -> Option<Deployment> {
    let cwd = std::env::current_dir().ok()?;
    let Ok(Deployment::Stdio {
        args, env: child, ..
    }) = crate::runtime::turn::host_factory::resolve_deployment(env, project_root)
    else {
        return None;
    };
    let command = env
        .get("PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND")
        .map(|command| command.trim().to_owned())
        .unwrap_or_else(|| sidecar.to_string_lossy().into_owned());
    Some(Deployment::Stdio {
        command,
        args,
        cwd,
        env: child,
    })
}
