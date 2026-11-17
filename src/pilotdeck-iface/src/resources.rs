//! The only effectful port used by the Native loop is a Scheduler-bound client.
//! A codec translates immutable boundaries; it does not own a product runtime.
use agentcpm_scheduler_protocol::{CallContext, Client, Failure};
use pilotdeck_agent::ports::{
    CallControl, CancelCause, ExecutionIdentity, HostCall, HostPort, HostReply, PortError,
    PortResult,
};
use pilotdeck_protocol::{JsValue, admission::CallEffect, model::BoundarySnapshot};
use serde_json::Value;
use std::sync::Arc;

/// A single request to an existing, public Kernel resource operation.
pub struct ResourceRequest {
    pub method: &'static str,
    pub payload: Value,
}

/// Reader-owned ordering attached to an original scoped SDK response. Only
/// KernelHostPort constructs this after checking the original client/slot.
pub struct VerifiedResourceResponse {
    response: agentcpm_scheduler_protocol::provider::ResourceResponse,
}
impl VerifiedResourceResponse {
    pub fn cancelled_before_response(&self) -> bool {
        self.response.cancelled_before_response()
    }
    pub fn into_result(self) -> Result<Value, Failure> {
        self.response.into_result()
    }
}
/// Whether the original SDK observation may be retired is independent of
/// whether the original business response returned an error.
pub struct ObservedResourceStep {
    pub result: PortResult<ResourceStep>,
    pub observation_consumed: bool,
}
struct OriginalResourceCancellation(CallControl);
impl agentcpm_scheduler_protocol::provider::ResourceCancellation for OriginalResourceCancellation {
    fn was_cancelled_by(&self, received_at: std::time::Instant) -> bool {
        self.0.cancellation.was_cancelled_by(received_at)
            || self
                .0
                .deadline
                .is_some_and(|deadline| deadline <= received_at)
    }
}

/// A codec may perform several resource round trips for one typed host call.
/// It must preserve provider payloads and distinguish acknowledgement from
/// confirmed completion. `Reply` may only describe facts already established.
pub enum ResourceStep {
    Request(ResourceRequest),
    /// Bounded idle poll spacing. This retains the same call/control identity;
    /// it does not open a new execution deadline or imply progress.
    Delay {
        duration: std::time::Duration,
        next: Box<ResourceStep>,
    },
    Reply(Box<HostReply>),
}

