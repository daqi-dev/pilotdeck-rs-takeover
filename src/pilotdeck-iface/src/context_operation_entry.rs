//! Native capability dispatch for the four non-Recovery Context operations:
//! `PrepareForModel`, `ApplyToolResults`, `CaptureTurn`, and `TryAutoCompact`.
//! `RecoverFromModelError` must use `--pilotdeck-context-recovery-v1`.
use crate::context_capability::{
    AdmissionAttempt, AdmittedContext, ContextBinding, ContextSettlement, NativeContextCapability,
};
use crate::scoped_native_entry::{NativeLoopCliError, valid_binding_id, validate_project_dir};
use agentcpm_iface::BoxFuture;
use agentcpm_scheduler_protocol::{
    CallContext, Client, Failure, Result, State, context_history as history,
    context_operations as ops,
};
use pilotdeck_context_iface::{
    AuthoritativeContextLedger, CONTEXT_LEDGER_PROTOCOL_VERSION, ContextControl, LedgerError,
    LedgerTransition, state_authority::BOUNDARY_CONTENT_TYPE,
};
use pilotdeck_context_runtime::{
    boundary_codec,
    history::Payload,
    host::HostError,
    operations::{ContextInput, Operation as RuntimeOperation},
    prepare::{DefaultContextRuntime, Options, SessionContextState},
};
use pilotdeck_protocol::{JsObject, JsValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// CLI flag selecting the non-Recovery Context operation capability mode.
pub const CONTEXT_OPERATION_FLAG: &str = "--pilotdeck-context-operation-v1";
/// Frozen profile string required by [`ContextOperationCliConfig`].
pub const CONTEXT_OPERATION_PROFILE: &str = "pilotdeck.context-operation.v1";

/// Typed configuration for `--pilotdeck-context-operation-v1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextOperationCliConfig {
    pub profile: String,
    pub binding: String,
    pub operation: agentcpm_scheduler_protocol::context_operations::Operation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
}

impl ContextOperationCliConfig {
    /// Validate profile, binding identifier, supported non-Recovery operation,
    /// and optional absolute project directory.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.profile != CONTEXT_OPERATION_PROFILE {
            return Err(format!(
                "unsupported profile {:?}: expected {CONTEXT_OPERATION_PROFILE:?}",
                self.profile
            ));
        }
        if !valid_binding_id(&self.binding) {
            return Err(
                "invalid binding: must be non-empty, <=256 bytes, trimmed, and free of control characters"
                    .into(),
            );
        }
        if self.operation == ops::Operation::RecoverFromModelError {
            return Err(format!(
                "operation recover_from_model_error is not supported by {CONTEXT_OPERATION_FLAG}; use --pilotdeck-context-recovery-v1"
            ));
        }
        if let Some(project_dir) = &self.project_dir {
            validate_project_dir(project_dir)?;
        }
        Ok(())
    }
}

fn to_runtime_operation(operation: ops::Operation) -> Result<RuntimeOperation> {
    match operation {
        ops::Operation::PrepareForModel => Ok(RuntimeOperation::PrepareForModel),
        ops::Operation::ApplyToolResults => Ok(RuntimeOperation::ApplyToolResults),
        ops::Operation::CaptureTurn => Ok(RuntimeOperation::CaptureTurn),
        ops::Operation::TryAutoCompact => Ok(RuntimeOperation::TryAutoCompact),
        ops::Operation::RecoverFromModelError => Err(Failure::external(
            "CONTEXT_UNSUPPORTED",
            "recover_from_model_error must use --pilotdeck-context-recovery-v1",
        )),
    }
}

fn to_wire_operation(operation: RuntimeOperation) -> Result<ops::Operation> {
    match operation {
        RuntimeOperation::PrepareForModel => Ok(ops::Operation::PrepareForModel),
        RuntimeOperation::ApplyToolResults => Ok(ops::Operation::ApplyToolResults),
        RuntimeOperation::CaptureTurn => Ok(ops::Operation::CaptureTurn),
        RuntimeOperation::TryAutoCompact => Ok(ops::Operation::TryAutoCompact),
        RuntimeOperation::RecoverFromModelError => Err(Failure::external(
            "CONTEXT_UNSUPPORTED",
            "recover_from_model_error must use --pilotdeck-context-recovery-v1",
        )),
    }
}

struct SessionSlot {
    runtime_state: Arc<SessionContextState>,
    ledger: Arc<AuthoritativeContextLedger>,
    last_kernel_revision: u64,
}

