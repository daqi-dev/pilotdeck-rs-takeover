//! `modules.tools` bound to an external capability module (PilotDeck cc7c4e6
//! `composition/runtimePorts.ts:33-138`, `agent/modules/capability/
//! hostToolPort.ts:136-330`, `composition/HttpModuleClient.ts`).
//!
//! The binding's `catalog` becomes an external tool port that the session
//! composes *in front of* its own tools (`composeToolPorts(external,
//! native)`): the catalog is listed first, each call is routed by its tool
//! name, and an external call never reaches the host tool runtime. In the
//! baseline that runtime is where the permission decision, run-mode gates and
//! input validation live, so an external tool runs without any of them (the
//! module owns its own policy); this port mirrors that and does not consult
//! the host permission state.
use crate::{
    host::{
        HostError, HostFuture, ModuleError,
        modules::{
            capability::{
                ToolCall, ToolCatalog, ToolDefinition, ToolExecution, ToolInvocationContext,
            },
            tool_context::{ExecutionContext, ToolRuntimeContext},
        },
    },
    runtime::tools::ToolRunContext,
    skills::{
        ExternalSkillBinding, ExternalSkillsPort, HyperModuleTransport, ModuleCall, SkillError,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{collections::HashMap, sync::Arc};

/// One `catalog` entry as a tool definition (`runtimePorts.ts:122-138`
/// `toToolDefinition`): `kind` defaults to `custom`, the predicates are the
/// declared booleans.
pub(super) struct ExternalToolDefinition {
    fields: JsObject,
    read_only: bool,
    concurrency_safe: bool,
    requires_user_interaction: Option<bool>,
}

impl ToolDefinition for ExternalToolDefinition {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(self.read_only))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(self.concurrency_safe))
    }
    fn requires_user_interaction(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(self
            .requires_user_interaction
            .map_or(JsValue::Undefined, JsValue::Bool))
    }
}

/// `createExternalToolPort(binding)`: one HTTP module client per session
/// (its manifest cache and `module-http-N` counter live with the session).
pub(super) struct ExternalToolPort {
    client: ExternalSkillsPort<HyperModuleTransport>,
    /// The resolved binding, for a child session's own port.
    binding: JsValue,
    tools: Vec<Arc<ExternalToolDefinition>>,
    /// `options.methods?.includes("execute_batch")`.
    batch: bool,
}

impl ExternalToolPort {
    /// `snapshot.config.modules.tools`, resolved by the registry
    /// (`config/snapshot.rs::external_module` keeps the catalog as `tools`).
    pub(super) fn from_binding(binding: &JsValue) -> Option<Self> {
        let object = binding.as_object()?;
        if !object.contains_key("implementationId") {
            return None;
        }
        let parsed = ExternalSkillBinding::from_value(binding).ok()?;
        let batch = parsed
            .methods
            .iter()
            .any(|method| method == "execute_batch");
        let Some(JsValue::Array(catalog)) = object.get("tools") else {
            return None;
        };
        let tools = catalog
            .iter()
            .filter_map(JsValue::as_object)
            .map(|descriptor| {
                let flag = |key: &str| descriptor.get(key).and_then(JsValue::as_bool);
                let mut fields = JsObject::new();
                for key in ["name", "description"] {
                    fields.insert(
                        key.into(),
                        descriptor.get(key).cloned().unwrap_or(JsValue::Undefined),
                    );
                }
                fields.insert(
                    "kind".into(),
                    descriptor
                        .get("kind")
                        .filter(|kind| !kind.is_undefined())
                        .cloned()
                        .unwrap_or_else(|| "custom".into()),
                );
                fields.insert(
                    "inputSchema".into(),
                    descriptor
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(JsValue::Undefined),
                );
                Arc::new(ExternalToolDefinition {
                    fields,
                    read_only: flag("readOnly") == Some(true),
                    concurrency_safe: flag("concurrencySafe") == Some(true),
                    requires_user_interaction: flag("requiresUserInteraction"),
                })
            })
            .collect();
        Some(Self {
            client: ExternalSkillsPort::new(parsed, Arc::new(HyperModuleTransport::new())),
            binding: binding.clone(),
            tools,
            batch,
        })
    }