/// Pure per-turn boundary translation. No sockets, gateway, authority or files.
/// Implementations advertise only the operations whose complete contract they
/// implement. Unsupported optional operations remain visibly unavailable.
pub trait ResourceCodec: Send {
    fn advertised(&self, module: &str, operation: &str) -> bool;
    /// Discriminate suboperations sharing an advertised method without
    /// granting or dispatching anything. Default preserves legacy routing.
    fn accepts_call(&self, call: &HostCall) -> bool {
        let (module, operation) = call.method();
        self.advertised(module, operation)
    }
    /// Actual public resource effect, never inferred from client payload.
    /// Default preserves the preexisting whole-HostCall classification.
    fn resource_effect(&self, call: &HostCall, _method: &str) -> CallEffect {
        call.effect()
    }
    /// The subrequest whose public effect remains unresolved in this call's
    /// sequence. The port supplies the last dispatched non-read-only
    /// subrequest as the legacy answer; a codec may refute it once its own
    /// protocol proves the effect settled (e.g. an acknowledged session close
    /// whose trailing reads only confirm the registry state).
    fn unresolved_effect(
        &self,
        _call: &HostCall,
        dispatched: Option<&'static str>,
    ) -> Option<&'static str> {
        dispatched
    }
    /// Opt in only for operations which require the original SDK reader's
    /// first-response/cancellation ordering; no worker timestamp is accepted.
    /// Register the validated original SDK ticket before waiting for its reply.
    /// The owning codec must retain it across cancellation and cleanup.
    fn admitted_observation(
        &mut self,
        _call: &HostCall,
        _control: &CallControl,
        _ticket: agentcpm_scheduler_protocol::provider::ResourceCall,
    ) -> PortResult<()> {
        Err(PortError::InvalidResponse {
            message: "observed resource owner unavailable".into(),
        })
    }
    fn observation_acknowledged(
        &mut self,
        _call: &HostCall,
        _ticket: &agentcpm_scheduler_protocol::provider::ResourceCall,
    ) {
    }
    fn observation_ack_failed(
        &mut self,
        _call: &HostCall,
        _ticket: agentcpm_scheduler_protocol::provider::ResourceCall,
    ) {
    }
    fn observation_unconsumed(
        &mut self,
        _call: &HostCall,
        _method: &str,
        _ticket: agentcpm_scheduler_protocol::provider::ResourceCall,
    ) {
    }
    fn observe_resource(&self, _method: &str) -> bool {
        false
    }
    fn resume_observed(
        &mut self,
        _call: &HostCall,
        _control: &CallControl,
        _response: VerifiedResourceResponse,
    ) -> ObservedResourceStep {
        ObservedResourceStep {
            result: Err(PortError::InvalidResponse {
                message: "observed resource is unsupported".into(),
            }),
            observation_consumed: false,
        }
    }
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep>;
    fn resume(&mut self, call: &HostCall, response: Value) -> PortResult<ResourceStep>;
    /// Explicit close operations for resources still owned by this invocation.
    /// Return an empty list only when this codec owns no unsettled resources.
    fn cleanup_calls(&self) -> Vec<HostCall>;
    /// Observe a classified resource failure without replaying it. A codec may
    /// retire a reservation only when this proves it was never admitted.
    fn resource_failed(&mut self, call: &HostCall, error: &PortError);
    /// Retry only an explicitly rejected pre-write request; ambiguous errors must
    /// be reconciled read-only. The codec owns the bound and request identity.
    fn retry_rejected(&mut self, _method: &str, _failure: &Failure) -> Option<ResourceStep> {
        None
    }
    /// An optional, single observation after a failed operation. The returned
    /// error remains authoritative; observation never replays the failed write.
    fn reconcile_failure(
        &mut self,
        _call: &HostCall,
        _method: &str,
        _failure: &Failure,
        _error: &PortError,
        _prior_effect: bool,
    ) -> Option<(ResourceRequest, PortError)> {
        None
    }
    fn reconciliation_observed(&mut self, _call: &HostCall, _result: Result<Value, Failure>) {}
    /// A verified read-only observation may recover the exact already-committed
    /// response; it must not initiate another write or provider operation.
    fn reconciliation_reply(
        &mut self,
        _call: &HostCall,
        _result: &Result<Value, Failure>,
    ) -> Option<PortResult<HostReply>> {
        None
    }
    /// Structured cause retained for diagnostics while this codec owns the resource.
    fn reconciliation_cause(&self) -> Option<&Failure> {
        None
    }
}

pub(crate) struct KernelHostPort {
    client: Arc<dyn Client>,
    context: CallContext,
    runtime: tokio::runtime::Handle,
    codec: Box<dyn ResourceCodec>,
}

impl KernelHostPort {
    #[cfg(test)]
    pub(crate) fn diagnostic_for_test(&self) -> Option<Failure> {
        self.codec.reconciliation_cause().cloned()
    }
    #[cfg(test)]
    pub(crate) fn cleanup_count_for_test(&self) -> usize {
        self.codec.cleanup_calls().len()
    }
    pub(crate) fn finish(&mut self, control: &CallControl) -> PortResult<()> {
        let control = CallControl {
            cancellation: control.cancellation.clone(),
            deadline: Some(control.deadline.map_or_else(
                || std::time::Instant::now() + std::time::Duration::from_secs(5),
                |deadline| {
                    deadline.min(std::time::Instant::now() + std::time::Duration::from_secs(5))
                },
            )),
        };
        let mut first = None;
        for call in self.codec.cleanup_calls() {
            if let Err(error) = self.call(&control, call.clone()) {
                let error = if matches!(error, PortError::Cancelled { .. }) {
                    self.failure(
                        &call,
                        None,
                        Failure::external(
                            "OUTCOME_UNKNOWN",
                            "owned resource cleanup ended before confirmation",
                        ),
                    )
                } else {
                    error
                };
                if first.is_none() || matches!(error, PortError::AcceptedUnknown { .. }) {
                    first = Some(error);
                }
            }
        }
        first.map_or(Ok(()), Err)
    }