/// Trusted Host/Kernel Context binding for `PrepareForModel`, `ApplyToolResults`,
/// `CaptureTurn`, and `TryAutoCompact` backed by [`DefaultContextRuntime`] and
/// [`AuthoritativeContextLedger`].
pub struct ContextOperationBinding {
    wire_op: ops::Operation,
    runtime_op: RuntimeOperation,
    project_dir: Option<String>,
    sessions: Mutex<BTreeMap<String, SessionSlot>>,
}

impl ContextOperationBinding {
    /// Construct a fixed-role Context operation binding. Rejects `RecoverFromModelError`.
    pub fn new(operation: ops::Operation, project_dir: Option<String>) -> Result<Self> {
        let runtime_op = to_runtime_operation(operation)?;
        if let Some(dir) = &project_dir {
            validate_project_dir(dir)
                .map_err(|msg| Failure::external("PILOTDECK_CONTEXT_CONFIG", msg))?;
        }
        Ok(Self {
            wire_op: operation,
            runtime_op,
            project_dir,
            sessions: Mutex::new(BTreeMap::new()),
        })
    }

    fn session_handles(
        &self,
        session_ref: &str,
        kernel_expected_revision: u64,
    ) -> Result<(
        Arc<SessionContextState>,
        Arc<AuthoritativeContextLedger>,
        u64,
    )> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| Failure::external("CONTEXT_LOCK", "context session registry poisoned"))?;
        let slot = sessions
            .entry(session_ref.to_owned())
            .or_insert_with(|| SessionSlot {
                runtime_state: Arc::new(SessionContextState::default()),
                ledger: Arc::new(AuthoritativeContextLedger::new(session_ref)),
                last_kernel_revision: 0,
            });
        if kernel_expected_revision < slot.last_kernel_revision {
            return Err(Failure::external(
                "CONTEXT_REVISION_REGRESSION",
                format!(
                    "reservation expected_revision {kernel_expected_revision} precedes observed revision {}",
                    slot.last_kernel_revision
                ),
            ));
        }
        slot.last_kernel_revision = kernel_expected_revision;
        let base_local_revision = slot.ledger.snapshot().revision;
        Ok((
            slot.runtime_state.clone(),
            slot.ledger.clone(),
            base_local_revision,
        ))
    }
}

/// Construct a [`NativeContextCapability`] for one of the four non-Recovery roles.
pub fn context_operation_capability(
    operation: ops::Operation,
    binding_ref: String,
    project_dir: Option<String>,
) -> Result<NativeContextCapability> {
    if !valid_binding_id(&binding_ref) {
        return Err(Failure::external(
            "CONTEXT_UNSUPPORTED",
            "binding identity must be non-empty, <=256 bytes, trimmed, and free of control characters",
        ));
    }
    let runtime_op = to_runtime_operation(operation)?;
    let binding = Arc::new(ContextOperationBinding::new(operation, project_dir)?);
    NativeContextCapability::new(runtime_op, binding_ref, binding)
}

/// Serve one fixed non-Recovery Context capability over stdin/stdout via the
/// public Kernel SDK, requiring both `context_history` and `context_operations`.
pub async fn serve_context_operation(
    operation: ops::Operation,
    binding_ref: String,
    project_dir: Option<String>,
) -> Result<()> {
    let capability = context_operation_capability(operation, binding_ref, project_dir)?;
    agentcpm_scheduler_protocol::provider::serve_with_required_host_features(
        agentcpm_scheduler_protocol::provider::Export::Capability(Arc::new(capability)),
        &[history::FEATURE, ops::FEATURE],
    )
    .await
}

/// Parse `--pilotdeck-context-operation-v1 <json>` payload, failing closed with
/// [`NativeLoopCliError::Usage`] (exit code 64) on invalid config/role or
/// [`NativeLoopCliError::Runtime`] (exit code 1) on provider runtime failure.
pub async fn dispatch_context_operation_cli_json(
    raw_json: &str,
) -> std::result::Result<(), NativeLoopCliError> {
    let config = parse_context_operation_cli_json(raw_json)?;
    serve_context_operation(config.operation, config.binding, config.project_dir)
        .await
        .map_err(NativeLoopCliError::Runtime)
}

pub(crate) fn parse_context_operation_cli_json(
    raw_json: &str,
) -> std::result::Result<ContextOperationCliConfig, NativeLoopCliError> {
    let config: ContextOperationCliConfig = serde_json::from_str(raw_json).map_err(|err| {
        NativeLoopCliError::Usage(format!(
            "invalid {CONTEXT_OPERATION_FLAG} JSON configuration: {err}"
        ))
    })?;
    config.validate().map_err(NativeLoopCliError::Usage)?;
    Ok(config)
}

impl ContextBinding for ContextOperationBinding {
    fn supports(&self, operation: RuntimeOperation) -> bool {
        operation == self.runtime_op
    }