    /// A child session's own port over the same binding: a subagent session
    /// composes its module ports anew (`createRuntimeModulePorts(
    /// config.moduleBindings, sessionId)`, AgentSessionRuntimeBundle.ts:156),
    /// so it has its own module client (manifest, message ids).
    pub(super) fn fresh(&self) -> Option<Self> {
        Self::from_binding(&self.binding)
    }

    pub(super) fn tools(&self) -> &[Arc<ExternalToolDefinition>] {
        &self.tools
    }

    fn tool(&self, name: &JsString) -> Option<&Arc<ExternalToolDefinition>> {
        self.tools
            .iter()
            .find(|tool| tool.fields.get("name").and_then(JsValue::as_string) == Some(name))
    }

    /// `hostToolPort.ts:166-292` `executeAll`.
    async fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        context: &ToolInvocationContext,
        host_permission: Option<&JsObject>,
        external_loop: bool,
    ) -> Result<Vec<JsObject>, HostError> {
        let runtime = context
            .runtime
            .clone()
            .downcast::<ToolRuntimeContext>()
            .map_err(|_| ModuleError::new("Tool runtime requires a host ToolRuntimeContext."))?;
        let execution = context
            .execution
            .clone()
            .downcast::<ExecutionContext>()
            .map_err(|_| ModuleError::new("Tool runtime requires a host ExecutionContext."))?;
        let run = Arc::new(ToolRunContext::new(runtime.clone()));
        let aborted = {
            let run = run.clone();
            move || run.is_aborted()
        };
        // `callModule({ abortSignal: execution.abortSignal })`
        // (hostToolPort.ts:192,244): a user abort cancels the in-flight
        // module request at once (HttpModuleClient.ts:107-113 aborts the
        // fetch, which rejects as MODULE_ABORTED); dropping the request
        // future closes its connection the same way.
        let module_call = |call: ModuleCall| {
            let run = run.clone();
            async move {
                tokio::select! {
                    biased;
                    () = run.cancelled() => None,
                    response = self.client.module_call(call) => Some(response),
                }
            }
        };
        let wire_context = serialize_tool_context(&runtime.fields, host_permission);
        let wire_execution = serialize_execution_context(&execution.fields, external_loop);
        let identity = CallIdentity::of(&execution.fields, external_loop);

        if self.batch && !calls.is_empty() {
            let payload = object([
                ("operation", "execute_batch".into()),
                (
                    "calls",
                    JsValue::Array(
                        calls
                            .iter()
                            .map(|call| {
                                JsValue::Object(object([
                                    ("name", JsValue::String(call.name.clone())),
                                    ("arguments", call.input.clone()),
                                    ("toolCallId", JsValue::String(call.id.clone())),
                                ]))
                            })
                            .collect(),
                    ),
                ),
                ("context", JsValue::Object(wire_context)),
                ("execution", JsValue::Object(wire_execution)),
            ]);
            let response = match module_call(
                identity.call(format!("tool-batch-{}", random_suffix()), payload),
            )
            .await
            {
                None => return Err(cancelled()),
                Some(Ok(response)) => response,
                Some(Err(error)) => {
                    if error.code.equals_str("MODULE_ABORTED") || aborted() {
                        return Err(cancelled());
                    }
                    return Ok(calls
                        .iter()
                        .map(|call| transport_failure_result(call, &error))
                        .collect());
                }
            };
            if is_result_unknown(&response) {
                return Err(result_unknown(&response));
            }
            if response.get("ok") != Some(&JsValue::Bool(true)) {
                return Ok(calls
                    .iter()
                    .map(|call| module_failure_result(call, &response))
                    .collect());
            }
            let results = response
                .get("payload")
                .and_then(|payload| payload.get("results"))
                .and_then(|results| match results {
                    JsValue::Array(items) if items.len() == calls.len() => Some(items),
                    _ => None,
                })
                .ok_or_else(|| {
                    ModuleError::new(
                        "Capability batch response must contain one result for every tool call.",
                    )
                })?;
            return calls
                .iter()
                .zip(results)
                .enumerate()
                .map(|(index, (call, result))| validate_batch_result(result, call, index))
                .collect();
        }

        let execute = |call: ToolCall| {
            let wire_context = wire_context.clone();
            let wire_execution = wire_execution.clone();
            let identity = identity.clone();
            let aborted = aborted.clone();
            async move {
                if aborted() {
                    return Err(cancelled());
                }
                let payload = object([
                    ("name", JsValue::String(call.name.clone())),
                    ("arguments", call.input.clone()),
                    ("toolCallId", JsValue::String(call.id.clone())),
                    ("context", JsValue::Object(wire_context)),
                    ("execution", JsValue::Object(wire_execution)),
                ]);
                let response =
                    match module_call(identity.call(format!("tool-{}", random_suffix()), payload))
                        .await
                    {
                        None => return Err(cancelled()),
                        Some(Ok(response)) => response,
                        Some(Err(error)) => {
                            if error.code.equals_str("MODULE_ABORTED") || aborted() {
                                return Err(cancelled());
                            }
                            return Ok(transport_failure_result(&call, &error));
                        }
                    };
                if aborted() {
                    return Err(cancelled());
                }
                capability_result(&call, &response)
            }
        };
        let mut slots: Vec<Option<JsObject>> = vec![None; calls.len()];
        let mut concurrent = Vec::new();
        let mut sequential = Vec::new();
        for (index, call) in calls.into_iter().enumerate() {
            let safe = self
                .tool(&call.name)
                .is_some_and(|tool| tool.concurrency_safe);
            if safe {
                concurrent.push((index, call));
            } else {
                sequential.push((index, call));
            }
        }
        let settled =
            futures_util::future::join_all(concurrent.into_iter().map(|(index, call)| {
                let run = execute(call);
                async move { (index, run.await) }
            }))
            .await;
        for (index, result) in settled {
            slots[index] = Some(result?);
        }
        for (index, call) in sequential {
            slots[index] = Some(execute(call).await?);
        }
        Ok(slots.into_iter().map(Option::unwrap_or_default).collect())
    }
}

