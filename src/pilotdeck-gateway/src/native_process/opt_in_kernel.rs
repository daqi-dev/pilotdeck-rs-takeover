//! Product-level `FullLate` and `ScopedResident` Kernel startup & `P7.default`
//! resolution for `pilotdeck server` and `LocalGateway::start`.
use crate::{ProxyError, config::Env};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

pub const KERNEL_EXECUTION_PROFILE_ENV: &str = "PILOTDECK_KERNEL_EXECUTION_PROFILE";
pub const KERNEL_LOOP_BIN_ENV: &str = "PILOTDECK_KERNEL_LOOP_BIN";
#[cfg(all(unix, feature = "kernel-resident"))]
pub const KERNEL_LOOP_ARGS_JSON_ENV: &str = "PILOTDECK_KERNEL_LOOP_ARGS_JSON";
#[cfg(all(unix, feature = "kernel-resident"))]
pub const KERNEL_CONTROLLER_ARGS_JSON_ENV: &str = "PILOTDECK_KERNEL_CONTROLLER_ARGS_JSON";
#[cfg(all(unix, feature = "kernel-resident"))]
pub const KERNEL_RUNTIME_ROOT_ENV: &str = "PILOTDECK_KERNEL_RUNTIME_ROOT";

const SIBLING_NATIVE_LOOP: &str = "pilotdeck-native-loop";
const DEFAULT_SIDECAR_NAME: &str = "pilotdeck-agent-loop-sidecar";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OptInKernelProfile {
    FullLate,
    ScopedResident,
    Legacy,
}

