//! Explicit opt-in `ScopedResident` launcher wiring for `pilotdeck server` and
//! `LocalGateway`. Default behavior stays 100% unchanged whenever
//! `PILOTDECK_KERNEL_EXECUTION_PROFILE` is unset.

use crate::{ProxyError, config::Env};

pub const KERNEL_EXECUTION_PROFILE_ENV: &str = "PILOTDECK_KERNEL_EXECUTION_PROFILE";
#[cfg(all(unix, feature = "kernel-resident"))]
pub const KERNEL_LOOP_BIN_ENV: &str = "PILOTDECK_KERNEL_LOOP_BIN";
#[cfg(all(unix, feature = "kernel-resident"))]
pub const KERNEL_LOOP_ARGS_JSON_ENV: &str = "PILOTDECK_KERNEL_LOOP_ARGS_JSON";
#[cfg(all(unix, feature = "kernel-resident"))]
pub const KERNEL_CONTROLLER_ARGS_JSON_ENV: &str = "PILOTDECK_KERNEL_CONTROLLER_ARGS_JSON";
#[cfg(all(unix, feature = "kernel-resident"))]
pub const KERNEL_RUNTIME_ROOT_ENV: &str = "PILOTDECK_KERNEL_RUNTIME_ROOT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OptInKernelProfile {
    #[cfg(all(unix, feature = "kernel-resident"))]
    ScopedResident,
}

/// Fail-closed parser for `PILOTDECK_KERNEL_EXECUTION_PROFILE`.
/// Returns `Ok(None)` when unset so default legacy execution stays untouched.
pub(super) fn parse_kernel_profile(
    value: Option<&str>,
) -> Result<Option<OptInKernelProfile>, ProxyError> {
    match value {
        None => Ok(None),
        Some("full-late") => Err(
            "FullLate profile requires explicit ResidentProductInputs with LOOP_SETTLEMENT_FEATURE"
                .into(),
        ),
        Some("scoped-resident") => {
            #[cfg(all(unix, feature = "kernel-resident"))]
            {
                Ok(Some(OptInKernelProfile::ScopedResident))
            }
            #[cfg(not(all(unix, feature = "kernel-resident")))]
            {
                Err(
                    "ScopedResident profile requires pilotdeck-gateway feature kernel-resident"
                        .into(),
                )
            }
        }
        Some(_) => Err(
            "Kernel execution profile (PILOTDECK_KERNEL_EXECUTION_PROFILE) must be exactly full-late or scoped-resident."
                .into(),
        ),
    }
}

/// Fail-closed gate used when no `kernel-resident` runtime is compiled in.
#[cfg(not(all(unix, feature = "kernel-resident")))]
pub(super) fn ensure_opt_in_allowed(env: &Env) -> Result<(), ProxyError> {
    let _ = parse_kernel_profile(env.get(KERNEL_EXECUTION_PROFILE_ENV).map(String::as_str))?;
    Ok(())
}

/// Server opt-in hook: returns `None` when `PILOTDECK_KERNEL_EXECUTION_PROFILE`
/// is unset so `native_process::serve` continues on the standard legacy path.
#[cfg(not(all(unix, feature = "kernel-resident")))]
pub(super) fn maybe_serve_with_scoped_kernel<F>(
    options: &super::ServerOptions,
    _listening: &mut Option<F>,
) -> Option<Result<(), ProxyError>>
where
    F: FnOnce(&super::Listening),
{
    match parse_kernel_profile(
        options
            .env
            .get(KERNEL_EXECUTION_PROFILE_ENV)
            .map(String::as_str),
    ) {
        Ok(None) => None,
        Ok(Some(_)) => unreachable!("scoped-resident is disabled without kernel-resident"),
        Err(error) => Some(Err(error)),
    }
}

