//! Production-grade scoped Native Loop and Product Controller entry points.
//!
//! Promotes the scoped model/tool loop composition (`ToolBatchCodec` +
//! `InitialReadCodec` + `ModelSessionCodec` under `ResourceMux` +
//! `ProductController` events + `ProductionTurnClock`) into reusable library
//! and binary entry points. Non-Recovery Context writers and advisory lifecycle
//! hooks remain intentionally omitted so unadvertised optional host modules
//! fail closed.
#![forbid(unsafe_code)]

use crate::{
    context_codec::ContextProofs,
    initial_read_codec::InitialReadCodec,
    model_session_codec::ModelSessionCodec,
    original::{OriginalBindings, OriginalPayloadAdmission},
    product,
    production_clock::ProductionTurnClock,
    resource_mux::ResourceMux,
    resources::ResourceCodec,
    tool_batch_codec::ToolBatchCodec,
    tool_batch_owner::SourceToolOwners,
};
use agentcpm_iface::types::TurnInput as IfaceTurnInput;
use agentcpm_scheduler_protocol::{CallContext, Client, Failure, Result as SchedulerResult};
use pilotdeck_agent::{
    ports::{CallControl, ExecutionIdentity, PortError, PortResult, Route},
    turn::{TurnClock, TurnEventPort, TurnInput},
};
use pilotdeck_protocol::model::BoundarySnapshot;
use pilotdeck_turn_preparation::PreparationEffects;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

/// CLI flag selecting the scoped Native Loop provider mode.
pub const SCOPED_NATIVE_LOOP_FLAG: &str = "--pilotdeck-scoped-native-loop-v1";
/// Frozen profile string required by [`ScopedNativeLoopConfig`].
pub const SCOPED_NATIVE_LOOP_PROFILE: &str = "pilotdeck.scoped-native-loop.v1";

/// CLI flag selecting the Product Controller provider mode.
pub const PRODUCT_CONTROLLER_FLAG: &str = "--pilotdeck-product-controller-v1";
/// Frozen profile string required by [`ProductControllerConfig`].
pub const PRODUCT_CONTROLLER_PROFILE: &str = "pilotdeck.product-controller.v1";

const NATIVE_MODEL_TOOL_FIXTURE_FLAG: &str = "--native-model-tool-loop";
const USAGE_EXIT_CODE: i32 = 64;
const RUNTIME_EXIT_CODE: i32 = 1;

/// Typed configuration for the scoped Native Loop binary/CLI entrypoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedNativeLoopConfig {
    pub profile: String,
    pub loop_binding: String,
    pub controller_binding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
}

impl ScopedNativeLoopConfig {
    /// Validate profile identity, binding identifiers, and optional absolute project directory.
    pub fn validate(&self) -> Result<(), String> {
        validate_profile_pair(
            &self.profile,
            SCOPED_NATIVE_LOOP_PROFILE,
            &self.loop_binding,
            &self.controller_binding,
        )?;
        if let Some(project_dir) = &self.project_dir {
            validate_project_dir(project_dir)?;
        }
        Ok(())
    }
}

/// Typed configuration for the Product Controller binary/CLI entrypoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductControllerConfig {
    pub profile: String,
    pub loop_binding: String,
    pub controller_binding: String,
}

impl ProductControllerConfig {
    /// Validate profile identity and binding identifiers.
    pub fn validate(&self) -> Result<(), String> {
        validate_profile_pair(
            &self.profile,
            PRODUCT_CONTROLLER_PROFILE,
            &self.loop_binding,
            &self.controller_binding,
        )
    }
}

/// Fail-closed CLI dispatch error distinguishing usage/config mistakes (`64`)
/// from runtime provider serving errors (`1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeLoopCliError {
    Usage(String),
    Runtime(Failure),
}

impl NativeLoopCliError {
    /// Exit code associated with this failure (`64` for invalid CLI/config, `1` for runtime errors).
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => USAGE_EXIT_CODE,
            Self::Runtime(_) => RUNTIME_EXIT_CODE,
        }
    }
}

impl fmt::Display for NativeLoopCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}"),
            Self::Runtime(failure) => write!(f, "{}", failure.wire()),
        }
    }
}

impl std::error::Error for NativeLoopCliError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    Version,
    Help,
    ServeScopedLoop(ScopedNativeLoopConfig),
    ServeProductController(ProductControllerConfig),
}