/// Fail-closed parser for `PILOTDECK_KERNEL_EXECUTION_PROFILE`.
/// Returns `Ok(None)` when unset so callers apply `P7.default` resolution.
pub(super) fn parse_kernel_profile(
    value: Option<&str>,
) -> Result<Option<OptInKernelProfile>, ProxyError> {
    match value {
        None => Ok(None),
        Some("full-late") => Ok(Some(OptInKernelProfile::FullLate)),
        Some("scoped-resident") => Ok(Some(OptInKernelProfile::ScopedResident)),
        Some("legacy") => Ok(Some(OptInKernelProfile::Legacy)),
        Some(_) => Err(ProxyError::from(
            "Kernel execution profile (PILOTDECK_KERNEL_EXECUTION_PROFILE) must be full-late, scoped-resident, or legacy.",
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum GatewayExecutionDecision {
    FullLate { program: PathBuf, sha256: String },
    ScopedResident { program: PathBuf, sha256: String },
    Legacy,
}

fn canonicalize_if_possible(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn validate_loop_binary_file(
    path: &Path,
    source_label: &str,
) -> Result<(PathBuf, String), ProxyError> {
    if !path.is_absolute() {
        return Err(format!("{source_label} must be an absolute path").into());
    }
    if path.to_string_lossy().contains('\0') {
        return Err(format!("{source_label} must not contain NUL bytes").into());
    }
    let metadata = fs::metadata(path).map_err(|error| {
        ProxyError::from(format!(
            "{source_label} `{}` is unreadable: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "{source_label} `{}` must name an existing regular file",
            path.display()
        )
        .into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "{source_label} `{}` must be an executable file",
                path.display()
            )
            .into());
        }
    }
    let bytes = fs::read(path).map_err(|error| {
        ProxyError::from(format!(
            "failed to read {source_label} `{}`: {error}",
            path.display()
        ))
    })?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok((path.to_path_buf(), sha256))
}

fn sibling_native_loop_candidates(current_exe: Option<&Path>) -> Vec<PathBuf> {
    let resolved_exe;
    let exe = match current_exe {
        Some(path) => Some(path),
        None => match std::env::current_exe() {
            Ok(path) => {
                resolved_exe = path;
                Some(resolved_exe.as_path())
            }
            Err(_) => None,
        },
    };
    let Some(exe) = exe else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    let canon = canonicalize_if_possible(exe);
    for base in [canon.as_path(), exe] {
        if let Some(dir) = base.parent().filter(|p| !p.as_os_str().is_empty()) {
            let candidate = dir.join(SIBLING_NATIVE_LOOP);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

fn default_sidecar_candidates(current_exe: Option<&Path>) -> Vec<PathBuf> {
    let resolved_exe;
    let exe = match current_exe {
        Some(path) => Some(path),
        None => match std::env::current_exe() {
            Ok(path) => {
                resolved_exe = path;
                Some(resolved_exe.as_path())
            }
            Err(_) => None,
        },
    };
    let Some(exe) = exe else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    let canon = canonicalize_if_possible(exe);
    for base in [canon.as_path(), exe] {
        if let Some(dir) = base.parent().filter(|p| !p.as_os_str().is_empty()) {
            let candidate = dir.join(DEFAULT_SIDECAR_NAME);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

fn is_default_sidecar_path(sidecar: &Path, current_exe: Option<&Path>) -> bool {
    if sidecar == Path::new(DEFAULT_SIDECAR_NAME) {
        return true;
    }
    let canon_sidecar = canonicalize_if_possible(sidecar);
    default_sidecar_candidates(current_exe)
        .into_iter()
        .any(|candidate| {
            candidate == sidecar || canonicalize_if_possible(&candidate) == canon_sidecar
        })
}

pub(super) fn has_explicit_external_sidecar(
    sidecar: &Path,
    env: &Env,
    project_has_agent_loop_module: bool,
    current_exe: Option<&Path>,
) -> bool {
    if project_has_agent_loop_module {
        return true;
    }
    if env
        .get("PILOTDECK_AGENT_LOOP_TRANSPORT")
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| !value.is_empty() && value != "native")
    {
        return true;
    }
    if env
        .get("PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND")
        .is_some_and(|value| !value.trim().is_empty())
    {
        return true;
    }
    if env
        .get("PILOTDECK_AGENT_LOOP_SIDECAR_PATH")
        .is_some_and(|value| !value.trim().is_empty())
    {
        return true;
    }
    if !fs::metadata(sidecar).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let file_name = sidecar.file_name().and_then(OsStr::to_str).unwrap_or("");
    if file_name.contains(SIBLING_NATIVE_LOOP) {
        return false;
    }
    !is_default_sidecar_path(sidecar, current_exe)
}

fn has_enabled_agent_loop_module(snapshot: &pilotdeck_protocol::JsValue) -> bool {
    let Some(binding) = snapshot
        .get("config")
        .and_then(|config| config.get("modules"))
        .and_then(|modules| modules.get("agentLoop"))
    else {
        return false;
    };
    binding.get("enabled") == Some(&pilotdeck_protocol::JsValue::Bool(true))
        && binding.get("implementationId").is_some()
}

pub(super) fn resolve_loop_binary_with(
    profile_name: &str,
    env: &Env,
    required: bool,
    current_exe: Option<&Path>,
) -> Result<Option<(PathBuf, String)>, ProxyError> {
    if let Some(raw) = env.get(KERNEL_LOOP_BIN_ENV) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "{profile_name} profile requires {KERNEL_LOOP_BIN_ENV} to name an absolute executable file"
            )
            .into());
        }
        if raw.contains('\0') {
            return Err(format!("{KERNEL_LOOP_BIN_ENV} must not contain NUL bytes").into());
        }
        let path = PathBuf::from(trimmed);
        return validate_loop_binary_file(&path, KERNEL_LOOP_BIN_ENV).map(Some);
    }

    for candidate in sibling_native_loop_candidates(current_exe) {
        if fs::metadata(&candidate).is_ok_and(|meta| meta.is_file()) {
            return validate_loop_binary_file(&candidate, "sibling pilotdeck-native-loop")
                .map(Some);
        }
    }

    if required {
        return Err(ProxyError::from(format!(
            "{profile_name} profile requires {KERNEL_LOOP_BIN_ENV} to name an absolute executable file or sibling {SIBLING_NATIVE_LOOP} executable"
        )));
    }
    Ok(None)
}

pub(super) fn resolve_gateway_execution_with(
    sidecar: &Path,
    env: &Env,
    project_has_agent_loop_module: bool,
    current_exe: Option<&Path>,
) -> Result<GatewayExecutionDecision, ProxyError> {
    match parse_kernel_profile(env.get(KERNEL_EXECUTION_PROFILE_ENV).map(String::as_str))? {
        Some(OptInKernelProfile::Legacy) => Ok(GatewayExecutionDecision::Legacy),
        Some(OptInKernelProfile::FullLate) => {
            let (program, sha256) = resolve_loop_binary_with("FullLate", env, true, current_exe)?
                .expect("required=true guarantees Some");
            Ok(GatewayExecutionDecision::FullLate { program, sha256 })
        }
        Some(OptInKernelProfile::ScopedResident) => {
            let (program, sha256) =
                resolve_loop_binary_with("ScopedResident", env, true, current_exe)?
                    .expect("required=true guarantees Some");
            Ok(GatewayExecutionDecision::ScopedResident { program, sha256 })
        }
        None => {
            if has_explicit_external_sidecar(
                sidecar,
                env,
                project_has_agent_loop_module,
                current_exe,
            ) {
                return Ok(GatewayExecutionDecision::Legacy);
            }
            match resolve_loop_binary_with("FullLate", env, false, current_exe)? {
                Some((program, sha256)) => {
                    Ok(GatewayExecutionDecision::FullLate { program, sha256 })
                }
                None => Ok(GatewayExecutionDecision::Legacy),
            }
        }
    }
}

pub(super) fn resolve_gateway_execution(
    sidecar: &Path,
    env: &Env,
    pilot_home: &Path,
    project: &Path,
) -> Result<GatewayExecutionDecision, ProxyError> {
    let project_has_agent_loop_module =
        crate::config::load_snapshot(pilot_home, project, env, Some("."), 1.0, "")
            .ok()
            .is_some_and(|snapshot| has_enabled_agent_loop_module(&snapshot));
    resolve_gateway_execution_with(sidecar, env, project_has_agent_loop_module, None)
}

/// Gate checked by non-`kernel-resident` / legacy fallback builds so explicit
/// Kernel profile or auto-discovered `FullLate` fails closed rather than silently
/// downgrading to legacy execution.
#[cfg_attr(all(unix, feature = "kernel-resident"), allow(dead_code))]
pub(super) fn ensure_opt_in_allowed(
    sidecar: &Path,
    env: &Env,
    pilot_home: &Path,
    project: &Path,
) -> Result<(), ProxyError> {
    match resolve_gateway_execution(sidecar, env, pilot_home, project)? {
        GatewayExecutionDecision::Legacy => Ok(()),
        GatewayExecutionDecision::FullLate { .. } => Err(ProxyError::from(
            "FullLate Kernel execution requires building with --features kernel-resident",
        )),
        GatewayExecutionDecision::ScopedResident { .. } => Err(ProxyError::from(
            "ScopedResident Kernel execution requires building with --features kernel-resident",
        )),
    }
}

/// Server opt-in / P7.default hook: returns `None` only when resolution selects `Legacy`.
#[cfg(not(all(unix, feature = "kernel-resident")))]
pub(super) fn maybe_serve_with_scoped_kernel<F>(
    options: &super::ServerOptions,
    _listening: &mut Option<F>,
) -> Option<Result<(), ProxyError>>
where
    F: FnOnce(&super::Listening),
{
    match ensure_opt_in_allowed(
        &options.sidecar,
        &options.env,
        &options.pilot_home,
        &options.project,
    ) {
        Ok(()) => None,
        Err(error) => Some(Err(error)),
    }
}

#[cfg(all(unix, feature = "kernel-resident"))]
mod enabled {
    use super::*;
    use crate::{
        native_process::{
            AuthenticatedKernelConnections, DeploymentKernelConfig, GatewayManagedModelSetup,
            KernelExecutionProfile, KernelSessionBinder, KernelTurnOptions, Listening,
            LocalOptions, ManagedModuleFailure, ManagedModuleModel, ManagedModulePrincipal,
            ManagedModuleResponse, ManagedSessionBinder, NativeLoopImage, NativeProductReads,
            NativeToolBatchSetup, OriginalProjectCatalogPolicy, OriginalProjectTurnAuthority,
            ResidentProductInputs, ScopedResidentProductInputs, ScopedResidentSessionBinder,
            ServerOptions, serve_with_resident_kernel_with_profile,
        },
        runtime::{
            project::RuntimeError,
            turn::kernel_client::{KernelExecutionOptions, scoped::ResidentSignalState},
        },
    };
    use agentcpm_host::{
        ExternalBackends, ResidentComposition, SchedulerAssembly, SchedulerComponent,
        SchedulerImplementation, StartupOptions, TrustedReadBinding, TrustedReadProvider,
    };
    use agentcpm_scheduler_protocol as p;
    use pilotdeck_http_transport::transport::ProxySettings;
    use pilotdeck_iface::{
        context_codec::InstalledContextOperation, native_loop_launch::NativeLoopLaunch,
        recovery_binding::RecoveryPolicy, recovery_composition::RecoveryInstallation,
        recovery_launch::RecoveryLaunch,
    };
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    const LOOP_BINDING: &str = "pilotdeck";
    const CONTROLLER_BINDING: &str = "controller";
    const CTX_PREPARE_BINDING: &str = "ctx-prepare";
    const CTX_APPLY_TOOLS_BINDING: &str = "ctx-apply-tools";
    const CTX_CAPTURE_BINDING: &str = "ctx-capture";
    const CTX_COMPACT_BINDING: &str = "ctx-compact";
    const CTX_RECOVERY_BINDING: &str = "ctx-recovery";

    const KERNEL_PRINCIPAL: &str = "gateway";
    const KERNEL_TENANT: &str = "pilotdeck";
    const IFACE_PACKAGE_NAME: &str = "pilotdeck-native-iface";
    const RPC_TIMEOUT_MS: u64 = 15_000;
    const READY_TIMEOUT: Duration = Duration::from_secs(60);
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);
    const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_UDS_PATH_BYTES: usize = 100;

    static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(1);

    const KERNEL_ALLOW_OPS: &str = concat!(
        "session.new,session.create,session.close,session.lease_acquire,",
        "session.lease_renew,session.get,session.deliver,session.stream_open,",
        "execution.open,execution.input,execution.get,execution.events,",
        "execution.output_ack,execution.control,execution.respond,",
        "tool.exec,tool.list,tool.authorize,",
        "tool.prepare_batch,tool.batch_status,tool.batch_close,",
        "model.session_open,model.session_get,model.session_update,",
        "model.input_append,model.input_commit,",
        "model.events_read,model.delivery_advance,model.session_close,model.session_abort,",
        "model.response_interrupt,product.plan_todo.read,product.model.metadata,",
        "context.history.v2.read,context.history.v2.prepare,context.history.v2.commit,",
        "context.history.v2.receipt,context.history.v2.facts,",
        "context.operations.dispatch,context.operations.commit,context.operations.status,",
        "context.operations.settle_failed",
    );

    const LOOP_RESOURCES: &[&str] = &[
        "model.session_open",
        "model.session_get",
        "model.input_append",
        "model.input_commit",
        "model.events_read",
        "model.delivery_advance",
        "model.session_close",
        "model.session_abort",
        p::tool_batch::PREPARE,
        p::tool_batch::STATUS,
        p::tool_batch::CLOSE,
        p::product_reads::PLAN_TODO,
        p::product_reads::MODEL_METADATA,
    ];

    const FULL_LATE_LOOP_RESOURCES: &[&str] = &[
        "model.session_open",
        "model.session_get",
        "model.session_update",
        "model.input_append",
        "model.input_commit",
        "model.events_read",
        "model.delivery_advance",
        "model.session_close",
        "model.session_abort",
        "model.response_interrupt",
        "tool.list",
        "tool.authorize",
        "tool.exec",
        p::tool_batch::PREPARE,
        p::tool_batch::STATUS,
        p::tool_batch::CLOSE,
        p::product_reads::PLAN_TODO,
        p::product_reads::MODEL_METADATA,
        p::context_history::PREPARE,
        p::context_history::COMMIT,
        p::context_history::RECEIPT,
        p::context_history::READ,
        p::context_history::FACTS,
        p::context_operations::DISPATCH,
        p::context_operations::COMMIT,
        p::context_operations::STATUS,
        p::context_operations::failure::SETTLE,
    ];

    const NON_RECOVERY_CONTEXT_ROLES: &[(&str, p::context_operations::Operation)] = &[
        (
            CTX_PREPARE_BINDING,
            p::context_operations::Operation::PrepareForModel,
        ),
        (
            CTX_APPLY_TOOLS_BINDING,
            p::context_operations::Operation::ApplyToolResults,
        ),
        (
            CTX_CAPTURE_BINDING,
            p::context_operations::Operation::CaptureTurn,
        ),
        (
            CTX_COMPACT_BINDING,
            p::context_operations::Operation::TryAutoCompact,
        ),
    ];

    /// Removes an auto-created short runtime root once the resident host has exited.
    pub(super) struct RuntimeRootGuard {
        owned_dir: Option<PathBuf>,
    }

    impl RuntimeRootGuard {
        fn defuse(&mut self) -> Option<PathBuf> {
            self.owned_dir.take()
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            if let Some(dir) = self.owned_dir.take() {
                let _ = fs::remove_dir_all(dir);
            }
        }
    }

    #[derive(Clone, Debug)]
    enum ResidentExit {
        Code(i32),
        Panic(String),
    }

    fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into())
    }

    fn exit_text(value: &ResidentExit) -> String {
        match value {
            ResidentExit::Code(code) => format!("Kernel resident exited with status {code}"),
            ResidentExit::Panic(message) => format!("Kernel resident panicked: {message}"),
        }
    }

    /// Owned embedded `agentcpm-host` lifecycle for `LocalGateway`.
    pub(in crate::native_process) struct OwnedScopedResidentHost {
        stop: agentcpm_host::ResidentShutdown,
        exit_rx: tokio::sync::watch::Receiver<Option<ResidentExit>>,
        reaper: Option<std::thread::JoinHandle<()>>,
        signal_state: Arc<ResidentSignalState>,
        shutdown_timeout: Duration,
        root_guard: RuntimeRootGuard,
    }

    impl OwnedScopedResidentHost {
        pub(in crate::native_process) fn shutdown_blocking(
            mut self,
            runtime: &tokio::runtime::Runtime,
            budget: Duration,
        ) -> Result<(), String> {
            self.signal_state.mark_gateway_stopping();
            let _ = self.stop.request_shutdown();
            let timeout = self.shutdown_timeout.max(budget);
            let mut exit_rx = self.exit_rx.clone();
            let exit = runtime
                .block_on(async { tokio::time::timeout(timeout, wait_exit(&mut exit_rx)).await });
            let result = match exit {
                Ok(ResidentExit::Code(0)) => {
                    if let Some(reaper) = self.reaper.take() {
                        let _ = reaper.join();
                    }
                    Ok(())
                }
                Ok(other) => {
                    if let Some(reaper) = self.reaper.take() {
                        let _ = reaper.join();
                    }
                    Err(exit_text(&other))
                }
                Err(_) => Err("Kernel resident shutdown remains unconfirmed".to_owned()),
            };
            if result.is_err() {
                let _ = self.root_guard.defuse();
            }
            result
        }

        pub(in crate::native_process) fn shutdown_on_drop(mut self) {
            self.stop_and_join();
        }

        fn stop_and_join(&mut self) {
            if self.reaper.is_some() {
                self.signal_state.mark_gateway_stopping();
                let _ = self.stop.request_shutdown();
                if let Some(reaper) = self.reaper.take() {
                    let _ = reaper.join();
                }
            }
        }
    }

    impl Drop for OwnedScopedResidentHost {
        fn drop(&mut self) {
            self.stop_and_join();
        }
    }

    struct RejectOptInModuleModel;
    impl ManagedModuleModel for RejectOptInModuleModel {
        fn call<'a>(
            &'a self,
            _principal: &'a ManagedModulePrincipal,
            _operation: &'a str,
            _input: &'a pilotdeck_protocol::JsValue,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<ManagedModuleResponse, ManagedModuleFailure>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Err(ManagedModuleFailure {
                    status: Some(403),
                    code: Some("UNMAPPED_MODULE_PRINCIPAL".into()),
                    message: Some(
                        "module-host model RPC is unavailable without an explicit principal mapping"
                            .into(),
                    ),
                })
            })
        }
    }

    struct ResolvedOptInSite {
        startup: StartupOptions,
        deployment: DeploymentKernelConfig,
        proxy: Option<ProxySettings>,
        root_guard: RuntimeRootGuard,
        project_utf8: String,
    }

    fn parse_args_json_env(env: &Env, key: &str) -> Result<Option<Vec<String>>, ProxyError> {
        let Some(raw) = env.get(key) else {
            return Ok(None);
        };
        let parsed: Vec<String> = serde_json::from_str(raw).map_err(|error| {
            ProxyError::from(format!("{key} must be a JSON array of strings: {error}"))
        })?;
        if parsed.is_empty() || parsed.iter().any(|arg| arg.contains('\0')) {
            return Err(format!("{key} must be a non-empty array of NUL-free strings").into());
        }
        Ok(Some(parsed))
    }

    pub(super) fn default_loop_args(program: &Path, project_utf8: &str) -> Vec<String> {
        let filename = program
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if filename.contains(SIBLING_NATIVE_LOOP) {
            let payload = serde_json::json!({
                "profile": "pilotdeck.scoped-native-loop.v1",
                "loop_binding": LOOP_BINDING,
                "controller_binding": CONTROLLER_BINDING,
                "project_dir": project_utf8,
            });
            vec![
                "--pilotdeck-scoped-native-loop-v1".to_owned(),
                payload.to_string(),
            ]
        } else {
            vec![
                "--native-model-tool-loop".to_owned(),
                project_utf8.to_owned(),
            ]
        }
    }

    pub(super) fn default_controller_args(program: &Path) -> Vec<String> {
        let filename = program
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if filename.contains(SIBLING_NATIVE_LOOP) {
            vec![
                "--pilotdeck-product-controller-v1".to_owned(),
                "{\"profile\":\"pilotdeck.product-controller.v1\",\"loop_binding\":\"pilotdeck\",\"controller_binding\":\"controller\"}"
                    .to_owned(),
            ]
        } else {
            vec!["--p2-product-controller".to_owned()]
        }
    }

    pub(super) fn default_context_operation_args(
        binding: &str,
        operation: p::context_operations::Operation,
        project_utf8: &str,
    ) -> Vec<String> {
        let payload = serde_json::json!({
            "profile": "pilotdeck.context-operation.v1",
            "binding": binding,
            "operation": operation,
            "project_dir": project_utf8,
        });
        vec![
            "--pilotdeck-context-operation-v1".to_owned(),
            payload.to_string(),
        ]
    }

    fn resolve_abs_project_utf8(project: &Path) -> Result<String, ProxyError> {
        let abs_project = if project.is_absolute() {
            project.to_path_buf()
        } else {
            std::env::current_dir()?.join(project)
        };
        abs_project
            .to_str()
            .filter(|text| !text.contains('\0'))
            .map(str::to_owned)
            .ok_or_else(|| ProxyError::from("project path must be valid NUL-free UTF-8"))
    }

    fn resolve_scoped_args(
        env: &Env,
        program: &Path,
        project_utf8: &str,
    ) -> Result<(Vec<String>, Vec<String>), ProxyError> {
        let loop_args = match parse_args_json_env(env, KERNEL_LOOP_ARGS_JSON_ENV)? {
            Some(args) => args,
            None => default_loop_args(program, project_utf8),
        };
        let controller_args = match parse_args_json_env(env, KERNEL_CONTROLLER_ARGS_JSON_ENV)? {
            Some(args) => args,
            None => default_controller_args(program),
        };
        Ok((loop_args, controller_args))
    }

    fn resolve_controller_args(env: &Env, program: &Path) -> Result<Vec<String>, ProxyError> {
        match parse_args_json_env(env, KERNEL_CONTROLLER_ARGS_JSON_ENV)? {
            Some(args) => Ok(args),
            None => Ok(default_controller_args(program)),
        }
    }

    fn resolve_runtime_root(env: &Env) -> Result<(PathBuf, RuntimeRootGuard), ProxyError> {
        let (root, guard) = match env.get(KERNEL_RUNTIME_ROOT_ENV) {
            Some(raw) => {
                if raw.trim().is_empty() || raw.contains('\0') {
                    return Err(format!(
                        "{KERNEL_RUNTIME_ROOT_ENV} must be a non-empty NUL-free absolute path"
                    )
                    .into());
                }
                let raw_path = PathBuf::from(raw);
                if !raw_path.is_absolute() {
                    return Err(
                        format!("{KERNEL_RUNTIME_ROOT_ENV} must be an absolute path").into(),
                    );
                }
                fs::create_dir_all(&raw_path)?;
                let canonical = fs::canonicalize(&raw_path).unwrap_or(raw_path);
                (canonical, RuntimeRootGuard { owned_dir: None })
            }
            None => {
                let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
                let raw_path = PathBuf::from(format!("/tmp/pd-kr-{short}"));
                let mut builder = fs::DirBuilder::new();
                builder.recursive(true).mode(0o700);
                builder.create(&raw_path)?;
                let guard = RuntimeRootGuard {
                    owned_dir: Some(raw_path.clone()),
                };
                let canonical = fs::canonicalize(&raw_path).unwrap_or(raw_path);
                (canonical, guard)
            }
        };
        let socket = root.join("k.s");
        if socket.as_os_str().len() >= MAX_UDS_PATH_BYTES {
            return Err(format!(
                "Kernel UDS socket path `{}` exceeds Unix SUN_LEN safety limit ({MAX_UDS_PATH_BYTES} bytes)",
                socket.display()
            )
            .into());
        }
        Ok((root, guard))
    }

    fn write_atomic_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), ProxyError> {
        let parent = path
            .parent()
            .ok_or_else(|| ProxyError::from("atomic file path requires a parent directory"))?;
        let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
        let temp = parent.join(format!(".{name}.tmp.{}.{seq}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(mode);
        let mut file = options.open(&temp)?;
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        drop(file);
        if let Err(error) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        fs::set_permissions(path, PermissionsExt::from_mode(mode))?;
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn prepare_runtime_site(runtime_root: &Path) -> Result<(StartupOptions, PathBuf), ProxyError> {
        let data_dir = runtime_root.join("data");
        let storage_dir = data_dir.join("storage");
        let components_dir = runtime_root.join("components");
        let iface_dir = components_dir.join(IFACE_PACKAGE_NAME);
        for dir in [&data_dir, &storage_dir, &components_dir, &iface_dir] {
            fs::create_dir_all(dir)?;
        }

        let ledger = serde_json::json!({
            "schema": "agentcpm.kernel/install@1",
            "packages": [{
                "name": IFACE_PACKAGE_NAME,
                "version": "0.0.0",
                "kind": "iface",
                "dir": IFACE_PACKAGE_NAME,
                "provides": { "interfaces": ["env"] },
                "sha256": {}
            }]
        })
        .to_string();
        write_atomic_file(
            &components_dir.join("install.json"),
            ledger.as_bytes(),
            0o644,
        )?;

        let config_path = runtime_root.join("kernel.toml");
        let kernel_toml = format!(
            "storage_dir = \"data/storage\"\n\
             guard = \"deny-all\"\n\
             exec_backend = \"none\"\n\
             allow_ops = \"{KERNEL_ALLOW_OPS}\"\n"
        );
        write_atomic_file(&config_path, kernel_toml.as_bytes(), 0o644)?;

        let socket = runtime_root.join("k.s");
        Ok((
            StartupOptions {
                config: Some(config_path),
                data_dir,
                components_dir,
            },
            socket,
        ))
    }

    fn resolve_proxy_settings(
        pilot_home: &Path,
        project: &Path,
        env: &Env,
    ) -> Option<ProxySettings> {
        let snapshot =
            crate::config::load_snapshot(pilot_home, project, env, Some("."), 1.0, "").ok()?;
        let config = snapshot.get("config")?;
        let proxy = config.get("proxy")?;
        ProxySettings::from_config(proxy)
    }

    fn resolve_opt_in_site(
        env: &Env,
        pilot_home: &Path,
        project: &Path,
    ) -> Result<ResolvedOptInSite, ProxyError> {
        let project_utf8 = resolve_abs_project_utf8(project)?;
        let (runtime_root, root_guard) = resolve_runtime_root(env)?;
        let (startup, socket) = prepare_runtime_site(&runtime_root)?;
        let deployment = DeploymentKernelConfig {
            socket,
            expected_uid: rustix::process::geteuid().as_raw(),
            principal: KERNEL_PRINCIPAL.into(),
            tenant: KERNEL_TENANT.into(),
            rpc_timeout_ms: RPC_TIMEOUT_MS,
        };
        deployment
            .validate()
            .map_err(|e| ProxyError::from(e.message))?;
        let proxy = resolve_proxy_settings(pilot_home, project, env);
        Ok(ResolvedOptInSite {
            startup,
            deployment,
            proxy,
            root_guard,
            project_utf8,
        })
    }

    struct ComponentSpec<'a> {
        id: &'a str,
        kind: p::Kind,
        effect: p::Effect,
        invoke: std::collections::BTreeSet<String>,
        publish: bool,
        resources: &'a [&'a str],
        program: PathBuf,
        args: Vec<String>,
        sha256: String,
        controller: bool,
    }

    fn scheduler_component(spec: ComponentSpec<'_>) -> SchedulerComponent {
        SchedulerComponent {
            binding: p::Binding {
                id: spec.id.into(),
                revision: "opt-in-v1".into(),
                kind: spec.kind,
                effect: spec.effect,
                permissions: p::Permissions {
                    invoke: spec.invoke,
                    publish: spec.publish,
                    state_namespace: None,
                    resources: spec.resources.iter().map(|v| (*v).into()).collect(),
                },
            },
            provider: SchedulerImplementation::Native {
                program: spec.program,
                args: spec.args,
                sha256: spec.sha256,
            },
            controller: spec.controller,
        }
    }

    fn scoped_profile() -> p::SessionConfig {
        p::SessionConfig {
            input_controller_ref: Some(CONTROLLER_BINDING.into()),
            default_loop_ref: LOOP_BINDING.into(),
            allowed_bindings: [LOOP_BINDING.into(), CONTROLLER_BINDING.into()].into(),
        }
    }

    fn full_late_profile() -> p::SessionConfig {
        p::SessionConfig {
            input_controller_ref: Some(CONTROLLER_BINDING.into()),
            default_loop_ref: LOOP_BINDING.into(),
            allowed_bindings: [
                LOOP_BINDING.into(),
                CONTROLLER_BINDING.into(),
                CTX_PREPARE_BINDING.into(),
                CTX_APPLY_TOOLS_BINDING.into(),
                CTX_CAPTURE_BINDING.into(),
                CTX_COMPACT_BINDING.into(),
                CTX_RECOVERY_BINDING.into(),
            ]
            .into(),
        }
    }

    fn turn_options() -> KernelTurnOptions {
        Arc::new(|_, _| {
            Ok(KernelExecutionOptions {
                loop_binding: LOOP_BINDING.into(),
                controller_binding: CONTROLLER_BINDING.into(),
                module_id: "kernel-native".into(),
                cleanup_timeout: CLEANUP_TIMEOUT,
            })
        })
    }

    struct FullLateAssemblyPieces {
        assembly: SchedulerAssembly,
        native_loop: NativeLoopImage,
        recovery: RecoveryInstallation,
        other_context_roles: Vec<p::context_operations::ContextBinding>,
        loop_binding_def: p::Binding,
    }

    fn build_full_late_assembly(
        program: &Path,
        sha256: &str,
        controller_args: Vec<String>,
        project_utf8: &str,
    ) -> Result<FullLateAssemblyPieces, ProxyError> {
        let controller_component = scheduler_component(ComponentSpec {
            id: CONTROLLER_BINDING,
            kind: p::Kind::Service,
            effect: p::Effect::State,
            invoke: [LOOP_BINDING.into()].into(),
            publish: true,
            resources: &[],
            program: program.to_path_buf(),
            args: controller_args,
            sha256: sha256.to_owned(),
            controller: true,
        });

        let mut bindings = vec![controller_component];
        let mut other_context_roles = Vec::with_capacity(NON_RECOVERY_CONTEXT_ROLES.len());
        for &(binding_id, operation) in NON_RECOVERY_CONTEXT_ROLES {
            let component = scheduler_component(ComponentSpec {
                id: binding_id,
                kind: p::Kind::Capability,
                effect: p::Effect::Read,
                invoke: Default::default(),
                publish: false,
                resources: &[],
                program: program.to_path_buf(),
                args: default_context_operation_args(binding_id, operation, project_utf8),
                sha256: sha256.to_owned(),
                controller: false,
            });
            bindings.push(component);
            other_context_roles.push(p::context_operations::ContextBinding {
                binding_ref: binding_id.into(),
                operation,
            });
        }

        let recovery_launch = RecoveryLaunch::new(
            program.to_path_buf(),
            sha256.to_owned(),
            CTX_RECOVERY_BINDING.to_owned(),
            RecoveryPolicy::LegacyFallback {
                first_keep_ratio: 0.5,
            },
        )
        .map_err(|err| ProxyError::from(format!("invalid recovery launch config: {err:?}")))?;
        let recovery_binding = p::Binding {
            id: CTX_RECOVERY_BINDING.into(),
            revision: "opt-in-v1".into(),
            kind: p::Kind::Capability,
            effect: p::Effect::Read,
            permissions: p::Permissions::calls([]),
        };
        let recovery = RecoveryInstallation::new(recovery_binding, &recovery_launch)
            .map_err(|err| ProxyError::from(format!("invalid recovery installation: {err:?}")))?;

        let loop_invoke = [
            CONTROLLER_BINDING.into(),
            CTX_PREPARE_BINDING.into(),
            CTX_APPLY_TOOLS_BINDING.into(),
            CTX_CAPTURE_BINDING.into(),
            CTX_COMPACT_BINDING.into(),
            CTX_RECOVERY_BINDING.into(),
        ]
        .into();
        let loop_binding_def = p::Binding {
            id: LOOP_BINDING.into(),
            revision: "opt-in-v1".into(),
            kind: p::Kind::Loop,
            effect: p::Effect::ToolWrite,
            permissions: p::Permissions {
                invoke: loop_invoke,
                publish: false,
                state_namespace: None,
                resources: FULL_LATE_LOOP_RESOURCES
                    .iter()
                    .map(|value| (*value).into())
                    .collect(),
            },
        };

        let assembly = SchedulerAssembly {
            protocol: p::PROTOCOL.into(),
            profile: full_late_profile(),
            bindings,
        };
        let native_loop = NativeLoopImage {
            binding: loop_binding_def.clone(),
            program: program.to_path_buf(),
            sha256: sha256.to_owned(),
        };

        Ok(FullLateAssemblyPieces {
            assembly,
            native_loop,
            recovery,
            other_context_roles,
            loop_binding_def,
        })
    }

    pub(in crate::native_process) fn build_full_late_server_inputs(
        options: &ServerOptions,
        program: PathBuf,
        sha256: String,
    ) -> Result<(ResidentProductInputs, RuntimeRootGuard), ProxyError> {
        let site = resolve_opt_in_site(&options.env, &options.pilot_home, &options.project)?;
        let controller_args = resolve_controller_args(&options.env, &program)?;
        let pieces =
            build_full_late_assembly(&program, &sha256, controller_args, &site.project_utf8)?;
        let inputs = ResidentProductInputs {
            deployment: site.deployment,
            startup: site.startup,
            external: ExternalBackends {
                scheduler_assembly: Some(pieces.assembly),
                ..Default::default()
            },
            native_loop: pieces.native_loop,
            loop_binding: LOOP_BINDING.into(),
            controller_binding: CONTROLLER_BINDING.into(),
            recovery: pieces.recovery,
            other_context_roles: pieces.other_context_roles,
            model_authority: Arc::new(OriginalProjectTurnAuthority),
            tool_batches: Arc::new(NativeToolBatchSetup),
            catalog: Arc::new(OriginalProjectCatalogPolicy),
            turn_options: turn_options(),
            module_model: Arc::new(RejectOptInModuleModel),
            proxy: site.proxy,
            ready_timeout: READY_TIMEOUT,
            shutdown_timeout: SHUTDOWN_TIMEOUT,
        };
        Ok((inputs, site.root_guard))
    }

    fn build_scoped_server_inputs(
        options: &ServerOptions,
        program: PathBuf,
        sha256: String,
    ) -> Result<(ScopedResidentProductInputs, RuntimeRootGuard), ProxyError> {
        let site = resolve_opt_in_site(&options.env, &options.pilot_home, &options.project)?;
        let (loop_args, controller_args) =
            resolve_scoped_args(&options.env, &program, &site.project_utf8)?;
        let loop_component = scheduler_component(ComponentSpec {
            id: LOOP_BINDING,
            kind: p::Kind::Loop,
            effect: p::Effect::ToolWrite,
            invoke: [CONTROLLER_BINDING.into()].into(),
            publish: false,
            resources: LOOP_RESOURCES,
            program: program.clone(),
            args: loop_args.clone(),
            sha256: sha256.clone(),
            controller: false,
        });
        let controller_component = scheduler_component(ComponentSpec {
            id: CONTROLLER_BINDING,
            kind: p::Kind::Service,
            effect: p::Effect::State,
            invoke: [LOOP_BINDING.into()].into(),
            publish: true,
            resources: &[],
            program: program.clone(),
            args: controller_args,
            sha256: sha256.clone(),
            controller: true,
        });
        let assembly = SchedulerAssembly {
            protocol: p::PROTOCOL.into(),
            profile: scoped_profile(),
            bindings: vec![controller_component],
        };
        let inputs = ScopedResidentProductInputs {
            deployment: site.deployment,
            startup: site.startup,
            external: ExternalBackends {
                scheduler_assembly: Some(assembly),
                ..Default::default()
            },
            native_loop: NativeLoopImage {
                binding: loop_component.binding,
                program,
                sha256,
            },
            loop_args,
            loop_binding: LOOP_BINDING.into(),
            controller_binding: CONTROLLER_BINDING.into(),
            model_authority: Arc::new(OriginalProjectTurnAuthority),
            tool_batches: Arc::new(NativeToolBatchSetup),
            catalog: Arc::new(OriginalProjectCatalogPolicy),
            turn_options: turn_options(),
            module_model: Arc::new(RejectOptInModuleModel),
            proxy: site.proxy,
            ready_timeout: READY_TIMEOUT,
            shutdown_timeout: SHUTDOWN_TIMEOUT,
        };
        Ok((inputs, site.root_guard))
    }

    pub(in crate::native_process) fn maybe_serve_with_scoped_kernel<F>(
        options: &ServerOptions,
        listening: &mut Option<F>,
    ) -> Option<Result<(), ProxyError>>
    where
        F: FnOnce(&Listening),
    {
        let decision = match resolve_gateway_execution(
            &options.sidecar,
            &options.env,
            &options.pilot_home,
            &options.project,
        ) {
            Ok(decision) => decision,
            Err(error) => return Some(Err(error)),
        };
        match decision {
            GatewayExecutionDecision::Legacy => None,
            GatewayExecutionDecision::FullLate { program, sha256 } => {
                let (inputs, _root_guard) =
                    match build_full_late_server_inputs(options, program, sha256) {
                        Ok(built) => built,
                        Err(error) => return Some(Err(error)),
                    };
                let listening = listening
                    .take()
                    .expect("server listening callback present for FullLate kernel");
                Some(serve_with_resident_kernel_with_profile(
                    options.clone(),
                    inputs,
                    KernelExecutionProfile::FullLate,
                    listening,
                ))
            }
            GatewayExecutionDecision::ScopedResident { program, sha256 } => {
                let (inputs, _root_guard) =
                    match build_scoped_server_inputs(options, program, sha256) {
                        Ok(built) => built,
                        Err(error) => return Some(Err(error)),
                    };
                let listening = listening
                    .take()
                    .expect("server listening callback present for scoped-resident opt-in");
                Some(serve_with_resident_kernel_with_profile(
                    options.clone(),
                    inputs,
                    KernelExecutionProfile::ScopedResident,
                    listening,
                ))
            }
        }
    }

    async fn wait_exit(
        receiver: &mut tokio::sync::watch::Receiver<Option<ResidentExit>>,
    ) -> ResidentExit {
        match receiver.wait_for(|value| value.is_some()).await {
            Ok(value) => value.clone().expect("predicate confirms exit"),
            Err(_) => ResidentExit::Panic("resident join monitor disappeared".into()),
        }
    }

    async fn await_ready(
        ready: tokio::sync::oneshot::Receiver<agentcpm_host::ReadyNotice>,
        exits: &mut tokio::sync::watch::Receiver<Option<ResidentExit>>,
        expected_socket: &Path,
        deadline: Duration,
    ) -> Result<(), String> {
        let startup = tokio::time::timeout(deadline, async {
            tokio::select! {
                ready = ready => ready.map_err(|_| "Kernel resident ended before ready".to_owned()),
                exited = wait_exit(exits) => Err(exit_text(&exited)),
            }
        })
        .await;
        match startup {
            Ok(Ok(notice))
                if notice.uds.as_deref() == Some(expected_socket) && exits.borrow().is_none() =>
            {
                Ok(())
            }
            Ok(Ok(_)) => Err("Kernel resident ready did not match the configured live UDS".into()),
            Ok(Err(message)) => Err(message),
            Err(_) => {
                Err("Kernel resident did not become ready before the configured deadline".into())
            }
        }
    }

    pub(in crate::native_process) type StartedLocalScopedKernel =
        (Arc<dyn ManagedSessionBinder>, OwnedScopedResidentHost);

    fn start_local_composition(
        site: ResolvedOptInSite,
        expected_loop: p::Binding,
        composition: ResidentComposition,
        runtime: &tokio::runtime::Runtime,
        full_late: bool,
    ) -> Result<StartedLocalScopedKernel, ProxyError> {
        let provider: Arc<dyn TrustedReadProvider> = Arc::new(NativeProductReads);
        let reads = [
            p::product_reads::PLAN_TODO,
            p::product_reads::MODEL_METADATA,
        ]
        .into_iter()
        .map(|method| TrustedReadBinding {
            method: method.into(),
            callers: vec![expected_loop.clone()],
            profiles: vec![pilotdeck_protocol::boundary_codec::PROFILE.into()],
            provider: provider.clone(),
        })
        .collect();

        let model_setup = Arc::new(GatewayManagedModelSetup::new(
            site.proxy,
            Arc::new(OriginalProjectTurnAuthority),
            runtime.handle().clone(),
        ));
        let composition = composition
            .with_trusted_reads(reads)
            .with_managed_model(model_setup.clone())
            .with_trusted_tool_batches(Arc::new(NativeToolBatchSetup));
        let (composition, control) = composition.with_shutdown_control();

        let mut host_options = site
            .deployment
            .host_options(site.startup)
            .map_err(|error| ProxyError::from(error.message))?;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        host_options.on_ready = Some(Box::new(move |notice| {
            let _ = ready_tx.send(notice);
        }));
        let (exit_tx, mut exit_rx) = tokio::sync::watch::channel(None::<ResidentExit>);
        let signal_state = Arc::new(ResidentSignalState::default());
        let reaper_signal_state = signal_state.clone();

        let reaper = std::thread::Builder::new()
            .name("pilotdeck-local-kernel-reaper".into())
            .spawn(move || {
                let result = std::thread::scope(|scope| {
                    match std::thread::Builder::new()
                        .name("pilotdeck-local-kernel-resident".into())
                        .spawn_scoped(scope, move || {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                agentcpm_host::serve_resident_composed(host_options, composition)
                            }))
                        }) {
                        Ok(worker) => match worker.join() {
                            Ok(Ok(code)) => ResidentExit::Code(code),
                            Ok(Err(panic)) | Err(panic) => ResidentExit::Panic(panic_text(panic)),
                        },
                        Err(error) => {
                            ResidentExit::Panic(format!("cannot start resident worker: {error}"))
                        }
                    }
                });
                let recorded = match &result {
                    ResidentExit::Code(code) => Ok(*code),
                    ResidentExit::Panic(message) => Err(message.clone()),
                };
                reaper_signal_state.record_host_exit(recorded);
                exit_tx.send_replace(Some(result));
            })?;

        let owned = OwnedScopedResidentHost {
            stop: control,
            exit_rx: exit_rx.clone(),
            reaper: Some(reaper),
            signal_state: signal_state.clone(),
            shutdown_timeout: SHUTDOWN_TIMEOUT,
            root_guard: site.root_guard,
        };

        if let Err(message) = runtime.block_on(await_ready(
            ready_rx,
            &mut exit_rx,
            &site.deployment.socket,
            READY_TIMEOUT,
        )) {
            let stop_res = owned.shutdown_blocking(runtime, SHUTDOWN_TIMEOUT);
            return Err(match stop_res {
                Ok(()) => ProxyError::from(message),
                Err(stop_err) => ProxyError::from(format!("{message}; {stop_err}")),
            });
        }

        let project_sessions = match model_setup.project_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                let stop_res = owned.shutdown_blocking(runtime, SHUTDOWN_TIMEOUT);
                return Err(match stop_res {
                    Ok(()) => ProxyError::from(error.message),
                    Err(stop_err) => ProxyError::from(format!("{}; {stop_err}", error.message)),
                });
            }
        };

        let expected_loop_id = LOOP_BINDING.to_owned();
        let expected_controller_id = CONTROLLER_BINDING.to_owned();
        let checked_options: KernelTurnOptions = Arc::new(move |context, owner| {
            let options = (turn_options())(context, owner)?;
            if options.loop_binding != expected_loop_id
                || options.controller_binding != expected_controller_id
            {
                return Err(RuntimeError::new(
                    "Kernel turn options do not match the installed Native bindings",
                ));
            }
            Ok(options)
        });

        let binder: Arc<dyn ManagedSessionBinder> = if full_late {
            Arc::new(KernelSessionBinder {
                connections: Arc::new(site.deployment) as Arc<dyn AuthenticatedKernelConnections>,
                project_sessions,
                catalog: Arc::new(OriginalProjectCatalogPolicy),
                turn_options: checked_options,
            })
        } else {
            Arc::new(
                ScopedResidentSessionBinder::new(
                    site.deployment,
                    project_sessions,
                    Arc::new(OriginalProjectCatalogPolicy),
                    checked_options,
                    LOOP_BINDING,
                    CONTROLLER_BINDING,
                )
                .with_resident_shutdown(signal_state),
            )
        };

        Ok((binder, owned))
    }

    pub(in crate::native_process) fn maybe_start_local_scoped_kernel(
        options: &LocalOptions,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Option<StartedLocalScopedKernel>, ProxyError> {
        let decision = resolve_gateway_execution(
            &options.sidecar,
            &options.env,
            &options.pilot_home,
            &options.project,
        )?;
        match decision {
            GatewayExecutionDecision::Legacy => Ok(None),
            GatewayExecutionDecision::FullLate { program, sha256 } => {
                let site =
                    resolve_opt_in_site(&options.env, &options.pilot_home, &options.project)?;
                let controller_args = resolve_controller_args(&options.env, &program)?;
                let mut pieces = build_full_late_assembly(
                    &program,
                    &sha256,
                    controller_args,
                    &site.project_utf8,
                )?;
                let controller_rev = pieces
                    .assembly
                    .bindings
                    .iter()
                    .find(|entry| entry.binding.id == CONTROLLER_BINDING && entry.controller)
                    .ok_or_else(|| ProxyError::from("FullLate controller missing"))?
                    .effective_binding()
                    .map_err(|error| ProxyError::from(error.to_string()))?
                    .revision;
                let mut installed: Vec<InstalledContextOperation> = pieces
                    .other_context_roles
                    .iter()
                    .map(|role| {
                        let component = pieces
                            .assembly
                            .bindings
                            .iter()
                            .find(|entry| entry.binding.id == role.binding_ref)
                            .ok_or_else(|| {
                                ProxyError::from("FullLate context role missing Native component")
                            })?;
                        let effective = component
                            .effective_binding()
                            .map_err(|error| ProxyError::from(error.to_string()))?;
                        Ok(InstalledContextOperation {
                            operation: role.operation,
                            binding: effective.id,
                            revision: effective.revision,
                        })
                    })
                    .collect::<Result<_, ProxyError>>()?;
                installed.push(pieces.recovery.expected().clone());

                let loop_launch = NativeLoopLaunch::new(
                    CONTROLLER_BINDING.to_owned(),
                    controller_rev,
                    site.project_utf8.clone(),
                    &installed,
                )
                .map_err(|err| ProxyError::from(format!("invalid Native Loop config: {err:?}")))?;
                let loop_component = SchedulerComponent {
                    binding: pieces.loop_binding_def,
                    provider: SchedulerImplementation::Native {
                        program: pieces.native_loop.program,
                        args: loop_launch.args().to_vec(),
                        sha256: pieces.native_loop.sha256,
                    },
                    controller: false,
                };
                let expected_loop = loop_component
                    .effective_binding()
                    .map_err(|error| ProxyError::from(error.to_string()))?;
                pieces.assembly.bindings.push(loop_component);

                let composition = pieces
                    .recovery
                    .compose(
                        ExternalBackends {
                            scheduler_assembly: Some(pieces.assembly),
                            ..Default::default()
                        },
                        pieces.other_context_roles,
                    )
                    .map_err(|error| ProxyError::from(error.to_string()))?;

                start_local_composition(site, expected_loop, composition, runtime, true).map(Some)
            }
            GatewayExecutionDecision::ScopedResident { program, sha256 } => {
                let site =
                    resolve_opt_in_site(&options.env, &options.pilot_home, &options.project)?;
                let (loop_args, controller_args) =
                    resolve_scoped_args(&options.env, &program, &site.project_utf8)?;
                let loop_component = scheduler_component(ComponentSpec {
                    id: LOOP_BINDING,
                    kind: p::Kind::Loop,
                    effect: p::Effect::ToolWrite,
                    invoke: [CONTROLLER_BINDING.into()].into(),
                    publish: false,
                    resources: LOOP_RESOURCES,
                    program: program.clone(),
                    args: loop_args,
                    sha256: sha256.clone(),
                    controller: false,
                });
                let controller_component = scheduler_component(ComponentSpec {
                    id: CONTROLLER_BINDING,
                    kind: p::Kind::Service,
                    effect: p::Effect::State,
                    invoke: [LOOP_BINDING.into()].into(),
                    publish: true,
                    resources: &[],
                    program,
                    args: controller_args,
                    sha256,
                    controller: true,
                });
                let expected_loop = loop_component
                    .effective_binding()
                    .map_err(|error| ProxyError::from(error.to_string()))?;
                let assembly = SchedulerAssembly {
                    protocol: p::PROTOCOL.into(),
                    profile: scoped_profile(),
                    bindings: vec![loop_component, controller_component],
                };
                let composition = ResidentComposition::new(ExternalBackends {
                    scheduler_assembly: Some(assembly),
                    ..Default::default()
                });

                start_local_composition(site, expected_loop, composition, runtime, false).map(Some)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn default_args_branch_on_pilotdeck_native_loop_filename() {
            let native_bin = Path::new("/opt/bin/pilotdeck-native-loop");
            let loop_args = default_loop_args(native_bin, "/abs/proj");
            assert_eq!(loop_args.len(), 2);
            assert_eq!(loop_args[0], "--pilotdeck-scoped-native-loop-v1");
            let loop_json: serde_json::Value = serde_json::from_str(&loop_args[1]).unwrap();
            assert_eq!(
                loop_json,
                serde_json::json!({
                    "profile": "pilotdeck.scoped-native-loop.v1",
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller",
                    "project_dir": "/abs/proj",
                })
            );
            assert_eq!(
                default_controller_args(native_bin),
                vec![
                    "--pilotdeck-product-controller-v1".to_owned(),
                    "{\"profile\":\"pilotdeck.product-controller.v1\",\"loop_binding\":\"pilotdeck\",\"controller_binding\":\"controller\"}"
                        .to_owned(),
                ]
            );

            let fixture_bin = Path::new("/tmp/managed_native");
            assert_eq!(
                default_loop_args(fixture_bin, "/abs/proj"),
                vec![
                    "--native-model-tool-loop".to_owned(),
                    "/abs/proj".to_owned()
                ]
            );
            assert_eq!(
                default_controller_args(fixture_bin),
                vec!["--p2-product-controller".to_owned()]
            );
        }

        #[test]
        fn context_operation_args_match_v1_schema() {
            let expected = [
                (
                    "ctx-prepare",
                    p::context_operations::Operation::PrepareForModel,
                    "prepare_for_model",
                ),
                (
                    "ctx-apply-tools",
                    p::context_operations::Operation::ApplyToolResults,
                    "apply_tool_results",
                ),
                (
                    "ctx-capture",
                    p::context_operations::Operation::CaptureTurn,
                    "capture_turn",
                ),
                (
                    "ctx-compact",
                    p::context_operations::Operation::TryAutoCompact,
                    "try_auto_compact",
                ),
            ];
            for (binding, op, op_str) in expected {
                let args = default_context_operation_args(binding, op, "/abs/project");
                assert_eq!(args.len(), 2);
                assert_eq!(args[0], "--pilotdeck-context-operation-v1");
                let decoded: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
                assert_eq!(
                    decoded,
                    serde_json::json!({
                        "profile": "pilotdeck.context-operation.v1",
                        "binding": binding,
                        "operation": op_str,
                        "project_dir": "/abs/project",
                    })
                );
            }
        }

        #[test]
        fn runtime_site_writes_atomic_0644_install_json_and_respects_sun_len() {
            let env = Env::new();
            let (root, guard) = resolve_runtime_root(&env).unwrap();
            let (_startup, socket) = prepare_runtime_site(&root).unwrap();
            assert!(socket.as_os_str().len() < MAX_UDS_PATH_BYTES);
            let mode = fs::metadata(root.join("components/install.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o644);
            let toml = fs::read_to_string(root.join("kernel.toml")).unwrap();
            assert!(toml.contains("context.history.v2.read"));
            assert!(toml.contains("context.operations.dispatch"));
            assert!(toml.contains("context.operations.settle_failed"));
            assert!(toml.contains("tool.prepare_batch"));
            drop(guard);
            assert!(!root.exists());
        }

        #[test]
        fn full_late_server_inputs_include_controller_4_context_roles_recovery_and_loop() {
            let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
            let dir = PathBuf::from(format!("/tmp/pd-fl-asm-{short}"));
            fs::create_dir_all(&dir).unwrap();
            let fake_bin = dir.join("pilotdeck-native-loop");
            fs::write(&fake_bin, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&fake_bin, PermissionsExt::from_mode(0o755)).unwrap();

            let project = dir.join("proj");
            let home = dir.join("home");
            fs::create_dir_all(&project).unwrap();
            fs::create_dir_all(&home).unwrap();
            let options = ServerOptions {
                telemetry_context: crate::telemetry::TelemetryRuntimeContext {
                    installation_id: "inst".into(),
                    instance_id: "id".into(),
                    deployment_mode: "test".into(),
                    commit_hash: "0".into(),
                    app_version: "0".into(),
                    platform: "unix".into(),
                },
                telemetry_enabled: false,
                pilot_home: home,
                project,
                os_home: Some(dir.clone()),
                host: Some("127.0.0.1".into()),
                port: 0.0,
                allow_remote_host: false,
                token: Some("tok".into()),
                token_path: dir.join("tok"),
                sidecar: dir.join("unused"),
                env: Env::from([(KERNEL_LOOP_BIN_ENV.into(), fake_bin.display().to_string())]),
                builtin_skills_root: None,
                builtin_plugins_root: None,
            };

            let (inputs, _guard) =
                build_full_late_server_inputs(&options, fake_bin, "a".repeat(64)).unwrap();
            let assembly = inputs.external.scheduler_assembly.as_ref().unwrap();
            assert_eq!(
                assembly.profile.input_controller_ref.as_deref(),
                Some("controller")
            );
            assert_eq!(assembly.profile.default_loop_ref, "pilotdeck");
            assert_eq!(assembly.profile.allowed_bindings.len(), 7);
            assert_eq!(inputs.other_context_roles.len(), 4);
            assert_eq!(inputs.recovery.expected().binding, "ctx-recovery");
            assert_eq!(inputs.native_loop.binding.permissions.invoke.len(), 6);
            assert!(
                inputs
                    .native_loop
                    .binding
                    .permissions
                    .resources
                    .contains(p::context_history::READ)
            );
            assert!(
                inputs
                    .native_loop
                    .binding
                    .permissions
                    .resources
                    .contains(p::context_operations::DISPATCH)
            );
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn missing_or_invalid_loop_bin_and_args_json_fail_closed() {
            let env = Env::new();
            assert!(
                resolve_loop_binary_with(
                    "ScopedResident",
                    &env,
                    true,
                    Some(Path::new("/nonexistent/exe"))
                )
                .is_err()
            );
            assert!(
                resolve_loop_binary_with(
                    "FullLate",
                    &env,
                    false,
                    Some(Path::new("/nonexistent/exe"))
                )
                .unwrap()
                .is_none()
            );

            let rel_env = Env::from([(KERNEL_LOOP_BIN_ENV.into(), "relative/bin".into())]);
            assert!(resolve_loop_binary_with("ScopedResident", &rel_env, true, None).is_err());

            let bad_args = Env::from([(KERNEL_LOOP_ARGS_JSON_ENV.into(), "not-json".into())]);
            assert!(parse_args_json_env(&bad_args, KERNEL_LOOP_ARGS_JSON_ENV).is_err());

            let empty_args = Env::from([(KERNEL_LOOP_ARGS_JSON_ENV.into(), "[]".into())]);
            assert!(parse_args_json_env(&empty_args, KERNEL_LOOP_ARGS_JSON_ENV).is_err());
        }
    }
}

#[cfg(all(unix, feature = "kernel-resident"))]
pub(super) use enabled::{
    OwnedScopedResidentHost, maybe_serve_with_scoped_kernel, maybe_start_local_scoped_kernel,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kernel_profile_supports_all_three_modes_and_fails_closed_on_unknown() {
        assert_eq!(parse_kernel_profile(None).unwrap(), None);
        assert_eq!(
            parse_kernel_profile(Some("full-late")).unwrap(),
            Some(OptInKernelProfile::FullLate)
        );
        assert_eq!(
            parse_kernel_profile(Some("scoped-resident")).unwrap(),
            Some(OptInKernelProfile::ScopedResident)
        );
        assert_eq!(
            parse_kernel_profile(Some("legacy")).unwrap(),
            Some(OptInKernelProfile::Legacy)
        );

        for bad in [
            "",
            " ",
            "unknown",
            "scoped_resident",
            "full_late",
            "FullLate",
        ] {
            assert!(
                parse_kernel_profile(Some(bad)).is_err(),
                "expected fail-closed error for `{bad}`"
            );
        }
    }

    #[test]
    fn p7_default_resolution_covers_sibling_discovery_mock_sidecar_and_legacy_override() {
        let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let dir = PathBuf::from(format!("/tmp/pd-p7def-{short}"));
        let bin_dir = dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let fake_cli = bin_dir.join("pilotdeck");
        let sibling_loop = bin_dir.join("pilotdeck-native-loop");
        let default_sidecar = bin_dir.join("pilotdeck-agent-loop-sidecar");
        let mock_sidecar = dir.join("mock_sidecar.mjs");
        for path in [&fake_cli, &sibling_loop, &default_sidecar, &mock_sidecar] {
            fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, PermissionsExt::from_mode(0o755)).unwrap();
            }
        }
        let unused_sidecar = dir.join("unused");

        // 1. Sibling `pilotdeck-native-loop` next to exe + default sidecar -> auto FullLate.
        let decision =
            resolve_gateway_execution_with(&default_sidecar, &Env::new(), false, Some(&fake_cli))
                .unwrap();
        assert!(matches!(
            decision,
            GatewayExecutionDecision::FullLate { .. }
        ));

        // 2. Sibling `pilotdeck-native-loop` + nonexistent `unused` sidecar -> auto FullLate.
        let decision =
            resolve_gateway_execution_with(&unused_sidecar, &Env::new(), false, Some(&fake_cli))
                .unwrap();
        assert!(matches!(
            decision,
            GatewayExecutionDecision::FullLate { .. }
        ));

        // 3. Explicit mock sidecar file on disk -> stays Legacy even with sibling binary.
        let decision =
            resolve_gateway_execution_with(&mock_sidecar, &Env::new(), false, Some(&fake_cli))
                .unwrap();
        assert_eq!(decision, GatewayExecutionDecision::Legacy);

        // 4. Explicit external agent-loop transport env -> stays Legacy.
        let decision = resolve_gateway_execution_with(
            &default_sidecar,
            &Env::from([("PILOTDECK_AGENT_LOOP_TRANSPORT".into(), "stdio".into())]),
            false,
            Some(&fake_cli),
        )
        .unwrap();
        assert_eq!(decision, GatewayExecutionDecision::Legacy);

        // 5. Explicit `modules.agentLoop.enabled` -> stays Legacy.
        let decision =
            resolve_gateway_execution_with(&default_sidecar, &Env::new(), true, Some(&fake_cli))
                .unwrap();
        assert_eq!(decision, GatewayExecutionDecision::Legacy);

        // 6. Explicit `PILOTDECK_KERNEL_EXECUTION_PROFILE=legacy` -> stays Legacy even with sibling.
        let decision = resolve_gateway_execution_with(
            &default_sidecar,
            &Env::from([(KERNEL_EXECUTION_PROFILE_ENV.into(), "legacy".into())]),
            false,
            Some(&fake_cli),
        )
        .unwrap();
        assert_eq!(decision, GatewayExecutionDecision::Legacy);

        // 7. No sibling and no env -> stays Legacy (baseline test behavior).
        let isolated_cli = dir.join("isolated/pilotdeck");
        fs::create_dir_all(isolated_cli.parent().unwrap()).unwrap();
        fs::write(&isolated_cli, b"#!/bin/sh\nexit 0\n").unwrap();
        let decision = resolve_gateway_execution_with(
            &unused_sidecar,
            &Env::new(),
            false,
            Some(&isolated_cli),
        )
        .unwrap();
        assert_eq!(decision, GatewayExecutionDecision::Legacy);

        let _ = fs::remove_dir_all(&dir);
    }
}