/// `runId` / `operationId` / `idempotencyKey` of a capability call: the
/// execution context's, with `operationId ?? turnId`.
#[derive(Clone)]
struct CallIdentity {
    run_id: JsString,
    operation_id: JsString,
    idempotency_key: Option<JsString>,
}
impl CallIdentity {
    /// The in-process loop's execution context has no `operationId` or
    /// `idempotencyKey`: the call falls back to the turn id. An external
    /// loop's context carries what its execute request named.
    fn of(execution: &JsObject, external_loop: bool) -> Self {
        let text = |key: &str| execution.get(key).and_then(JsValue::as_string).cloned();
        Self {
            run_id: text("runId").unwrap_or_default(),
            operation_id: external_loop
                .then(|| text("operationId"))
                .flatten()
                .or_else(|| text("turnId"))
                .unwrap_or_default(),
            idempotency_key: external_loop.then(|| text("idempotencyKey")).flatten(),
        }
    }
    fn call(&self, request_id: String, payload: JsObject) -> ModuleCall {
        ModuleCall {
            run_id: self.run_id.clone(),
            operation_id: self.operation_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            request_id,
            module: "capability",
            payload,
        }
    }
}

/// The per-call outcome of one validated response (`hostToolPort.ts:251-289`).
fn capability_result(call: &ToolCall, response: &JsValue) -> Result<JsObject, HostError> {
    let ok = response.get("ok") == Some(&JsValue::Bool(true));
    let payload = response.get("payload").and_then(JsValue::as_object);
    if !ok {
        let error = response.get("error");
        let code = match response.get("code").filter(|code| !is_nullish(code)) {
            Some(code) => to_text(code),
            None => error
                .and_then(|error| error.get("code"))
                .filter(|code| !is_nullish(code))
                .map(to_text)
                .unwrap_or_default(),
        }
        .to_uppercase();
        let message = error
            .and_then(|error| error.get("message"))
            .filter(|message| !is_nullish(message))
            .map(to_text)
            .unwrap_or_default();
        if ["CANCELLED", "ABORTED", "TOOL_ABORTED"].contains(&code.as_str())
            || mentions_cancel(&format!("{code} {message}"))
        {
            return Err(cancelled());
        }
    }
    if ok
        && let Some(payload) = payload
        && payload
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|t| t.equals_str("error"))
        && let Some(error) = payload.get("error").and_then(JsValue::as_object)
    {
        let code = error
            .get("code")
            .filter(|code| !is_nullish(code))
            .map(to_text)
            .unwrap_or_default()
            .to_uppercase();
        let message = error
            .get("message")
            .filter(|message| !is_nullish(message))
            .map(to_text)
            .unwrap_or_default();
        if ["CANCELLED", "ABORTED", "TOOL_ABORTED"].contains(&code.as_str())
            || mentions_cancel(&message)
        {
            return Err(cancelled());
        }
    }
    if is_result_unknown(response) {
        return Err(result_unknown(response));
    }
    if ok
        && let Some(payload) = payload
        && payload.contains_key("type")
    {
        return Ok(payload.clone());
    }
    Ok(module_failure_result(call, response))
}

