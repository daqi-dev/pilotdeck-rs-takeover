//! Native SDK Context capability execution seam.
//! The bound host is required; no local I/O, default capabilities or fallback.
use agentcpm_iface::BoxFuture;
use agentcpm_scheduler_protocol::{
    CallContext, Capability, Client, Failure, Result, context_operations as ops,
};
use pilotdeck_context_runtime::{
    host::HostError,
    operations::{ContextInput, ContextRuntime, Operation},
};
use pilotdeck_protocol::JsValue;
use serde_json::Value;
use std::sync::Arc;

/// Trusted Kernel-backed binding. `supports` is a frozen installation fact,
/// never a value supplied by the invocation payload. `admit` validates source,
/// generation, scope and the Kernel-owned history reservation/dispatch permit,
/// decodes the negotiated profile and constructs only scoped resource proxies.
/// It must not initiate model, fs, memory, artifact or history mutations itself.
pub trait ContextBinding: Send + Sync {
    fn supports(&self, operation: Operation) -> bool;
    /// Synchronous read-only construction; registers an owner-held admission
    /// attempt before any await. It may not dispatch external effects.
    fn begin(
        &self,
        context: &CallContext,
        client: Arc<dyn Client>,
        operation: Operation,
        request: &ops::InvocationRequest,
        decoded_input: JsValue,
    ) -> Result<Arc<dyn AdmissionAttempt>>;
}
pub trait AdmissionAttempt: Send + Sync {
    fn admit(&self) -> BoxFuture<'_, Result<AdmittedContext>>;
    /// Fence new work and retain any admitted identities, not release proof.
    fn retire(&self);
}
struct AdmissionRetire(Arc<dyn AdmissionAttempt>);
impl Drop for AdmissionRetire {
    fn drop(&mut self) {
        self.0.retire();
    }
}
pub struct AdmittedContext {
    pub runtime: Arc<dyn ContextRuntime>,
    pub input: ContextInput,
    pub settlement: Arc<dyn ContextSettlement>,
}
/// Kernel-backed operation owner retains every admitted resource independent of
/// the capability future. `finish` must preserve definite vs unknown errors,
/// await or retain real model/artifact/memory release facts, encode the agreed
/// lossless profile and return only candidate data to the parent. The parent
/// owns the subsequent materialization/replacement history commit.
pub trait ContextSettlement: Send + Sync {
    fn finish<'a>(
        &'a self,
        outcome: std::result::Result<JsValue, HostError>,
    ) -> BoxFuture<'a, Result<ops::OperationResult>>;
    /// Validate every proposed replacement, fact and boundary against the
    /// installed product profile and the actual staged operation outcomes.
    fn validate_history(&self, mutation: &ops::HistoryMutation) -> Result<()>;
    /// Idempotently fence new work and retain already-admitted identities for
    /// observation; retirement is not a release receipt and must not replay.
    fn retire(&self);
}
struct RetireOnDrop(Arc<dyn ContextSettlement>);
impl Drop for RetireOnDrop {
    fn drop(&mut self) {
        self.0.retire();
    }
}

pub struct NativeContextCapability {
    operation: Operation,
    binding_ref: String,
    binding: Arc<dyn ContextBinding>,
}
impl NativeContextCapability {
    /// The host installs one operation per fixed binding. Absence is explicit;
    /// an operation with missing real ports may not be installed or advertised.
    pub fn new(
        operation: Operation,
        binding_ref: String,
        binding: Arc<dyn ContextBinding>,
    ) -> Result<Self> {
        if !binding.supports(operation) {
            return Err(Failure::external(
                "CONTEXT_UNSUPPORTED",
                "operation has no complete installed binding",
            ));
        }
        if binding_ref.is_empty() {
            return Err(Failure::external(
                "CONTEXT_UNSUPPORTED",
                "binding identity is empty",
            ));
        }
        Ok(Self {
            operation,
            binding_ref,
            binding,
        })
    }
}
impl Capability for NativeContextCapability {
    fn invoke(
        &self,
        context: CallContext,
        client: Arc<dyn Client>,
    ) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            // Invocation payload is Kernel-created typed DTO evidence, never
            // an authority grant. The binding validates the source and scope.
            if context.binding.id != self.binding_ref {
                return Err(Failure::external(
                    "CONTEXT_BINDING_MISMATCH",
                    "wrong installed context capability",
                ));
            }
            let raw_bytes = context
                .invocation
                .payload
                .get("input")
                .and_then(|input| input.get("bytes"))
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Failure::external("CONTEXT_PROTOCOL", "missing opaque input bytes")
                })?;
            if raw_bytes.len() > pilotdeck_context_runtime::boundary_codec::MAX_BYTES {
                return Err(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "context input exceeds profile limit",
                ));
            }
            let request: ops::InvocationRequest =
                serde_json::from_value(context.invocation.payload.clone()).map_err(|_| {
                    Failure::external("CONTEXT_PROTOCOL", "invalid typed invocation")
                })?;
            if request.operation.name() != self.operation.name()
                || request.reservation.owner.session != context.session_ref
                || request.reservation.owner.origin != context.interaction
                || context.invocation.parent_execution_ref.as_deref()
                    != Some(request.reservation.owner.call.as_str())
                || request.input.profile != pilotdeck_context_runtime::boundary_codec::PROFILE
                || request.input.bytes.len() > pilotdeck_context_runtime::boundary_codec::MAX_BYTES
            {
                return Err(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "context child does not match fixed operation or reservation",
                ));
            }
            let decoded = pilotdeck_context_runtime::boundary_codec::decode(&request.input.bytes)
                .map_err(|_| {
                Failure::external("CONTEXT_PROTOCOL", "invalid context input encoding")
            })?;
            if decoded.as_object().is_none() {
                return Err(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "context input must be an object",
                ));
            }
            let attempt =
                self.binding
                    .begin(&context, client, self.operation, &request, decoded)?;
            let _admission_retire = AdmissionRetire(attempt.clone());
            let admitted = attempt.admit().await?;
            let _retire = RetireOnDrop(admitted.settlement.clone());
            if !admitted.runtime.supports(self.operation) {
                return Err(Failure::external(
                    "CONTEXT_UNSUPPORTED",
                    "runtime lacks the fixed operation",
                ));
            }
            let result = match self.operation {
                Operation::PrepareForModel => {
                    admitted.runtime.prepare_for_model(admitted.input).await
                }
                Operation::ApplyToolResults => {
                    admitted.runtime.apply_tool_results(admitted.input).await
                }
                Operation::RecoverFromModelError => {
                    admitted
                        .runtime
                        .recover_from_model_error(admitted.input)
                        .await
                }
                Operation::CaptureTurn => admitted.runtime.capture_turn(admitted.input).await,
                Operation::TryAutoCompact => {
                    admitted.runtime.try_auto_compact(admitted.input).await
                }
            };
            let output = admitted.settlement.finish(result).await?;
            output.validate_for(request.operation)?;
            if output.output.profile != pilotdeck_context_runtime::boundary_codec::PROFILE
                || output.output.bytes.len() > pilotdeck_context_runtime::boundary_codec::MAX_BYTES
            {
                return Err(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "invalid context output profile or size",
                ));
            }
            pilotdeck_context_runtime::boundary_codec::decode(&output.output.bytes).map_err(
                |_| Failure::external("CONTEXT_PROTOCOL", "invalid context output encoding"),
            )?;
            validate_history_payloads(&output.history, output.output.bytes.len())?;
            admitted.settlement.validate_history(&output.history)?;
            serde_json::to_value(output)
                .map_err(|_| Failure::external("CONTEXT_PROTOCOL", "cannot encode context result"))
        })
    }
}