fn valid_binding_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn validate_binding_pair(loop_binding: &str, controller_binding: &str) -> Result<(), String> {
    if !valid_binding_id(loop_binding) {
        return Err(
            "invalid loop_binding: must be non-empty, <=256 bytes, trimmed, and free of control characters"
                .into(),
        );
    }
    if !valid_binding_id(controller_binding) {
        return Err(
            "invalid controller_binding: must be non-empty, <=256 bytes, trimmed, and free of control characters"
                .into(),
        );
    }
    if loop_binding == controller_binding {
        return Err("loop_binding and controller_binding must be distinct".into());
    }
    Ok(())
}

fn validate_project_dir(dir: &str) -> Result<(), String> {
    if dir.is_empty()
        || dir.trim() != dir
        || !Path::new(dir).is_absolute()
        || dir.chars().any(char::is_control)
    {
        return Err(
            "invalid project_dir: must be non-empty, trimmed, absolute, and free of control characters"
                .into(),
        );
    }
    Ok(())
}

fn validate_profile_pair(
    profile: &str,
    expected_profile: &str,
    loop_binding: &str,
    controller_binding: &str,
) -> Result<(), String> {
    if profile != expected_profile {
        return Err(format!(
            "unsupported profile {profile:?}: expected {expected_profile:?}"
        ));
    }
    validate_binding_pair(loop_binding, controller_binding)
}

fn parse_cli_action(args: &[String]) -> Result<CliAction, NativeLoopCliError> {
    match args {
        [flag] if flag == "--version" => Ok(CliAction::Version),
        [flag] if flag == "--help" => Ok(CliAction::Help),
        [flag, raw_json] if flag == SCOPED_NATIVE_LOOP_FLAG => {
            let config: ScopedNativeLoopConfig = serde_json::from_str(raw_json).map_err(|err| {
                NativeLoopCliError::Usage(format!(
                    "invalid {SCOPED_NATIVE_LOOP_FLAG} JSON configuration: {err}"
                ))
            })?;
            config.validate().map_err(NativeLoopCliError::Usage)?;
            Ok(CliAction::ServeScopedLoop(config))
        }
        [flag, raw_json] if flag == PRODUCT_CONTROLLER_FLAG => {
            let config: ProductControllerConfig =
                serde_json::from_str(raw_json).map_err(|err| {
                    NativeLoopCliError::Usage(format!(
                        "invalid {PRODUCT_CONTROLLER_FLAG} JSON configuration: {err}"
                    ))
                })?;
            config.validate().map_err(NativeLoopCliError::Usage)?;
            Ok(CliAction::ServeProductController(config))
        }
        _ => Err(NativeLoopCliError::Usage(format!(
            "usage: pilotdeck-native-loop ({SCOPED_NATIVE_LOOP_FLAG} <json> | {PRODUCT_CONTROLLER_FLAG} <json> | --version | --help)"
        ))),
    }
}

