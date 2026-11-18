//! Default factory composition: authentic bound host + Rust owning turn policy.
//! Missing domain profiles produce explicit failures, never a fake successful loop.
use crate::RemoteHost;
use crate::execute::{FactoryPayloadDecoder, OriginalFactoryPayloadDecoder};
use crate::notifications::InstructionBridge;
use crate::server::{
    ExecuteRequest, ExecutionClock, ExecutionContext, ExecutionFactory, ExecutionTerminal,
};
use pilotdeck_agent::ports::*;
use pilotdeck_agent::session_state::{snapshot_file_state, snapshot_model_state};
use pilotdeck_agent::turn::{TurnClock, TurnEventPort, TurnInput, run_admitted_turn};
use pilotdeck_protocol::model::BoundarySnapshot;
use pilotdeck_protocol::module::ModuleOutcome;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::{Arc, Mutex};

pub struct OriginalAgentFactory {
    pub clock: Arc<dyn ExecutionClock>,
    pub process_cwd: JsString,
}
/// Per-execution observer source for the owned native loop. It never changes
/// the presentation stream. Tickets represent occurrences, including equal
/// payloads, and commit only after admission and delivery have both succeeded.
#[derive(Default)]
pub struct NativeLoopActivity(Mutex<ActivityState>);
#[derive(Default)]
struct ActivityState {
    owned: Vec<JsValue>,
    pending: std::collections::VecDeque<Arc<Mutex<ActivityOccurrence>>>,
    ordered: std::collections::VecDeque<Arc<Mutex<ActivityOccurrence>>>,
    committed: Vec<JsValue>,
    closed: bool,
}
struct ActivityOccurrence {
    events: Vec<JsValue>,
    admitted: bool,
    delivered: bool,
    discarded: bool,
}
pub struct NativeActivityAdmission {
    source: Arc<NativeLoopActivity>,
    occurrence: Arc<Mutex<ActivityOccurrence>>,
    settled: bool,
}
pub struct NativeActivityDelivery {
    source: Arc<NativeLoopActivity>,
    occurrence: Arc<Mutex<ActivityOccurrence>>,
    settled: bool,
}
impl NativeLoopActivity {
    pub fn emit_owned(&self, event: JsValue) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !state.closed && state.owned.len() < 64 {
            state.owned.push(event);
        }
    }
    fn drain_owned(&self) {
        // This is the loop's source-only yield port, not a presentation-wire
        // callback. Synchronous delivery into this owned sink accepts this
        // occurrence independently of any subsequent wire event or ACK.
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let events = std::mem::take(&mut state.owned);
        if !state.closed && !events.is_empty() {
            state
                .ordered
                .push_back(Arc::new(Mutex::new(ActivityOccurrence {
                    events,
                    admitted: true,
                    delivered: true,
                    discarded: false,
                })));
        }
        drop(state);
        self.settle();
    }
    fn stage(self: &Arc<Self>, event: JsValue) -> NativeActivityAdmission {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let events = vec![event];
        let occurrence = Arc::new(Mutex::new(ActivityOccurrence {
            events,
            admitted: false,
            delivered: false,
            discarded: state.closed,
        }));
        state.pending.push_back(occurrence.clone());
        state.ordered.push_back(occurrence.clone());
        NativeActivityAdmission {
            source: self.clone(),
            occurrence,
            settled: false,
        }
    }
    /// Claim the next occurrence at synchronous, ordered wire callback capture.
    /// No event name, value or identity comparison is involved.
    pub fn claim(self: &Arc<Self>) -> Option<NativeActivityDelivery> {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        while state.pending.front().is_some_and(|occurrence| {
            occurrence
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .discarded
        }) {
            state.pending.pop_front();
        }
        state
            .pending
            .pop_front()
            .map(|occurrence| NativeActivityDelivery {
                source: self.clone(),
                occurrence,
                settled: false,
            })
    }
    fn settle(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        while let Some(front) = state.ordered.front() {
            let occurrence = front.lock().unwrap_or_else(|p| p.into_inner());
            if !occurrence.discarded && !(occurrence.admitted && occurrence.delivered) {
                break;
            }
            if !state.closed && !occurrence.discarded {
                let events = occurrence.events.clone();
                drop(occurrence);
                let remaining = 64usize.saturating_sub(state.committed.len());
                state.committed.extend(events.into_iter().take(remaining));
            } else {
                drop(occurrence);
            }
            state.ordered.pop_front();
        }
    }
    pub fn finish(&self) -> Vec<JsValue> {
        self.settle();
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.owned.clear();
        // Closing discards unsettled wire deliveries, but a later owned
        // source yield may already have independently completed. Preserve
        // those settled occurrences in order instead of discarding the suffix.
        while let Some(occurrence) = state.ordered.pop_front() {
            let mut occurrence = occurrence.lock().unwrap_or_else(|p| p.into_inner());
            if !occurrence.discarded && occurrence.admitted && occurrence.delivered {
                let remaining = 64usize.saturating_sub(state.committed.len());
                state
                    .committed
                    .extend(occurrence.events.iter().take(remaining).cloned());
            }
            occurrence.discarded = true;
        }
        state.pending.clear();
        std::mem::take(&mut state.committed)
    }
}
impl NativeActivityAdmission {
    fn admit(mut self) {
        self.occurrence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .admitted = true;
        self.settled = true;
        self.source.settle();
    }
}
impl Drop for NativeActivityAdmission {
    fn drop(&mut self) {
        if !self.settled {
            self.occurrence
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .discarded = true;
            self.source.settle();
        }
    }
}
impl NativeActivityDelivery {
    pub fn commit(mut self) {
        self.occurrence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .delivered = true;
        self.settled = true;
        self.source.settle();
    }
}
impl Drop for NativeActivityDelivery {
    fn drop(&mut self) {
        if !self.settled {
            self.occurrence
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .discarded = true;
            self.source.settle();
        }
    }
}
pub struct ObservedOriginalAgentFactory {
    factory: OriginalAgentFactory,
    activity: Arc<NativeLoopActivity>,
}
/// External stdio/TCP AgentLoop source events, without native durable fields.
pub struct SidecarOriginalAgentFactory(OriginalAgentFactory);
impl OriginalAgentFactory {
    pub fn sidecar_public(self) -> SidecarOriginalAgentFactory {
        SidecarOriginalAgentFactory(self)
    }
    pub fn observed(self, activity: Arc<NativeLoopActivity>) -> ObservedOriginalAgentFactory {
        ObservedOriginalAgentFactory {
            factory: self,
            activity,
        }
    }
}
impl ExecutionFactory for SidecarOriginalAgentFactory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal> {
        self.0
            .execute_observed(request, context, None, StatusExposure::SidecarPublic)
    }
}
impl ExecutionFactory for ObservedOriginalAgentFactory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal> {
        self.factory.execute_observed(
            request,
            context,
            Some(self.activity.clone()),
            StatusExposure::NativeDurable,
        )
    }
}
/// IO composition only: final context capture cannot run while an accepted
/// advisory notice is still uncertain. Domain policy and the host remain owned
/// by their original ports; this wrapper supplies no model or Session state.
struct NotificationFencedHost<H> {
    host: H,
    notifications: Arc<Mutex<InstructionBridge>>,
}
impl<H: HostPort> HostPort for NotificationFencedHost<H> {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        self.host.advertised(module, operation)
    }
    fn call(&mut self, control: &CallControl, call: HostCall) -> PortResult<HostReply> {
        if matches!(&call, HostCall::CaptureTurn(_)) {
            self.notifications
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .finish()?;
        }
        self.host.call(control, call)
    }
    fn call_parallel(
        &mut self,
        control: &CallControl,
        calls: Vec<HostCall>,
    ) -> PortResult<Vec<HostReply>> {
        self.host.call_parallel(control, calls)
    }
}
struct ProductionTurnClock(Arc<dyn ExecutionClock>);
impl TurnClock for ProductionTurnClock {
    fn now_iso(&self) -> JsString {
        self.0.now_iso()
    }
    fn uuid(&self) -> JsString {
        uuid::Uuid::new_v4().to_string().into()
    }
    /// The gateway builds its agent loops without a `uuid` dependency
    /// (`AgentTurnCapabilities.clock`), so their transient prompts are
    /// numbered per run.
    fn offers_uuid(&self) -> bool {
        false
    }
}
impl pilotdeck_turn_preparation::PreparationEffects for InstructionBridge {
    fn notice(&mut self, input: &TurnInput, has_system_prompt: bool) -> PortResult<()> {
        InstructionBridge::notice(self, input, has_system_prompt)
    }
    fn before_model_request(&mut self, route: &Route) -> PortResult<()> {
        InstructionBridge::before_model_request(self, route)
    }
    fn finish(&mut self) -> PortResult<()> {
        InstructionBridge::finish(self)
    }
}
#[derive(Clone, Copy)]
enum StatusExposure {
    SidecarPublic,
    NativeDurable,
}
impl StatusExposure {
    fn project(self, kind: &JsString, payload: JsObject) -> JsObject {
        if matches!(self, Self::NativeDurable) || !kind.equals_str("agent_status") {
            return payload;
        }
        // AgentLoop.toAgentStatusEvent selects these fields. The external
        // client's durable callback reconstructs text from this public event;
        // native execution keeps the original internal callback payload.
        let mut public = JsObject::new();
        for key in ["type", "sessionId", "turnId", "event", "detail"] {
            public.insert(
                key.into(),
                payload.get(key).cloned().unwrap_or(JsValue::Undefined),
            );
        }
        public
    }
}
struct EventSink<'a> {
    context: &'a mut ExecutionContext,
    notifications: Arc<Mutex<InstructionBridge>>,
    activity: Option<Arc<NativeLoopActivity>>,
    status_exposure: StatusExposure,
}
impl TurnEventPort for EventSink<'_> {
    fn drain_owned_activity(&mut self) {
        if let Some(activity) = &self.activity {
            activity.drain_owned();
        }
    }
    fn emit(&mut self, boundary: BoundarySnapshot) -> PortResult<()> {
        self.publish(boundary, false)
    }
    fn emit_accepted(&mut self, boundary: BoundarySnapshot) -> PortResult<()> {
        self.publish(boundary, true)
    }
}
impl EventSink<'_> {
    fn publish(&mut self, boundary: BoundarySnapshot, accepted: bool) -> PortResult<()> {
        let payload = boundary
            .value()
            .as_object()
            .ok_or_else(|| gap("AgentEvent object admission"))?
            .clone();
        let kind = payload
            .get("type")
            .and_then(JsValue::as_string)
            .ok_or_else(|| gap("AgentEvent type admission"))?
            .clone();
        // An aborted turn under deadline is an informational source event, not
        // a successful execution or a settled host call. The factory still
        // returns its original Unknown and keeps every accepted-call fence.
        let deadline_abort = accepted
            && matches!(
                self.context.control.check(),
                Err(PortError::Cancelled {
                    cause: pilotdeck_agent::ports::CancelCause::Deadline,
                })
            )
            && payload.get("result").is_some_and(|result| {
                result
                    .get("type")
                    .and_then(JsValue::as_string)
                    .is_some_and(|kind| kind.equals_str("aborted"))
            });
        if kind.equals_str("turn_completed") {
            if !deadline_abort {
                self.context.check_accepted_uncertainty()?;
            }
            let settled = self
                .notifications
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .finish();
            if !matches!(&settled, Err(PortError::AcceptedUnknown { .. }) if deadline_abort) {
                settled?;
            }
        }
        let mut name: Vec<u16> = "agent.".encode_utf16().collect();
        name.extend_from_slice(kind.units());
        let admission = self
            .activity
            .as_ref()
            .map(|activity| activity.stage(JsValue::Object(payload.clone())));
        let payload = self.status_exposure.project(&kind, payload);
        let result = if accepted {
            self.context
                .emit_accepted(JsString::from_utf16(name), payload)
        } else {
            self.context.emit(JsString::from_utf16(name), payload)
        };
        if result.is_ok()
            && let Some(admission) = admission
        {
            admission.admit();
        }
        result
    }
}
impl ExecutionFactory for OriginalAgentFactory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal> {
        self.execute_observed(request, context, None, StatusExposure::NativeDurable)
    }
}
impl OriginalAgentFactory {
    fn execute_observed(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
        activity: Option<Arc<NativeLoopActivity>>,
        status_exposure: StatusExposure,
    ) -> PortResult<ExecutionTerminal> {
        context.control.check()?;
        let mut decoded = OriginalFactoryPayloadDecoder
            .decode(&request.boundary, &self.process_cwd)
            .map_err(|error| {
                let message = match error {
                    crate::execute::FactoryPayloadError::NativeGraphRequired { message }
                    | crate::execute::FactoryPayloadError::SourceTypeError { message }
                    | crate::execute::FactoryPayloadError::InvalidSubagentModel { message }
                    | crate::execute::FactoryPayloadError::InvalidEnvelope { message } => message,
                };
                PortError::SourceException {
                    message,
                    boundary: BoundarySnapshot::capture(&request.boundary),
                }
            })?;
        // #1371: the env-armed controlled loop entry. Inert (unset, or armed
        // without a plan) keeps the production path byte for byte; a malformed
        // plan fails closed instead of quietly running a real turn.
        if let Some(stimulus) = crate::loop_stimulus::LoopStimulus::global() {
            match stimulus.plan() {
                Ok(None) => {}
                Ok(Some(plan)) => {
                    return crate::controlled_loop::run(
                        stimulus.as_ref(),
                        plan,
                        &decoded.input,
                        context,
                    );
                }
                Err(message) => {
                    return Err(PortError::InvalidResponse {
                        message: message.into(),
                    });
                }
            }
        }
        let identity = context.identity(
            decoded.input.session_id.clone(),
            decoded.input.turn_id.clone(),
        );
        let exchange = context.exchange(identity.clone())?;
        let mut effective_methods = decoded.methods.clone();
        // Original model.stream and capability.execute are baseline operations;
        // advertisements select optional preparation, pull and batch protocols.
        for (module, operation) in [("model", "stream"), ("capability", "execute")] {
            if let Some((_, methods)) = effective_methods
                .modules
                .iter_mut()
                .find(|(m, _)| m.equals_str(module))
                && !methods.iter().any(|m| m.equals_str(operation))
            {
                methods.push(operation.into());
            }
        }
        let mut host = RemoteHost::new(exchange, effective_methods);
        // Order matches the original default factory: initialize plan state,
        // fetch initial model metadata, then parse file/model projections.
        let pilotdeck_turn_preparation::initialization::InitialPreparationState {
            state,
            file_state,
            model_state,
        } = pilotdeck_turn_preparation::initialization::initialize(
            &decoded,
            &mut host,
            &context.control,
        )?;
        let execution = ExecutionHints {
            run_id: identity.run_id.clone(),
            operation_id: identity.operation_id.clone(),
            idempotency_key: identity.idempotency_key.clone(),
            operation_deadline: request
                .boundary
                .get("operationDeadline")
                .and_then(JsValue::as_string)
                .cloned(),
        };
        let notifications = Arc::new(Mutex::new(
            InstructionBridge::new(
                context.exchange(identity.clone())?,
                context.exchange(identity.clone())?,
                decoded.methods.clone(),
                context.control.clone(),
            )?
            .with_activity(activity.clone()),
        ));
        let mut preparation = pilotdeck_turn_preparation::OriginalTurnPreparation::new(
            state,
            model_state,
            pilotdeck_turn_preparation::initialization::model_hints(
                &decoded.input,
                execution.clone(),
            ),
            decoded.methods,
            notifications.clone(),
        );
        let mut tools = pilotdeck_turn_preparation::initialization::tool_hints(
            &decoded.config,
            &decoded.input,
            execution,
        );
        let initial = decoded
            .input
            .messages
            .iter()
            .map(|m| crate::admit_message(m.value()))
            .collect::<PortResult<Vec<_>>>()?;
        let control = context.control.clone();
        let cleanup = CallControl {
            cancellation: Cancellation::default(),
            // The invocation owner starts the bounded close budget when cleanup
            // is needed, rather than expiring it during a long healthy stream.
            deadline: None,
        };
        let clock = ProductionTurnClock(self.clock.clone());
        let mut events = EventSink {
            context,
            notifications: notifications.clone(),
            activity,
            status_exposure,
        };
        let mut host = NotificationFencedHost {
            host,
            notifications: notifications.clone(),
        };
        let output = run_admitted_turn(
            &mut decoded.config,
            &decoded.input,
            initial,
            &mut host,
            &mut preparation,
            &mut events,
            &clock,
            &control,
            &cleanup,
            &mut tools,
        );
        // Every owning exit explicitly settles notices. Preserve an existing
        // Unknown first; otherwise a newly unresolved accepted notice outranks
        // an ordinary primary failure. Drop is cleanup, not classification.
        let settled = notifications
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .finish();
        let responsibility = context.check_accepted_uncertainty();
        let output = match (output, settled, responsibility) {
            (Err(error @ PortError::AcceptedUnknown { .. }), _, _) => return Err(error),
            (_, Err(error @ PortError::AcceptedUnknown { .. }), _) => return Err(error),
            (_, _, Err(error @ PortError::AcceptedUnknown { .. })) => return Err(error),
            (Err(error), _, _) | (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => {
                return Err(error);
            }
            (Ok(output), Ok(()), Ok(())) => output,
        };
        let mut payload = JsObject::new();
        payload.insert(
            "result".into(),
            pilotdeck_agent::turn_wire::result(&decoded.input, &output.result)
                .value()
                .clone(),
        );
        payload.insert(
            "messages".into(),
            JsValue::Array(
                output
                    .messages
                    .iter()
                    .map(|m| m.boundary.value().clone())
                    .collect(),
            ),
        );
        payload.insert(
            "seedState".into(),
            snapshot_file_state(&file_state).value().clone(),
        );
        payload.insert(
            "modelState".into(),
            snapshot_model_state(preparation.model_state())
                .value()
                .clone(),
        );
        Ok(ExecutionTerminal {
            outcome: match output.result.kind {
                pilotdeck_agent::turn::TurnResultKind::Success => ModuleOutcome::Completed,
                pilotdeck_agent::turn::TurnResultKind::Aborted => ModuleOutcome::Cancelled,
                _ => ModuleOutcome::Failed,
            },
            payload,
            error: output
                .result
                .errors
                .first()
                .and_then(|error| error.value().as_object())
                .cloned(),
        })
    }
}
fn gap(profile: &str) -> PortError {
    PortError::InvalidResponse {
        message: format!("Rust factory candidate has not implemented {profile}.").into(),
    }
}
#[cfg(test)]
#[path = "default_factory/activity_tests.rs"]
mod activity_tests;

#[cfg(all(test, unix))]
#[path = "default_factory/budget_boundary_tests.rs"]
mod budget_boundary_tests;