fn is_result_unknown(response: &JsValue) -> bool {
    response.get("ok") == Some(&JsValue::Bool(false))
        && response
            .get("outcome")
            .and_then(JsValue::as_string)
            .is_some_and(|outcome| outcome.equals_str("result_unknown"))
}

/// `resultUnknownCapabilityError(response)`.
fn result_unknown(response: &JsValue) -> HostError {
    let message = response
        .get("error")
        .and_then(|error| error.get("message"))
        .filter(|message| !is_nullish(message))
        .map(to_js_text)
        .unwrap_or_else(|| "Host capability execution outcome is unknown.".into());
    let mut error = ModuleError::new(message);
    error.code = Some("RESULT_UNKNOWN".into());
    error.into()
}

fn cancelled() -> HostError {
    ModuleError::new("Tool execution cancelled.").into()
}

/// `validateBatchToolResult(result, call, index)`.
fn validate_batch_result(
    result: &JsValue,
    call: &ToolCall,
    index: usize,
) -> Result<JsObject, HostError> {
    let object = result
        .as_object()
        .filter(|object| object.contains_key("type"))
        .ok_or_else(|| ModuleError::new(format!("Capability batch result {index} is invalid.")))?;
    if object.get("toolCallId") != Some(&JsValue::String(call.id.clone())) {
        let mut units: Vec<u16> =
            format!("Capability batch result {index} does not match tool call ")
                .encode_utf16()
                .collect();
        units.extend_from_slice(call.id.units());
        units.push(u16::from(b'.'));
        return Err(ModuleError::new(JsString::from_utf16(units)).into());
    }
    Ok(object.clone())
}

/// `moduleFailureResult(call, response)`.
fn module_failure_result(call: &ToolCall, response: &JsValue) -> JsObject {
    let code = response.get("code");
    let error = response.get("error");
    let message: JsString = error
        .and_then(|error| error.get("message"))
        .filter(|message| !is_nullish(message))
        .map(to_js_text)
        .unwrap_or_else(|| "Capability module failed.".into());
    let mut tool_error = object([
        ("code", as_tool_error_code(code).into()),
        ("message", JsValue::String(message.clone())),
    ]);
    let code_truthy = code.is_some_and(truthy);
    let error_truthy = error.is_some_and(truthy);
    if code_truthy || error_truthy {
        let mut details = JsObject::new();
        if code_truthy {
            details.insert(
                "moduleCode".into(),
                code.cloned().unwrap_or(JsValue::Undefined),
            );
        }
        if error_truthy {
            details.insert(
                "moduleError".into(),
                error.cloned().unwrap_or(JsValue::Undefined),
            );
        }
        tool_error.insert("details".into(), JsValue::Object(details));
    }
    failure_result(call, tool_error, message)
}