/// Dispatch the `pilotdeck-native-loop` CLI arguments cleanly and fail closed on
/// malformed arguments, unknown fields, or invalid profile/binding declarations.
pub async fn dispatch_native_loop_cli(args: &[String]) -> Result<(), NativeLoopCliError> {
    match parse_cli_action(args)? {
        CliAction::Version => {
            println!("pilotdeck-native-loop {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CliAction::Help => {
            println!(
                "pilotdeck-native-loop {}\n\
                 Usage:\n  \
                   pilotdeck-native-loop {SCOPED_NATIVE_LOOP_FLAG} <json>\n  \
                   pilotdeck-native-loop {PRODUCT_CONTROLLER_FLAG} <json>\n  \
                   pilotdeck-native-loop --version\n  \
                   pilotdeck-native-loop --help",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        CliAction::ServeScopedLoop(config) => serve_scoped_model_tool_loop_with_project(
            &config.loop_binding,
            &config.controller_binding,
            config.project_dir.as_deref(),
        )
        .await
        .map_err(NativeLoopCliError::Runtime),
        CliAction::ServeProductController(config) => {
            serve_product_controller(&config.loop_binding, &config.controller_binding)
                .await
                .map_err(NativeLoopCliError::Runtime)
        }
    }
}

fn invalid(message: impl Into<String>) -> PortError {
    PortError::InvalidResponse {
        message: message.into().into(),
    }
}

struct ModelToolBindings {
    loop_binding: String,
    controller_binding: String,
}

struct ScopedModelTool {
    context: CallContext,
    input: IfaceTurnInput,
    client: Arc<dyn Client>,
    runtime: tokio::runtime::Handle,
    owners: Arc<SourceToolOwners>,
    controller: String,
    sequence: Arc<AtomicU64>,
}

impl OriginalBindings for ModelToolBindings {
    fn supports_context_call(&self) -> bool {
        true
    }

    fn bind_context_call(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
        _proofs: Arc<ContextProofs>,
    ) -> PortResult<Option<Arc<dyn OriginalBindings>>> {
        control.check()?;
        if context.binding.id != self.loop_binding {
            return Err(invalid("scoped loop binding mismatch"));
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| invalid("SDK runtime is unavailable"))?;
        let owners = SourceToolOwners::new(client.clone());
        Ok(Some(Arc::new(ScopedModelTool {
            context: context.clone(),
            input: input.clone(),
            client,
            runtime,
            owners,
            controller: self.controller_binding.clone(),
            sequence: Arc::new(AtomicU64::new(0)),
        })))
    }

    fn resources(&self, _: &CallContext, _: &IfaceTurnInput) -> PortResult<Box<dyn ResourceCodec>> {
        Err(invalid("scoped model tool loop requires scoped binding"))
    }

    fn effects(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        Err(invalid("scoped model tool loop requires scoped binding"))
    }

    fn events(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        Err(invalid("scoped model tool loop requires scoped binding"))
    }

    fn clock(&self, _: &CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        Err(invalid("scoped model tool loop requires scoped binding"))
    }
}

impl OriginalBindings for ScopedModelTool {
    fn resources(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>> {
        if context.call_ref != self.context.call_ref
            || context.session_ref != self.context.session_ref
            || context.binding != self.context.binding
            || context.invocation != self.context.invocation
            || context.interaction != self.context.interaction
            || input != &self.input
        {
            return Err(invalid("scoped call identity changed"));
        }
        let (tool, owner) =
            ToolBatchCodec::new(context.clone(), self.client.clone(), self.runtime.clone())?;
        if !self.owners.matches_client(&self.client) {
            return Err(invalid("tool source client mismatch"));
        }
        self.owners
            .register_native(&self.client)
            .map_err(|_| invalid("native source custody registration failed"))?;
        self.owners
            .retain(owner)
            .map_err(|_| invalid("tool source owner retention failed"))?;
        let initial = InitialReadCodec::new(self.client.as_ref());
        let model = ModelSessionCodec::new_with_local_resource_facts(context.clone())
            .with_consumer_demand()
            .with_invocation_context();
        Ok(Box::new(ResourceMux::new(vec![
            Box::new(tool),
            Box::new(initial),
            Box::new(model),
        ])?))
    }

    fn effects(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        Ok(Arc::new(Mutex::new(ScopedPreparationEffects {
            control: control.clone(),
        })))
    }

    fn events(
        &self,
        context: &CallContext,
        _: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        Ok(Box::new(ProductEventPort {
            context: context.clone(),
            client,
            runtime: self.runtime.clone(),
            control: control.clone(),
            controller: self.controller.clone(),
            sequence: self.sequence.clone(),
        }))
    }

    fn clock(&self, _: &CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        Ok(Box::new(ProductionTurnClock))
    }
}

struct ScopedPreparationEffects {
    control: CallControl,
}

impl PreparationEffects for ScopedPreparationEffects {
    fn notice(&mut self, _: &TurnInput, _: bool) -> PortResult<()> {
        self.control.check()
    }

    fn before_model_request(&mut self, _: &Route) -> PortResult<()> {
        self.control.check()
    }

    fn finish(&mut self) -> PortResult<()> {
        Ok(())
    }
}

struct ProductEventPort {
    context: CallContext,
    client: Arc<dyn Client>,
    runtime: tokio::runtime::Handle,
    control: CallControl,
    controller: String,
    sequence: Arc<AtomicU64>,
}

impl ProductEventPort {
    fn publish(&mut self, event: BoundarySnapshot, accepted: bool) -> PortResult<()> {
        let control = if accepted {
            CallControl {
                cancellation: self.control.cancellation.cleanup_scope(),
                deadline: None,
            }
        } else {
            self.control.clone()
        };
        control.check()?;
        let raw = pilotdeck_protocol::stringify_json(event.value())
            .map_err(|_| invalid("event serialization"))?
            .ok_or_else(|| invalid("event is not JSON"))?;
        let seq = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let key = format!("{}:event:{seq}", self.context.call_ref);
        let client = self.client.clone();
        let context = self.context.clone();
        let controller = self.controller.clone();
        let result = self.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                product::publish_event(&*client, &context, &controller, &key, raw),
            )
            .await
        });
        if matches!(result, Ok(Ok(()))) {
            return Ok(());
        }
        Err(PortError::AcceptedUnknown {
            identity: Box::new(ExecutionIdentity {
                session_id: self.context.session_ref.as_str().into(),
                turn_id: self.context.interaction.turn.to_string().into(),
                run_id: self.context.call_ref.as_str().into(),
                operation_id: self.context.invocation.correlation_id.as_str().into(),
                request_id: key.clone().into(),
                idempotency_key: Some(key.into()),
                module_instance_id: self.context.binding.id.as_str().into(),
                connection_generation: self.context.interaction.generation.to_string().into(),
            }),
            last_applied_sequence: None,
            cause: None,
        })
    }
}

impl TurnEventPort for ProductEventPort {
    fn emit(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        self.publish(event, false)
    }

    fn emit_accepted(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        self.publish(event, true)
    }
}

fn resolve_process_cwd() -> SchedulerResult<String> {
    let mut args = std::env::args_os();
    let _program = args.next();
    if args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .as_deref()
        == Some(NATIVE_MODEL_TOOL_FIXTURE_FLAG)
    {
        return args
            .next()
            .and_then(|arg| arg.into_string().ok())
            .filter(|arg| Path::new(arg).is_absolute() && args.next().is_none())
            .ok_or_else(|| {
                Failure::external(
                    "PILOTDECK_NATIVE_CONFIG",
                    format!("{NATIVE_MODEL_TOOL_FIXTURE_FLAG} requires an absolute project path"),
                )
            });
    }
    let cwd = std::env::current_dir().map_err(|err| {
        Failure::external(
            "PILOTDECK_NATIVE_CONFIG",
            format!("process current working directory is unavailable: {err}"),
        )
    })?;
    if !cwd.is_absolute() {
        return Err(Failure::external(
            "PILOTDECK_NATIVE_CONFIG",
            "process current working directory must be absolute",
        ));
    }
    cwd.into_os_string().into_string().map_err(|_| {
        Failure::external(
            "PILOTDECK_NATIVE_CONFIG",
            "process current working directory must be valid UTF-8",
        )
    })
}

/// Serve the production-grade scoped model/tool Native Loop over stdin/stdout
/// using the official bidirectional Scheduler SDK and requiring `step_scope`,
/// binding `OriginalPayloadAdmission` to the configured `project_dir` when set.
pub async fn serve_scoped_model_tool_loop_with_project(
    loop_binding: impl AsRef<str>,
    controller_binding: impl AsRef<str>,
    project_dir: Option<&str>,
) -> SchedulerResult<()> {
    let loop_binding = loop_binding.as_ref().to_owned();
    let controller_binding = controller_binding.as_ref().to_owned();
    validate_binding_pair(&loop_binding, &controller_binding)
        .map_err(|msg| Failure::external("PILOTDECK_NATIVE_CONFIG", msg))?;
    let process_cwd = match project_dir {
        Some(dir) => {
            validate_project_dir(dir)
                .map_err(|msg| Failure::external("PILOTDECK_NATIVE_CONFIG", msg))?;
            dir.to_owned()
        }
        None => resolve_process_cwd()?,
    };
    agentcpm_scheduler_protocol::provider::serve_with_required_host_features(
        agentcpm_scheduler_protocol::provider::Export::Loop(Arc::new(
            crate::NativeFactory::new(Arc::new(OriginalPayloadAdmission::new(
                process_cwd.as_str().into(),
                Arc::new(ModelToolBindings {
                    loop_binding,
                    controller_binding,
                }),
            )))
            .with_scoped_steps(),
        )),
        &[agentcpm_scheduler_protocol::step_scope::FEATURE],
    )
    .await
}

/// Serve the production-grade scoped model/tool Native Loop over stdin/stdout
/// using the official bidirectional Scheduler SDK and requiring `step_scope`.
pub async fn serve_scoped_model_tool_loop(
    loop_binding: impl AsRef<str>,
    controller_binding: impl AsRef<str>,
) -> SchedulerResult<()> {
    serve_scoped_model_tool_loop_with_project(loop_binding, controller_binding, None).await
}

/// Serve the PilotDeck [`product::ProductController`] over stdin/stdout for
/// the explicitly configured `loop_binding` and `controller_binding`.
pub async fn serve_product_controller(
    loop_binding: impl AsRef<str>,
    controller_binding: impl AsRef<str>,
) -> SchedulerResult<()> {
    let loop_binding = loop_binding.as_ref().to_owned();
    let controller_binding = controller_binding.as_ref().to_owned();
    validate_binding_pair(&loop_binding, &controller_binding)
        .map_err(|msg| Failure::external("PILOTDECK_CONTROLLER_CONFIG", msg))?;
    product::serve_controller(product::ProductController {
        loop_binding,
        controller_binding,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_native_entry_parses_valid_configs_and_help_version() {
        assert_eq!(
            parse_cli_action(&["--version".into()]).unwrap(),
            CliAction::Version
        );
        assert_eq!(
            parse_cli_action(&["--help".into()]).unwrap(),
            CliAction::Help
        );

        let loop_cfg = ScopedNativeLoopConfig {
            profile: SCOPED_NATIVE_LOOP_PROFILE.into(),
            loop_binding: "pilotdeck".into(),
            controller_binding: "controller".into(),
            project_dir: None,
        };
        let loop_json = serde_json::to_string(&loop_cfg).unwrap();
        assert_eq!(
            parse_cli_action(&[SCOPED_NATIVE_LOOP_FLAG.into(), loop_json]).unwrap(),
            CliAction::ServeScopedLoop(loop_cfg)
        );

        let loop_cfg_with_project = ScopedNativeLoopConfig {
            profile: SCOPED_NATIVE_LOOP_PROFILE.into(),
            loop_binding: "pilotdeck".into(),
            controller_binding: "controller".into(),
            project_dir: Some("/tmp/pilotdeck-project".into()),
        };
        let loop_project_json = serde_json::to_string(&loop_cfg_with_project).unwrap();
        assert_eq!(
            parse_cli_action(&[SCOPED_NATIVE_LOOP_FLAG.into(), loop_project_json]).unwrap(),
            CliAction::ServeScopedLoop(loop_cfg_with_project)
        );

        let ctrl_cfg = ProductControllerConfig {
            profile: PRODUCT_CONTROLLER_PROFILE.into(),
            loop_binding: "pilotdeck".into(),
            controller_binding: "controller".into(),
        };
        let ctrl_json = serde_json::to_string(&ctrl_cfg).unwrap();
        assert_eq!(
            parse_cli_action(&[PRODUCT_CONTROLLER_FLAG.into(), ctrl_json]).unwrap(),
            CliAction::ServeProductController(ctrl_cfg)
        );
    }

    #[test]
    fn scoped_native_entry_rejects_invalid_args_unknown_fields_and_profiles_with_exit_64() {
        let bad_cases: Vec<Vec<String>> = vec![
            vec![],
            vec!["--unknown".into()],
            vec!["--version".into(), "extra".into()],
            vec!["--help".into(), "extra".into()],
            vec![SCOPED_NATIVE_LOOP_FLAG.into()],
            vec![PRODUCT_CONTROLLER_FLAG.into()],
            vec![SCOPED_NATIVE_LOOP_FLAG.into(), "{}".into(), "extra".into()],
            vec![SCOPED_NATIVE_LOOP_FLAG.into(), "not-json".into()],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": PRODUCT_CONTROLLER_PROFILE,
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller"
                })
                .to_string(),
            ],
            vec![
                PRODUCT_CONTROLLER_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller"
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller",
                    "extra_field": true
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "",
                    "controller_binding": "controller"
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": " pilotdeck",
                    "controller_binding": "controller"
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "same",
                    "controller_binding": "same"
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller",
                    "project_dir": ""
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller",
                    "project_dir": "relative/path"
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller",
                    "project_dir": " /tmp/trimmed"
                })
                .to_string(),
            ],
            vec![
                SCOPED_NATIVE_LOOP_FLAG.into(),
                serde_json::json!({
                    "profile": SCOPED_NATIVE_LOOP_PROFILE,
                    "loop_binding": "pilotdeck",
                    "controller_binding": "controller",
                    "project_dir": "/tmp/bad\npath"
                })
                .to_string(),
            ],
        ];
        for args in bad_cases {
            let err = parse_cli_action(&args).expect_err(&format!("must fail closed: {args:?}"));
            assert_eq!(err.exit_code(), 64, "wrong exit code for {args:?}: {err}");
        }
    }
}
