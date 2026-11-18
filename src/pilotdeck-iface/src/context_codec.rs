//! Product Context HostCall projection via public Kernel Context/History SDK.
//! Capability inventory is supplied by the trusted host.
use crate::port_helpers::invalid;
use crate::resources::{ResourceCodec, ResourceRequest, ResourceStep};
use agentcpm_scheduler_protocol::{
    CallContext, State, context_history as history, context_operations as ops,
};
use pilotdeck_agent::ports::{HostCall, HostReply, PortError, PortResult};
use pilotdeck_protocol::JsValue;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

/// Shared only between the actual codec and preparation notice owner of this
/// execution. A proof is a Kernel result, never supplied by a JS payload.
#[derive(Default)]
pub struct ContextProofs {
    committed: Mutex<Vec<ops::Committed>>,
    unresolved: Mutex<Vec<ContextUnresolved>>,
    snapshots: Mutex<Vec<Pending>>,
    next: AtomicU64,
    active: AtomicBool,
    scope: Mutex<Option<CallContext>>,
}
#[derive(Clone, Debug)]
pub struct ContextUnresolved {
    pub transaction_id: String,
    pub request_digest: Option<String>,
    pub operation: Option<ops::Operation>,
    pub child_ref: Option<String>,
}
impl ContextProofs {
    /// Observations only; host must validate against the Kernel owner.
    pub fn observed(&self) -> Vec<ops::Committed> {
        self.committed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// References only. Missing data never proves no effect; read the Kernel
    /// owner by transaction ID and digest without redispatching.
    pub fn unresolved(&self) -> Vec<ContextUnresolved> {
        self.unresolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Record an unresolved transaction unless already present.
    pub fn retain_unresolved(&self, entry: ContextUnresolved) {
        let mut unresolved = self.unresolved.lock().unwrap_or_else(|p| p.into_inner());
        if !unresolved
            .iter()
            .any(|value| value.transaction_id == entry.transaction_id)
        {
            unresolved.push(entry);
        }
    }
    /// Upsert the pending snapshot for a transaction.
    fn upsert_snapshot(&self, state: &Pending) {
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = snapshots.iter_mut().find(|value| value.tx == state.tx) {
            *existing = state.clone();
        } else {
            snapshots.push(state.clone());
        }
    }
    /// Drop a transaction from the unresolved and snapshot registries.
    pub fn forget(&self, tx: &str) {
        self.unresolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|entry| entry.transaction_id != tx);
        self.snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|entry| entry.tx != tx);
    }
}
#[derive(Clone)]
pub struct InstalledContextOperation {
    pub operation: ops::Operation,
    pub binding: String,
    pub revision: String,
}
#[derive(Clone)]
struct Pending {
    operation: ops::Operation,
    tx: String,
    input: history::OpaquePayload,
    digest: Option<String>,
    child: Option<String>,
    output: Option<JsValue>,
    result_digest: Option<String>,
    revision: u64,
    reconciling: bool,
    phase: Phase,
}
#[derive(Clone, Copy)]
enum Phase {
    Read,
    ReconcileRead,
    Reserve,
    Dispatch,
    Status,
    Commit,
    SettleFailure,
}
pub struct ContextCodec {
    context: CallContext,
    installed: Vec<InstalledContextOperation>,
    proofs: Arc<ContextProofs>,
    pending: Option<Pending>,
    failure_settle_retries: usize,
    reserve_rejection_failure: Option<agentcpm_scheduler_protocol::Failure>,
}
impl ContextCodec {
    pub fn new(
        context: CallContext,
        installed: Vec<InstalledContextOperation>,
        proofs: Arc<ContextProofs>,
    ) -> PortResult<Self> {
        for (index, item) in installed.iter().enumerate() {
            if item.binding.is_empty()
                || item.revision.is_empty()
                || installed[..index]
                    .iter()
                    .any(|old| old.operation == item.operation || old.binding == item.binding)
            {
                return Err(invalid("invalid or duplicated installed Context binding"));
            }
        }
        {
            let mut scope = proofs.scope.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(original) = scope.as_ref() {
                if !crate::port_helpers::same_call_scope(original, &context) {
                    return Err(invalid("Context operation owner scope mismatch"));
                }
            } else {
                *scope = Some(context.clone());
            }
        }
        if proofs
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(invalid(
                "Context operation owner already has an active codec",
            ));
        }
        Ok(Self {
            context,
            installed,
            proofs,
            pending: None,
            failure_settle_retries: 0,
            reserve_rejection_failure: None,
        })
    }
    fn history_owner(&self) -> history::Owner {
        history::Owner {
            session: self.context.session_ref.clone(),
            binding: self.context.binding.id.clone(),
            context: self.context.invocation.context_ref.clone(),
            call: self.context.call_ref.clone(),
            origin: self.context.interaction.clone(),
        }
    }
    /// Fail closed unless the child call view matches the pending state,
    /// installed binding, and scheduler call context.
    fn require_matching_child(
        &self,
        state: &Pending,
        installed: &InstalledContextOperation,
        child: &agentcpm_scheduler_protocol::CallView,
    ) -> Result<(), PortError> {
        if child.session_ref != self.context.session_ref
            || child.parent_session_ref.as_deref() != Some(self.context.session_ref.as_str())
            || child.origin != self.context.interaction
            || child.security != self.context.security
            || child.binding.id != installed.binding
            || child.binding.revision != installed.revision
            || child.interaction != self.context.interaction
            || child.source_binding.as_deref() != Some(self.context.binding.id.as_str())
            || child.cause_execution_ref.as_deref() != Some(self.context.call_ref.as_str())
        {
            return Err(self.retain_unknown(state));
        }
        Ok(())
    }
    /// Parse the child invocation payload and fail closed unless it matches the
    /// pending state, history reservation, and scheduler call context.
    fn require_matching_invocation(
        &self,
        state: &Pending,
        child: &agentcpm_scheduler_protocol::CallView,
    ) -> Result<(), PortError> {
        let invocation: ops::InvocationRequest =
            serde_json::from_value(child.invocation.payload.clone())
                .map_err(|_| self.retain_unknown(state))?;
        if invocation.operation != state.operation
            || invocation.input != state.input
            || invocation
                .reservation
                .verify(&self.history_owner(), &Self::prepare_request(state))
                .is_err()
            || Some(&invocation.reservation.request_digest) != state.digest.as_ref()
            || invocation.reservation.owner.session != self.context.session_ref
            || invocation.reservation.owner.binding != self.context.binding.id
            || invocation.reservation.owner.context != self.context.invocation.context_ref
            || invocation.reservation.owner.call != self.context.call_ref
            || invocation.reservation.owner.origin != self.context.interaction
        {
            return Err(self.retain_unknown(state));
        }
        Ok(())
    }
    fn prepare_request(state: &Pending) -> history::PrepareRequest {
        history::PrepareRequest {
            transaction_id: state.tx.clone(),
            expected_revision: state.revision,
            operation: state.operation.name().into(),
            request: state.input.clone(),
        }
    }
    fn binding(&self, operation: ops::Operation) -> Option<&InstalledContextOperation> {
        self.installed.iter().find(|v| v.operation == operation)
    }
    /// Verify commit receipt/proof continuity against pending state (shared by
    /// the status-observation and commit-acknowledgement paths).
    fn committed_matches(
        &self,
        committed: &ops::Committed,
        state: &Pending,
        installed: &InstalledContextOperation,
        expected_call_ref: Option<&str>,
        result_digest: Option<&str>,
    ) -> bool {
        committed.receipt.transaction_id == state.tx
            && Some(&committed.receipt.request_digest) == state.digest.as_ref()
            && Some(committed.proof.call_ref.as_str()) == expected_call_ref
            && committed.proof.binding_ref == installed.binding
            && committed.proof.binding_revision == installed.revision
            && committed.proof.operation == state.operation
            && committed.proof.origin == self.context.interaction
            && result_digest == Some(committed.proof.result_digest.as_str())
            && state.revision.checked_add(1) == Some(committed.receipt.revision)
            && (committed.receipt.consumed_inputs.is_empty()
                || committed.receipt.consumed_inputs.as_slice()
                    == [self.context.interaction.clone()])
    }
    /// Continue with a durable receipt query for the pending transaction.
    fn status_step(state: &Pending, request_digest: String) -> PortResult<ResourceStep> {
        Self::request(
            ops::STATUS,
            &history::ReceiptQuery {
                transaction_id: state.tx.clone(),
                request_digest,
            },
        )
    }
    fn request<T: serde::Serialize>(method: &'static str, value: &T) -> PortResult<ResourceStep> {
        let payload = serde_json::to_value(value)
            .map_err(|_| invalid("Context SDK request encoding failed"))?;
        Ok(ResourceStep::Request(ResourceRequest { method, payload }))
    }
    /// Drop a provably never-admitted transaction from the unresolved and
    /// snapshot registries: the kernel's admission gate is authority that no
    /// effect started, so nothing remains to reconcile.
    fn retire_pre_admission(&self, state: &Pending) {
        self.proofs.forget(&state.tx);
    }
    /// Retire a transaction whose prepare the kernel provably never admitted
    /// and surface the original rejection as a known host failure.
    fn retire_pre_admission_rejection(
        &self,
        state: &Pending,
        failure: &agentcpm_scheduler_protocol::Failure,
    ) -> PortResult<HostReply> {
        self.retire_pre_admission(state);
        let msg: pilotdeck_protocol::JsString = failure.wire().into();
        Err(PortError::HostFailure {
            code: failure.code.as_str().into(),
            boundary: pilotdeck_protocol::model::BoundarySnapshot::capture(&JsValue::String(
                msg.clone(),
            )),
            message: msg,
        })
    }
    fn retain_unknown(&self, state: &Pending) -> PortError {
        self.proofs.retain_unresolved(ContextUnresolved {
            transaction_id: state.tx.clone(),
            request_digest: state.digest.clone(),
            operation: Some(state.operation),
            child_ref: state.child.clone(),
        });
        self.proofs.upsert_snapshot(state);
        self.unknown(&state.tx)
    }
    /// Start an explicit read-only observation of an existing uncertain
    /// operation. This never dispatches, commits or grants new work.
    pub fn begin_reconciliation(&mut self, transaction_id: &str) -> PortResult<ResourceStep> {
        if self.pending.is_some() {
            return Err(invalid("another Context operation is pending"));
        }
        let mut state = self
            .proofs
            .snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|value| value.tx == transaction_id)
            .cloned()
            .ok_or_else(|| self.unknown(transaction_id))?;
        state.reconciling = true;
        let step = if let Some(digest) = &state.digest {
            state.phase = Phase::Status;
            Self::status_step(&state, digest.clone())?
        } else {
            state.phase = Phase::ReconcileRead;
            Self::request(history::READ, &json!({}))?
        };
        self.pending = Some(state);
        Ok(step)
    }
    fn unknown(&self, tx: &str) -> PortError {
        crate::port_helpers::unknown_with_identity(crate::port_helpers::execution_identity(
            &self.context,
            tx,
            self.context.call_ref.as_str(),
            tx,
        ))
    }
}
impl Drop for ContextCodec {
    fn drop(&mut self) {
        if let Some(state) = self.pending.take()
            && !matches!(state.phase, Phase::Read)
        {
            let _ = self.retain_unknown(&state);
        }
        self.proofs.active.store(false, Ordering::Release);
    }
}
fn is_preadmission_cancel_rejection(failure: &agentcpm_scheduler_protocol::Failure) -> bool {
    if matches!(
        failure.code.as_str(),
        "CALLER_ENDED" | "STEP_AUTHORITY_REVOKED"
    ) {
        return true;
    }
    failure.code == "OUTCOME_UNKNOWN"
        && failure
            .params
            .get("cause")
            .and_then(|cause| cause.get("code"))
            .and_then(Value::as_str)
            .is_some_and(|code| matches!(code, "CALLER_ENDED" | "STEP_AUTHORITY_REVOKED"))
}
fn op(call: &HostCall) -> Option<ops::Operation> {
    Some(match call {
        HostCall::PrepareContext(_) => ops::Operation::PrepareForModel,
        HostCall::ApplyToolResults(_) => ops::Operation::ApplyToolResults,
        HostCall::RecoverModelError(_) | HostCall::RecoverModelErrorBoundary(_) => {
            ops::Operation::RecoverFromModelError
        }
        HostCall::CaptureTurn(_) => ops::Operation::CaptureTurn,
        HostCall::AutoCompact(_) => ops::Operation::TryAutoCompact,
        _ => return None,
    })
}
impl ResourceCodec for ContextCodec {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        module == "context"
            && self
                .installed
                .iter()
                .any(|item| item.operation.name() == operation)
    }
    fn resource_effect(
        &self,
        call: &HostCall,
        method: &str,
    ) -> pilotdeck_protocol::admission::CallEffect {
        if matches!(method, history::READ | history::RECEIPT | ops::STATUS) {
            pilotdeck_protocol::admission::CallEffect::ReadOnly
        } else {
            call.effect()
        }
    }
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep> {
        self.reserve_rejection_failure = None;
        if let Some(unresolved) = self.proofs.unresolved().first() {
            return Err(self.unknown(&unresolved.transaction_id));
        }
        if self.pending.is_some() {
            return Err(invalid("another Context operation is pending"));
        }
        self.failure_settle_retries = 0;
        let operation = op(call).ok_or_else(|| invalid("not a Context operation"))?;
        if self.binding(operation).is_none() {
            return Err(PortError::NotAdvertised {
                module: "context",
                operation: call.method().1,
            });
        }
        let payload = pilotdeck_transport::context_operation_payload(call)?;
        let input = payload
            .get("input")
            .ok_or_else(|| invalid("missing original Context input"))?;
        let bytes = pilotdeck_context_runtime::boundary_codec::encode(input)
            .map_err(|_| invalid("Context input exceeds profile or is unsupported"))?;
        let ordinal = crate::port_helpers::next_sequence(
            &self.proofs.next,
            "Context operation sequence exhausted",
        )?;
        let tx = format!("{}:context:{}", self.context.call_ref, ordinal);
        self.pending = Some(Pending {
            operation,
            tx,
            input: history::OpaquePayload {
                profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
                bytes,
            },
            digest: None,
            child: None,
            output: None,
            result_digest: None,
            revision: 0,
            reconciling: false,
            phase: Phase::Read,
        });
        Self::request(history::READ, &json!({}))
    }
    fn resume(&mut self, call: &HostCall, response: Value) -> PortResult<ResourceStep> {
        let mut state = self
            .pending
            .take()
            .ok_or_else(|| invalid("Context response without pending request"))?;
        if op(call) != Some(state.operation) {
            self.pending = Some(state);
            return Err(invalid("Context response operation changed"));
        }
        let outcome = match state.phase {
            Phase::Read => {
                let view: history::HistoryView = serde_json::from_value(response)
                    .map_err(|_| invalid("invalid Context history view"))?;
                if let Some(reservation) = view.pending {
                    self.proofs.retain_unresolved(ContextUnresolved {
                        transaction_id: reservation.transaction_id.clone(),
                        request_digest: Some(reservation.request_digest),
                        operation: None,
                        child_ref: None,
                    });
                    return Err(self.unknown(&reservation.transaction_id));
                }
                state.revision = view.revision;
                state.phase = Phase::Reserve;
                Self::request(
                    history::PREPARE,
                    &history::PrepareRequest {
                        transaction_id: state.tx.clone(),
                        expected_revision: view.revision,
                        operation: state.operation.name().into(),
                        request: state.input.clone(),
                    },
                )
            }
            Phase::ReconcileRead => {
                let view: history::HistoryView =
                    serde_json::from_value(response).map_err(|_| self.retain_unknown(&state))?;
                let digest = if let Some(reservation) =
                    view.pending.filter(|r| r.transaction_id == state.tx)
                {
                    if reservation
                        .verify(&self.history_owner(), &Self::prepare_request(&state))
                        .is_err()
                    {
                        return Err(self.retain_unknown(&state));
                    }
                    Some(reservation.request_digest)
                } else {
                    let expected = history::request_digest(
                        &self.history_owner(),
                        &Self::prepare_request(&state),
                    )
                    .map_err(|_| self.retain_unknown(&state))?;
                    view.last_receipt
                        .filter(|r| r.transaction_id == state.tx && r.request_digest == expected)
                        .map(|r| r.request_digest)
                };
                let Some(digest) = digest else {
                    return Err(self.retain_unknown(&state));
                };
                state.digest = Some(digest.clone());
                state.phase = Phase::Status;
                Self::request(
                    ops::STATUS,
                    &history::ReceiptQuery {
                        transaction_id: state.tx.clone(),
                        request_digest: digest,
                    },
                )
            }
            Phase::Reserve => {
                let prepared: history::Prepared =
                    serde_json::from_value(response).map_err(|_| self.retain_unknown(&state))?;
                let r = prepared.reservation;
                if r.verify(&self.history_owner(), &Self::prepare_request(&state))
                    .is_err()
                {
                    return Err(self.retain_unknown(&state));
                }
                state.digest = Some(r.request_digest.clone());
                // A previous reservation never mints another dispatch grant.
                if !prepared.may_dispatch {
                    return Err(self.retain_unknown(&state));
                }
                state.phase = Phase::Dispatch;
                Self::request(
                    ops::DISPATCH,
                    &ops::DispatchRequest {
                        transaction_id: state.tx.clone(),
                        request_digest: r.request_digest,
                        operation: state.operation,
                    },
                )
            }
            Phase::Dispatch => {
                let dispatched: ops::Dispatched =
                    serde_json::from_value(response).map_err(|_| self.retain_unknown(&state))?;
                if dispatched.transaction_id != state.tx
                    || Some(&dispatched.request_digest) != state.digest.as_ref()
                    || dispatched.call_ref.is_empty()
                {
                    return Err(self.retain_unknown(&state));
                }
                state.child = Some(dispatched.call_ref);
                state.phase = Phase::Status;
                Self::status_step(&state, state.digest.clone().unwrap())
            }
            Phase::Status => {
                let status: ops::OperationStatus =
                    serde_json::from_value(response).map_err(|_| self.retain_unknown(&state))?;
                match status {
                    ops::OperationStatus::Dispatched { child } => {
                        if state.reconciling {
                            return Err(self.retain_unknown(&state));
                        }
                        let installed = self
                            .binding(state.operation)
                            .ok_or_else(|| self.retain_unknown(&state))?;
                        if Some(&child.id) != state.child.as_ref() {
                            return Err(self.retain_unknown(&state));
                        }
                        self.require_matching_child(&state, installed, &child)?;
                        self.require_matching_invocation(&state, &child)?;
                        if child.state != State::Completed {
                            if matches!(child.state, State::Failed | State::Cancelled) {
                                if state.operation != ops::Operation::RecoverFromModelError {
                                    return Err(self.retain_unknown(&state));
                                }
                                state.phase = Phase::SettleFailure;
                                return {
                                    let step = Self::request(
                                        ops::failure::SETTLE,
                                        &ops::failure::SettleRequest {
                                            transaction_id: state.tx.clone(),
                                            request_digest: state.digest.clone().unwrap(),
                                            call_ref: child.id,
                                        },
                                    );
                                    self.pending = Some(state);
                                    step
                                };
                            }
                            if child.state == State::Unknown {
                                return Err(self.retain_unknown(&state));
                            }
                            let step = Self::status_step(&state, state.digest.clone().unwrap())?;
                            self.pending = Some(state);
                            return Ok(ResourceStep::Delay {
                                duration: Duration::from_millis(20),
                                next: Box::new(step),
                            });
                        }
                        let raw_result = child.result.ok_or_else(|| self.retain_unknown(&state))?;
                        state.result_digest = Some(
                            ops::result_digest(&raw_result)
                                .map_err(|_| self.retain_unknown(&state))?,
                        );
                        let result: ops::OperationResult = serde_json::from_value(raw_result)
                            .map_err(|_| self.retain_unknown(&state))?;
                        result
                            .validate_for(state.operation)
                            .map_err(|_| self.retain_unknown(&state))?;
                        if result.output.profile
                            != pilotdeck_context_runtime::boundary_codec::PROFILE
                        {
                            return Err(self.retain_unknown(&state));
                        }
                        state.output = Some(
                            pilotdeck_context_runtime::boundary_codec::decode(&result.output.bytes)
                                .map_err(|_| self.retain_unknown(&state))?,
                        );
                        state.phase = Phase::Commit;
                        Self::request(
                            ops::COMMIT,
                            &ops::CommitRequest {
                                transaction_id: state.tx.clone(),
                                request_digest: state.digest.clone().unwrap(),
                                call_ref: child.id,
                            },
                        )
                    }
                    ops::OperationStatus::Committed {
                        result: committed,
                        child,
                    } => {
                        let installed = self
                            .binding(state.operation)
                            .ok_or_else(|| self.retain_unknown(&state))?;
                        if child.state != State::Completed
                            || state
                                .child
                                .as_ref()
                                .is_some_and(|expected| expected != &child.id)
                            || child.invocation.parent_execution_ref.as_deref()
                                != Some(self.context.call_ref.as_str())
                        {
                            return Err(self.retain_unknown(&state));
                        }
                        self.require_matching_child(&state, installed, &child)?;
                        self.require_matching_invocation(&state, &child)?;
                        if !self.committed_matches(
                            committed.as_ref(),
                            &state,
                            installed,
                            Some(child.id.as_str()),
                            child
                                .result
                                .as_ref()
                                .and_then(|raw| ops::result_digest(raw).ok())
                                .as_deref(),
                        ) {
                            return Err(self.retain_unknown(&state));
                        }
                        let operation_result: ops::OperationResult = serde_json::from_value(
                            child.result.ok_or_else(|| self.retain_unknown(&state))?,
                        )
                        .map_err(|_| self.retain_unknown(&state))?;
                        operation_result
                            .validate_for(state.operation)
                            .map_err(|_| self.retain_unknown(&state))?;
                        if operation_result.output.profile
                            != pilotdeck_context_runtime::boundary_codec::PROFILE
                        {
                            return Err(self.retain_unknown(&state));
                        }
                        let value = pilotdeck_context_runtime::boundary_codec::decode(
                            &operation_result.output.bytes,
                        )
                        .map_err(|_| self.retain_unknown(&state))?;
                        let reply = pilotdeck_transport::context_operation_reply(call, value)
                            .map_err(|_| self.retain_unknown(&state))?;
                        self.proofs.forget(&state.tx);
                        self.proofs
                            .committed
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(*committed);
                        return Ok(ResourceStep::Reply(Box::new(reply)));
                    }
                    ops::OperationStatus::Failed { receipt, child } => {
                        let installed = self
                            .binding(state.operation)
                            .ok_or_else(|| self.retain_unknown(&state))?;
                        if state.operation != ops::Operation::RecoverFromModelError
                            || child.security != self.context.security
                            || child.binding.id != installed.binding
                            || child.binding.revision != installed.revision
                            || state.child.as_ref().is_some_and(|id| id != &child.id)
                        {
                            return Err(self.retain_unknown(&state));
                        }
                        self.require_matching_invocation(&state, &child)?;
                        if Some(&receipt.request_digest) != state.digest.as_ref()
                            || receipt.transaction_id != state.tx
                            || receipt.verify_child(&child).is_err()
                        {
                            return Err(self.retain_unknown(&state));
                        }
                        let (code, message) = match child.error.as_ref() {
                            Some(failure) if failure.code == "CONTEXT_PRODUCT_ERROR" => {
                                let units = failure
                                    .params
                                    .get("message_utf16")
                                    .and_then(|v| v.as_array())
                                    .and_then(|v| {
                                        v.iter()
                                            .map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                                            .collect::<Option<Vec<_>>>()
                                    })
                                    .ok_or_else(|| self.retain_unknown(&state))?;
                                (
                                    failure.code.as_str().into(),
                                    pilotdeck_protocol::JsString::from_utf16(units),
                                )
                            }
                            Some(failure) => (failure.code.as_str().into(), failure.wire().into()),
                            None if child.state == State::Cancelled => {
                                ("CANCELLED".into(), "Context recovery cancelled".into())
                            }
                            None => return Err(self.retain_unknown(&state)),
                        };
                        self.proofs.forget(&state.tx);
                        return Err(PortError::HostFailure {
                            code,
                            boundary: pilotdeck_protocol::model::BoundarySnapshot::capture(
                                &JsValue::String(message.clone()),
                            ),
                            message,
                        });
                    }
                    ops::OperationStatus::Unknown => return Err(self.retain_unknown(&state)),
                }
            }
            Phase::SettleFailure => {
                // The acknowledgement alone is insufficient: re-read the durable receipt
                // with its original child, then verify the complete proof.
                let receipt: ops::failure::Receipt =
                    serde_json::from_value(response).map_err(|_| self.retain_unknown(&state))?;
                if receipt.transaction_id != state.tx
                    || Some(&receipt.request_digest) != state.digest.as_ref()
                    || Some(&receipt.proof.call_ref) != state.child.as_ref()
                {
                    return Err(self.retain_unknown(&state));
                }
                state.phase = Phase::Status;
                Self::status_step(&state, state.digest.clone().unwrap())
            }
            Phase::Commit => {
                let committed: ops::Committed =
                    serde_json::from_value(response).map_err(|_| self.retain_unknown(&state))?;
                let installed = self
                    .binding(state.operation)
                    .ok_or_else(|| self.retain_unknown(&state))?;
                if !self.committed_matches(
                    &committed,
                    &state,
                    installed,
                    state.child.as_deref(),
                    state.result_digest.as_deref(),
                ) {
                    return Err(self.retain_unknown(&state));
                }
                let output = state
                    .output
                    .take()
                    .ok_or_else(|| self.retain_unknown(&state))?;
                let reply = pilotdeck_transport::context_operation_reply(call, output)
                    .map_err(|_| self.retain_unknown(&state))?;
                self.proofs
                    .committed
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(committed);
                return Ok(ResourceStep::Reply(Box::new(reply)));
            }
        }?;
        self.pending = Some(state);
        Ok(outcome)
    }
    fn cleanup_calls(&self) -> Vec<HostCall> {
        Vec::new()
    }
    fn retry_rejected(
        &mut self,
        method: &str,
        failure: &agentcpm_scheduler_protocol::Failure,
    ) -> Option<ResourceStep> {
        // Kernel's explicit UNCONFIRMED reply is produced before the failure
        // journal append. It does not assert release and grants no new work.
        // A lost reply or any other error may follow CAS and is never retried.
        if method != ops::failure::SETTLE
            || failure.code != "CONTEXT_FAILURE_UNCONFIRMED"
            || self.failure_settle_retries >= 50
        {
            return None;
        }
        let state = self.pending.as_ref()?;
        if !matches!(state.phase, Phase::SettleFailure) || state.reconciling {
            return None;
        }
        let request = ops::failure::SettleRequest {
            transaction_id: state.tx.clone(),
            request_digest: state.digest.clone()?,
            call_ref: state.child.clone()?,
        };
        let next = Self::request(ops::failure::SETTLE, &request).ok()?;
        self.failure_settle_retries += 1;
        Some(ResourceStep::Delay {
            duration: Duration::from_millis(20),
            next: Box::new(next),
        })
    }
    fn resource_failed(&mut self, _: &HostCall, error: &PortError) {
        let reserved = self.reserve_rejection_failure.take();
        if let Some(state) = self.pending.take() {
            // A pre-admission rejection whose reconciliation observation was
            // never dispatched still retires the provably never-admitted
            // transaction: the kernel's admission gate is authority, so the
            // effect cannot be unknown. Only the bare CALLER_ENDED /
            // STEP_AUTHORITY_REVOKED forms retire; the wrapped
            // OUTCOME_UNKNOWN(cause) form keeps the unknown retention.
            if matches!(
                reserved.as_ref().map(|failure| failure.code.as_str()),
                Some("CALLER_ENDED" | "STEP_AUTHORITY_REVOKED")
            ) && matches!(state.phase, Phase::ReconcileRead)
                && state.digest.is_none()
                && state.child.is_none()
                && matches!(error, PortError::HostFailure { .. })
            {
                self.retire_pre_admission(&state);
            } else if !matches!(state.phase, Phase::Read) {
                let _ = self.retain_unknown(&state);
            }
        }
        // Snapshots retain ephemeral input/output and phase for verification;
        // the Kernel owns durable settlement and no product journal or CAS exists.
    }
    fn reconcile_failure(
        &mut self,
        call: &HostCall,
        method: &str,
        failure: &agentcpm_scheduler_protocol::Failure,
        _: &PortError,
        prior_effect: bool,
    ) -> Option<(ResourceRequest, PortError)> {
        let state = self
            .proofs
            .snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last()
            .cloned()?;
        if op(call) != Some(state.operation) {
            return None;
        }
        if state.digest.is_none() {
            if prior_effect
                || method != history::PREPARE
                || !is_preadmission_cancel_rejection(failure)
                || state.child.is_some()
            {
                return None;
            }
            let unknown = self.unknown(&state.tx);
            return match self.begin_reconciliation(&state.tx).ok()? {
                ResourceStep::Request(request) if request.method == history::READ => {
                    self.reserve_rejection_failure = Some(failure.clone());
                    Some((request, unknown))
                }
                _ => {
                    self.reserve_rejection_failure = None;
                    None
                }
            };
        }
        let unknown = self.unknown(&state.tx);
        match self.begin_reconciliation(&state.tx).ok()? {
            ResourceStep::Request(request) if request.method == ops::STATUS => {
                Some((request, unknown))
            }
            _ => None,
        }
    }
    fn reconciliation_reply(
        &mut self,
        call: &HostCall,
        result: &std::result::Result<Value, agentcpm_scheduler_protocol::Failure>,
    ) -> Option<PortResult<HostReply>> {
        if let Some(failure) = self.reserve_rejection_failure.take() {
            let state = self.pending.take()?;
            if !matches!(state.phase, Phase::ReconcileRead)
                || state.digest.is_some()
                || state.child.is_some()
            {
                let _ = self.retain_unknown(&state);
                return None;
            }
            let Ok(response) = result else {
                // The read-back itself was refused. When the original failure
                // already proved the prepare was never admitted (a bare
                // CALLER_ENDED / STEP_AUTHORITY_REVOKED pre-admission
                // rejection), the kernel's admission gate is authoritative:
                // retire the never-started transaction and keep the failure
                // known, so an aborted turn is not turned into an unknown
                // outcome. The wrapped OUTCOME_UNKNOWN(cause) variant stays
                // unknown.
                if matches!(
                    failure.code.as_str(),
                    "CALLER_ENDED" | "STEP_AUTHORITY_REVOKED"
                ) {
                    return Some(self.retire_pre_admission_rejection(&state, &failure));
                }
                let _ = self.retain_unknown(&state);
                return None;
            };
            let Ok(view) = serde_json::from_value::<history::HistoryView>(response.clone()) else {
                let _ = self.retain_unknown(&state);
                return None;
            };
            let expected_digest =
                history::request_digest(&self.history_owner(), &Self::prepare_request(&state)).ok();
            let clean = view.revision == state.revision
                && view.pending.is_none()
                && view.last_receipt.as_ref().is_none_or(|r| {
                    r.transaction_id != state.tx
                        && expected_digest
                            .as_deref()
                            .is_none_or(|d| r.request_digest != d)
                });
            if !clean {
                let _ = self.retain_unknown(&state);
                return None;
            }
            return Some(self.retire_pre_admission_rejection(&state, &failure));
        }
        match result {
            Ok(response) => match self.resume(call, response.clone()) {
                Ok(ResourceStep::Reply(reply)) => Some(Ok(*reply)),
                Err(error @ PortError::HostFailure { .. }) => Some(Err(error)),
                _ => {
                    if let Some(state) = self.pending.take() {
                        let _ = self.retain_unknown(&state);
                    }
                    None
                }
            },
            Err(_) => {
                if let Some(state) = self.pending.take() {
                    let _ = self.retain_unknown(&state);
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_agent::ports::RecoverModelErrorBoundary;
    use pilotdeck_protocol::model::BoundarySnapshot;
    fn recovery() -> HostCall {
        HostCall::RecoverModelErrorBoundary(RecoverModelErrorBoundary {
            session_id: "session".into(),
            turn_id: "turn".into(),
            error: BoundarySnapshot::capture(&JsValue::Null),
            messages: vec![],
            has_attempted_compact: false,
        })
    }
    #[test]
    fn owner_rejects_parallel_codec_and_other_generation() {
        let scope = crate::tests::context();
        let proofs = Arc::new(ContextProofs::default());
        let first = ContextCodec::new(scope.clone(), vec![], proofs.clone()).unwrap();
        assert!(ContextCodec::new(scope.clone(), vec![], proofs.clone()).is_err());
        drop(first);
        let mut wrong_attempt = scope.clone();
        wrong_attempt.attempt += 1;
        assert!(ContextCodec::new(wrong_attempt, vec![], proofs.clone()).is_err());
        let mut wrong = scope.clone();
        wrong.interaction.generation += 1;
        assert!(ContextCodec::new(wrong, vec![], proofs.clone()).is_err());
        assert!(ContextCodec::new(scope, vec![], proofs).is_ok());
    }
    #[test]
    fn history_read_is_read_only_and_prepare_is_effectful() {
        let codec = ContextCodec::new(
            crate::tests::context(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            Arc::new(ContextProofs::default()),
        )
        .unwrap();
        assert_eq!(
            codec.resource_effect(&recovery(), history::READ),
            pilotdeck_protocol::admission::CallEffect::ReadOnly
        );
        assert_ne!(
            codec.resource_effect(&recovery(), history::PREPARE),
            pilotdeck_protocol::admission::CallEffect::ReadOnly
        );
    }
    #[test]
    fn unsupported_operation_is_rejected_before_history() {
        let mut codec = ContextCodec::new(
            crate::tests::context(),
            vec![],
            Arc::new(ContextProofs::default()),
        )
        .unwrap();
        assert!(matches!(
            codec.begin(&recovery()),
            Err(PortError::NotAdvertised { .. })
        ));
    }
    #[test]
    fn replayed_reservation_does_not_dispatch() {
        let context = crate::tests::context();
        let mut codec = ContextCodec::new(
            context.clone(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            Arc::new(ContextProofs::default()),
        )
        .unwrap();
        let call = recovery();
        assert!(matches!(
            codec.begin(&call),
            Ok(ResourceStep::Request(ResourceRequest {
                method: history::READ,
                ..
            }))
        ));
        let view = history::HistoryView {
            revision: 0,
            history: None,
            last_receipt: None,
            pending: None,
        };
        let step = codec
            .resume(&call, serde_json::to_value(view).unwrap())
            .unwrap();
        let ResourceStep::Request(request) = step else {
            panic!("prepare request")
        };
        assert_eq!(request.method, history::PREPARE);
        let prepare: history::PrepareRequest = serde_json::from_value(request.payload).unwrap();
        let owner = history::Owner {
            session: context.session_ref.clone(),
            binding: context.binding.id.clone(),
            context: context.invocation.context_ref.clone(),
            call: context.call_ref.clone(),
            origin: context.interaction.clone(),
        };
        let digest = history::request_digest(&owner, &prepare).unwrap();
        let prepared = history::Prepared {
            may_dispatch: false,
            reservation: history::Reservation {
                transaction_id: prepare.transaction_id,
                request_digest: digest,
                expected_revision: 0,
                prepare_cursor: 1,
                owner,
            },
        };
        assert!(matches!(
            codec.resume(&call, serde_json::to_value(prepared).unwrap()),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert!(matches!(
            codec.begin(&call),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert_eq!(codec.proofs.unresolved().len(), 1);
    }
    #[test]
    fn tampered_reservation_never_dispatches() {
        let context = crate::tests::context();
        let mut codec = ContextCodec::new(
            context.clone(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            Arc::new(ContextProofs::default()),
        )
        .unwrap();
        let call = recovery();
        assert!(matches!(
            codec.begin(&call),
            Ok(ResourceStep::Request(ResourceRequest {
                method: history::READ,
                ..
            }))
        ));
        let view = history::HistoryView {
            revision: 0,
            history: None,
            last_receipt: None,
            pending: None,
        };
        let step = codec
            .resume(&call, serde_json::to_value(view).unwrap())
            .unwrap();
        let ResourceStep::Request(request) = step else {
            panic!("prepare request")
        };
        assert_eq!(request.method, history::PREPARE);
        let prepare: history::PrepareRequest = serde_json::from_value(request.payload).unwrap();
        let owner = history::Owner {
            session: context.session_ref.clone(),
            binding: context.binding.id.clone(),
            context: context.invocation.context_ref.clone(),
            call: context.call_ref.clone(),
            origin: context.interaction.clone(),
        };
        let digest = "tampered".to_string();
        let prepared = history::Prepared {
            may_dispatch: true,
            reservation: history::Reservation {
                transaction_id: prepare.transaction_id,
                request_digest: digest,
                expected_revision: 0,
                prepare_cursor: 1,
                owner,
            },
        };
        assert!(matches!(
            codec.resume(&call, serde_json::to_value(prepared).unwrap()),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert!(matches!(
            codec.begin(&call),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert_eq!(codec.proofs.unresolved().len(), 1);
    }
    #[test]
    fn failed_resource_can_only_start_read_only_status_observation() {
        let context = crate::tests::context();
        let proofs = Arc::new(ContextProofs::default());
        let mut codec = ContextCodec::new(
            context,
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            proofs.clone(),
        )
        .unwrap();
        let input = history::OpaquePayload {
            profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
            bytes: vec![1],
        };
        codec.pending = Some(Pending {
            operation: ops::Operation::RecoverFromModelError,
            tx: "tx".into(),
            input,
            digest: Some("digest".into()),
            child: Some("child".into()),
            output: None,
            result_digest: None,
            revision: 0,
            reconciling: false,
            phase: Phase::Commit,
        });
        let error = codec.unknown("tx");
        codec.resource_failed(&recovery(), &error);
        assert_eq!(proofs.unresolved().len(), 1);
        let ResourceStep::Request(request) = codec.begin_reconciliation("tx").unwrap() else {
            panic!("status request")
        };
        assert_eq!(request.method, ops::STATUS);
        assert!(matches!(
            codec.resume(
                &recovery(),
                serde_json::to_value(ops::OperationStatus::Unknown).unwrap()
            ),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert!(matches!(
            codec.begin(&recovery()),
            Err(PortError::AcceptedUnknown { .. })
        ));
    }
    #[test]
    fn confirmed_failure_reconciles_exact_utf16_and_rejects_forgery() {
        let context = crate::tests::context();
        let proofs = Arc::new(ContextProofs::default());
        let mut codec = ContextCodec::new(
            context.clone(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            proofs.clone(),
        )
        .unwrap();
        let input = history::OpaquePayload {
            profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
            bytes: pilotdeck_context_runtime::boundary_codec::encode(&JsValue::Object(
                Default::default(),
            ))
            .unwrap(),
        };
        let owner = history::Owner {
            session: context.session_ref.clone(),
            binding: context.binding.id.clone(),
            context: context.invocation.context_ref.clone(),
            call: context.call_ref.clone(),
            origin: context.interaction.clone(),
        };
        let prepare = history::PrepareRequest {
            transaction_id: "tx".into(),
            expected_revision: 0,
            operation: ops::Operation::RecoverFromModelError.name().into(),
            request: input.clone(),
        };
        let digest = history::request_digest(&owner, &prepare).unwrap();
        let reservation = history::Reservation {
            transaction_id: "tx".into(),
            request_digest: digest.clone(),
            expected_revision: 0,
            prepare_cursor: 1,
            owner,
        };
        let request = ops::InvocationRequest {
            operation: ops::Operation::RecoverFromModelError,
            input: input.clone(),
            reservation: reservation.clone(),
        };
        let mut binding = context.binding.clone();
        binding.id = "recovery".into();
        binding.revision = "v1".into();
        let mut invocation = context.invocation.clone();
        invocation.parent_execution_ref = Some(context.call_ref.clone());
        invocation.payload = serde_json::to_value(request).unwrap();
        let child = agentcpm_scheduler_protocol::CallView {
            id: "child".into(),
            session_ref: context.session_ref.clone(),
            source_binding: Some(context.binding.id.clone()),
            parent_session_ref: Some(context.session_ref.clone()),
            cause_execution_ref: Some(context.call_ref.clone()),
            binding,
            invocation,
            state: State::Failed,
            result: None,
            presentation: None,
            settlement_data: None,
            settlement_error: None,
            error: Some(
                agentcpm_scheduler_protocol::Failure::external("CONTEXT_PRODUCT_ERROR", "failure")
                    .with("message_utf16", json!([65, 55296, 66])),
            ),
            waiting: None,
            cursor: 0,
            origin: context.interaction.clone(),
            interaction: context.interaction.clone(),
            interactions: vec![],
            cancel_requested: false,
            stepped: false,
            security: context.security.clone(),
        };
        let proof = ops::failure::Proof {
            call_ref: child.id.clone(),
            binding_ref: child.binding.id.clone(),
            binding_revision: child.binding.revision.clone(),
            operation: ops::Operation::RecoverFromModelError,
            origin: child.origin.clone(),
            disposition: ops::failure::Disposition::Failed,
            outcome_digest: ops::failure::outcome_digest(&child).unwrap(),
            release_scope: ops::failure::ReleaseScope::KernelOwnedNativeResources,
            trusted_boundary: ops::failure::TrustedBoundary {
                contract_ref: ops::failure::TRUST_CONTRACT.into(),
                artifact_sha256: "a".repeat(64),
                export_name: "recover_from_model_error".into(),
                args_digest: "b".repeat(64),
                configuration_digest: "c".repeat(64),
            },
            managed_inventory_digest: "d".repeat(64),
        };
        let receipt = ops::failure::Receipt {
            transaction_id: "tx".into(),
            request_digest: digest.clone(),
            settlement_digest: ops::failure::settlement_digest(&reservation, &proof).unwrap(),
            revision: 1,
            event_cursor: 2,
            proof,
        };
        let state = Pending {
            operation: ops::Operation::RecoverFromModelError,
            tx: "tx".into(),
            input,
            digest: Some(digest),
            child: Some("child".into()),
            output: None,
            result_digest: None,
            revision: 0,
            reconciling: false,
            phase: Phase::Status,
        };
        codec.pending = Some(state.clone());
        let ResourceStep::Request(settle) = codec
            .resume(
                &recovery(),
                serde_json::to_value(ops::OperationStatus::Dispatched {
                    child: child.clone(),
                })
                .unwrap(),
            )
            .unwrap()
        else {
            panic!("settle")
        };
        assert_eq!(settle.method, ops::failure::SETTLE);
        let unconfirmed = agentcpm_scheduler_protocol::Failure::new(
            "CONTEXT_FAILURE_UNCONFIRMED",
            "release.pending",
        );
        let ResourceStep::Delay { next, .. } = codec
            .retry_rejected(ops::failure::SETTLE, &unconfirmed)
            .unwrap()
        else {
            panic!("bounded retry")
        };
        let ResourceStep::Request(retry) = *next else {
            panic!("settlement request")
        };
        assert_eq!(retry.method, ops::failure::SETTLE);
        assert_eq!(retry.payload, settle.payload);
        for _ in 1..50 {
            assert!(
                codec
                    .retry_rejected(ops::failure::SETTLE, &unconfirmed)
                    .is_some()
            );
        }
        assert!(
            codec
                .retry_rejected(ops::failure::SETTLE, &unconfirmed)
                .is_none()
        );
        assert!(
            codec
                .retry_rejected(
                    ops::failure::SETTLE,
                    &agentcpm_scheduler_protocol::Failure::new("HISTORY_UNKNOWN", "ambiguous")
                )
                .is_none()
        );
        let ResourceStep::Request(status) = codec
            .resume(&recovery(), serde_json::to_value(&receipt).unwrap())
            .unwrap()
        else {
            panic!("status")
        };
        assert_eq!(status.method, ops::STATUS);
        let mut forged = receipt.clone();
        forged.proof.trusted_boundary.configuration_digest = "e".repeat(64);
        let error = codec
            .resume(
                &recovery(),
                serde_json::to_value(ops::OperationStatus::Failed {
                    receipt: Box::new(forged),
                    child: child.clone(),
                })
                .unwrap(),
            )
            .err()
            .unwrap();
        assert!(matches!(error, PortError::AcceptedUnknown { .. }));
        assert_eq!(proofs.unresolved().len(), 1);
        let ResourceStep::Request(status) = codec.begin_reconciliation("tx").unwrap() else {
            panic!("reconcile")
        };
        assert_eq!(status.method, ops::STATUS);
        let error = codec
            .resume(
                &recovery(),
                serde_json::to_value(ops::OperationStatus::Failed {
                    receipt: Box::new(receipt),
                    child,
                })
                .unwrap(),
            )
            .err()
            .unwrap();
        let PortError::HostFailure { message, .. } = error else {
            panic!("product failure")
        };
        assert_eq!(message.units(), &[65, 55296, 66]);
        assert!(proofs.unresolved().is_empty());
    }

    #[test]
    fn lost_commit_ack_recovers_original_output_by_status_without_write() {
        let context = crate::tests::context();
        let proofs = Arc::new(ContextProofs::default());
        let mut codec = ContextCodec::new(
            context.clone(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            proofs.clone(),
        )
        .unwrap();
        let input = history::OpaquePayload {
            profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
            bytes: pilotdeck_context_runtime::boundary_codec::encode(&JsValue::Object(
                Default::default(),
            ))
            .unwrap(),
        };
        let owner = history::Owner {
            session: context.session_ref.clone(),
            binding: context.binding.id.clone(),
            context: context.invocation.context_ref.clone(),
            call: context.call_ref.clone(),
            origin: context.interaction.clone(),
        };
        let digest = history::request_digest(
            &owner,
            &history::PrepareRequest {
                transaction_id: "tx".into(),
                expected_revision: 0,
                operation: ops::Operation::RecoverFromModelError.name().into(),
                request: input.clone(),
            },
        )
        .unwrap();
        let reservation = history::Reservation {
            transaction_id: "tx".into(),
            request_digest: digest.clone(),
            expected_revision: 0,
            prepare_cursor: 1,
            owner,
        };
        let request = ops::InvocationRequest {
            operation: ops::Operation::RecoverFromModelError,
            input: input.clone(),
            reservation,
        };
        let value =
            pilotdeck_protocol::parse_json(r#"{"type":"give_up","reason":"test"}"#).unwrap();
        let output = ops::OperationResult {
            operation: ops::Operation::RecoverFromModelError,
            output: history::OpaquePayload {
                profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
                bytes: pilotdeck_context_runtime::boundary_codec::encode(&value).unwrap(),
            },
            history: ops::HistoryMutation {
                replacement: None,
                consume_input: false,
                facts: vec![],
                compaction_boundary: None,
            },
            preparation_notice: None,
        };
        let mut child_binding = context.binding.clone();
        child_binding.id = "recovery".into();
        child_binding.revision = "v1".into();
        let mut invocation = context.invocation.clone();
        invocation.parent_execution_ref = Some(context.call_ref.clone());
        invocation.payload = serde_json::to_value(request).unwrap();
        let child = agentcpm_scheduler_protocol::CallView {
            id: "child".into(),
            session_ref: context.session_ref.clone(),
            source_binding: Some(context.binding.id.clone()),
            parent_session_ref: Some(context.session_ref.clone()),
            cause_execution_ref: Some(context.call_ref.clone()),
            binding: child_binding,
            invocation,
            state: State::Completed,
            result: Some(serde_json::to_value(output).unwrap()),
            presentation: None,
            settlement_data: None,
            settlement_error: None,
            error: None,
            waiting: None,
            cursor: 0,
            origin: context.interaction.clone(),
            interaction: context.interaction.clone(),
            interactions: vec![],
            cancel_requested: false,
            stepped: false,
            security: context.security.clone(),
        };
        let result_digest = ops::result_digest(child.result.as_ref().unwrap()).unwrap();
        let committed = ops::Committed {
            receipt: history::Receipt {
                transaction_id: "tx".into(),
                request_digest: digest.clone(),
                commit_digest: "commit".into(),
                revision: 1,
                event_cursor: 2,
                consumed_inputs: vec![],
                replacement_digest: None,
                compaction_digest: None,
            },
            proof: ops::CompletionProof {
                call_ref: "child".into(),
                binding_ref: "recovery".into(),
                binding_revision: "v1".into(),
                operation: ops::Operation::RecoverFromModelError,
                result_digest: result_digest.clone(),
                origin: context.interaction.clone(),
            },
        };
        codec.pending = Some(Pending {
            operation: ops::Operation::RecoverFromModelError,
            tx: "tx".into(),
            input,
            digest: Some(digest),
            child: Some("child".into()),
            output: Some(value.clone()),
            result_digest: Some(result_digest),
            revision: 0,
            reconciling: false,
            phase: Phase::Commit,
        });
        let failure = codec.unknown("tx");
        codec.resource_failed(&recovery(), &failure);
        let ResourceStep::Request(observation) = codec.begin_reconciliation("tx").unwrap() else {
            panic!("status")
        };
        assert_eq!(observation.method, ops::STATUS);
        let step = codec
            .resume(
                &recovery(),
                serde_json::to_value(ops::OperationStatus::Committed {
                    result: Box::new(committed),
                    child,
                })
                .unwrap(),
            )
            .unwrap();
        let ResourceStep::Reply(reply) = step else {
            panic!("expected recovered reply")
        };
        let boundary = match *reply {
            HostReply::Recovery(result) => result.boundary,
            HostReply::RecoveryBoundaryOnly(boundary) => boundary,
            _ => panic!("wrong reply"),
        };
        assert_eq!(boundary.value(), &value);
        assert!(proofs.unresolved().is_empty());
        assert_eq!(proofs.observed().len(), 1);
    }
    #[test]
    fn changed_result_digest_never_recovers_success() {
        let context = crate::tests::context();
        let proofs = Arc::new(ContextProofs::default());
        let mut codec = ContextCodec::new(
            context.clone(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            proofs.clone(),
        )
        .unwrap();
        let input = history::OpaquePayload {
            profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
            bytes: pilotdeck_context_runtime::boundary_codec::encode(&JsValue::Object(
                Default::default(),
            ))
            .unwrap(),
        };
        let owner = history::Owner {
            session: context.session_ref.clone(),
            binding: context.binding.id.clone(),
            context: context.invocation.context_ref.clone(),
            call: context.call_ref.clone(),
            origin: context.interaction.clone(),
        };
        let digest = history::request_digest(
            &owner,
            &history::PrepareRequest {
                transaction_id: "tx".into(),
                expected_revision: 0,
                operation: ops::Operation::RecoverFromModelError.name().into(),
                request: input.clone(),
            },
        )
        .unwrap();
        let reservation = history::Reservation {
            transaction_id: "tx".into(),
            request_digest: digest.clone(),
            expected_revision: 0,
            prepare_cursor: 1,
            owner,
        };
        let request = ops::InvocationRequest {
            operation: ops::Operation::RecoverFromModelError,
            input: input.clone(),
            reservation,
        };
        let value =
            pilotdeck_protocol::parse_json(r#"{"type":"give_up","reason":"test"}"#).unwrap();
        let output = ops::OperationResult {
            operation: ops::Operation::RecoverFromModelError,
            output: history::OpaquePayload {
                profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
                bytes: pilotdeck_context_runtime::boundary_codec::encode(&value).unwrap(),
            },
            history: ops::HistoryMutation {
                replacement: None,
                consume_input: false,
                facts: vec![],
                compaction_boundary: None,
            },
            preparation_notice: None,
        };
        let mut child_binding = context.binding.clone();
        child_binding.id = "recovery".into();
        child_binding.revision = "v1".into();
        let mut invocation = context.invocation.clone();
        invocation.parent_execution_ref = Some(context.call_ref.clone());
        invocation.payload = serde_json::to_value(request).unwrap();
        let child = agentcpm_scheduler_protocol::CallView {
            id: "child".into(),
            session_ref: context.session_ref.clone(),
            source_binding: Some(context.binding.id.clone()),
            parent_session_ref: Some(context.session_ref.clone()),
            cause_execution_ref: Some(context.call_ref.clone()),
            binding: child_binding,
            invocation,
            state: State::Completed,
            result: Some(serde_json::to_value(output).unwrap()),
            presentation: None,
            settlement_data: None,
            settlement_error: None,
            error: None,
            waiting: None,
            cursor: 0,
            origin: context.interaction.clone(),
            interaction: context.interaction.clone(),
            interactions: vec![],
            cancel_requested: false,
            stepped: false,
            security: context.security.clone(),
        };
        let result_digest = ops::result_digest(child.result.as_ref().unwrap()).unwrap();
        let committed = ops::Committed {
            receipt: history::Receipt {
                transaction_id: "tx".into(),
                request_digest: digest.clone(),
                commit_digest: "commit".into(),
                revision: 1,
                event_cursor: 2,
                consumed_inputs: vec![],
                replacement_digest: None,
                compaction_digest: None,
            },
            proof: ops::CompletionProof {
                call_ref: "child".into(),
                binding_ref: "recovery".into(),
                binding_revision: "v1".into(),
                operation: ops::Operation::RecoverFromModelError,
                result_digest: "tampered".into(),
                origin: context.interaction.clone(),
            },
        };
        codec.pending = Some(Pending {
            operation: ops::Operation::RecoverFromModelError,
            tx: "tx".into(),
            input,
            digest: Some(digest),
            child: Some("child".into()),
            output: Some(value.clone()),
            result_digest: Some(result_digest),
            revision: 0,
            reconciling: false,
            phase: Phase::Commit,
        });
        let failure = codec.unknown("tx");
        codec.resource_failed(&recovery(), &failure);
        let ResourceStep::Request(observation) = codec.begin_reconciliation("tx").unwrap() else {
            panic!("status")
        };
        assert_eq!(observation.method, ops::STATUS);
        let result = codec.resume(
            &recovery(),
            serde_json::to_value(ops::OperationStatus::Committed {
                result: Box::new(committed),
                child,
            })
            .unwrap(),
        );
        assert!(matches!(result, Err(PortError::AcceptedUnknown { .. })));
        assert_eq!(proofs.unresolved().len(), 1);
        assert!(proofs.observed().is_empty());
    }

    #[test]
    fn wrong_parent_session_never_recovers_success() {
        let context = crate::tests::context();
        let proofs = Arc::new(ContextProofs::default());
        let mut codec = ContextCodec::new(
            context.clone(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            proofs.clone(),
        )
        .unwrap();
        let input = history::OpaquePayload {
            profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
            bytes: pilotdeck_context_runtime::boundary_codec::encode(&JsValue::Object(
                Default::default(),
            ))
            .unwrap(),
        };
        let owner = history::Owner {
            session: context.session_ref.clone(),
            binding: context.binding.id.clone(),
            context: context.invocation.context_ref.clone(),
            call: context.call_ref.clone(),
            origin: context.interaction.clone(),
        };
        let digest = history::request_digest(
            &owner,
            &history::PrepareRequest {
                transaction_id: "tx".into(),
                expected_revision: 0,
                operation: ops::Operation::RecoverFromModelError.name().into(),
                request: input.clone(),
            },
        )
        .unwrap();
        let reservation = history::Reservation {
            transaction_id: "tx".into(),
            request_digest: digest.clone(),
            expected_revision: 0,
            prepare_cursor: 1,
            owner,
        };
        let request = ops::InvocationRequest {
            operation: ops::Operation::RecoverFromModelError,
            input: input.clone(),
            reservation,
        };
        let value =
            pilotdeck_protocol::parse_json(r#"{"type":"give_up","reason":"test"}"#).unwrap();
        let output = ops::OperationResult {
            operation: ops::Operation::RecoverFromModelError,
            output: history::OpaquePayload {
                profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
                bytes: pilotdeck_context_runtime::boundary_codec::encode(&value).unwrap(),
            },
            history: ops::HistoryMutation {
                replacement: None,
                consume_input: false,
                facts: vec![],
                compaction_boundary: None,
            },
            preparation_notice: None,
        };
        let mut child_binding = context.binding.clone();
        child_binding.id = "recovery".into();
        child_binding.revision = "v1".into();
        let mut invocation = context.invocation.clone();
        invocation.parent_execution_ref = Some(context.call_ref.clone());
        invocation.payload = serde_json::to_value(request).unwrap();
        let mut child = agentcpm_scheduler_protocol::CallView {
            id: "child".into(),
            session_ref: context.session_ref.clone(),
            source_binding: Some(context.binding.id.clone()),
            parent_session_ref: Some(context.session_ref.clone()),
            cause_execution_ref: Some(context.call_ref.clone()),
            binding: child_binding,
            invocation,
            state: State::Completed,
            result: Some(serde_json::to_value(output).unwrap()),
            presentation: None,
            settlement_data: None,
            settlement_error: None,
            error: None,
            waiting: None,
            cursor: 0,
            origin: context.interaction.clone(),
            interaction: context.interaction.clone(),
            interactions: vec![],
            cancel_requested: false,
            stepped: false,
            security: context.security.clone(),
        };
        let result_digest = ops::result_digest(child.result.as_ref().unwrap()).unwrap();
        let committed = ops::Committed {
            receipt: history::Receipt {
                transaction_id: "tx".into(),
                request_digest: digest.clone(),
                commit_digest: "commit".into(),
                revision: 1,
                event_cursor: 2,
                consumed_inputs: vec![],
                replacement_digest: None,
                compaction_digest: None,
            },
            proof: ops::CompletionProof {
                call_ref: "child".into(),
                binding_ref: "recovery".into(),
                binding_revision: "v1".into(),
                operation: ops::Operation::RecoverFromModelError,
                result_digest: result_digest.clone(),
                origin: context.interaction.clone(),
            },
        };
        codec.pending = Some(Pending {
            operation: ops::Operation::RecoverFromModelError,
            tx: "tx".into(),
            input,
            digest: Some(digest),
            child: Some("child".into()),
            output: Some(value.clone()),
            result_digest: Some(result_digest),
            revision: 0,
            reconciling: false,
            phase: Phase::Commit,
        });
        child.parent_session_ref = Some("other-session".into());
        let failure = codec.unknown("tx");
        codec.resource_failed(&recovery(), &failure);
        let ResourceStep::Request(observation) = codec.begin_reconciliation("tx").unwrap() else {
            panic!("status")
        };
        assert_eq!(observation.method, ops::STATUS);
        let result = codec.resume(
            &recovery(),
            serde_json::to_value(ops::OperationStatus::Committed {
                result: Box::new(committed),
                child,
            })
            .unwrap(),
        );
        assert!(matches!(result, Err(PortError::AcceptedUnknown { .. })));
        assert_eq!(proofs.unresolved().len(), 1);
        assert!(proofs.observed().is_empty());
    }

    #[test]
    fn commit_ack_with_wrong_result_digest_remains_unknown() {
        let context = crate::tests::context();
        let proofs = Arc::new(ContextProofs::default());
        let mut codec = ContextCodec::new(
            context.clone(),
            vec![InstalledContextOperation {
                operation: ops::Operation::RecoverFromModelError,
                binding: "recovery".into(),
                revision: "v1".into(),
            }],
            proofs.clone(),
        )
        .unwrap();
        let input = history::OpaquePayload {
            profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
            bytes: pilotdeck_context_runtime::boundary_codec::encode(&JsValue::Object(
                Default::default(),
            ))
            .unwrap(),
        };
        let owner = history::Owner {
            session: context.session_ref.clone(),
            binding: context.binding.id.clone(),
            context: context.invocation.context_ref.clone(),
            call: context.call_ref.clone(),
            origin: context.interaction.clone(),
        };
        let digest = history::request_digest(
            &owner,
            &history::PrepareRequest {
                transaction_id: "tx".into(),
                expected_revision: 0,
                operation: ops::Operation::RecoverFromModelError.name().into(),
                request: input.clone(),
            },
        )
        .unwrap();
        let reservation = history::Reservation {
            transaction_id: "tx".into(),
            request_digest: digest.clone(),
            expected_revision: 0,
            prepare_cursor: 1,
            owner,
        };
        let request = ops::InvocationRequest {
            operation: ops::Operation::RecoverFromModelError,
            input: input.clone(),
            reservation,
        };
        let value =
            pilotdeck_protocol::parse_json(r#"{"type":"give_up","reason":"test"}"#).unwrap();
        let output = ops::OperationResult {
            operation: ops::Operation::RecoverFromModelError,
            output: history::OpaquePayload {
                profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
                bytes: pilotdeck_context_runtime::boundary_codec::encode(&value).unwrap(),
            },
            history: ops::HistoryMutation {
                replacement: None,
                consume_input: false,
                facts: vec![],
                compaction_boundary: None,
            },
            preparation_notice: None,
        };
        let mut child_binding = context.binding.clone();
        child_binding.id = "recovery".into();
        child_binding.revision = "v1".into();
        let mut invocation = context.invocation.clone();
        invocation.parent_execution_ref = Some(context.call_ref.clone());
        invocation.payload = serde_json::to_value(request).unwrap();
        let child = agentcpm_scheduler_protocol::CallView {
            id: "child".into(),
            session_ref: context.session_ref.clone(),
            source_binding: Some(context.binding.id.clone()),
            parent_session_ref: Some(context.session_ref.clone()),
            cause_execution_ref: Some(context.call_ref.clone()),
            binding: child_binding,
            invocation,
            state: State::Completed,
            result: Some(serde_json::to_value(output).unwrap()),
            presentation: None,
            settlement_data: None,
            settlement_error: None,
            error: None,
            waiting: None,
            cursor: 0,
            origin: context.interaction.clone(),
            interaction: context.interaction.clone(),
            interactions: vec![],
            cancel_requested: false,
            stepped: false,
            security: context.security.clone(),
        };
        let result_digest = ops::result_digest(child.result.as_ref().unwrap()).unwrap();
        let committed = ops::Committed {
            receipt: history::Receipt {
                transaction_id: "tx".into(),
                request_digest: digest.clone(),
                commit_digest: "commit".into(),
                revision: 1,
                event_cursor: 2,
                consumed_inputs: vec![],
                replacement_digest: None,
                compaction_digest: None,
            },
            proof: ops::CompletionProof {
                call_ref: "child".into(),
                binding_ref: "recovery".into(),
                binding_revision: "v1".into(),
                operation: ops::Operation::RecoverFromModelError,
                result_digest: "tampered".into(),
                origin: context.interaction.clone(),
            },
        };
        codec.pending = Some(Pending {
            operation: ops::Operation::RecoverFromModelError,
            tx: "tx".into(),
            input,
            digest: Some(digest),
            child: Some("child".into()),
            output: Some(value.clone()),
            result_digest: Some(result_digest),
            revision: 0,
            reconciling: false,
            phase: Phase::Commit,
        });
        let result = codec.resume(&recovery(), serde_json::to_value(committed).unwrap());
        assert!(matches!(result, Err(PortError::AcceptedUnknown { .. })));
        assert_eq!(proofs.unresolved().len(), 1);
        assert!(proofs.observed().is_empty());
    }
}

#[cfg(test)]
#[path = "context_port_tests.rs"]
mod port_tests;