    fn begin(
        &self,
        context: &CallContext,
        client: Arc<dyn Client>,
        operation: RuntimeOperation,
        request: &ops::InvocationRequest,
        decoded_input: JsValue,
    ) -> Result<Arc<dyn AdmissionAttempt>> {
        if operation != self.runtime_op || request.operation != self.wire_op {
            return Err(Failure::external(
                "CONTEXT_UNSUPPORTED",
                "invocation operation does not match fixed context binding",
            ));
        }
        let fields = decoded_input.as_object().cloned().ok_or_else(|| {
            Failure::external("CONTEXT_PROTOCOL", "context input is not an object")
        })?;
        let (runtime_state, ledger, base_local_revision) =
            self.session_handles(&context.session_ref, request.reservation.expected_revision)?;
        Ok(Arc::new(Attempt {
            context: context.clone(),
            request: request.clone(),
            client,
            fields,
            project_dir: self.project_dir.clone(),
            runtime_state,
            ledger,
            base_local_revision,
            retired: AtomicBool::new(false),
        }))
    }
}

struct Attempt {
    context: CallContext,
    request: ops::InvocationRequest,
    client: Arc<dyn Client>,
    fields: JsObject,
    project_dir: Option<String>,
    runtime_state: Arc<SessionContextState>,
    ledger: Arc<AuthoritativeContextLedger>,
    base_local_revision: u64,
    retired: AtomicBool,
}

impl AdmissionAttempt for Attempt {
    fn admit(&self) -> BoxFuture<'_, Result<AdmittedContext>> {
        Box::pin(async move {
            if self.retired.load(Ordering::Acquire) {
                return Err(Failure::external(
                    "AUTHORITY_REVOKED",
                    "context admission retired",
                ));
            }
            let current = self.client.get(&self.context.call_ref).await?;
            let owner = &self.request.reservation.owner;
            let parent = self.client.get(&owner.call).await?;
            if self.retired.load(Ordering::Acquire)
                || current.id != self.context.call_ref
                || current.session_ref != self.context.session_ref
                || current.binding != self.context.binding
                || current.invocation != self.context.invocation
                || current.interaction != self.context.interaction
                || current.origin != self.context.origin
                || current.security != self.context.security
                || current.parent_session_ref.as_deref() != Some(self.context.session_ref.as_str())
                || owner.session != self.context.session_ref
                || current.state != State::Running
                || current.cancel_requested
                || current.source_binding.as_deref() != Some(owner.binding.as_str())
                || current.cause_execution_ref.as_deref() != Some(owner.call.as_str())
                || parent.id != owner.call
                || parent.session_ref != owner.session
                || parent.binding.id != owner.binding
                || parent.invocation.context_ref != owner.context
                || parent.interaction != owner.origin
                || parent.security != current.security
                || parent.state != State::Running
                || parent.cancel_requested
            {
                return Err(Failure::external(
                    "AUTHORITY_REVOKED",
                    "context source is not live or does not match reservation",
                ));
            }

            let cwd_fallback =
                js_string_field(&self.fields, "cwd").or_else(|| self.project_dir.clone());
            let runtime = DefaultContextRuntime::new(Options {
                project_root: cwd_fallback,
                session_state: Some(self.runtime_state.clone()),
                ..Default::default()
            });
            let settlement = Arc::new(OperationSettlement {
                operation: to_runtime_operation(self.request.operation)?,
                wire_operation: self.request.operation,
                transaction_id: self.request.reservation.transaction_id.clone(),
                control: extract_control(&self.fields),
                fields: self.fields.clone(),
                ledger: self.ledger.clone(),
                base_local_revision: self.base_local_revision,
                retired: AtomicBool::new(false),
            });
            Ok(AdmittedContext {
                runtime: Arc::new(runtime),
                input: ContextInput {
                    fields: self.fields.clone(),
                    abort_signal: None,
                    budget_evaluator: None,
                },
                settlement,
            })
        })
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}

struct OperationSettlement {
    operation: RuntimeOperation,
    wire_operation: ops::Operation,
    transaction_id: String,
    control: ContextControl,
    fields: JsObject,
    ledger: Arc<AuthoritativeContextLedger>,
    base_local_revision: u64,
    retired: AtomicBool,
}

impl OperationSettlement {
    fn encode_boundary_payload(value: &JsValue, label: &str) -> Result<history::OpaquePayload> {
        let bytes = boundary_codec::encode(value).map_err(|_| {
            Failure::external(
                "CONTEXT_PROTOCOL",
                format!("{label} output exceeds profile or is unsupported"),
            )
        })?;
        Ok(history::OpaquePayload {
            profile: boundary_codec::PROFILE.into(),
            bytes,
        })
    }