/// `transportFailureResult(call, error)`.
fn transport_failure_result(call: &ToolCall, error: &SkillError) -> JsObject {
    let code = if error.code.is_empty() {
        JsString::from("MODULE_TRANSPORT_UNAVAILABLE")
    } else {
        error.code.clone()
    };
    let timeout = code.equals_str("MODULE_TIMEOUT");
    let tool_error = object([
        (
            "code",
            if timeout {
                "tool_timeout"
            } else {
                "tool_execution_failed"
            }
            .into(),
        ),
        ("message", JsValue::String(error.message.clone())),
        (
            "details",
            JsValue::Object(object([
                ("moduleCode", JsValue::String(code)),
                ("outcome", "result_unknown".into()),
                ("retryability", "unsafe".into()),
            ])),
        ),
    ]);
    failure_result(call, tool_error, error.message.clone())
}

fn failure_result(call: &ToolCall, error: JsObject, text: JsString) -> JsObject {
    let started = now_iso();
    let completed = now_iso();
    object([
        ("type", "error".into()),
        ("toolCallId", JsValue::String(call.id.clone())),
        ("toolName", JsValue::String(call.name.clone())),
        ("error", JsValue::Object(error)),
        (
            "content",
            JsValue::Array(vec![JsValue::Object(object([
                ("type", "text".into()),
                ("text", JsValue::String(text)),
            ]))]),
        ),
        ("startedAt", started.as_str().into()),
        ("completedAt", completed.as_str().into()),
    ])
}

/// `asToolErrorCode(response.code)`.
fn as_tool_error_code(value: Option<&JsValue>) -> &'static str {
    const CODES: [&str; 17] = [
        "tool_not_found",
        "tool_unavailable",
        "invalid_tool_input",
        "permission_denied",
        "permission_cancelled",
        "permission_required",
        "tool_execution_failed",
        "tool_aborted",
        "tool_timeout",
        "result_too_large",
        "path_not_allowed",
        "file_not_found",
        "file_conflict",
        "unsupported_tool",
        "setup_required",
        "plan_mode_violation",
        "ask_mode_violation",
    ];
    value
        .and_then(JsValue::as_string)
        .and_then(|code| {
            CODES
                .into_iter()
                .find(|candidate| code.equals_str(candidate))
        })
        .unwrap_or("tool_execution_failed")
}

/// `serializeToolContext(context)`: undefined members drop out of the JSON.
/// The baseline runs the loop in the host process, so the context carries
/// the session's own permission context (`bypassAvailable` / `acceptEdits`
/// as the session configured them) with the tool context's
/// `planDirectoryPath` appended last; the mode is the turn's current one.
fn serialize_tool_context(fields: &JsObject, host_permission: Option<&JsObject>) -> JsObject {
    let mut context = pick(
        fields,
        &[
            "sessionId",
            "turnId",
            "cwd",
            "permissionMode",
            "permissionContext",
            "runMode",
            "currentToolCallId",
            "maxResultBytes",
            "outputTruncated",
        ],
    );
    if let (Some(turn), Some(host)) = (
        context
            .get("permissionContext")
            .and_then(JsValue::as_object)
            .cloned(),
        host_permission,
    ) {
        let mut merged = JsObject::new();
        let from = |key: &str, source: &JsObject| {
            source
                .get(key)
                .filter(|value| !value.is_undefined())
                .cloned()
        };
        for (key, source) in [
            ("mode", &turn),
            ("canPrompt", &turn),
            ("policyCanPrompt", &turn),
            ("bypassAvailable", host),
            ("acceptEdits", host),
            ("cwd", &turn),
            ("additionalWorkingDirectories", &turn),
            ("rules", &turn),
            ("planDirectoryPath", &turn),
        ] {
            if let Some(value) = from(key, source) {
                merged.insert(key.into(), value);
            }
        }
        for (key, value) in turn.entries() {
            if merged.get_key(key).is_none() {
                merged.insert(key.clone(), value.clone());
            }
        }
        context.insert("permissionContext".into(), JsValue::Object(merged));
    }
    context
}