fn validate_history_payloads(history: &ops::HistoryMutation, output_len: usize) -> Result<()> {
    if history.facts.len() > 1_024 {
        return Err(Failure::external(
            "CONTEXT_PROTOCOL",
            "too many history facts",
        ));
    }
    let mut total = output_len;
    for fact in &history.facts {
        if fact.kind.is_empty() || fact.kind.len() > 256 {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "invalid history fact kind",
            ));
        }
        total = total.checked_add(fact.kind.len()).ok_or_else(|| {
            Failure::external("CONTEXT_PROTOCOL", "history payload size overflow")
        })?;
    }
    for payload in history
        .replacement
        .iter()
        .chain(history.compaction_boundary.iter())
        .chain(history.facts.iter().map(|fact| &fact.payload))
    {
        if payload.profile.is_empty() || payload.profile.len() > 256 {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "invalid history payload profile",
            ));
        }
        total = total
            .checked_add(payload.profile.len())
            .and_then(|n| n.checked_add(payload.bytes.len()))
            .ok_or_else(|| {
                Failure::external("CONTEXT_PROTOCOL", "history payload size overflow")
            })?;
        if total > pilotdeck_context_runtime::boundary_codec::MAX_BYTES {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "history candidate exceeds inline profile limit",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_scheduler_protocol::context_history::{OpaquePayload, Owner, Reservation};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct RejectBinding(Arc<AtomicUsize>);
    impl ContextBinding for RejectBinding {
        fn supports(&self, operation: Operation) -> bool {
            operation == Operation::PrepareForModel
        }
        fn begin(
            &self,
            _: &CallContext,
            _: Arc<dyn Client>,
            _: Operation,
            _: &ops::InvocationRequest,
            _: JsValue,
        ) -> Result<Arc<dyn AdmissionAttempt>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(Failure::external("TEST_UNAVAILABLE", "must not run"))
        }
    }
    #[tokio::test]
    async fn invalid_profile_rejected_before_binding_or_effects() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut context = crate::tests::context();
        let parent = "parent-call";
        let request = ops::InvocationRequest {
            operation: ops::Operation::PrepareForModel,
            input: OpaquePayload {
                profile: "wrong.profile".into(),
                bytes: vec![1],
            },
            reservation: Reservation {
                transaction_id: "transaction".into(),
                request_digest: "digest".into(),
                expected_revision: 0,
                prepare_cursor: 1,
                owner: Owner {
                    session: context.session_ref.clone(),
                    binding: "loop".into(),
                    context: "turn".into(),
                    call: parent.into(),
                    origin: context.interaction.clone(),
                },
            },
        };
        context.invocation.parent_execution_ref = Some(parent.into());
        context.invocation.payload = serde_json::to_value(request).unwrap();
        let capability = NativeContextCapability::new(
            Operation::PrepareForModel,
            context.binding.id.clone(),
            Arc::new(RejectBinding(calls.clone())),
        )
        .unwrap();
        assert_eq!(
            capability
                .invoke(context, crate::tests::client(None))
                .await
                .unwrap_err()
                .code,
            "CONTEXT_PROTOCOL"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
