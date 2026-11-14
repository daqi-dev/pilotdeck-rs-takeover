//! The in-process gateway behind the CLI channel: `createLocalGateway({
//! projectRoot: process.cwd() })` (src/cli/pilotdeck.ts:347) — the same
//! assembly `pilotdeck server` runs, with no listener, no token and none of
//! the server's automation or channel adapters. Turns are submitted straight
//! to the serial host, the way `CliChannel` iterates `gateway.submitTurn`.
use super::{Options, assembly, server::ensure_router_dir};
use crate::{ProxyError, config::Env, runtime::turn::gateway::TurnGateway};
use pilotdeck_protocol::JsValue;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

/// The `--native` entry's default drain bound; nothing here ever drains (the
/// baseline process only ends when it is killed), the assembly just needs one.
const DRAIN_BUDGET: Duration = Duration::from_millis(5000);

/// What the CLI resolved before building its gateway.
pub struct LocalOptions {
    /// `resolvePilotHome(process.env)`.
    pub pilot_home: PathBuf,
    /// `process.cwd()`: both `projectRoot` and, since the CLI passes no
    /// `fallbackProjectRoot`, the project a turn without `projectKey` is
    /// served from (LocalGatewayBootConfig.ts:52).
    pub project: PathBuf,
    /// `os.homedir()`.
    pub os_home: Option<PathBuf>,
    /// The agent-loop sidecar executable each session is run in.
    pub sidecar: PathBuf,
    /// `process.env`.
    pub env: Env,
    pub builtin_skills_root: Option<PathBuf>,
    pub builtin_plugins_root: Option<PathBuf>,
    pub telemetry_context: Option<crate::telemetry::TelemetryRuntimeContext>,
}

/// Optional source-level host services, composable without changing wire options.
#[derive(Default)]
pub struct NativeGatewayServices {
    pub lsp_factory: Option<Arc<dyn crate::runtime::lsp::LspServiceFactory>>,
    pub session_storage: Option<Arc<crate::storage::AsyncTranscriptAdapter>>,
    pub native_archive_policy: Option<super::NativeArchivePolicy>,
}

/// A started in-process gateway. It is never stopped: the baseline CLI keeps
/// its gateway (and with it the process) alive after the last turn.
pub struct LocalGateway {
    runtime: tokio::runtime::Runtime,
    host: TurnGateway,
}

impl LocalGateway {
    pub fn start(options: LocalOptions) -> Result<Self, ProxyError> {
        Self::start_with_extensions(options, crate::extensions::NativeExtensions::default())
    }

    /// Start the production gateway assembly with Rust source-level extensions.
    /// Keep registration tokens alive to admit their contributions to new sessions.
    pub fn start_with_extensions(
        options: LocalOptions,
        extensions: crate::extensions::NativeExtensions,
    ) -> Result<Self, ProxyError> {
        Self::start_with_services(options, extensions, NativeGatewayServices::default())
    }

    /// Select a native LSP service factory at the same project-generation
    /// boundary as the fixed baseline lspServiceFactory. This source-level
    /// supplier is independent of NativeExtensions registration updates.
    pub fn start_with_lsp_factory(
        options: LocalOptions,
        extensions: crate::extensions::NativeExtensions,
        factory: Arc<dyn crate::runtime::lsp::LspServiceFactory>,
    ) -> Result<Self, ProxyError> {
        Self::start_with_services(
            options,
            extensions,
            NativeGatewayServices {
                lsp_factory: Some(factory),
                ..Default::default()
            },
        )
    }

    /// Select one host-owned transcript store for creation, recovery and management.
    pub fn start_with_session_storage(
        options: LocalOptions,
        extensions: crate::extensions::NativeExtensions,
        storage: Arc<crate::storage::AsyncTranscriptAdapter>,
    ) -> Result<Self, ProxyError> {
        Self::start_with_services(
            options,
            extensions,
            NativeGatewayServices {
                session_storage: Some(storage),
                ..Default::default()
            },
        )
    }

    pub fn start_with_services(
        options: LocalOptions,
        extensions: crate::extensions::NativeExtensions,
        services: NativeGatewayServices,
    ) -> Result<Self, ProxyError> {
        let NativeGatewayServices {
            lsp_factory,
            session_storage,
            native_archive_policy,
        } = services;
        if let Some(storage) = &session_storage {
            storage
                .validate_project_layout(&crate::storage::ProjectKey {
                    project_root: options.project.clone(),
                    pilot_home: options.pilot_home.clone(),
                })
                .map_err(|error| ProxyError::from(error.to_string()))?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let host = runtime.block_on(async {
            #[cfg(unix)]
            crate::ensure_sigxfsz_ignored();
            ensure_router_dir(&options.pilot_home);
            let process = Options {
                home: options.pilot_home.clone(),
                project: options.project.clone(),
                project_root: options.project.clone(),
                // Never bound or read: there is no listener and no token.
                listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                token_file: PathBuf::new(),
                sidecar: options.sidecar.clone(),
                sidecar_args: Vec::new(),
                drain_budget: DRAIN_BUDGET,
                storage_root: options.env.get("PILOTDECK_STORAGE_ROOT").cloned(),
                os_home: Some(
                    options
                        .os_home
                        .clone()
                        .or_else(std::env::home_dir)
                        .unwrap_or_else(|| PathBuf::from("/")),
                ),
                builtin_skills_root: options.builtin_skills_root.clone(),
                builtin_plugins_root: options.builtin_plugins_root.clone(),
                in_process_cli: true,
                telemetry: None,
                telemetry_context: options.telemetry_context.clone(),
                telemetry_reload: false,
            };
            let (host, _webhook, _switch) = match native_archive_policy {
                Some(policy) => {
                    assembly::start_with_native_host_services(
                        process,
                        options.env.clone(),
                        extensions,
                        lsp_factory,
                        session_storage,
                        Some(policy),
                    )
                    .await?
                }
                None => {
                    assembly::start_with_native_services(
                        process,
                        options.env.clone(),
                        extensions,
                        lsp_factory,
                        session_storage,
                    )
                    .await?
                }
            };
            Ok::<_, ProxyError>(host)
        })?;
        Ok(Self { runtime, host })
    }

    /// Share this host with the existing explicit WS listener or connection API.
    /// Keep this LocalGateway alive: it owns the runtime serving the cloned host.
    pub fn turn_gateway(&self) -> TurnGateway {
        self.host.clone()
    }

    /// Drain sessions, tool tasks and their retained extension resources.
    pub fn shutdown(&self, budget: Duration) -> Result<(), String> {
        self.runtime.block_on(self.host.shutdown(budget))
    }

    /// Invoke an existing SDK method through the production serial gateway.
    pub fn request(
        &self,
        method: &str,
        params: JsValue,
    ) -> Result<JsValue, crate::runtime::connection::RequestError> {
        self.runtime
            .block_on(self.host.request(method.into(), params))
    }

    /// `for await (const event of gateway.submitTurn(input))`: blocks until
    /// the iterator ends, handing every event to `emit` in order. `Err` is
    /// the message of the error the iterator throws.
    pub fn submit_turn(
        &self,
        input: JsValue,
        emit: Arc<dyn Fn(JsValue) + Send + Sync>,
    ) -> Result<(), String> {
        self.runtime
            .block_on(self.host.submit_turn(input, emit))
            .map_err(|error| String::from_utf16_lossy(error.message.units()))
    }
}

#[cfg(test)]
#[path = "local_lsp_tests.rs"]
mod lsp_tests;
