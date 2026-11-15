//! Source-bound original tool batches over the public Scheduler resource API.
//! This draft needs the ResourceCodec first-receipt observation hook before it
//! can be installed; no fallback guesses cancellation ordering.
use crate::{
    resources::{
        ObservedResourceStep, ResourceCodec, ResourceRequest, ResourceStep,
        VerifiedResourceResponse,
    },
    tool_batch_owner::{BatchToken, PrepareGuard, ToolBatchOwner},
};
use agentcpm_scheduler_protocol::{
    self as p, Client,
    tool_batch::{self, BusinessResult, PrepareRequest, Query, Receipt, State},
};
use pilotdeck_agent::ports::{CallControl, ExecutionIdentity, HostCall, PortError, PortResult};
use pilotdeck_protocol::{
    JsValue, boundary_codec,
    model::BoundarySnapshot,
    module_response::{TOOL_MODULE_REQUEST_V1, TOOL_MODULE_RESPONSE_V1},
};
use pilotdeck_transport::{
    HostExchangeResponse, settle_tool_response, tool_operation_payload, tool_operation_request,
};
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

enum Phase {
    Prepare(PrepareGuard),
    Status(BatchToken),
}
struct Pending {
    call: HostCall,
    fingerprint: Vec<u8>,
    identity: ExecutionIdentity,
    phase: Phase,
    batch_ref: Option<String>,
    ticket: Option<p::provider::ResourceCall>,
}
pub struct ToolBatchCodec {
    context: p::CallContext,
    owner: Arc<ToolBatchOwner>,
    ids: p::provider::NativeCorrelationIssuer,
    pending: Mutex<Option<Pending>>,
}
impl ToolBatchCodec {
    /// Correlation comes only from the original Native client/Peer; it is not
    /// authority. `client` is the exact Host source-bound scoped Client.
    pub fn new(
        context: p::CallContext,
        client: Arc<dyn Client>,
        runtime: tokio::runtime::Handle,
    ) -> PortResult<(Self, Arc<ToolBatchOwner>)> {
        let step = client
            .step_scope()
            .ok_or_else(|| Self::invalid("Tool step source is unavailable."))?;
        if step.session_ref != context.session_ref
            || step.call_ref != context.call_ref
            || step.interaction != context.interaction
        {
            return Err(Self::invalid(
                "Tool step source does not match the original call.",
            ));
        }
        if !client
            .host_features()
            .iter()
            .any(|feature| feature == tool_batch::FEATURE)
        {
            return Err(PortError::NotAdvertised {
                module: "capability",
                operation: "execute_batch",
            });
        }
        let ids = client
            .native_correlation_issuer()
            .map_err(|_| Self::invalid("Native connection correlation issuer is unavailable."))?;
        client
            .validate_native_correlation_issuer(&ids)
            .map_err(|_| {
                Self::invalid("Native correlation issuer does not belong to the original client.")
            })?;
        let owner = ToolBatchOwner::new(client, runtime);
        Ok((
            Self {
                context,
                owner: owner.clone(),
                ids,
                pending: Mutex::new(None),
            },
            owner,
        ))
    }
    fn request(method: &'static str, payload: Value) -> ResourceStep {
        ResourceStep::Request(ResourceRequest { method, payload })
    }
    fn invalid(message: &'static str) -> PortError {
        PortError::InvalidResponse {
            message: message.into(),
        }
    }
    fn fingerprint(call: &HostCall) -> PortResult<Vec<u8>> {
        let object = tool_operation_payload(call)?;
        boundary_codec::encode(&JsValue::Object(object))
            .map_err(|_| Self::invalid("Unsupported tool request boundary."))
    }
    fn query(batch_ref: &str) -> PortResult<ResourceStep> {
        let payload = serde_json::to_value(Query {
            batch_ref: batch_ref.into(),
        })
        .map_err(|_| Self::invalid("Invalid tool batch reference."))?;
        Ok(ResourceStep::Delay {
            duration: Duration::from_millis(20),
            next: Box::new(Self::request(tool_batch::STATUS, payload)),
        })
    }
    fn resume_reader_response(
        &mut self,
        call: &HostCall,
        response: Value,
        control: &CallControl,
        cancelled_before_response: bool,
    ) -> ObservedResourceStep {
        let mut consumed = false;
        let result = (|| -> PortResult<ResourceStep> {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| Self::invalid("No pending tool batch."))?;
            if Self::fingerprint(call)? != pending.fingerprint {
                return Err(Self::invalid("Tool call changed while pending."));
            }
            let receipt: Receipt = serde_json::from_value(response)
                .map_err(|_| Self::invalid("Invalid tool batch receipt."))?;
            let token = match pending.phase {
                Phase::Prepare(guard) => {
                    let token = guard.received(receipt.clone()).map_err(Self::invalid)?;
                    pending.batch_ref = Some(receipt.batch_ref.clone());
                    token
                }
                Phase::Status(token) => {
                    if pending.batch_ref.as_deref() != Some(receipt.batch_ref.as_str()) {
                        return Err(Self::invalid("Tool batch receipt changed identity."));
                    }
                    token
                }
            };
            if let Some(result) = receipt.result {
                let payload = match result {
                    BusinessResult::Succeeded { payload } | BusinessResult::Failed { payload } => {
                        payload
                    }
                };
                if payload.profile != TOOL_MODULE_RESPONSE_V1 {
                    return Err(Self::invalid("Unsupported tool response profile."));
                }
                let response = boundary_codec::decode(&payload.bytes)
                    .map_err(|_| Self::invalid("Invalid tool response boundary."))?;
                let boundary = BoundarySnapshot::capture(&response);
                let exchanged = HostExchangeResponse {
                    identity: pending.identity,
                    boundary,
                    cancelled_before_response,
                };
                let result = settle_tool_response(control, pending.call, exchanged);
                token.handoff().map_err(Self::invalid)?;
                consumed = !matches!(
                    &result,
                    Err(PortError::AcceptedUnknown { .. }) | Err(PortError::InvalidResponse { .. })
                );
                return result.map(|reply| ResourceStep::Reply(Box::new(reply)));
            }
            if matches!(
                receipt.state,
                State::Failed | State::Unknown | State::Completed
            ) || receipt.closed
            {
                return Err(PortError::AcceptedUnknown {
                    identity: Box::new(pending.identity),
                    last_applied_sequence: None,
                    cause: None,
                });
            }
            let next = Self::query(&receipt.batch_ref)?;
            pending.phase = Phase::Status(token);
            *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending);
            consumed = true;
            Ok(next)
        })();
        ObservedResourceStep {
            result,
            observation_consumed: consumed,
        }
    }
}
impl ResourceCodec for ToolBatchCodec {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        module == "capability" && matches!(operation, "execute" | "execute_batch")
    }
    fn resource_effect(
        &self,
        call: &HostCall,
        method: &str,
    ) -> pilotdeck_protocol::admission::CallEffect {
        if method == tool_batch::STATUS {
            pilotdeck_protocol::admission::CallEffect::ReadOnly
        } else {
            call.effect()
        }
    }
    fn admitted_observation(
        &mut self,
        call: &HostCall,
        control: &CallControl,
        ticket: p::provider::ResourceCall,
    ) -> PortResult<()> {
        let mut slot = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let pending = slot
            .as_mut()
            .ok_or_else(|| Self::invalid("No pending tool batch."))?;
        if Self::fingerprint(call)? != pending.fingerprint {
            return Err(Self::invalid("Tool call changed while pending."));
        }
        self.owner
            .track_observation(
                ticket.clone(),
                call.clone(),
                control.clone(),
                pending.identity.clone(),
                pending.batch_ref.clone(),
            )
            .map_err(Self::invalid)?;
        if let Phase::Prepare(guard) = &mut pending.phase {
            guard.attach(ticket.clone()).map_err(Self::invalid)?;
        }
        pending.ticket = Some(ticket);
        Ok(())
    }
    fn observation_acknowledged(&mut self, _: &HostCall, ticket: &p::provider::ResourceCall) {
        self.owner.observation_acknowledged(ticket);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = pending.as_mut() {
            pending.ticket = None;
        }
    }
    fn observation_ack_failed(&mut self, _: &HostCall, ticket: p::provider::ResourceCall) {
        self.owner.recover_status(ticket, true);
    }
    fn observation_unconsumed(
        &mut self,
        _: &HostCall,
        method: &str,
        ticket: p::provider::ResourceCall,
    ) {
        if method == tool_batch::STATUS {
            self.owner.recover_status(ticket, false);
        } else if method == tool_batch::PREPARE {
            self.owner.observation_needs_consumer(&ticket);
        }
    }
    fn observe_resource(&self, method: &str) -> bool {
        matches!(method, tool_batch::PREPARE | tool_batch::STATUS)
    }
    fn resume_observed(
        &mut self,
        call: &HostCall,
        control: &CallControl,
        response: VerifiedResourceResponse,
    ) -> ObservedResourceStep {
        let cancelled = response.cancelled_before_response();
        match response.into_result() {
            Ok(response) => self.resume_reader_response(call, response, control, cancelled),
            Err(_) => ObservedResourceStep {
                result: Err(Self::invalid("Observed tool response failed.")),
                observation_consumed: false,
            },
        }
    }
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep> {
        if self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return Err(Self::invalid("Concurrent tool call on one codec."));
        }
        let hints = match call {
            HostCall::ExecuteSingle { hints, .. } | HostCall::ExecuteBatch { hints, .. } => hints,
            _ => return Err(Self::invalid("Unsupported tool call.")),
        };
        let message_id: pilotdeck_protocol::JsString = self
            .ids
            .next_id(p::provider::NativeCorrelationKind::ModuleCall)
            .map_err(|_| Self::invalid("Native connection correlation is exhausted."))?
            .into();
        let request_id: pilotdeck_protocol::JsString = self
            .ids
            .next_id(p::provider::NativeCorrelationKind::HostRequest)
            .map_err(|_| Self::invalid("Native connection correlation is exhausted."))?
            .into();
        let binding = self.ids.binding();
        let identity = ExecutionIdentity {
            session_id: hints.session_id.clone(),
            turn_id: hints.turn_id.clone(),
            run_id: hints.execution.run_id.clone(),
            operation_id: hints.execution.operation_id.clone(),
            request_id,
            idempotency_key: hints.execution.idempotency_key.clone(),
            module_instance_id: binding.module_instance_id().into(),
            connection_generation: binding.connection_generation().into(),
        };
        let request = tool_operation_request(call, &identity, message_id.clone())?;
        let envelope = p::context_history::OpaquePayload {
            profile: TOOL_MODULE_REQUEST_V1.into(),
            bytes: boundary_codec::encode(&request)
                .map_err(|_| Self::invalid("Unsupported tool request boundary."))?,
        };
        let id = message_id
            .to_utf8()
            .map_err(|_| Self::invalid("Tool request identity is not UTF-8."))?;
        let request = PrepareRequest {
            idempotency_key: format!("{}:{id}", self.context.call_ref),
            envelope,
        };
        let payload = serde_json::to_value(request)
            .map_err(|_| Self::invalid("Tool batch request could not be encoded."))?;
        let fingerprint = Self::fingerprint(call)?;
        let guard = self.owner.begin_prepare().map_err(Self::invalid)?;
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pending {
            call: call.clone(),
            fingerprint,
            identity,
            phase: Phase::Prepare(guard),
            batch_ref: None,
            ticket: None,
        });
        Ok(Self::request(tool_batch::PREPARE, payload))
    }
    fn resume(&mut self, _: &HostCall, _: Value) -> PortResult<ResourceStep> {
        Err(Self::invalid(
            "Tool codec requires first-receipt observation.",
        ))
    }
    fn cleanup_calls(&self) -> Vec<HostCall> {
        // Transfer any pending accepted batch to its original owner before
        // returning an empty HostCall list; no synthetic cleanup call exists.
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(pending) = pending {
            if matches!(&pending.phase, Phase::Status(_))
                && let Some(ticket) = pending.ticket.clone()
            {
                self.owner.recover_status(ticket, false);
            }
            drop(pending);
        }
        self.owner.seal();
        Vec::new()
    }
    fn resource_failed(&mut self, _: &HostCall, error: &PortError) {
        let Some(pending) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        let rejected = matches!(error, PortError::Cancelled { .. })
            || matches!(error,
                PortError::HostFailure { code, .. } if ["CAPABILITY_DENIED", "POLICY_DENIED", "DELEGATION_DENIED", "SESSION_FORBIDDEN", "AUTHORITY_REVOKED", "CAPABILITY_PARAMS"].iter().any(|name| code.equals_str(name))
            );
        if matches!(&pending.phase, Phase::Status(_))
            && let Some(ticket) = pending.ticket.clone()
        {
            self.owner.recover_status(ticket, false);
        }
        if let Phase::Prepare(guard) = pending.phase {
            if rejected {
                guard.rejected();
            } else {
                drop(guard);
            }
        }
    }
}