/// `serializeExecutionContext(execution)` (hostToolPort.ts:421-429). The
/// in-process loop's execution context carries only the run and turn ids; an
/// external loop's also carries its operation fields.
fn serialize_execution_context(fields: &JsObject, external_loop: bool) -> JsObject {
    if external_loop {
        pick(
            fields,
            &[
                "runId",
                "turnId",
                "operationId",
                "idempotencyKey",
                "operationDeadline",
            ],
        )
    } else {
        pick(fields, &["runId", "turnId"])
    }
}

fn pick(fields: &JsObject, keys: &[&str]) -> JsObject {
    let mut picked = JsObject::new();
    for key in keys {
        if let Some(value) = fields.get(key).filter(|value| !value.is_undefined()) {
            picked.insert((*key).into(), value.clone());
        }
    }
    picked
}

/// `/\bcancel(?:led|lation)?\b/i`.
fn mentions_cancel(text: &str) -> bool {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let word_at = |i: usize| lower.get(i).copied().is_some_and(word);
    (0..lower.len()).any(|start| {
        if start > 0 && word_at(start - 1) {
            return false;
        }
        let rest: String = lower[start..].iter().collect();
        if !rest.starts_with("cancel") {
            return false;
        }
        ["led", "lation", ""]
            .iter()
            .any(|suffix| rest[6..].starts_with(suffix) && !word_at(start + 6 + suffix.len()))
    })
}

fn is_nullish(value: &JsValue) -> bool {
    matches!(value, JsValue::Undefined | JsValue::Null)
}

fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        _ => true,
    }
}

/// `String(value)`.
fn to_js_text(value: &JsValue) -> JsString {
    crate::js_value::to_string_units(value)
        .map(JsString::from_utf16)
        .unwrap_or_default()
}

fn to_text(value: &JsValue) -> String {
    String::from_utf16_lossy(to_js_text(value).units())
}

fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64);
    pilotdeck_transport::system_clock::format_epoch_iso(ms)
}

/// `Math.random().toString(36).slice(2)`: the base-36 digits of a random
/// fraction (V8 prints the shortest round-trip digits; their count varies,
/// eleven is the common length and the last digit is never `0`).
fn random_suffix() -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let mut text: String = bytes[..10]
        .iter()
        .map(|byte| DIGITS[usize::from(*byte) % 36] as char)
        .collect();
    text.push(DIGITS[1 + usize::from(bytes[10]) % 35] as char);
    text
}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in entries {
        object.insert(key.into(), value);
    }
    object
}

/// `composeToolPorts(external, native)` (runtimePorts.ts:33-79): the
/// external catalog is listed before the host's tools; concurrency-safe calls
/// run concurrently per owner, every other call runs alone and in order,
/// whichever owner it belongs to.
pub(super) struct ComposedTools {
    pub(super) external: Arc<ExternalToolPort>,
    pub(super) native: Arc<dyn ToolExecution>,
    /// Host tools registered after the composition took its snapshot of the
    /// host catalog (`fallback.list()` runs once in `composeToolPorts`; the
    /// continuation tools are attached to the session afterwards,
    /// SessionSubagentContinuationBundle.ts:67-71). They are not listed, and
    /// their calls are scheduled as unknown descriptors (one at a time), but
    /// a call still reaches the host runtime.
    pub(super) late: Vec<JsString>,
    /// The session's own permission context for this turn (the host
    /// config's `permissionContext`, SessionAgentConfigBundle.ts:176-186).
    pub(super) host_permission: Option<JsObject>,
    /// The loop is an external module: the execution context the host
    /// received from it is the sidecar's whole serialized context.
    pub(super) external_loop: bool,
}

