//! `modules.context` bound to an external HTTP context module (PilotDeck
//! cc7c4e6 `composition/runtimePorts.ts:115-122` `createExternalContextPort`,
//! `agent/modules/context/hostContextRuntime.ts:34-110`,
//! `composition/HttpModuleClient.ts`).
//!
//! The module replaces the session's whole context runtime
//! (`AgentSessionRuntimeBundle.ts:156-163`: `externalPorts.context ??
//! dependencies.context`), still inside the durable recorder. Each operation
//! is one HTTP `module_call` with `module: "context"`, `runId` the session id,
//! `operationId` `context-<session id>`, `requestId`
//! `context-<operation>-<random>` and payload `{operation, input}`; the input
//! is the loop's own input minus its host-local capabilities. The module
//! answers `payload.result`. `prepare_for_model` is required; the other four
//! operations exist only when the binding lists them.
#![allow(clippy::result_large_err)]

use crate::{
    host::{
        AbortSignal, HostFuture, ModuleError,
        modules::context::{ContextInput, ContextRuntime, Operation},
    },
    skills::{
        ExternalSkillBinding, ExternalSkillsPort, HyperModuleTransport, ModuleCall, SkillError,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;

/// One `createExternalContextPort(binding, sessionId)`: one module client
/// (manifest cache, HTTP message ids) per session.
pub(super) struct ExternalContextPort {
    client: ExternalSkillsPort<HyperModuleTransport>,
    methods: Vec<String>,
    session_id: JsString,
}

impl ExternalContextPort {
    /// `snapshot.config.modules.context` as the registry resolved it. Only an
    /// external HTTP binding (`isExternalModuleBinding`) composes a port.
    pub(super) fn from_binding(binding: &JsValue, session_id: &JsString) -> Option<Self> {
        let object = binding.as_object()?;
        if !object.contains_key("implementationId")
            || !object
                .get("transport")
                .and_then(JsValue::as_string)
                .is_some_and(|transport| transport.equals_str("module-http-v2"))
        {
            return None;
        }
        let parsed = ExternalSkillBinding::from_value(binding).ok()?;
        Some(Self {
            methods: parsed.methods.clone(),
            client: ExternalSkillsPort::new(parsed, Arc::new(HyperModuleTransport::new())),
            session_id: session_id.clone(),
        })
    }

    /// `invoke(operation, input)` (hostContextRuntime.ts:45-65).
    async fn invoke(
        self: Arc<Self>,
        operation: Operation,
        input: ContextInput,
    ) -> Result<JsValue, ModuleError> {
        let name = operation.name();
        let abort = input
            .abort_signal
            .as_deref()
            .and_then(AbortSignal::from_context);
        let mut operation_id: Vec<u16> = "context-".encode_utf16().collect();
        operation_id.extend_from_slice(self.session_id.units());
        let call = ModuleCall {
            run_id: self.session_id.clone(),
            operation_id: JsString::from_utf16(operation_id),
            idempotency_key: None,
            request_id: format!("context-{name}-{}", super::module_tools::random_suffix()),
            module: "context",
            payload: object([
                ("operation", name.into()),
                (
                    "input",
                    JsValue::Object(serialize_input(operation, &input.fields)),
                ),
            ]),
        };
        // The request runs as its own task: a caller that stops waiting does
        // not tear down the exchange half-way (only the abort signal does,
        // like the fetch signal).
        let port = self.clone();
        let response = tokio::spawn(async move {
            if abort.as_ref().is_some_and(AbortSignal::is_cancelled) {
                return Err(aborted("Module request was cancelled before dispatch."));
            }
            match abort {
                Some(abort) => tokio::select! {
                    biased;
                    () = abort.cancelled() => Err(aborted("Module request was cancelled.")),
                    response = port.client.module_call(call) => response.map_err(client_error),
                },
                None => port.client.module_call(call).await.map_err(client_error),
            }
        })
        .await
        .map_err(|_| ModuleError::new("Context module request ended without completing."))??;
        if response.get("ok") != Some(&JsValue::Bool(true)) {
            // `String(response.error?.message ?? response.code ?? fallback)`.
            let message = response
                .get("error")
                .and_then(|error| error.get("message"))
                .filter(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
                .or_else(|| {
                    response
                        .get("code")
                        .filter(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
                })
                .map(|value| text(Some(value)))
                .unwrap_or_else(|| format!("Context module {name} failed.").into());
            return Err(ModuleError {
                code: response.get("code").and_then(JsValue::as_string).cloned(),
                ..ModuleError::new(message)
            });
        }
        // `!response.payload || !("result" in response.payload)`.
        match response
            .get("payload")
            .and_then(JsValue::as_object)
            .and_then(|payload| payload.get("result"))
        {
            Some(result) => Ok(result.clone()),
            None => Err(ModuleError::new(format!(
                "Context module {name} returned no result."
            ))),
        }
    }
}

/// The session's `AgentContextRuntime` over its external port.
pub(super) struct ExternalContextRuntime(pub(super) Arc<ExternalContextPort>);

impl ExternalContextRuntime {
    fn run(&self, operation: Operation, input: ContextInput) -> HostFuture<JsValue> {
        let port = self.0.clone();
        Box::pin(async move { Ok(port.invoke(operation, input).await?) })
    }
}

impl ContextRuntime for ExternalContextRuntime {
    fn supports(&self, operation: Operation) -> bool {
        self.0
            .methods
            .iter()
            .any(|method| method == operation.name())
    }
    fn takes_loop_input(&self) -> bool {
        true
    }
    fn prepare_for_model(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.run(Operation::PrepareForModel, input)
    }
    fn apply_tool_results(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.run(Operation::ApplyToolResults, input)
    }
    fn recover_from_model_error(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.run(Operation::RecoverFromModelError, input)
    }
    fn capture_turn(&self, input: ContextInput) -> HostFuture<JsValue> {
        // `await invoke("capture_turn", input)` resolves to undefined.
        let run = self.run(Operation::CaptureTurn, input);
        Box::pin(async move {
            run.await?;
            Ok(JsValue::Undefined)
        })
    }
    fn try_auto_compact(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.run(Operation::TryAutoCompact, input)
    }
}

/// `serializeContextInput`: the input without `abortSignal`,
/// `budgetEvaluator` and `budgetStage`, its keys in the order the baseline's
/// in-process loop builds them.
///
/// The loop here speaks the sidecar wire, whose key order differs from the
/// in-process input for two operations: `apply_tool_results`
/// (`AgentLoop.ts:2042-2046` puts `supplementalMessages` before `messages`)
/// and `try_auto_compact` (`AgentLoop.ts:631-648`: the budget fields follow
/// the token limits; the durable wrapper then appends `trigger`,
/// `durableContextRuntime.ts:72-80`; the serializer appends
/// `budgetPreparationId` and `budgetProjection`,
/// `hostContextRuntime.ts:96-107`). Keys outside these lists keep their
/// relative order after the listed ones.
fn serialize_input(operation: Operation, fields: &JsObject) -> JsObject {
    const APPLY_TOOL_RESULTS: &[&str] = &[
        "sessionId",
        "turnId",
        "toolResultMessage",
        "supplementalMessages",
        "messages",
    ];
    const TRY_AUTO_COMPACT: &[&str] = &[
        "sessionId",
        "turnId",
        "messages",
        "maxContextTokens",
        "reservedOutputTokens",
        "manualForce",
        "allowFallbackOnFailure",
        "budgetRequest",
        "budgetPreparation",
        "budgetCalibration",
        "trigger",
        "budgetPreparationId",
        "budgetProjection",
    ];
    let order: &[&str] = match operation {
        Operation::ApplyToolResults => APPLY_TOOL_RESULTS,
        Operation::TryAutoCompact => TRY_AUTO_COMPACT,
        _ => &[],
    };
    let skipped = |key: &JsString| {
        key.equals_str("abortSignal")
            || key.equals_str("budgetEvaluator")
            || key.equals_str("budgetStage")
    };
    let mut serialized = JsObject::new();
    for name in order {
        if let Some(value) = fields.get(name) {
            serialized.insert((*name).into(), value.clone());
        }
    }
    for (key, value) in fields.entries() {
        if skipped(key) || order.iter().any(|name| key.equals_str(name)) {
            continue;
        }
        serialized.insert(key.clone(), value.clone());
    }
    serialized
}

fn client_error(error: SkillError) -> ModuleError {
    ModuleError {
        code: (!error.code.is_empty()).then(|| error.code.clone()),
        ..ModuleError::new(error.message.clone())
    }
}

fn aborted(message: &str) -> ModuleError {
    ModuleError {
        code: Some("MODULE_ABORTED".into()),
        ..ModuleError::new(message)
    }
}

fn text(value: Option<&JsValue>) -> JsString {
    value
        .and_then(|value| crate::js_value::to_string_units(value).ok())
        .map(JsString::from_utf16)
        .unwrap_or_default()
}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in entries {
        object.insert(key.into(), value);
    }
    object
}
