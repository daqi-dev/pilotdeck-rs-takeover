//! Explicit trusted product composition. No authority is derived from a
//! Gateway request, and readiness is not a resource-release receipt.
use super::{
    AuthenticatedKernelConnections, DeploymentKernelConfig, GatewayManagedModelSetup,
    KernelExecutionProfile, KernelSessionBinder, KernelTurnOptions, ManagedListenerServices,
    ManagedSessionBinder, ProjectCatalogPolicy, ProjectTurnAuthority, ScopedResidentSessionBinder,
    ServerOptions,
};
use crate::{
    ProxyError,
    runtime::{project::RuntimeError, turn::kernel_client::scoped::ResidentSignalState},
};
use agentcpm_host::{ExternalBackends, StartupOptions, TrustedToolBatchSetup};
use agentcpm_scheduler_protocol::{self as p, context_operations::ContextBinding};
use pilotdeck_http_transport::transport::ProxySettings;
use pilotdeck_iface::{
    context_codec::InstalledContextOperation, native_loop_launch::NativeLoopLaunch,
    recovery_composition::RecoveryInstallation,
};
use std::{sync::Arc, time::Duration};

/// The source-built Native Loop artifact and its explicitly reviewed binding.
/// Program bytes are verified and staged by Kernel, not by the turn request.
pub struct NativeLoopImage {
    pub binding: p::Binding,
    pub program: std::path::PathBuf,
    pub sha256: String,
}

/// All values here are trusted startup inputs, never request parameters. The
/// actual Tool setup and Model authority must borrow the same admitted product
/// owners used by the Kernel binder; there is no default or permissive version.
pub struct ResidentProductInputs {
    pub deployment: DeploymentKernelConfig,
    pub startup: StartupOptions,
    pub recovery: RecoveryInstallation,
    pub other_context_roles: Vec<ContextBinding>,
    pub external: ExternalBackends,
    pub native_loop: NativeLoopImage,
    pub loop_binding: String,
    pub controller_binding: String,
    pub model_authority: Arc<dyn ProjectTurnAuthority>,
    pub tool_batches: Arc<dyn TrustedToolBatchSetup>,
    pub catalog: Arc<dyn ProjectCatalogPolicy>,
    pub turn_options: KernelTurnOptions,
    pub module_model: Arc<dyn super::ManagedModuleModel>,
    pub proxy: Option<ProxySettings>,
    pub ready_timeout: Duration,
    pub shutdown_timeout: Duration,
}

/// Trusted startup inputs for an explicitly selected `ScopedResident`
/// (`fb54c74a8`-compatible) Host where the Native Loop command arguments are
/// supplied directly without Full-Late Context recovery composition.
pub struct ScopedResidentProductInputs {
    pub deployment: DeploymentKernelConfig,
    pub startup: StartupOptions,
    pub external: ExternalBackends,
    pub native_loop: NativeLoopImage,
    pub loop_args: Vec<String>,
    pub loop_binding: String,
    pub controller_binding: String,
    pub model_authority: Arc<dyn ProjectTurnAuthority>,
    pub tool_batches: Arc<dyn TrustedToolBatchSetup>,
    pub catalog: Arc<dyn ProjectCatalogPolicy>,
    pub turn_options: KernelTurnOptions,
    pub module_model: Arc<dyn super::ManagedModuleModel>,
    pub proxy: Option<ProxySettings>,
    pub ready_timeout: Duration,
    pub shutdown_timeout: Duration,
}

/// Startup inputs accepted by [`serve_with_resident_kernel_with_profile`].
pub enum ResidentInputs {
    Full(Box<ResidentProductInputs>),
    Scoped(Box<ScopedResidentProductInputs>),
}

impl From<ResidentProductInputs> for ResidentInputs {
    fn from(inputs: ResidentProductInputs) -> Self {
        Self::Full(Box::new(inputs))
    }
}

impl From<ScopedResidentProductInputs> for ResidentInputs {
    fn from(inputs: ScopedResidentProductInputs) -> Self {
        Self::Scoped(Box::new(inputs))
    }
}