#[cfg(all(unix, feature = "kernel-resident"))]
mod enabled {
    use super::*;
    use crate::{
        native_process::{
            DeploymentKernelConfig, GatewayManagedModelSetup, KernelExecutionProfile,
            KernelTurnOptions, Listening, LocalOptions, ManagedModuleFailure, ManagedModuleModel,
            ManagedModulePrincipal, ManagedModuleResponse, ManagedSessionBinder, NativeLoopImage,
            NativeProductReads, NativeToolBatchSetup, OriginalProjectCatalogPolicy,
            OriginalProjectTurnAuthority, ScopedResidentProductInputs, ScopedResidentSessionBinder,
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
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    const LOOP_BINDING: &str = "pilotdeck";
    const CONTROLLER_BINDING: &str = "controller";
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
        "execution.output_ack,execution.control,tool.exec,",
        "model.session_open,model.session_get,model.input_append,model.input_commit,",
        "model.events_read,model.delivery_advance,model.session_close,model.session_abort,",
        "model.response_interrupt,product.plan_todo.read,product.model.metadata",
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
            // Drop the runtime root guard only after joining (or leave it if still unconfirmed).
            if result.is_err() {
                let _ = self.root_guard.defuse();
            }
            result
        }

        pub(in crate::native_process) fn shutdown_on_drop(mut self) {
            self.signal_state.mark_gateway_stopping();
            let _ = self.stop.request_shutdown();
            if let Some(reaper) = self.reaper.take() {
                let _ = reaper.join();
            }
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

    struct ResolvedOptInConfig {
        program: PathBuf,
        sha256: String,
        loop_args: Vec<String>,
        controller_args: Vec<String>,
        startup: StartupOptions,
        deployment: DeploymentKernelConfig,
        proxy: Option<ProxySettings>,
        root_guard: RuntimeRootGuard,
    }

    fn resolve_loop_binary(env: &Env) -> Result<(PathBuf, String), ProxyError> {
        let raw = env
            .get(KERNEL_LOOP_BIN_ENV)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ProxyError::from(format!(
                    "ScopedResident profile requires {KERNEL_LOOP_BIN_ENV} to name an absolute executable file"
                ))
            })?;
        if raw.contains('\0') {
            return Err(format!("{KERNEL_LOOP_BIN_ENV} must not contain NUL bytes").into());
        }
        let program = PathBuf::from(raw);
        if !program.is_absolute() {
            return Err(format!("{KERNEL_LOOP_BIN_ENV} must be an absolute path").into());
        }
        let metadata = fs::metadata(&program).map_err(|error| {
            ProxyError::from(format!(
                "{KERNEL_LOOP_BIN_ENV} `{}` is unreadable: {error}",
                program.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "{KERNEL_LOOP_BIN_ENV} `{}` must name an existing regular file",
                program.display()
            )
            .into());
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "{KERNEL_LOOP_BIN_ENV} `{}` must be an executable file",
                program.display()
            )
            .into());
        }
        let bytes = fs::read(&program).map_err(|error| {
            ProxyError::from(format!(
                "failed to read {KERNEL_LOOP_BIN_ENV} `{}`: {error}",
                program.display()
            ))
        })?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        Ok((program, sha256))
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
        if filename.contains("pilotdeck-native-loop") {
            vec![
                "--pilotdeck-scoped-native-loop-v1".to_owned(),
                "{\"profile\":\"pilotdeck.scoped-native-loop.v1\",\"loop_binding\":\"pilotdeck\",\"controller_binding\":\"controller\"}"
                    .to_owned(),
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
        if filename.contains("pilotdeck-native-loop") {
            vec![
                "--pilotdeck-product-controller-v1".to_owned(),
                "{\"profile\":\"pilotdeck.product-controller.v1\",\"loop_binding\":\"pilotdeck\",\"controller_binding\":\"controller\"}"
                    .to_owned(),
            ]
        } else {
            vec!["--p2-product-controller".to_owned()]
        }
    }

    fn resolve_args(
        env: &Env,
        program: &Path,
        project: &Path,
    ) -> Result<(Vec<String>, Vec<String>), ProxyError> {
        let abs_project = if project.is_absolute() {
            project.to_path_buf()
        } else {
            std::env::current_dir()?.join(project)
        };
        let project_utf8 = abs_project
            .to_str()
            .filter(|text| !text.contains('\0'))
            .ok_or_else(|| ProxyError::from("project path must be valid NUL-free UTF-8"))?;
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

    fn resolve_opt_in_config(
        env: &Env,
        pilot_home: &Path,
        project: &Path,
    ) -> Result<ResolvedOptInConfig, ProxyError> {
        let (program, sha256) = resolve_loop_binary(env)?;
        let (loop_args, controller_args) = resolve_args(env, &program, project)?;
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
        Ok(ResolvedOptInConfig {
            program,
            sha256,
            loop_args,
            controller_args,
            startup,
            deployment,
            proxy,
            root_guard,
        })
    }

    fn scheduler_component(
        id: &str,
        kind: p::Kind,
        resources: &[&str],
        program: PathBuf,
        args: Vec<String>,
        sha256: String,
        controller: bool,
    ) -> SchedulerComponent {
        SchedulerComponent {
            binding: p::Binding {
                id: id.into(),
                revision: "opt-in-v1".into(),
                kind,
                effect: if controller {
                    p::Effect::State
                } else {
                    p::Effect::ToolWrite
                },
                permissions: p::Permissions {
                    invoke: if controller {
                        [LOOP_BINDING.into()].into()
                    } else {
                        [CONTROLLER_BINDING.into()].into()
                    },
                    publish: controller,
                    state_namespace: None,
                    resources: resources.iter().map(|v| (*v).into()).collect(),
                },
            },
            provider: SchedulerImplementation::Native {
                program,
                args,
                sha256,
            },
            controller,
        }
    }

    fn base_profile() -> p::SessionConfig {
        p::SessionConfig {
            input_controller_ref: Some(CONTROLLER_BINDING.into()),
            default_loop_ref: LOOP_BINDING.into(),
            allowed_bindings: [LOOP_BINDING.into(), CONTROLLER_BINDING.into()].into(),
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

    fn build_scoped_server_inputs(
        options: &ServerOptions,
    ) -> Result<(ScopedResidentProductInputs, RuntimeRootGuard), ProxyError> {
        let resolved = resolve_opt_in_config(&options.env, &options.pilot_home, &options.project)?;
        let loop_component = scheduler_component(
            LOOP_BINDING,
            p::Kind::Loop,
            LOOP_RESOURCES,
            resolved.program.clone(),
            resolved.loop_args.clone(),
            resolved.sha256.clone(),
            false,
        );
        let controller_component = scheduler_component(
            CONTROLLER_BINDING,
            p::Kind::Service,
            &[],
            resolved.program.clone(),
            resolved.controller_args,
            resolved.sha256.clone(),
            true,
        );
        let assembly = SchedulerAssembly {
            protocol: p::PROTOCOL.into(),
            profile: base_profile(),
            bindings: vec![controller_component],
        };
        let inputs = ScopedResidentProductInputs {
            deployment: resolved.deployment,
            startup: resolved.startup,
            external: ExternalBackends {
                scheduler_assembly: Some(assembly),
                ..Default::default()
            },
            native_loop: NativeLoopImage {
                binding: loop_component.binding,
                program: resolved.program,
                sha256: resolved.sha256,
            },
            loop_args: resolved.loop_args,
            loop_binding: LOOP_BINDING.into(),
            controller_binding: CONTROLLER_BINDING.into(),
            model_authority: Arc::new(OriginalProjectTurnAuthority),
            tool_batches: Arc::new(NativeToolBatchSetup),
            catalog: Arc::new(OriginalProjectCatalogPolicy),
            turn_options: turn_options(),
            module_model: Arc::new(RejectOptInModuleModel),
            proxy: resolved.proxy,
            ready_timeout: READY_TIMEOUT,
            shutdown_timeout: SHUTDOWN_TIMEOUT,
        };
        Ok((inputs, resolved.root_guard))
    }

    pub(in crate::native_process) fn maybe_serve_with_scoped_kernel<F>(
        options: &ServerOptions,
        listening: &mut Option<F>,
    ) -> Option<Result<(), ProxyError>>
    where
        F: FnOnce(&Listening),
    {
        match parse_kernel_profile(
            options
                .env
                .get(KERNEL_EXECUTION_PROFILE_ENV)
                .map(String::as_str),
        ) {
            Ok(None) => None,
            Ok(Some(OptInKernelProfile::ScopedResident)) => {
                let (inputs, _root_guard) = match build_scoped_server_inputs(options) {
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
            Err(error) => Some(Err(error)),
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

    pub(in crate::native_process) fn maybe_start_local_scoped_kernel(
        options: &LocalOptions,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Option<StartedLocalScopedKernel>, ProxyError> {
        let Some(OptInKernelProfile::ScopedResident) = parse_kernel_profile(
            options
                .env
                .get(KERNEL_EXECUTION_PROFILE_ENV)
                .map(String::as_str),
        )?
        else {
            return Ok(None);
        };

        let resolved = resolve_opt_in_config(&options.env, &options.pilot_home, &options.project)?;
        let loop_component = scheduler_component(
            LOOP_BINDING,
            p::Kind::Loop,
            LOOP_RESOURCES,
            resolved.program.clone(),
            resolved.loop_args,
            resolved.sha256.clone(),
            false,
        );
        let controller_component = scheduler_component(
            CONTROLLER_BINDING,
            p::Kind::Service,
            &[],
            resolved.program,
            resolved.controller_args,
            resolved.sha256,
            true,
        );
        let expected_loop = loop_component
            .effective_binding()
            .map_err(|error| ProxyError::from(error.to_string()))?;
        let assembly = SchedulerAssembly {
            protocol: p::PROTOCOL.into(),
            profile: base_profile(),
            bindings: vec![loop_component, controller_component],
        };

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
            resolved.proxy,
            Arc::new(OriginalProjectTurnAuthority),
            runtime.handle().clone(),
        ));
        let composition = ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly),
            ..Default::default()
        })
        .with_trusted_reads(reads)
        .with_managed_model(model_setup.clone())
        .with_trusted_tool_batches(Arc::new(NativeToolBatchSetup));
        let (composition, control) = composition.with_shutdown_control();

        let mut host_options = resolved
            .deployment
            .host_options(resolved.startup)
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
            root_guard: resolved.root_guard,
        };

        if let Err(message) = runtime.block_on(await_ready(
            ready_rx,
            &mut exit_rx,
            &resolved.deployment.socket,
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

        let binder: Arc<dyn ManagedSessionBinder> = Arc::new(
            ScopedResidentSessionBinder::new(
                resolved.deployment,
                project_sessions,
                Arc::new(OriginalProjectCatalogPolicy),
                checked_options,
                LOOP_BINDING,
                CONTROLLER_BINDING,
            )
            .with_resident_shutdown(signal_state),
        );

        Ok(Some((binder, owned)))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn default_args_branch_on_pilotdeck_native_loop_filename() {
            let native_bin = Path::new("/opt/bin/pilotdeck-native-loop");
            assert_eq!(
                default_loop_args(native_bin, "/abs/proj"),
                vec![
                    "--pilotdeck-scoped-native-loop-v1".to_owned(),
                    "{\"profile\":\"pilotdeck.scoped-native-loop.v1\",\"loop_binding\":\"pilotdeck\",\"controller_binding\":\"controller\"}"
                        .to_owned(),
                ]
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
            drop(guard);
            assert!(!root.exists());
        }

        #[test]
        fn missing_or_invalid_loop_bin_and_args_json_fail_closed() {
            let env = Env::new();
            assert!(resolve_loop_binary(&env).is_err());

            let rel_env = Env::from([(KERNEL_LOOP_BIN_ENV.into(), "relative/bin".into())]);
            assert!(resolve_loop_binary(&rel_env).is_err());

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
    fn parse_kernel_profile_preserves_none_and_fails_closed_on_full_late_or_unknown() {
        assert_eq!(parse_kernel_profile(None).unwrap(), None);

        let full_late_err = parse_kernel_profile(Some("full-late"))
            .unwrap_err()
            .to_string();
        assert!(full_late_err.contains("FullLate"));
        assert!(full_late_err.contains("LOOP_SETTLEMENT_FEATURE"));

        for bad in ["", " ", "legacy", "unknown", "scoped_resident"] {
            assert!(
                parse_kernel_profile(Some(bad)).is_err(),
                "expected `{bad}` to fail closed"
            );
        }

        let scoped = parse_kernel_profile(Some("scoped-resident"));
        if cfg!(all(unix, feature = "kernel-resident")) {
            assert!(scoped.is_ok());
        } else {
            let err = scoped.unwrap_err().to_string();
            assert!(err.contains("kernel-resident"), "unexpected error: {err}");
        }
    }
}