impl ToolCatalog for ComposedTools {
    fn list(&self) -> HostFuture<Vec<Arc<dyn ToolDefinition>>> {
        let external = self.external.clone();
        let native = self.native.clone();
        let late = self.late.clone();
        Box::pin(async move {
            let mut tools: Vec<Arc<dyn ToolDefinition>> = external
                .tools()
                .iter()
                .map(|tool| tool.clone() as Arc<dyn ToolDefinition>)
                .collect();
            tools.extend(native.list().await?.into_iter().filter(|tool| {
                tool.fields()
                    .get("name")
                    .and_then(JsValue::as_string)
                    .is_none_or(|name| !late.contains(name))
            }));
            Ok(tools)
        })
    }
}

impl ToolExecution for ComposedTools {
    fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        context: ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let external = self.external.clone();
        let native = self.native.clone();
        let late = self.late.clone();
        let host_permission = self.host_permission.clone();
        let external_loop = self.external_loop;
        Box::pin(async move {
            let native_tools = native.list().await?;
            let native_safe: HashMap<JsString, Arc<dyn ToolDefinition>> = native_tools
                .into_iter()
                .filter_map(|tool| {
                    let name = tool
                        .fields()
                        .get("name")
                        .and_then(JsValue::as_string)?
                        .clone();
                    (!late.contains(&name)).then_some((name, tool))
                })
                .collect();
            let count = calls.len();
            let mut slots: Vec<Option<JsObject>> = vec![None; count];
            let mut external_safe = Vec::new();
            let mut host_safe = Vec::new();
            let mut sequential = Vec::new();
            for (index, call) in calls.into_iter().enumerate() {
                let owned = external.tool(&call.name).is_some();
                let safe = if owned {
                    external
                        .tool(&call.name)
                        .is_some_and(|tool| tool.concurrency_safe)
                } else {
                    native_safe.get(&call.name).is_some_and(|tool| {
                        tool.is_concurrency_safe(&call.input)
                            .is_ok_and(|value| value == JsValue::Bool(true))
                    })
                };
                match (safe, owned) {
                    (true, true) => external_safe.push((index, call)),
                    (true, false) => host_safe.push((index, call)),
                    (false, _) => sequential.push((index, call, owned)),
                }
            }
            let fresh = |context: &ToolInvocationContext| ToolInvocationContext {
                runtime: context.runtime.clone(),
                execution: context.execution.clone(),
            };
            let run_group = |group: Vec<(usize, ToolCall)>, owned: bool| {
                let external = external.clone();
                let native = native.clone();
                let context = fresh(&context);
                let host_permission = host_permission.clone();
                async move {
                    if group.is_empty() {
                        return Ok::<_, HostError>(Vec::new());
                    }
                    let (indexes, calls): (Vec<usize>, Vec<ToolCall>) = group.into_iter().unzip();
                    let results = if owned {
                        external
                            .execute_all(calls, &context, host_permission.as_ref(), external_loop)
                            .await?
                    } else {
                        native.execute_all(calls, context).await?
                    };
                    if results.len() != indexes.len() {
                        return Err(ModuleError::new(
                            "Composed tool port returned an incomplete result set.",
                        )
                        .into());
                    }
                    Ok(indexes.into_iter().zip(results).collect())
                }
            };
            let (external_results, host_results) = futures_util::future::join(
                run_group(external_safe, true),
                run_group(host_safe, false),
            )
            .await;
            for (index, result) in external_results?.into_iter().chain(host_results?) {
                slots[index] = Some(result);
            }
            for (index, call, owned) in sequential {
                let results = run_group(vec![(index, call)], owned).await?;
                for (index, result) in results {
                    slots[index] = Some(result);
                }
            }
            Ok(slots.into_iter().map(Option::unwrap_or_default).collect())
        })
    }
}