struct CommonResidentInputs {
    deployment: DeploymentKernelConfig,
    startup: StartupOptions,
    external: ExternalBackends,
    native_loop: NativeLoopImage,
    loop_binding: String,
    controller_binding: String,
    model_authority: Arc<dyn ProjectTurnAuthority>,
    tool_batches: Arc<dyn TrustedToolBatchSetup>,
    catalog: Arc<dyn ProjectCatalogPolicy>,
    turn_options: KernelTurnOptions,
    module_model: Arc<dyn super::ManagedModuleModel>,
    proxy: Option<ProxySettings>,
    ready_timeout: Duration,
    shutdown_timeout: Duration,
}

enum ModeResidentInputs {
    Full {
        recovery: Box<RecoveryInstallation>,
        other_context_roles: Vec<ContextBinding>,
    },
    Scoped {
        loop_args: Vec<String>,
    },
}

impl ResidentInputs {
    fn split(self) -> (CommonResidentInputs, ModeResidentInputs) {
        match self {
            Self::Full(inputs) => {
                let inputs = *inputs;
                (
                    CommonResidentInputs {
                        deployment: inputs.deployment,
                        startup: inputs.startup,
                        external: inputs.external,
                        native_loop: inputs.native_loop,
                        loop_binding: inputs.loop_binding,
                        controller_binding: inputs.controller_binding,
                        model_authority: inputs.model_authority,
                        tool_batches: inputs.tool_batches,
                        catalog: inputs.catalog,
                        turn_options: inputs.turn_options,
                        module_model: inputs.module_model,
                        proxy: inputs.proxy,
                        ready_timeout: inputs.ready_timeout,
                        shutdown_timeout: inputs.shutdown_timeout,
                    },
                    ModeResidentInputs::Full {
                        recovery: Box::new(inputs.recovery),
                        other_context_roles: inputs.other_context_roles,
                    },
                )
            }
            Self::Scoped(inputs) => {
                let inputs = *inputs;
                (
                    CommonResidentInputs {
                        deployment: inputs.deployment,
                        startup: inputs.startup,
                        external: inputs.external,
                        native_loop: inputs.native_loop,
                        loop_binding: inputs.loop_binding,
                        controller_binding: inputs.controller_binding,
                        model_authority: inputs.model_authority,
                        tool_batches: inputs.tool_batches,
                        catalog: inputs.catalog,
                        turn_options: inputs.turn_options,
                        module_model: inputs.module_model,
                        proxy: inputs.proxy,
                        ready_timeout: inputs.ready_timeout,
                        shutdown_timeout: inputs.shutdown_timeout,
                    },
                    ModeResidentInputs::Scoped {
                        loop_args: inputs.loop_args,
                    },
                )
            }
        }
    }
}

/// Start the same resident Host used by `agentcpm_host::serve_resident`, then
/// the real product Gateway listener. A caller must install all trusted inputs;
/// missing product authorities are rejected before this entry is called.
/// Defaults strictly to [`KernelExecutionProfile::FullLate`].
pub fn serve_with_resident_kernel(
    options: ServerOptions,
    inputs: ResidentProductInputs,
    listening: impl FnOnce(&super::Listening),
) -> Result<(), ProxyError> {
    serve_with_resident_kernel_with_profile(
        options,
        inputs,
        KernelExecutionProfile::FullLate,
        listening,
    )
}

/// Explicit opt-in variant of [`serve_with_resident_kernel`] allowing the
/// caller to select [`KernelExecutionProfile::ScopedResident`] when running
/// against a scoped `fb54c74a8`-compatible Host.
pub fn serve_with_resident_kernel_with_profile(
    options: ServerOptions,
    inputs: impl Into<ResidentInputs>,
    profile: KernelExecutionProfile,
    listening: impl FnOnce(&super::Listening),
) -> Result<(), ProxyError> {
    let inputs = inputs.into();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        crate::ensure_sigxfsz_ignored();
        run(options, inputs, profile, listening).await
    })
}