    pub(crate) fn new(
        client: Arc<dyn Client>,
        context: CallContext,
        runtime: tokio::runtime::Handle,
        codec: Box<dyn ResourceCodec>,
    ) -> Self {
        Self {
            client,
            context,
            runtime,
            codec,
        }
    }

    fn read_failure(&self, failure: Failure) -> PortError {
        PortError::HostFailure {
            code: failure.code.as_str().into(),
            message: failure.wire().into(),
            boundary: BoundarySnapshot::capture(&JsValue::String(failure.wire().into())),
        }
    }

    fn soft_cancelled_port_error(
        &self,
        control: &CallControl,
        wait_for_interrupt: bool,
    ) -> Option<PortError> {
        let check = || match control.check() {
            Err(PortError::Cancelled { cause })
                if control.cancellation.hard_stop_cause().is_none()
                    && matches!(cause, CancelCause::Requested(_) | CancelCause::Deadline) =>
            {
                Some(PortError::Cancelled { cause })
            }
            _ => None,
        };
        if let Some(cancelled) = check() {
            return Some(cancelled);
        }
        if !wait_for_interrupt {
            return None;
        }
        self.runtime.block_on(async {
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                if let Some(cancelled) = check() {
                    return Some(cancelled);
                }
            }
            None
        })
    }

    fn is_delivery_advance_cancel_rejection(failure: &Failure) -> bool {
        let is_code = |code: &str| {
            matches!(
                code,
                "MODEL_SESSION_BAD_STATE"
                    | "KERNEL_REJECTED"
                    | "CAPABILITY_DENIED"
                    | "SESSION_FORBIDDEN"
                    | "AUTHORITY_REVOKED"
                    | "STEP_AUTHORITY_REVOKED"
                    | "SESSION_CLOSED"
                    | "CALLER_ENDED"
                    | "CANCELLED"
            )
        };
        if is_code(failure.code.as_str()) {
            return true;
        }
        failure.code == "OUTCOME_UNKNOWN"
            && (failure.message_key == "scheduler.resource.accepted_delivery_unconfirmed"
                || failure
                    .params
                    .get("cause")
                    .and_then(|cause| cause.get("code"))
                    .and_then(Value::as_str)
                    .is_some_and(is_code))
    }
    fn failure(&self, call: &HostCall, method: Option<&str>, failure: Failure) -> PortError {
        // Only affirmative rejection codes prove that an effect was not admitted.
        // A transport/lifetime failure after sending a mutation remains unknown.
        // CALLER_ENDED refuses admission because the calling step already ended:
        // the request was never dispatched, so its outcome stays known.
        // Audited 2026-11-17: every kernel CALLER_ENDED is emitted by a
        // pre-dispatch admission/lifecycle gate; a failure observed after
        // dispatch is always rewrapped as OUTCOME_UNKNOWN(cause=CALLER_ENDED)
        // (kernel client.rs resource_delivery_error), which stays unknown
        // below. Narrowing to specific message keys would miss the other
        // pre-dispatch gates (history/context/tool-batch source lifetimes).
        let rejected = matches!(
            failure.code.as_str(),
            "CAPABILITY_DENIED"
                | "POLICY_DENIED"
                | "DELEGATION_DENIED"
                | "SESSION_FORBIDDEN"
                | "AUTHORITY_REVOKED"
                | "CAPABILITY_PARAMS"
                | "CALLER_ENDED"
        );
        // The resource method that failed decides whether an ambiguous outcome
        // can leave an unresolved effect behind, not the whole host call: a
        // close rerouted into a delivery advance is as read-only as a pull's.
        let effect = method.map_or_else(
            || call.effect(),
            |method| self.codec.resource_effect(call, method),
        );
        if effect != CallEffect::ReadOnly && !rejected {
            return PortError::AcceptedUnknown {
                identity: Box::new(ExecutionIdentity {
                    session_id: self.context.session_ref.as_str().into(),
                    turn_id: self.context.interaction.turn.to_string().into(),
                    run_id: self.context.call_ref.as_str().into(),
                    operation_id: self.context.invocation.correlation_id.as_str().into(),
                    request_id: self.context.call_ref.as_str().into(),
                    idempotency_key: Some(self.context.invocation.idempotency_key.as_str().into()),
                    module_instance_id: self.context.binding.id.as_str().into(),
                    connection_generation: self.context.interaction.generation.to_string().into(),
                }),
                last_applied_sequence: None,
                cause: None,
            };
        }
        PortError::HostFailure {
            code: failure.code.as_str().into(),
            message: failure.wire().into(),
            boundary: BoundarySnapshot::capture(&JsValue::String(failure.wire().into())),
        }
    }
}