    fn ledger_payload(opaque: &history::OpaquePayload) -> Payload {
        Payload {
            profile: opaque.profile.clone(),
            content_type: BOUNDARY_CONTENT_TYPE.into(),
            bytes: opaque.bytes.clone(),
        }
    }

    fn map_ledger_error(err: LedgerError) -> Failure {
        match err {
            LedgerError::RevisionMismatch { expected, actual } => Failure::external(
                "CONTEXT_CONFLICT",
                format!("context ledger CAS mismatch: expected {expected}, actual {actual}"),
            ),
            LedgerError::IdempotencyConflict { transaction_id, .. } => Failure::external(
                "CONTEXT_IDEMPOTENCY_CONFLICT",
                format!("context transaction {transaction_id} digest conflict"),
            ),
            LedgerError::InputAlreadyConsumed { input_id } => Failure::external(
                "CONTEXT_INPUT_CONSUMED",
                format!("context input {input_id} already consumed"),
            ),
            other => Failure::external("CONTEXT_PROTOCOL", other.to_string()),
        }
    }
}

impl ContextSettlement for OperationSettlement {
    fn finish<'a>(
        &'a self,
        outcome: std::result::Result<JsValue, HostError>,
    ) -> BoxFuture<'a, Result<ops::OperationResult>> {
        Box::pin(async move {
            if self.retired.load(Ordering::Acquire) {
                return Err(Failure::external(
                    "AUTHORITY_REVOKED",
                    "context operation retired",
                ));
            }
            let value = outcome.map_err(|error| {
                Failure::external("CONTEXT_PRODUCT_ERROR", "original context operation failed")
                    .with("message_utf16", json!(error.module_error.0.units()))
            })?;
            let output = Self::encode_boundary_payload(&value, self.wire_operation.name())?;

            let mut transition = LedgerTransition::new(
                self.transaction_id.clone(),
                self.base_local_revision,
                self.operation,
            )
            .with_control(self.control.clone());

            let (history, preparation_notice) = match to_wire_operation(self.operation)? {
                ops::Operation::PrepareForModel => {
                    let has_system_prompt = value
                        .get("systemPrompt")
                        .and_then(JsValue::as_string)
                        .is_some_and(|s| !s.is_empty());
                    let facts = if self.control.preview_only {
                        Vec::new()
                    } else {
                        let summary = json_object([
                            (
                                "messageCount",
                                JsValue::Number(js_array_len(value.get("messages")) as f64),
                            ),
                            (
                                "toolCount",
                                JsValue::Number(js_array_len(value.get("tools")) as f64),
                            ),
                            ("hasSystemPrompt", JsValue::Bool(has_system_prompt)),
                        ]);
                        let fact_payload =
                            Self::encode_boundary_payload(&summary, "prepare_for_model fact")?;
                        transition =
                            transition.with_facts(vec![Self::ledger_payload(&fact_payload)]);
                        vec![history::Fact {
                            kind: "context.prepare_for_model".into(),
                            payload: fact_payload,
                        }]
                    };
                    (
                        ops::HistoryMutation {
                            replacement: None,
                            consume_input: false,
                            facts,
                            compaction_boundary: None,
                        },
                        Some(ops::PreparationNotice { has_system_prompt }),
                    )
                }
                ops::Operation::ApplyToolResults => {
                    let messages = value.get("messages").ok_or_else(|| {
                        Failure::external(
                            "CONTEXT_PROTOCOL",
                            "apply_tool_results output missing messages",
                        )
                    })?;
                    let replacement =
                        Self::encode_boundary_payload(messages, "apply_tool_results messages")?;
                    transition = transition.with_replacement(Self::ledger_payload(&replacement));
                    (
                        ops::HistoryMutation {
                            replacement: Some(replacement),
                            consume_input: false,
                            facts: Vec::new(),
                            compaction_boundary: None,
                        },
                        None,
                    )
                }
                ops::Operation::CaptureTurn => {
                    let summary = json_object([
                        (
                            "sessionId",
                            self.fields
                                .get("sessionId")
                                .cloned()
                                .unwrap_or(JsValue::Null),
                        ),
                        (
                            "turnId",
                            self.fields.get("turnId").cloned().unwrap_or(JsValue::Null),
                        ),
                        (
                            "errored",
                            JsValue::Bool(is_truthy(self.fields.get("errored"))),
                        ),
                        (
                            "messageCount",
                            JsValue::Number(js_array_len(self.fields.get("messages")) as f64),
                        ),
                    ]);
                    let fact_payload =
                        Self::encode_boundary_payload(&summary, "capture_turn fact")?;
                    transition = transition.with_facts(vec![Self::ledger_payload(&fact_payload)]);
                    (
                        ops::HistoryMutation {
                            replacement: None,
                            consume_input: false,
                            facts: vec![history::Fact {
                                kind: "context.capture_turn".into(),
                                payload: fact_payload,
                            }],
                            compaction_boundary: None,
                        },
                        None,
                    )
                }
                ops::Operation::TryAutoCompact => {
                    let compacted = value
                        .get("type")
                        .and_then(JsValue::as_string)
                        .is_some_and(|kind| kind.equals_str("compacted"));
                    if compacted {
                        let messages = value.get("messages").ok_or_else(|| {
                            Failure::external(
                                "CONTEXT_PROTOCOL",
                                "try_auto_compact compacted output missing messages",
                            )
                        })?;
                        let replacement =
                            Self::encode_boundary_payload(messages, "try_auto_compact messages")?;
                        let boundary_meta = json_object([
                            ("type", value.get("type").cloned().unwrap_or(JsValue::Null)),
                            ("tier", value.get("tier").cloned().unwrap_or(JsValue::Null)),
                            (
                                "snapshot",
                                value.get("snapshot").cloned().unwrap_or(JsValue::Null),
                            ),
                        ]);
                        let compaction_boundary = Self::encode_boundary_payload(
                            &boundary_meta,
                            "try_auto_compact boundary",
                        )?;
                        transition = transition
                            .with_replacement(Self::ledger_payload(&replacement))
                            .with_compaction(Self::ledger_payload(&compaction_boundary));
                        (
                            ops::HistoryMutation {
                                replacement: Some(replacement),
                                consume_input: false,
                                facts: Vec::new(),
                                compaction_boundary: Some(compaction_boundary),
                            },
                            None,
                        )
                    } else {
                        (
                            ops::HistoryMutation {
                                replacement: None,
                                consume_input: false,
                                facts: Vec::new(),
                                compaction_boundary: None,
                            },
                            None,
                        )
                    }
                }
                ops::Operation::RecoverFromModelError => {
                    return Err(Failure::external(
                        "CONTEXT_UNSUPPORTED",
                        "recover_from_model_error must use --pilotdeck-context-recovery-v1",
                    ));
                }
            };

            self.ledger
                .commit_transition(transition)
                .map_err(Self::map_ledger_error)?;

            Ok(ops::OperationResult {
                operation: self.wire_operation,
                output,
                history,
                preparation_notice,
            })
        })
    }

    fn validate_history(&self, history: &ops::HistoryMutation) -> Result<()> {
        if history.consume_input {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "unexpected consume_input in context operation mutation",
            ));
        }
        if self.operation == RuntimeOperation::PrepareForModel
            && (history.replacement.is_some() || history.compaction_boundary.is_some())
        {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "prepare_for_model cannot replace durable history",
            ));
        }
        if self.control.preview_only
            && (history.replacement.is_some()
                || !history.facts.is_empty()
                || history.compaction_boundary.is_some())
        {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "preview_only prepare_for_model cannot propose history mutations",
            ));
        }
        if self.operation == RuntimeOperation::CaptureTurn
            && (history.replacement.is_some() || history.compaction_boundary.is_some())
        {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "capture_turn cannot emit replacement or compaction_boundary",
            ));
        }
        Ok(())
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}