/// A panic or cancellation of the Gateway startup must still request the
/// very same resident's shutdown. It does not claim that join has completed.
struct ShutdownOnDrop(agentcpm_host::ResidentShutdown);
impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ = self.0.request_shutdown();
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
fn error(message: impl Into<String>) -> ProxyError {
    std::io::Error::other(message.into()).into()
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
    expected_socket: &std::path::Path,
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
        Err(_) => Err("Kernel resident did not become ready before the configured deadline".into()),
    }
}

async fn run(
    options: ServerOptions,
    inputs: ResidentInputs,
    profile: KernelExecutionProfile,
    listening: impl FnOnce(&super::Listening),
) -> Result<(), ProxyError> {
    if profile == KernelExecutionProfile::Legacy {
        return Err(error(
            "KernelExecutionProfile::Legacy cannot run through resident Kernel startup",
        ));
    }
    let (common, mode) = inputs.split();
    if matches!(&mode, ModeResidentInputs::Scoped { .. })
        && profile == KernelExecutionProfile::FullLate
    {
        return Err(error(
            "KernelExecutionProfile::FullLate requires full ResidentProductInputs with Context recovery roles",
        ));
    }
    let CommonResidentInputs {
        deployment,
        startup,
        mut external,
        native_loop,
        loop_binding,
        controller_binding,
        model_authority,
        tool_batches,
        catalog,
        turn_options,
        module_model,
        proxy,
        ready_timeout,
        shutdown_timeout,
    } = common;
    if ready_timeout.is_zero()
        || shutdown_timeout.is_zero()
        || loop_binding.is_empty()
        || controller_binding.is_empty()
    {
        return Err(error(
            "resident startup needs explicit bindings and positive deadlines",
        ));
    }
    deployment.validate().map_err(|e| error(e.message))?;
    let assembly = external
        .scheduler_assembly
        .as_mut()
        .ok_or_else(|| error("resident Native assembly is missing"))?;
    if native_loop.binding.id != loop_binding
        || native_loop.binding.kind != p::Kind::Loop
        || !native_loop.program.is_absolute()
        || native_loop.sha256.len() != 64
        || !native_loop
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || assembly
            .bindings
            .iter()
            .any(|entry| entry.binding.id == loop_binding)
    {
        return Err(error("resident Native Loop image or binding is invalid"));
    }
    let controller = assembly.bindings.iter().find(|entry| {
        entry.binding.id == controller_binding
            && entry.binding.kind == p::Kind::Service
            && entry.controller
    });
    let has_controller = controller.is_some();
    let controller_revision = controller
        .ok_or_else(|| error("resident controller missing"))?
        .effective_binding()
        .map_err(|e| error(e.to_string()))?
        .revision;
    if !has_controller
        || assembly.profile.default_loop_ref != loop_binding
        || assembly.profile.input_controller_ref.as_deref() != Some(controller_binding.as_str())
    {
        return Err(error(
            "resident Native loop/controller bindings do not match the configured scheduler profile",
        ));
    }
    let loop_args = match &mode {
        ModeResidentInputs::Full {
            recovery,
            other_context_roles,
        } => {
            let recovery_expected = recovery.expected().clone();
            // Every non-Recovery role is resolved from the exact component already in
            // the Host assembly, using Kernel's own effective-binding algorithm.
            let mut installed = Vec::with_capacity(other_context_roles.len() + 1);
            for role in other_context_roles {
                let component = assembly
                    .bindings
                    .iter()
                    .find(|entry| entry.binding.id == role.binding_ref)
                    .ok_or_else(|| error("a protected Context binding has no Native component"))?;
                if component.binding.kind != p::Kind::Capability || component.controller {
                    return Err(error(
                        "a protected Context binding is not a Native capability",
                    ));
                }
                let effective = component
                    .effective_binding()
                    .map_err(|e| error(e.to_string()))?;
                installed.push(InstalledContextOperation {
                    operation: role.operation,
                    binding: effective.id,
                    revision: effective.revision,
                });
            }
            installed.push(recovery_expected);
            let process_cwd = options
                .project
                .to_str()
                .filter(|_| options.project.is_absolute())
                .ok_or_else(|| {
                    error("resident Native Loop requires an absolute UTF-8 project path")
                })?;
            let launch = NativeLoopLaunch::new(
                controller_binding.clone(),
                controller_revision,
                process_cwd.to_owned(),
                &installed,
            )
            .map_err(|e| error(format!("invalid Native Loop configuration: {e:?}")))?;
            launch.args().to_vec()
        }
        ModeResidentInputs::Scoped { loop_args } => {
            if loop_args.is_empty() || loop_args.iter().any(|arg| arg.contains('\0')) {
                return Err(error(
                    "scoped resident Native Loop requires non-empty NUL-free loop_args",
                ));
            }
            loop_args.clone()
        }
    };
    let loop_component = agentcpm_host::SchedulerComponent {
        binding: native_loop.binding,
        provider: agentcpm_host::SchedulerImplementation::Native {
            program: native_loop.program,
            args: loop_args,
            sha256: native_loop.sha256,
        },
        controller: false,
    };
    let loop_effective = loop_component
        .effective_binding()
        .map_err(|e| error(e.to_string()))?;
    assembly.bindings.push(loop_component);
    let provider: Arc<dyn agentcpm_host::TrustedReadProvider> = Arc::new(super::NativeProductReads);
    let reads = [
        p::product_reads::PLAN_TODO,
        p::product_reads::MODEL_METADATA,
    ]
    .into_iter()
    .map(|method| agentcpm_host::TrustedReadBinding {
        method: method.into(),
        callers: vec![loop_effective.clone()],
        profiles: vec![pilotdeck_protocol::boundary_codec::PROFILE.into()],
        provider: provider.clone(),
    })
    .collect();
    let model_setup = Arc::new(GatewayManagedModelSetup::new(
        proxy,
        model_authority,
        tokio::runtime::Handle::current(),
    ));
    let composition = match mode {
        ModeResidentInputs::Full {
            recovery,
            other_context_roles,
        } => recovery
            .compose(external, other_context_roles)
            .map_err(|e| error(e.to_string()))?,
        ModeResidentInputs::Scoped { .. } => agentcpm_host::ResidentComposition::new(external),
    }
    .with_trusted_reads(reads)
    .with_managed_model(model_setup.clone())
    .with_trusted_tool_batches(tool_batches);
    let (composition, control) = composition.with_shutdown_control();
    let _shutdown_on_drop = ShutdownOnDrop(control.clone());
    let mut host_options = deployment
        .host_options(startup)
        .map_err(|e| error(e.message))?;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    host_options.on_ready = Some(Box::new(move |notice| {
        let _ = ready_tx.send(notice);
    }));
    let (exit_tx, mut exit_rx) = tokio::sync::watch::channel(None::<ResidentExit>);
    let (failure_tx, failure_rx) = tokio::sync::watch::channel(None::<String>);
    let signal_state = Arc::new(ResidentSignalState::default());
    let reaper_signal_state = signal_state.clone();
    // The reaper owns a scoped worker and publishes only after join. If the
    // caller times out while trusted setup is stuck, this owner remains alive
    // and the result is explicitly unconfirmed, never a false release.
    std::thread::Builder::new()
        .name("pilotdeck-kernel-reaper".into())
        .spawn(move || {
            let result = std::thread::scope(|scope| {
                match std::thread::Builder::new()
                    .name("pilotdeck-kernel-resident".into())
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
            // If SIGINT/SIGTERM hit the shared process, give Gateway's signal
            // task a brief beat to mark gateway_stopping before flagging exit 0
            // as an unexpected early host exit.
            if matches!(result, ResidentExit::Code(0)) && !reaper_signal_state.is_gateway_stopping()
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            // Publish failure before the joined result, so the Gateway observes
            // an exit directly without depending on a separately scheduled task.
            if !matches!(result, ResidentExit::Code(0))
                || !reaper_signal_state.is_gateway_stopping()
            {
                failure_tx.send_replace(Some(exit_text(&result)));
            }
            exit_tx.send_replace(Some(result));
        })?;

    if let Err(message) =
        await_ready(ready_rx, &mut exit_rx, &deployment.socket, ready_timeout).await
    {
        let _ = control.request_shutdown();
        return match tokio::time::timeout(shutdown_timeout, wait_exit(&mut exit_rx)).await {
            Ok(exit) => Err(error(format!("{message}; {}", exit_text(&exit)))),
            Err(_) => Err(error(format!(
                "{message}; Kernel shutdown remains unconfirmed"
            ))),
        };
    }
    // `project_sessions` exists only after the real managed setup constructed;
    // on_ready precedes its use, but neither proves fullLate or releases.
    let project_sessions = match model_setup.project_sessions() {
        Ok(sessions) => sessions,
        Err(e) => {
            let _ = control.request_shutdown();
            let observed = tokio::time::timeout(shutdown_timeout, wait_exit(&mut exit_rx)).await;
            return Err(error(match observed {
                Ok(ResidentExit::Code(0)) => e.message,
                Ok(exit) => format!("{}; {}", e.message, exit_text(&exit)),
                Err(_) => format!("{}; Kernel shutdown remains unconfirmed", e.message),
            }));
        }
    };
    let expected_loop = loop_binding.clone();
    let expected_controller = controller_binding.clone();
    let checked_options: KernelTurnOptions = Arc::new(move |context, owner| {
        let options = turn_options(context, owner)?;
        if options.loop_binding != expected_loop
            || options.controller_binding != expected_controller
        {
            return Err(RuntimeError::new(
                "Kernel turn options do not match the installed Native bindings",
            ));
        }
        Ok(options)
    });
    let binder: Arc<dyn ManagedSessionBinder> = match profile {
        KernelExecutionProfile::FullLate => Arc::new(KernelSessionBinder {
            connections: Arc::new(deployment.clone()) as Arc<dyn AuthenticatedKernelConnections>,
            project_sessions,
            catalog,
            turn_options: checked_options,
            resident_shutdown: Some(signal_state.clone()),
        }),
        KernelExecutionProfile::ScopedResident => Arc::new(
            ScopedResidentSessionBinder::new(
                deployment.clone(),
                project_sessions,
                catalog,
                checked_options,
                loop_binding,
                controller_binding,
            )
            .with_resident_shutdown(signal_state.clone()),
        ),
        KernelExecutionProfile::Legacy => {
            return Err(error(
                "KernelExecutionProfile::Legacy cannot run through resident Kernel startup",
            ));
        }
    };
    let observed_failure = failure_rx.clone();
    let gateway = super::RESIDENT_SIGNAL_STATE
        .scope(
            signal_state.clone(),
            super::server::serve_on_runtime_with_services(
                options,
                listening,
                Some(ManagedListenerServices {
                    binder,
                    module_model,
                }),
                Some(failure_rx),
            ),
        )
        .await;
    let prior_exit = observed_failure.borrow().clone();
    let _ = control.request_shutdown();
    let host = match tokio::time::timeout(shutdown_timeout, wait_exit(&mut exit_rx)).await {
        Ok(ResidentExit::Code(0)) if prior_exit.is_none() || signal_state.is_gateway_stopping() => {
            Ok(())
        }
        Ok(exit) if prior_exit.is_some() => Err(format!(
            "Kernel resident exited before shutdown was requested: {}",
            exit_text(&exit)
        )),
        Ok(ResidentExit::Code(0)) => Ok(()),
        Ok(exit) => Err(exit_text(&exit)),
        Err(_) => Err("Kernel shutdown remains unconfirmed".to_owned()),
    };
    match (gateway, host) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(gateway), Ok(())) => Err(gateway),
        (Ok(()), Err(host)) => Err(error(host)),
        (Err(gateway), Err(host)) => Err(error(format!("{gateway}; {host}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn notice(path: &str) -> agentcpm_host::ReadyNotice {
        agentcpm_host::ReadyNotice {
            listen: None,
            uds: Some(PathBuf::from(path)),
            ws: None,
            a2a: None,
        }
    }

    #[tokio::test]
    async fn ready_requires_expected_live_socket_and_handles_early_exit() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (exit_tx, mut exit_rx) = tokio::sync::watch::channel(None);
        ready_tx.send(notice("/isolated/kernel.sock")).unwrap();
        await_ready(
            ready_rx,
            &mut exit_rx,
            std::path::Path::new("/isolated/kernel.sock"),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        drop(exit_tx);

        let (_ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (exit_tx, mut exit_rx) = tokio::sync::watch::channel(None);
        exit_tx.send_replace(Some(ResidentExit::Code(7)));
        let error = await_ready(
            ready_rx,
            &mut exit_rx,
            std::path::Path::new("/isolated/kernel.sock"),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(error.contains("status 7"));

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (_exit_tx, mut exit_rx) = tokio::sync::watch::channel(None);
        ready_tx.send(notice("/isolated/wrong.sock")).unwrap();
        assert!(
            await_ready(
                ready_rx,
                &mut exit_rx,
                std::path::Path::new("/isolated/kernel.sock"),
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
    }

    struct RejectModuleModel;
    impl super::super::ManagedModuleModel for RejectModuleModel {
        fn call<'a>(
            &'a self,
            _principal: &'a super::super::ManagedModulePrincipal,
            _operation: &'a str,
            _input: &'a pilotdeck_protocol::JsValue,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            super::super::ManagedModuleResponse,
                            super::super::ManagedModuleFailure,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Err(super::super::ManagedModuleFailure {
                    status: Some(403),
                    code: Some("REJECTED".into()),
                    message: Some("unused".into()),
                })
            })
        }
    }

    #[test]
    fn scoped_inputs_fail_closed_under_full_late_profile() {
        let dir = PathBuf::from("/tmp/p5-scoped-reject");
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
            pilot_home: dir.clone(),
            project: dir.clone(),
            os_home: Some(dir.clone()),
            host: Some("127.0.0.1".into()),
            port: 0.0,
            allow_remote_host: false,
            token: Some("tok".into()),
            token_path: dir.join("token"),
            sidecar: dir.join("unused"),
            env: crate::config::Env::new(),
            builtin_skills_root: None,
            builtin_plugins_root: None,
        };
        let inputs = ScopedResidentProductInputs {
            deployment: DeploymentKernelConfig {
                socket: dir.join("k.s"),
                expected_uid: 1000,
                principal: "gateway".into(),
                tenant: "fixture".into(),
                rpc_timeout_ms: 1000,
            },
            startup: StartupOptions {
                config: None,
                data_dir: dir.join("data"),
                components_dir: dir.join("components"),
            },
            external: ExternalBackends::default(),
            native_loop: NativeLoopImage {
                binding: p::Binding {
                    id: "pilotdeck".into(),
                    kind: p::Kind::Loop,
                    revision: "1".into(),
                    effect: p::Effect::ToolWrite,
                    permissions: p::Permissions {
                        invoke: ["controller".into()].into(),
                        publish: false,
                        state_namespace: None,
                        resources: Default::default(),
                    },
                },
                program: dir.join("loop"),
                sha256: "0".repeat(64),
            },
            loop_args: vec!["--native-model-tool-loop".into(), dir.display().to_string()],
            loop_binding: "pilotdeck".into(),
            controller_binding: "controller".into(),
            model_authority: Arc::new(super::super::OriginalProjectTurnAuthority),
            tool_batches: Arc::new(super::super::NativeToolBatchSetup),
            catalog: Arc::new(super::super::OriginalProjectCatalogPolicy),
            turn_options: Arc::new(|_, _| Err(RuntimeError::new("unused"))),
            module_model: Arc::new(RejectModuleModel),
            proxy: None,
            ready_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
        };
        let err = serve_with_resident_kernel_with_profile(
            options,
            inputs,
            KernelExecutionProfile::FullLate,
            |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("FullLate"));
    }
}