impl HostPort for KernelHostPort {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        self.codec.advertised(module, operation)
    }

    fn call(&mut self, control: &CallControl, call: HostCall) -> PortResult<HostReply> {
        control.check()?;
        let (module, operation) = call.method();
        if !self.advertised(module, operation) {
            return Err(PortError::NotAdvertised { module, operation });
        }
        let mut step = self.codec.begin(&call)?;
        let mut dispatched = false;
        // The last dispatched subrequest whose public effect was not
        // read-only; an ambiguous failure of the sequence is attributed to it.
        let mut dispatched_effect: Option<&'static str> = None;
        loop {
            match step {
                ResourceStep::Delay { duration, next } => {
                    let until =
                        std::time::Instant::now()
                            .checked_add(duration)
                            .ok_or_else(|| PortError::InvalidResponse {
                                message: "resource poll delay overflowed".into(),
                            })?;
                    let delayed = self.runtime.block_on(async {
                        loop {
                            if let Err(cancelled) = control.check() {
                                return Err(
                                    if let Some(unresolved) =
                                        self.codec.unresolved_effect(&call, dispatched_effect)
                                    {
                                        self.failure(
                                            &call,
                                            Some(unresolved),
                                            Failure::external(
                                                "OUTCOME_UNKNOWN",
                                                "resource sequence interrupted while awaiting progress",
                                            ),
                                        )
                                    } else {
                                        cancelled
                                    },
                                );
                            }
                            let remaining =
                                until.saturating_duration_since(std::time::Instant::now());
                            if remaining.is_zero() {
                                return Ok(());
                            }
                            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(10)))
                                .await;
                        }
                    });
                    if let Err(error) = delayed {
                        self.codec.resource_failed(&call, &error);
                        return Err(error);
                    }
                    step = *next;
                }
                ResourceStep::Reply(reply) => {
                    if !call.accepts(&reply) {
                        if let Some(unresolved) =
                            self.codec.unresolved_effect(&call, dispatched_effect)
                        {
                            return Err(self.failure(
                                &call,
                                Some(unresolved),
                                Failure::external(
                                    "OUTCOME_UNKNOWN",
                                    "effect reply had the wrong host operation",
                                ),
                            ));
                        }
                        return Err(PortError::InvalidResponse {
                            message: "Kernel resource codec returned a different host operation"
                                .into(),
                        });
                    }
                    return Ok(*reply);
                }
                ResourceStep::Request(request) => {
                    if let Err(cancelled) = control.check() {
                        let error = if let Some(unresolved) =
                            self.codec.unresolved_effect(&call, dispatched_effect)
                        {
                            self.failure(
                                &call,
                                Some(unresolved),
                                Failure::external(
                                    "OUTCOME_UNKNOWN",
                                    "resource sequence interrupted before final reconciliation",
                                ),
                            )
                        } else {
                            cancelled
                        };
                        self.codec.resource_failed(&call, &error);
                        return Err(error);
                    }
                    let prior_effect = dispatched;
                    let method = request.method;
                    let current_effect =
                        self.codec.resource_effect(&call, method) != CallEffect::ReadOnly;
                    let mut original_failure: Option<Failure> = None;
                    dispatched |= current_effect;
                    if current_effect {
                        dispatched_effect = Some(method);
                    }
                    if self.codec.observe_resource(method) {
                        // Registration itself is source-bound and observes the original
                        // cancellation. An interrupted waiter never redispatches.
                        let ticket = self.runtime.block_on(async {
                            let observer = Arc::new(OriginalResourceCancellation(control.clone()));
                            let begin = self.client.begin_observed_resource(method, request.payload, observer);
                            tokio::pin!(begin);
                            loop {
                                tokio::select! {
                                    biased;
                                    result = &mut begin => return result,
                                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                                        if control.check().is_err() {
                                            // Generic Client may have dispatched while Pending.
                                            return Err(Failure::external("OUTCOME_UNKNOWN", "resource admission interrupted"));
                                        }
                                    }
                                }
                            }
                        });
                        let ticket = match ticket {
                            Ok(ticket) => ticket,
                            Err(_failure) => {
                                // Generic Client overrides may report an error after
                                // dispatch. A missing ticket is not proof of rejection.
                                let error = self.failure(
                                    &call,
                                    Some(method),
                                    Failure::external(
                                        "OUTCOME_UNKNOWN",
                                        "resource admission was not confirmed",
                                    ),
                                );
                                self.codec.resource_failed(&call, &error);
                                return Err(error);
                            }
                        };
                        if self.client.validate_observed_resource(&ticket).is_err() {
                            // A ticket was already returned: even an identity
                            // validation failure cannot prove non-dispatch.
                            let error = self.failure(
                                &call,
                                Some(method),
                                Failure::external(
                                    "OUTCOME_UNKNOWN",
                                    "resource observation identity was not confirmed",
                                ),
                            );
                            self.codec.resource_failed(&call, &error);
                            return Err(error);
                        }
                        if let Err(error) =
                            self.codec
                                .admitted_observation(&call, control, ticket.clone())
                        {
                            self.codec.resource_failed(&call, &error);
                            return Err(error);
                        }
                        let response = self.runtime.block_on(async {
                            let pending = ticket.response();
                            tokio::pin!(pending);
                            loop {
                                tokio::select! {
                                    biased;
                                    result = &mut pending => return result,
                                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                                        if control.check().is_err() {
                                            return Err(Failure::external("OUTCOME_UNKNOWN", "observed resource pending at cancellation"));
                                        }
                                    }
                                }
                            }
                        });
                        let response = match response {
                            Ok(response) => response,
                            Err(failure) => {
                                let error = self.failure(&call, Some(method), failure);
                                self.codec.resource_failed(&call, &error);
                                return Err(error);
                            }
                        };
                        let outcome = if let Err(failure) = response.result() {
                            let confirmed_rejection = !prior_effect
                                && matches!(
                                    failure.code.as_str(),
                                    "CAPABILITY_DENIED"
                                        | "POLICY_DENIED"
                                        | "DELEGATION_DENIED"
                                        | "SESSION_FORBIDDEN"
                                        | "AUTHORITY_REVOKED"
                                        | "CAPABILITY_PARAMS"
                                );
                            let error = self.failure(
                                &call,
                                Some(method),
                                if prior_effect {
                                    Failure::external(
                                        "OUTCOME_UNKNOWN",
                                        "accepted resource sequence not reconciled",
                                    )
                                } else {
                                    failure.clone()
                                },
                            );
                            self.codec.resource_failed(&call, &error);
                            // Only an affirmative rejection of this first effect
                            // can retire both its prepare guard and observation.
                            ObservedResourceStep {
                                result: Err(error),
                                observation_consumed: confirmed_rejection,
                            }
                        } else {
                            self.codec.resume_observed(
                                &call,
                                control,
                                VerifiedResourceResponse { response },
                            )
                        };
                        // A completed original business error still consumes its
                        // response; an unknown or malformed response remains recoverable.
                        if outcome.observation_consumed {
                            if let Err(failure) =
                                self.runtime.block_on(ticket.acknowledge_response())
                            {
                                self.codec.observation_ack_failed(&call, ticket.clone());
                                return Err(self.failure(&call, Some(method), failure));
                            }
                            self.codec.observation_acknowledged(&call, &ticket);
                        } else {
                            self.codec
                                .observation_unconsumed(&call, method, ticket.clone());
                        }
                        step = outcome.result?;
                        continue;
                    }
                    let response = self.runtime.block_on(async {
                        let pending = self.client.resource(request.method, request.payload);
                        tokio::pin!(pending);
                        loop {
                            tokio::select! {
                                biased;
                                result = &mut pending => return result.map_err(|failure| {
                                    original_failure = Some(failure.clone());
                                    if !prior_effect && !current_effect { self.read_failure(failure) }
                                    else { self.failure(&call, self.codec.unresolved_effect(&call, dispatched_effect).or(Some(method)), if prior_effect {
                                        Failure::external("OUTCOME_UNKNOWN", format!("resource sequence not reconciled: {}", failure.wire()))
                                    } else { failure }) }
                                }),
                                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                                    if let Err(cancelled) = control.check() {
                                        return Err(if !prior_effect && !current_effect {
                                            cancelled
                                        } else {
                                            // An earlier effect whose sequence is still
                                            // unreconciled stays unknown; otherwise only
                                            // this request's own effect decides.
                                            self.failure(&call, self.codec.unresolved_effect(&call, dispatched_effect).or(Some(method)), Failure::external("OUTCOME_UNKNOWN", "resource reply pending at cancellation"))
                                        });
                                    }
                                }
                            }
                        }
                    });
                    let response = match response {
                        Ok(response) => response,
                        Err(error) => {
                            if let Some(failure) = original_failure.as_ref()
                                && let Some(retry) = self.codec.retry_rejected(method, failure)
                            {
                                step = retry;
                                continue;
                            }
                            if !prior_effect
                                && !current_effect
                                && method == "model.delivery_advance"
                            {
                                let is_cancel_rejection = original_failure
                                    .as_ref()
                                    .is_some_and(Self::is_delivery_advance_cancel_rejection);
                                if let Some(cancelled) =
                                    self.soft_cancelled_port_error(control, is_cancel_rejection)
                                {
                                    self.codec.resource_failed(&call, &cancelled);
                                    return Err(cancelled);
                                }
                            }
                            self.codec.resource_failed(&call, &error);
                            if let Some(failure) = original_failure.as_ref()
                                && let Some((observation, unknown)) = self.codec.reconcile_failure(
                                    &call,
                                    method,
                                    failure,
                                    &error,
                                    prior_effect,
                                )
                            {
                                // Hard boundary: a recovery hook may perform one observation only.
                                let allow_observation = match observation.method {
                                    "model.events_read"
                                    | agentcpm_scheduler_protocol::context_operations::STATUS => {
                                        control.check().is_ok()
                                    }
                                    agentcpm_scheduler_protocol::context_history::READ => {
                                        control.cancellation.hard_stop_cause().is_none()
                                    }
                                    _ => false,
                                };
                                if allow_observation {
                                    let is_history_read = observation.method
                                        == agentcpm_scheduler_protocol::context_history::READ;
                                    let result = self.runtime.block_on(async {
                                        let pending = self.client.resource(observation.method, observation.payload);
                                        tokio::pin!(pending);
                                        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
                                        loop {
                                            tokio::select! {
                                                biased;
                                                result = &mut pending => return result,
                                                _ = tokio::time::sleep_until(until) => return Err(Failure::new("OBSERVATION_TIMEOUT", "model.reconciliation.timeout")),
                                                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                                                    let cancelled = if is_history_read {
                                                        control.cancellation.hard_stop_cause().is_some()
                                                    } else {
                                                        control.check().is_err()
                                                    };
                                                    if cancelled {
                                                        return Err(Failure::new("OBSERVATION_CANCELLED", "model.reconciliation.cancelled"));
                                                    }
                                                }
                                            }
                                        }
                                    });
                                    // Decode the already-observed authoritative result even if
                                    // cancellation races after the read. No new request is issued.
                                    if let Some(reply) =
                                        self.codec.reconciliation_reply(&call, &result)
                                    {
                                        return match reply {
                                            Ok(reply) if call.accepts(&reply) => Ok(reply),
                                            Err(error @ PortError::HostFailure { .. })
                                                if observation.method == agentcpm_scheduler_protocol::context_operations::STATUS => Err(error),
                                            Err(error @ PortError::HostFailure { .. })
                                                if is_history_read =>
                                            {
                                                if let Err(cancelled) = control.check() {
                                                    Err(cancelled)
                                                } else {
                                                    Err(error)
                                                }
                                            }
                                            _ => Err(unknown),
                                        };
                                    }
                                    self.codec.reconciliation_observed(&call, result);
                                } else if matches!(
                                    observation.method,
                                    agentcpm_scheduler_protocol::context_operations::STATUS
                                        | agentcpm_scheduler_protocol::context_history::READ
                                ) {
                                    // The observation was never sent. A bare
                                    // CALLER_ENDED / STEP_AUTHORITY_REVOKED already
                                    // proved the request was never admitted, so the
                                    // kernel's admission gate keeps the outcome known:
                                    // retire the never-started transaction instead of
                                    // turning an attributable cancellation unknown.
                                    // The wrapped OUTCOME_UNKNOWN(cause) form stays
                                    // unknown.
                                    if !prior_effect
                                        && matches!(
                                            failure.code.as_str(),
                                            "CALLER_ENDED" | "STEP_AUTHORITY_REVOKED"
                                        )
                                    {
                                        self.codec.resource_failed(&call, &error);
                                        if let Some(cancelled) =
                                            self.soft_cancelled_port_error(control, false)
                                        {
                                            return Err(cancelled);
                                        }
                                        return Err(error);
                                    }
                                    // Retain the unresolved identity while releasing
                                    // the transient pending slot.
                                    self.codec.resource_failed(&call, &unknown);
                                }
                                if !prior_effect
                                    && !current_effect
                                    && let Some(cancelled) =
                                        self.soft_cancelled_port_error(control, false)
                                {
                                    self.codec.resource_failed(&call, &cancelled);
                                    return Err(cancelled);
                                }
                                return Err(unknown);
                            }
                            return Err(error);
                        }
                    };
                    // Decode the acknowledged effect before observing a later
                    // cancellation. Dropping a reply would erase acceptance.
                    step = self.codec.resume(&call, response).map_err(|error| {
                        if call.effect() != CallEffect::ReadOnly
                            && matches!(error, PortError::InvalidResponse { .. })
                        {
                            self.failure(
                                &call,
                                self.codec
                                    .unresolved_effect(&call, dispatched_effect)
                                    .or(Some(method)),
                                Failure::external(
                                    "OUTCOME_UNKNOWN",
                                    "effect reply could not be reconciled",
                                ),
                            )
                        } else {
                            error
                        }
                    })?;
                }
            }
        }
    }
}

#[cfg(test)]
mod original_resource_control_tests {
    use super::*;
    use agentcpm_scheduler_protocol::provider::ResourceCancellation;
    use pilotdeck_agent::ports::{CancelCause, Cancellation};
    use std::time::{Duration, Instant};

    #[test]
    fn observes_the_original_control_and_frozen_reader_instant() {
        let cancellation = Cancellation::default();
        let control = CallControl {
            cancellation: cancellation.clone(),
            deadline: None,
        };
        let observer = OriginalResourceCancellation(control);
        let received_before_cancel = Instant::now() - Duration::from_secs(1);
        cancellation.cancel(CancelCause::Requested("test".into()));
        assert!(!observer.was_cancelled_by(received_before_cancel));
        assert!(observer.was_cancelled_by(Instant::now()));
    }

    #[test]
    fn original_deadline_is_compared_with_reader_time() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let observer = OriginalResourceCancellation(CallControl {
            cancellation: Cancellation::default(),
            deadline: Some(deadline),
        });
        assert!(!observer.was_cancelled_by(deadline - Duration::from_nanos(1)));
        assert!(observer.was_cancelled_by(deadline));
    }
}