fn js_array_len(value: Option<&JsValue>) -> usize {
    match value {
        Some(JsValue::Array(items)) => items.len(),
        _ => 0,
    }
}

fn json_object<const N: usize>(entries: [(&str, JsValue); N]) -> JsValue {
    let mut obj = JsObject::new();
    for (key, value) in entries {
        obj.insert(key.into(), value);
    }
    JsValue::Object(obj)
}

fn js_string_field(fields: &JsObject, key: &str) -> Option<String> {
    fields
        .get(key)
        .and_then(JsValue::as_string)
        .and_then(|s| s.to_utf8().ok())
}

fn is_truthy(value: Option<&JsValue>) -> bool {
    match value {
        None | Some(JsValue::Undefined | JsValue::Null) => false,
        Some(JsValue::Bool(flag)) => *flag,
        Some(JsValue::Number(num)) => *num != 0.0 && !num.is_nan(),
        Some(JsValue::String(s)) => !s.is_empty(),
        Some(JsValue::Array(_) | JsValue::Object(_)) => true,
    }
}

fn extract_control(fields: &JsObject) -> ContextControl {
    let route = match (
        js_string_field(fields, "provider"),
        js_string_field(fields, "model"),
    ) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (Some(p), None) => Some(p),
        _ => None,
    };
    let max_context_tokens = fields
        .get("maxContextTokens")
        .and_then(JsValue::as_number)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as u64);
    ContextControl {
        protocol_version: CONTEXT_LEDGER_PROTOCOL_VERSION,
        route,
        protocol: js_string_field(fields, "protocol"),
        preview_only: is_truthy(fields.get("previewOnly")),
        manual_force: is_truthy(fields.get("manualForce")),
        max_context_tokens,
        reserved_output_tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_iface::BoxFuture;
    use agentcpm_scheduler_protocol::{
        CallView, Capability, ControlKind, ControlOutcome, Destination, Event, Invocation,
        Result as PResult, VersionedState, Wait,
        context_history::{OpaquePayload, Owner, Reservation},
    };

    struct LiveReservationClient {
        context: CallContext,
        parent: CallView,
    }

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    fn call_view_from(
        ctx: &CallContext,
        parent_session_ref: Option<String>,
        source_binding: Option<String>,
        cause_execution_ref: Option<String>,
    ) -> CallView {
        CallView {
            id: ctx.call_ref.clone(),
            session_ref: ctx.session_ref.clone(),
            source_binding,
            parent_session_ref,
            cause_execution_ref,
            binding: ctx.binding.clone(),
            invocation: ctx.invocation.clone(),
            state: State::Running,
            result: None,
            presentation: None,
            settlement_data: None,
            settlement_error: None,
            error: None,
            waiting: None,
            cursor: 0,
            origin: ctx.origin.clone(),
            interaction: ctx.interaction.clone(),
            interactions: Vec::new(),
            cancel_requested: false,
            stepped: false,
            security: ctx.security.clone(),
        }
    }

    impl Client for LiveReservationClient {
        fn get(&self, call: &str) -> BoxFuture<'_, PResult<CallView>> {
            let call = call.to_owned();
            let view = if call == self.context.call_ref {
                call_view_from(
                    &self.context,
                    Some(self.context.session_ref.clone()),
                    Some(self.parent.binding.id.clone()),
                    Some(self.parent.id.clone()),
                )
            } else if call == self.parent.id {
                self.parent.clone()
            } else {
                return Box::pin(async { Err(Failure::external("NOT_FOUND", "missing call")) });
            };
            Box::pin(async move { Ok(view) })
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<CallView>>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<CallView>> {
            unused()
        }
        fn publish(
            &self,
            _: &str,
            _: &str,
            _: serde_json::Value,
            _: State,
            _: Option<Failure>,
            _: Option<Wait>,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<CallView>>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<agentcpm_scheduler_protocol::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(
            &self,
            _: agentcpm_scheduler_protocol::StateChange,
        ) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn resource(
            &self,
            _: &str,
            _: serde_json::Value,
        ) -> BoxFuture<'_, PResult<serde_json::Value>> {
            unused()
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    fn make_child_context_and_client(
        op: ops::Operation,
        binding: &str,
        input_json: &str,
        tx_id: &str,
    ) -> (CallContext, Arc<dyn Client>) {
        let mut child_ctx = crate::tests::context();
        child_ctx.binding.id = binding.into();
        child_ctx.call_ref = "child-ctx-call".into();
        let parent_call = "parent-loop-call".to_owned();
        let parent_binding = "pilotdeck-loop".to_owned();
        child_ctx.invocation.parent_execution_ref = Some(parent_call.clone());

        let js_input = pilotdeck_protocol::parse_json(input_json).unwrap();
        let bytes = boundary_codec::encode(&js_input).unwrap();
        let request = ops::InvocationRequest {
            operation: op,
            input: OpaquePayload {
                profile: boundary_codec::PROFILE.into(),
                bytes,
            },
            reservation: Reservation {
                transaction_id: tx_id.into(),
                request_digest: "req-digest-1".into(),
                expected_revision: 0,
                prepare_cursor: 1,
                owner: Owner {
                    session: child_ctx.session_ref.clone(),
                    binding: parent_binding.clone(),
                    context: child_ctx.invocation.context_ref.clone(),
                    call: parent_call.clone(),
                    origin: child_ctx.interaction.clone(),
                },
            },
        };
        child_ctx.invocation.payload = serde_json::to_value(request).unwrap();

        let mut parent_ctx = crate::tests::context();
        parent_ctx.call_ref = parent_call.clone();
        parent_ctx.binding.id = parent_binding;
        parent_ctx.invocation.context_ref = child_ctx.invocation.context_ref.clone();
        let parent_view = call_view_from(&parent_ctx, None, None, None);
        let client: Arc<dyn Client> = Arc::new(LiveReservationClient {
            context: child_ctx.clone(),
            parent: parent_view,
        });
        (child_ctx, client)
    }

    #[test]
    fn cli_config_validates_four_roles_and_rejects_recovery_or_bad_fields() {
        for op in [
            ops::Operation::PrepareForModel,
            ops::Operation::ApplyToolResults,
            ops::Operation::CaptureTurn,
            ops::Operation::TryAutoCompact,
        ] {
            let cfg = ContextOperationCliConfig {
                profile: CONTEXT_OPERATION_PROFILE.into(),
                binding: format!("ctx-{}", op.name()),
                operation: op,
                project_dir: Some("/abs/workspace".into()),
            };
            let raw = serde_json::to_string(&cfg).unwrap();
            assert_eq!(parse_context_operation_cli_json(&raw).unwrap(), cfg);
        }

        // RecoverFromModelError must be rejected cleanly with exit 64 and hint toward recovery flag.
        let recovery_json = serde_json::json!({
            "profile": CONTEXT_OPERATION_PROFILE,
            "binding": "ctx-recover",
            "operation": "recover_from_model_error"
        })
        .to_string();
        let err = parse_context_operation_cli_json(&recovery_json).unwrap_err();
        assert_eq!(err.exit_code(), 64);
        assert!(err.to_string().contains("--pilotdeck-context-recovery-v1"));

        // Invalid profile / unknown fields / bad binding / relative project_dir reject with exit 64.
        for bad in [
            "not-json".to_owned(),
            serde_json::json!({
                "profile": "wrong",
                "binding": "b",
                "operation": "prepare_for_model"
            })
            .to_string(),
            serde_json::json!({
                "profile": CONTEXT_OPERATION_PROFILE,
                "binding": "b",
                "operation": "prepare_for_model",
                "extra": 1
            })
            .to_string(),
            serde_json::json!({
                "profile": CONTEXT_OPERATION_PROFILE,
                "binding": "b",
                "operation": "prepare_for_model",
                "project_dir": "relative/dir"
            })
            .to_string(),
        ] {
            let err = parse_context_operation_cli_json(&bad).unwrap_err();
            assert_eq!(err.exit_code(), 64);
        }

        // Direct construction rejects RecoverFromModelError too.
        let direct_err = context_operation_capability(
            ops::Operation::RecoverFromModelError,
            "ctx-recover".into(),
            None,
        )
        .err()
        .expect("RecoverFromModelError must fail closed");
        assert_eq!(direct_err.code, "CONTEXT_UNSUPPORTED");
    }

    #[tokio::test]
    async fn four_non_recovery_capabilities_execute_real_runtime_and_emit_strict_results() {
        // 1. PrepareForModel -> returns PreparationNotice and boundary-encoded output.
        let prepare_cap = context_operation_capability(
            ops::Operation::PrepareForModel,
            "ctx-prepare".into(),
            Some("/tmp".into()),
        )
        .unwrap();
        let (ctx, client) = make_child_context_and_client(
            ops::Operation::PrepareForModel,
            "ctx-prepare",
            r#"{"sessionId":"s1","turnId":"t1","cwd":"/tmp","provider":"anthropic","model":"claude","permissionMode":"default","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"tools":[]}"#,
            "tx-prepare-1",
        );
        let raw_val = prepare_cap.invoke(ctx, client).await.unwrap();
        let res: ops::OperationResult = serde_json::from_value(raw_val).unwrap();
        res.validate_for(ops::Operation::PrepareForModel).unwrap();
        assert!(res.preparation_notice.is_some());
        assert!(res.history.replacement.is_none());
        assert!(!res.history.consume_input);
        assert_eq!(res.history.facts.len(), 1);
        let decoded_prepare = boundary_codec::decode(&res.output.bytes).unwrap();
        assert!(decoded_prepare.get("messages").is_some());

        // 2. ApplyToolResults -> appends tool_result and returns replacement without preparation_notice.
        let apply_cap = context_operation_capability(
            ops::Operation::ApplyToolResults,
            "ctx-apply".into(),
            None,
        )
        .unwrap();
        let (ctx, client) = make_child_context_and_client(
            ops::Operation::ApplyToolResults,
            "ctx-apply",
            r#"{"turnId":"t1","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"tool_call","id":"call_1","name":"read_file","input":{}}]}],"toolResultMessage":{"role":"user","content":[{"type":"tool_result","toolCallId":"call_1","content":[{"type":"text","text":"ok"}]}]}}"#,
            "tx-apply-1",
        );
        let raw_val = apply_cap.invoke(ctx, client).await.unwrap();
        let res: ops::OperationResult = serde_json::from_value(raw_val).unwrap();
        res.validate_for(ops::Operation::ApplyToolResults).unwrap();
        assert!(res.preparation_notice.is_none());
        assert!(res.history.replacement.is_some());
        let decoded_apply = boundary_codec::decode(&res.output.bytes).unwrap();
        let Some(JsValue::Array(applied_msgs)) = decoded_apply.get("messages") else {
            panic!("apply_tool_results messages must be an array");
        };
        assert_eq!(applied_msgs.len(), 3);

        // 3. CaptureTurn -> emits compact fact without replacement or preparation_notice.
        let capture_cap =
            context_operation_capability(ops::Operation::CaptureTurn, "ctx-capture".into(), None)
                .unwrap();
        let (ctx, client) = make_child_context_and_client(
            ops::Operation::CaptureTurn,
            "ctx-capture",
            r#"{"sessionId":"s1","turnId":"t1","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"errored":false}"#,
            "tx-capture-1",
        );
        let raw_val = capture_cap.invoke(ctx, client).await.unwrap();
        let res: ops::OperationResult = serde_json::from_value(raw_val).unwrap();
        res.validate_for(ops::Operation::CaptureTurn).unwrap();
        assert!(res.preparation_notice.is_none());
        assert!(res.history.replacement.is_none());
        assert_eq!(res.history.facts.len(), 1);
        assert_eq!(
            boundary_codec::decode(&res.output.bytes).unwrap(),
            JsValue::Null
        );

        // 4. TryAutoCompact -> executes DefaultContextRuntime::try_auto_compact without preparation_notice.
        let compact_cap = context_operation_capability(
            ops::Operation::TryAutoCompact,
            "ctx-compact".into(),
            None,
        )
        .unwrap();
        let (ctx, client) = make_child_context_and_client(
            ops::Operation::TryAutoCompact,
            "ctx-compact",
            r#"{"sessionId":"s1","messages":[]}"#,
            "tx-compact-1",
        );
        let raw_val = compact_cap.invoke(ctx, client).await.unwrap();
        let res: ops::OperationResult = serde_json::from_value(raw_val).unwrap();
        res.validate_for(ops::Operation::TryAutoCompact).unwrap();
        assert!(res.preparation_notice.is_none());
        let decoded_compact = boundary_codec::decode(&res.output.bytes).unwrap();
        assert_eq!(
            decoded_compact
                .get("type")
                .and_then(JsValue::as_string)
                .unwrap()
                .to_utf8()
                .unwrap(),
            "skipped"
        );

        // 5. PrepareForModel with previewOnly=true emits PreparationNotice and zero history mutation.
        let (preview_ctx, preview_client) = make_child_context_and_client(
            ops::Operation::PrepareForModel,
            "ctx-prepare",
            r#"{"sessionId":"s1","turnId":"t2","cwd":"/tmp","provider":"anthropic","model":"claude","permissionMode":"default","previewOnly":true,"messages":[{"role":"user","content":[{"type":"text","text":"preview"}]}],"tools":[]}"#,
            "tx-prepare-preview",
        );
        let preview_val = prepare_cap
            .invoke(preview_ctx, preview_client)
            .await
            .unwrap();
        let preview_res: ops::OperationResult = serde_json::from_value(preview_val).unwrap();
        preview_res
            .validate_for(ops::Operation::PrepareForModel)
            .unwrap();
        assert!(preview_res.preparation_notice.is_some());
        assert!(preview_res.history.replacement.is_none());
        assert!(preview_res.history.facts.is_empty());
        assert!(preview_res.history.compaction_boundary.is_none());

        // 6. Conflicting transaction_id with mutated payload rejects via AuthoritativeContextLedger.
        let (conflict_ctx, conflict_client) = make_child_context_and_client(
            ops::Operation::PrepareForModel,
            "ctx-prepare",
            r#"{"sessionId":"s1","turnId":"t99","cwd":"/tmp","provider":"anthropic","model":"claude","permissionMode":"default","messages":[{"role":"user","content":[{"type":"text","text":"mutated"}]}],"tools":[]}"#,
            "tx-prepare-1",
        );
        let err = prepare_cap
            .invoke(conflict_ctx, conflict_client)
            .await
            .unwrap_err();
        assert_eq!(err.code, "CONTEXT_IDEMPOTENCY_CONFLICT");

        // 7. Invalid runtime input maps cleanly to CONTEXT_PRODUCT_ERROR with UTF-16 message.
        let (bad_ctx, bad_client) = make_child_context_and_client(
            ops::Operation::PrepareForModel,
            "ctx-prepare",
            r#"{"sessionId":"s2","turnId":"t1","messages":"not-an-array","tools":[]}"#,
            "tx-prepare-bad",
        );
        let bad_err = prepare_cap.invoke(bad_ctx, bad_client).await.unwrap_err();
        assert_eq!(bad_err.code, "CONTEXT_PRODUCT_ERROR");
        assert!(bad_err.params.contains_key("message_utf16"));
    }
}
