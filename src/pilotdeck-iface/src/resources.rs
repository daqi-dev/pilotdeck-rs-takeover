//! The only effectful port used by the Native loop is a Scheduler-bound client.
//! A codec translates immutable boundaries; it does not own a product runtime.
use agentcpm_scheduler_protocol::{CallContext, Client, Failure};
use pilotdeck_agent::ports::{
    CallControl, ExecutionIdentity, HostCall, HostPort, HostReply, PortError, PortResult,
};
use pilotdeck_protocol::{JsValue, admission::CallEffect, model::BoundarySnapshot};
use serde_json::Value;
use std::sync::Arc;

/// A single request to an existing, public Kernel resource operation.
pub struct ResourceRequest {
    pub method: &'static str,
    pub payload: Value,
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
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep>;
    fn resume(&mut self, call: &HostCall, response: Value) -> PortResult<ResourceStep>;
    /// Explicit close operations for resources still owned by this invocation.
    /// Return an empty list only when this codec owns no unsettled resources.
    fn cleanup_calls(&self) -> Vec<HostCall>;
    /// Observe a classified resource failure without replaying it. A codec may
    /// retire a reservation only when this proves it was never admitted.
    fn resource_failed(&mut self, call: &HostCall, error: &PortError);
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

    fn failure(&self, call: &HostCall, failure: Failure) -> PortError {
        // Only affirmative rejection codes prove that an effect was not admitted.
        // A transport/lifetime failure after sending a mutation remains unknown.
        let rejected = matches!(
            failure.code.as_str(),
            "CAPABILITY_DENIED"
                | "POLICY_DENIED"
                | "DELEGATION_DENIED"
                | "SESSION_FORBIDDEN"
                | "AUTHORITY_REVOKED"
                | "CAPABILITY_PARAMS"
        );
        if call.effect() != CallEffect::ReadOnly && !rejected {
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
        loop {
            match step {
                ResourceStep::Delay { duration, next } => {
                    let until =
                        std::time::Instant::now()
                            .checked_add(duration)
                            .ok_or_else(|| PortError::InvalidResponse {
                                message: "resource poll delay overflowed".into(),
                            })?;
                    self.runtime.block_on(async {
                        loop {
                            if let Err(cancelled) = control.check() {
                                return Err(
                                    if dispatched && call.effect() != CallEffect::ReadOnly {
                                        self.failure(
                                        &call,
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
                    })?;
                    step = *next;
                }
                ResourceStep::Reply(reply) => {
                    if !call.accepts(&reply) {
                        if dispatched && call.effect() != CallEffect::ReadOnly {
                            return Err(self.failure(
                                &call,
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
                        return Err(if dispatched && call.effect() != CallEffect::ReadOnly {
                            self.failure(
                                &call,
                                Failure::external(
                                    "OUTCOME_UNKNOWN",
                                    "resource sequence interrupted before final reconciliation",
                                ),
                            )
                        } else {
                            cancelled
                        });
                    }
                    let prior_effect = dispatched && call.effect() != CallEffect::ReadOnly;
                    let method = request.method;
                    let mut original_failure: Option<Failure> = None;
                    dispatched = true;
                    let response = self.runtime.block_on(async {
                        let pending = self.client.resource(request.method, request.payload);
                        tokio::pin!(pending);
                        loop {
                            tokio::select! {
                                biased;
                                result = &mut pending => return result.map_err(|failure| {
                                    original_failure = Some(failure.clone());
                                    self.failure(&call, if prior_effect {
                                        Failure::external("OUTCOME_UNKNOWN", format!("resource sequence not reconciled: {}", failure.wire()))
                                    } else { failure })
                                }),
                                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                                    if let Err(cancelled) = control.check() {
                                        return Err(if call.effect() == CallEffect::ReadOnly {
                                            cancelled
                                        } else {
                                            self.failure(&call, Failure::external("OUTCOME_UNKNOWN", "resource reply pending at cancellation"))
                                        });
                                    }
                                }
                            }
                        }
                    });
                    let response = match response {
                        Ok(response) => response,
                        Err(error) => {
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
                                if observation.method == "model.events_read"
                                    && control.check().is_ok()
                                {
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
                                                    if control.check().is_err() { return Err(Failure::new("OBSERVATION_CANCELLED", "model.reconciliation.cancelled")); }
                                                }
                                            }
                                        }
                                    });
                                    self.codec.reconciliation_observed(&call, result);
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
