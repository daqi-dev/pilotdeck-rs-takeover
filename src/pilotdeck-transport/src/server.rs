//! Concurrent Module2.0 execution service. Factory owns AgentLoop policy;
//! this owner owns immutable acceptance, streams, controls and bounded shutdown.
//! It does not provide a fake factory, a Date.parse approximation, or persistence.
use crate::{HostExchange, HostExchangeResponse, stdio::ConnectionOwner};
use pilotdeck_agent::ports::{
    CallControl, CancelCause, Cancellation, ExecutionIdentity, PortError, PortResult,
};
use pilotdeck_protocol::admission::CallEffect;
use pilotdeck_protocol::module::ModuleOutcome;
use pilotdeck_protocol::wire::{EnvelopeRef, MessageRef, RequestBodyRef, RequestRef};
use pilotdeck_protocol::{JsObject, JsString, JsValue, MODULE_PROTOCOL_VERSION, parse_json};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound for [`ExecutionServer::settle_on_input_end`]: the original
/// publishes these synchronously, so this only absorbs thread scheduling.
const INPUT_END_SETTLE: Duration = Duration::from_secs(1);

/// Implementations must declare their actual original-engine date profile.
/// Strings with an unsupported engine-valid form must not silently mean no deadline.
pub trait ExecutionClock: Send + Sync + 'static {
    fn now_epoch_ms(&self) -> f64;
    fn now_iso(&self) -> JsString;
    fn parse_date(&self, input: &JsString) -> Result<Option<f64>, JsString>;
}
#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub message_id: JsString,
    pub run_id: JsString,
    pub operation_id: JsString,
    pub request_id: JsString,
    /// Full validated wire request, including payload and deadline presence.
    pub boundary: JsValue,
}
#[derive(Debug, Clone)]
pub struct ExecutionTerminal {
    pub outcome: ModuleOutcome,
    /// Original terminal payload (result/messages/optional checkpoint projections).
    pub payload: JsObject,
    pub error: Option<JsObject>,
}
/// A production implementation must run the real Rust AgentLoop. This component
/// only establishes ownership; implementing this trait is not an acceptance gate.
pub trait ExecutionFactory: Send + Sync + 'static {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal>;
}
pub struct ExecutionContext {
    owner: ConnectionOwner,
    request: ExecuteRequest,
    stream_id: JsString,
    sequence: Arc<Mutex<f64>>,
    identities: Mutex<Vec<ExecutionIdentity>>,
    scope: Arc<crate::stdio::ExecutionScope>,
    pub control: CallControl,
}
impl ExecutionContext {
    pub fn binding(&self) -> &pilotdeck_protocol::admission::SidecarBinding {
        self.owner.binding()
    }
    /// Factory must derive session/turn from the original execute payload rules.
    /// Binding/run/operation/request identities are issued by this owner.
    pub fn identity(&self, session_id: JsString, turn_id: JsString) -> ExecutionIdentity {
        ExecutionIdentity {
            session_id,
            turn_id,
            run_id: self.request.run_id.clone(),
            operation_id: self.request.operation_id.clone(),
            request_id: self.request.request_id.clone(),
            idempotency_key: self
                .request
                .boundary
                .get("idempotencyKey")
                .and_then(JsValue::as_string)
                .cloned(),
            module_instance_id: self.owner.binding().module_instance_id.clone(),
            connection_generation: self.owner.binding().connection_generation.clone(),
        }
    }
    pub fn exchange(&self, identity: ExecutionIdentity) -> PortResult<crate::stdio::BoundExchange> {
        if identity.run_id != self.request.run_id
            || identity.operation_id != self.request.operation_id
            || identity.request_id != self.request.request_id
        {
            return Err(PortError::InvalidResponse {
                message: "Execution identity belongs to another operation.".into(),
            });
        }
        let exchange = self
            .owner
            .bound_exchange(identity.clone())?
            .with_scope(self.scope.clone());
        let mut identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
        if !identities.contains(&identity) {
            identities.push(identity);
        }
        Ok(exchange)
    }
    pub fn call(
        &self,
        identity: ExecutionIdentity,
        module: &'static str,
        operation: &'static str,
        effect: CallEffect,
        payload: JsObject,
    ) -> PortResult<HostExchangeResponse> {
        self.exchange(identity)?
            .exchange(&self.control, module, operation, effect, payload)
    }
    /// Cached accepted-call uncertainty belongs to this execution, even if a
    /// collaborator ignored a cleanup error or has not consumed its reply yet.
    pub(crate) fn check_accepted_uncertainty(&self) -> PortResult<()> {
        let identities = self
            .identities
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for identity in identities {
            self.owner.check_known_uncertainty(&identity)?;
        }
        Ok(())
    }
    pub fn emit(&mut self, event_type: JsString, payload: JsObject) -> PortResult<()> {
        self.control.check()?;
        // Commit sequence together with successful queue admission. Holding this
        // per-stream lock prevents retirement allocating a final in that gap;
        // no connection lock spans this bounded/cancellation-aware capacity wait.
        let mut sequence = self.sequence.lock().unwrap_or_else(|p| p.into_inner());
        let event = self.build_event(event_type, payload, None, None, *sequence);
        self.owner.publish(&self.control, &event)?;
        *sequence += 1.0;
        Ok(())
    }
    /// Publication of already accepted provider data and its aborted partial
    /// result has a separate bounded delivery budget. It cannot authorize a new
    /// host effect and still observes connection retirement/output failure.
    pub(crate) fn emit_accepted(
        &mut self,
        event_type: JsString,
        payload: JsObject,
    ) -> PortResult<()> {
        if matches!(
            self.control.cancellation.cause(),
            Some(CancelCause::OwnerRetired | CancelCause::TransportClosed)
        ) || self.control.cancellation.hard_stop_cause().is_some()
        {
            return self.control.check();
        }
        let delivery = CallControl {
            cancellation: Cancellation::default(),
            deadline: Instant::now().checked_add(Duration::from_secs(5)),
        };
        let mut sequence = self.sequence.lock().unwrap_or_else(|p| p.into_inner());
        let event = self.build_event(event_type, payload, None, None, *sequence);
        self.owner.publish(&delivery, &event)?;
        *sequence += 1.0;
        Ok(())
    }
    fn event(
        &self,
        event_type: JsString,
        payload: JsObject,
        outcome: Option<ModuleOutcome>,
        error: Option<JsObject>,
    ) -> JsValue {
        let mut sequence = self.sequence.lock().unwrap_or_else(|p| p.into_inner());
        let event = self.build_event(event_type, payload, outcome, error, *sequence);
        *sequence += 1.0;
        event
    }
    fn build_event(
        &self,
        event_type: JsString,
        payload: JsObject,
        outcome: Option<ModuleOutcome>,
        error: Option<JsObject>,
        sequence: f64,
    ) -> JsValue {
        // agentLoopSidecarServer.ts names each frame after the branch that
        // sends it: loop events `event` (283), the loop's own result `final`
        // (321, always with payload.result), a thrown execution `failed`
        // (363, payload { error }), an unknown outcome `result-unknown` (422).
        let prefix = match outcome {
            None => "event",
            Some(ModuleOutcome::ResultUnknown) => "result-unknown",
            Some(_) if payload.get("result").is_some() => "final",
            Some(_) => "failed",
        };
        let mut event = object([
            ("kind", JsValue::from("event")),
            ("messageId", JsValue::String(self.owner.next_id(prefix))),
            ("eventType", JsValue::String(event_type)),
            ("streamId", JsValue::String(self.stream_id.clone())),
            ("sequence", JsValue::Number(sequence)),
            ("runId", JsValue::String(self.request.run_id.clone())),
            (
                "operationId",
                JsValue::String(self.request.operation_id.clone()),
            ),
            (
                "requestId",
                JsValue::String(self.request.request_id.clone()),
            ),
            ("final", JsValue::Bool(outcome.is_some())),
        ]);
        // Terminal frames put outcome / code / error before payload (319-345).
        if let Some(outcome) = outcome {
            event.insert("outcome".into(), JsValue::from(outcome_name(outcome)));
        }
        if let Some(error) = error {
            if let Some(JsValue::String(code)) = error.get("code") {
                event.insert("code".into(), JsValue::String(code.clone()));
            }
            event.insert("error".into(), JsValue::Object(error));
        }
        event.insert("payload".into(), JsValue::Object(payload));
        JsValue::Object(event)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationPhase {
    Running,
    CancelRequested,
    Completed,
    Failed,
    Cancelled,
    Resolving,
}
/// `ModuleOperationHost`'s snapshot of one operation (moduleRuntime.ts:21-120):
/// it outlives the executions that run under the operation id, collects every
/// request id accepted for it, and keeps the first terminal outcome.
#[derive(Debug)]
struct OperationSnapshot {
    run_id: JsString,
    operation_id: JsString,
    phase: OperationPhase,
    outcome: Option<ModuleOutcome>,
    request_ids: Vec<JsString>,
    cancel_requested: bool,
    updated_at: JsString,
}
impl OperationSnapshot {
    /// `recordFinal` / `expire` (moduleRuntime.ts:48-61): the first outcome wins.
    fn record_final(&mut self, outcome: ModuleOutcome, now: JsString) {
        if self.outcome.is_some() {
            return;
        }
        self.outcome = Some(outcome);
        self.phase = outcome_phase(outcome);
        self.updated_at = now;
    }
    fn to_payload(&self) -> JsObject {
        let mut snapshot = object([
            ("runId", JsValue::String(self.run_id.clone())),
            ("operationId", JsValue::String(self.operation_id.clone())),
            ("state", JsValue::from(phase_name(self.phase))),
            (
                "requestIds",
                JsValue::Array(
                    self.request_ids
                        .iter()
                        .cloned()
                        .map(JsValue::String)
                        .collect(),
                ),
            ),
            ("cancelRequested", JsValue::Bool(self.cancel_requested)),
            ("updatedAt", JsValue::String(self.updated_at.clone())),
        ]);
        if let Some(outcome) = self.outcome {
            snapshot.insert("outcome".into(), JsValue::from(outcome_name(outcome)));
        }
        snapshot
    }
}
/// An execution between acceptance and its terminal (the baseline's
/// `abortControllers` entry, agentLoopSidecarServer.ts:245, 395-398). `ready`
/// is the owning connection's wake signal.
#[derive(Debug)]
struct ActiveOperation {
    run_id: JsString,
    request_id: JsString,
    stream_id: JsString,
    cancellation: Cancellation,
    deadline: Option<Instant>,
    ready: Arc<pilotdeck_agent::ports::WakeSignal>,
}
#[derive(Debug, Default)]
struct Registry {
    operations: HashMap<JsString, OperationSnapshot>,
    requests: HashMap<JsString, JsString>,
    active: HashMap<JsString, ActiveOperation>,
}
/// Operation state that the baseline keeps on its single
/// `AgentLoopSidecarServer` instance (`operations`, `requestOperations`,
/// `abortControllers`) and therefore shares between every connection a TCP
/// sidecar accepts (tcpAgentLoopSidecarServer.ts:72-80): a status or cancel
/// on one connection sees an execution started on another.
#[derive(Debug, Clone, Default)]
pub struct OperationRegistry(Arc<Mutex<Registry>>);
impl OperationRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}
/// This connection's own executions: what its worker runs and what its
/// retirement settles.
#[derive(Debug)]
struct Operation {
    request: ExecuteRequest,
    stream_id: JsString,
    sequence: Arc<Mutex<f64>>,
    cancellation: Cancellation,
    /// The worker has handed this operation's final event to the connection.
    terminal_published: bool,
    timer: Option<thread::JoinHandle<()>>,
    terminal: Option<JsValue>,
}
#[derive(Debug)]
struct Work {
    request: ExecuteRequest,
    stream_id: JsString,
    sequence: Arc<Mutex<f64>>,
    control: CallControl,
}
#[derive(Debug, Default)]
struct State {
    operations: HashMap<JsString, Operation>,
    queue: VecDeque<Work>,
}
pub struct ExecutionServer {
    owner: ConnectionOwner,
    state: Mutex<State>,
    registry: OperationRegistry,
    ready: Arc<pilotdeck_agent::ports::WakeSignal>,
    factory: Arc<dyn ExecutionFactory>,
    clock: Arc<dyn ExecutionClock>,
    queue_capacity: usize,
    /// The threads running this connection's executions.
    executions: Mutex<Vec<thread::JoinHandle<()>>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    pub reader_joined: bool,
    pub writer_joined: bool,
    pub worker_joined: bool,
    /// The connection ended while executions of the baseline's external loop
    /// still waited on it for a reply, with no deadline that could ever
    /// settle them: the state in which Node, with nothing left on its event
    /// loop, ends the sidecar on its unsettled top-level await.
    pub stalled: bool,
}
impl ShutdownReport {
    pub fn complete(self) -> bool {
        self.reader_joined && self.writer_joined && self.worker_joined
    }
}
impl ExecutionServer {
    pub fn new(
        owner: ConnectionOwner,
        factory: Arc<dyn ExecutionFactory>,
        clock: Arc<dyn ExecutionClock>,
        queue_capacity: usize,
    ) -> Arc<Self> {
        Self::with_registry(
            owner,
            factory,
            clock,
            queue_capacity,
            OperationRegistry::default(),
        )
    }
    /// A connection of a sidecar whose other connections share `registry`.
    pub fn with_registry(
        owner: ConnectionOwner,
        factory: Arc<dyn ExecutionFactory>,
        clock: Arc<dyn ExecutionClock>,
        queue_capacity: usize,
        registry: OperationRegistry,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            state: Mutex::new(State::default()),
            registry,
            ready: Arc::default(),
            factory,
            clock,
            queue_capacity: queue_capacity.max(1),
            executions: Mutex::default(),
        })
    }
    /// Called by one reader. No factory work or blocking output occurs here.
    pub fn receive(&self, value: &JsValue) -> io::Result<()> {
        let envelope = match EnvelopeRef::decode(value) {
            Ok(envelope) => envelope,
            Err(error) => return self.send_error("validation", error.code, error.message.into()),
        };
        let MessageRef::Request(request) = envelope.message else {
            if matches!(envelope.message, MessageRef::Response(_)) {
                self.owner.receive_response(value);
            }
            return Ok(());
        };
        match request.body {
            RequestBodyRef::Hello { .. } | RequestBodyRef::Capabilities { .. } => {
                // handshake (488-505): no requestId; capabilities only fills payload.
                let capabilities = matches!(request.body, RequestBodyRef::Capabilities { .. });
                let mut response = self.response(
                    if capabilities {
                        "capabilities"
                    } else {
                        "hello"
                    },
                    request,
                    None,
                    true,
                );
                let binding = self.owner.binding();
                for (key, value) in [
                    ("protocolVersion", JsValue::from(MODULE_PROTOCOL_VERSION)),
                    ("moduleId", JsValue::String(binding.module_id.clone())),
                    (
                        "moduleInstanceId",
                        JsValue::String(binding.module_instance_id.clone()),
                    ),
                    (
                        "connectionGeneration",
                        JsValue::String(binding.connection_generation.clone()),
                    ),
                    (
                        "capabilitiesVersion",
                        JsValue::String(binding.capabilities_version.clone()),
                    ),
                ] {
                    response.insert(key.into(), value);
                }
                response.insert(
                    "payload".into(),
                    JsValue::Object(if capabilities {
                        self.capabilities()
                    } else {
                        JsObject::new()
                    }),
                );
                self.owner.send_control(&JsValue::Object(response))
            }
            RequestBodyRef::Execute { identity, .. } => self.accept(
                request,
                ExecuteRequest {
                    message_id: request.message_id.clone(),
                    run_id: identity.run_id.clone(),
                    operation_id: identity.operation_id.clone(),
                    request_id: identity.request_id.clone(),
                    boundary: value.clone(),
                },
            ),
            RequestBodyRef::Cancel {
                run_id,
                operation_id,
                reason,
            } => {
                // 169-192: the active execution of this operation, on any connection.
                let request_id = request.fields.get("requestId").and_then(JsValue::as_string);
                let mut guard = self.registry.lock();
                let registry = &mut *guard;
                let active = registry.active.get(operation_id);
                let matches = active.is_some_and(|active| {
                    &active.run_id == run_id && request_id.is_none_or(|id| id == &active.request_id)
                });
                let code = if active.is_some() {
                    "OPERATION_IDENTITY_MISMATCH"
                } else {
                    "UNKNOWN_OPERATION"
                };
                let mut woken = None;
                if matches {
                    let active = active.expect("matched active operation");
                    if let Some(snapshot) = registry.operations.get_mut(operation_id)
                        && snapshot.outcome.is_none()
                    {
                        snapshot.cancel_requested = true;
                        snapshot.phase = OperationPhase::CancelRequested;
                        snapshot.updated_at = self.clock.now_iso();
                    }
                    if active.deadline.is_some_and(|d| Instant::now() >= d) {
                        active.cancellation.cancel(CancelCause::Deadline);
                    }
                    let mut timeout: Vec<u16> = "timeout:".encode_utf16().collect();
                    timeout.extend_from_slice(run_id.units());
                    // The baseline's external loop module marks the execution
                    // as past its deadline for this reason alone, whether or
                    // not the execute carried a deadline (178-180). The
                    // gateway's own in-process loop gets neither a deadline
                    // nor a deadline terminal (the baseline runs it as a plain
                    // AgentLoop), so there the reason marks a deadline only for
                    // an operation that carries one.
                    let cause = if reason.units() == timeout
                        && (active.deadline.is_some()
                            || crate::factory_payload::external_deployment())
                    {
                        CancelCause::Deadline
                    } else {
                        CancelCause::Requested(reason.clone())
                    };
                    active.cancellation.cancel(cause);
                    woken = Some(active.ready.clone());
                }
                drop(guard);
                if let Some(ready) = woken {
                    ready.notify();
                }
                self.ready.notify();
                let mut response = self.response("cancel", request, request_id, matches);
                if matches {
                    response.insert(
                        "payload".into(),
                        JsValue::Object(object([
                            ("operationId", JsValue::String(operation_id.clone())),
                            ("cancelled", JsValue::Bool(true)),
                        ])),
                    );
                } else {
                    response.insert("code".into(), JsValue::from(code));
                }
                self.owner.send_control(&JsValue::Object(response))
            }
            RequestBodyRef::Status { request_id } => {
                // status (507-520).
                let registry = self.registry.lock();
                let snapshot = registry
                    .requests
                    .get(request_id)
                    .and_then(|id| registry.operations.get(id))
                    .map(OperationSnapshot::to_payload);
                drop(registry);
                let mut response =
                    self.response("status", request, Some(request_id), snapshot.is_some());
                match snapshot {
                    Some(snapshot) => {
                        response.insert("payload".into(), JsValue::Object(snapshot));
                    }
                    None => {
                        response.insert("code".into(), JsValue::from("UNKNOWN_REQUEST"));
                    }
                }
                self.owner.send_control(&JsValue::Object(response))
            }
            RequestBodyRef::ModuleCall { identity, .. } => {
                // module_call is sidecar-originated (158-168).
                self.reject(
                    "module-call",
                    request,
                    Some(identity.request_id),
                    "UNEXPECTED_MODULE_CALL",
                    true,
                )
            }
            // controlFailure (526-528, 558-560, 747-755): no requestId.
            RequestBodyRef::Resume { .. } => {
                self.reject("resume", request, None, "RESUME_UNSUPPORTED", false)
            }
            RequestBodyRef::Ack { .. } => {
                self.reject("ack", request, None, "ACK_UNSUPPORTED", false)
            }
        }
    }
    /// handleExecute's admission (agentLoopSidecarServer.ts:208-267). The
    /// baseline does not deduplicate by request id: a new execute of an
    /// operation that is no longer active runs again under the same
    /// operation snapshot.
    fn accept(&self, request: RequestRef<'_>, input: ExecuteRequest) -> io::Result<()> {
        let request_id = Some(&input.request_id);
        // Parse before accepting. A restricted parser fails visibly, never drops a deadline.
        let mut deadline_ms: Option<f64> = None;
        for name in ["operationDeadline", "attemptDeadline"] {
            if let Some(JsValue::String(value)) = request.fields.get(name) {
                if value.is_empty() {
                    continue;
                }
                match self.clock.parse_date(value) {
                    Ok(Some(ms)) if ms.is_finite() => {
                        deadline_ms = Some(deadline_ms.map_or(ms, |old| old.min(ms)))
                    }
                    Ok(_) => {}
                    Err(_) => {
                        return self.reject(
                            "execute-rejected",
                            request,
                            request_id,
                            "UNSUPPORTED_DEADLINE_PROFILE",
                            true,
                        );
                    }
                }
            }
        }
        let now = self.clock.now_epoch_ms();
        if !now.is_finite() {
            return self.reject(
                "execute-rejected",
                request,
                request_id,
                "INVALID_OWNER_CLOCK",
                true,
            );
        }
        let mut registry = self.registry.lock();
        if let Some(active) = registry.active.get(&input.operation_id) {
            // 209-220.
            let code = if active.run_id == input.run_id {
                "OPERATION_ALREADY_ACTIVE"
            } else {
                "OPERATION_IDENTITY_MISMATCH"
            };
            drop(registry);
            return self.reject("execute-rejected", request, request_id, code, false);
        }
        // The baseline's `operations.accept` throws OPERATION_RUN_MISMATCH
        // here (moduleRuntime.ts:30) and, uncaught inside handleExecute, the
        // rejection takes the whole sidecar process down. This answers it
        // like the active-operation mismatch above instead.
        if registry
            .operations
            .get(&input.operation_id)
            .is_some_and(|snapshot| snapshot.run_id != input.run_id)
        {
            drop(registry);
            return self.reject(
                "execute-rejected",
                request,
                request_id,
                "OPERATION_IDENTITY_MISMATCH",
                false,
            );
        }
        if self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue
            .len()
            >= self.queue_capacity
        {
            drop(registry);
            return self.reject(
                "execute-rejected",
                request,
                request_id,
                "EXECUTION_QUEUE_FULL",
                false,
            );
        }
        // operations.accept (moduleRuntime.ts:27-46) and requestOperations.
        let updated_at = self.clock.now_iso();
        let snapshot = registry
            .operations
            .entry(input.operation_id.clone())
            .or_insert_with(|| OperationSnapshot {
                run_id: input.run_id.clone(),
                operation_id: input.operation_id.clone(),
                phase: OperationPhase::Running,
                outcome: None,
                request_ids: Vec::new(),
                cancel_requested: false,
                updated_at: updated_at.clone(),
            });
        if !snapshot.request_ids.contains(&input.request_id) {
            snapshot.request_ids.push(input.request_id.clone());
        }
        snapshot.updated_at = updated_at;
        registry
            .requests
            .insert(input.request_id.clone(), input.operation_id.clone());
        if deadline_ms.is_some_and(|ms| ms <= now) {
            // 223-236.
            if let Some(snapshot) = registry.operations.get_mut(&input.operation_id) {
                snapshot.record_final(ModuleOutcome::Failed, self.clock.now_iso());
            }
            drop(registry);
            return self.reject("deadline", request, request_id, "DEADLINE_EXCEEDED", true);
        }
        let stream_id = self.owner.next_id("stream");
        let sequence = Arc::new(Mutex::new(0.0));
        let cancellation = Cancellation::default();
        // Freeze the earliest deadline once, never from a later host response.
        let deadline = deadline_ms
            .and_then(|ms| Instant::now().checked_add(execution_deadline_delay(ms - now)));
        let timer = deadline.map(|deadline| {
            let cancellation = cancellation.clone();
            thread::spawn(move || {
                if matches!(
                    cancellation.wait_until(deadline),
                    Some(CancelCause::Deadline)
                ) {
                    cancellation.cancel(CancelCause::Deadline);
                }
            })
        });
        registry.active.insert(
            input.operation_id.clone(),
            ActiveOperation {
                run_id: input.run_id.clone(),
                request_id: input.request_id.clone(),
                stream_id: stream_id.clone(),
                cancellation: cancellation.clone(),
                deadline,
                ready: self.ready.clone(),
            },
        );
        drop(registry);
        let mut accepted = self.response("accepted", request, request_id, true);
        accepted.insert("streamId".into(), JsValue::String(stream_id.clone()));
        accepted.insert("cursor".into(), JsValue::Number(0.0));
        let accepted = JsValue::Object(accepted);
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let replaced = state.operations.insert(
                input.operation_id.clone(),
                Operation {
                    request: input.clone(),
                    stream_id: stream_id.clone(),
                    sequence: sequence.clone(),
                    cancellation: cancellation.clone(),
                    terminal_published: false,
                    timer,
                    terminal: None,
                },
            );
            drop(state);
            if let Some(timer) = replaced.and_then(|op| op.timer) {
                let _ = timer.join();
            }
        }
        if let Err(error) = self.owner.send_control(&accepted) {
            self.retire();
            return Err(error);
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.queue.push_back(Work {
            request: input,
            stream_id,
            sequence,
            control: CallControl {
                cancellation,
                deadline,
            },
        });
        drop(state);
        self.ready.notify();
        Ok(())
    }
    /// Each accepted execute runs on its own thread: the baseline starts every
    /// execution without awaiting it (agentLoopSidecarServer.ts:153-156), so
    /// executions on one connection proceed concurrently.
    fn worker(self: &Arc<Self>) {
        loop {
            let generation = self.ready.generation();
            if self.owner.is_closed() {
                return;
            }
            let work = self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .queue
                .pop_front();
            let Some(work) = work else {
                self.ready.wait(generation, None);
                continue;
            };
            let server = self.clone();
            let handle = thread::spawn(move || {
                let owner = server.owner.clone();
                owner.run_in_turns(|| server.run(work));
            });
            let mut executions = self.executions.lock().unwrap_or_else(|p| p.into_inner());
            executions.retain(|execution| !execution.is_finished());
            executions.push(handle);
        }
    }
    /// Every execution still running waits on the closed connection, and
    /// none has a deadline timer that could abort it.
    fn stalled(&self) -> bool {
        let running = self
            .executions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|execution| !execution.is_finished())
            .count() as u64;
        running > 0
            && self.owner.parked() == running
            && self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .operations
                .values()
                .all(|op| op.terminal.is_some() || op.timer.is_none())
    }
    fn executions_finished(&self) -> bool {
        self.executions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .all(thread::JoinHandle::is_finished)
    }
    fn run(&self, work: Work) {
        let mut context = ExecutionContext {
            owner: self.owner.clone(),
            request: work.request.clone(),
            stream_id: work.stream_id.clone(),
            sequence: work.sequence,
            identities: Mutex::new(Vec::new()),
            scope: Arc::default(),
            control: work.control,
        };
        // Catching a worker panic prevents a permanently running operation.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::default_factory::requested_cancel_left_to_loop(context.control.check())?;
            self.factory.execute(&work.request, &mut context)
        }));
        let terminal = match result {
            Ok(Ok(terminal)) => with_module_failure(terminal, context.scope.module_failure()),
            Ok(Err(error)) => failure_terminal(error),
            Err(_) => failure_terminal(PortError::InvalidResponse {
                message: "Execution worker panicked.".into(),
            }),
        };
        if context
            .control
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            context.control.cancellation.cancel(CancelCause::Deadline);
        }
        let terminal = if matches!(
            context.control.cancellation.cause(),
            Some(CancelCause::Deadline)
        ) {
            ExecutionTerminal {
                outcome: ModuleOutcome::ResultUnknown,
                payload: JsObject::new(),
                error: Some(object([
                    ("code", JsValue::from("DEADLINE_EXCEEDED")),
                    (
                        "message",
                        JsValue::from("Module execution deadline exceeded."),
                    ),
                ])),
            }
        } else {
            terminal
        };
        let event_type = format!("agent.execute.{}", outcome_name(terminal.outcome));
        let final_event = context.event(
            event_type.into(),
            terminal.payload,
            Some(terminal.outcome),
            terminal.error,
        );
        // Wake and reap the exact owned timer; completion is already classified.
        context
            .control
            .cancellation
            .cancel(CancelCause::OwnerRetired);
        let timer = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .operations
            .get_mut(&work.request.operation_id)
            .filter(|op| op.stream_id == work.stream_id)
            .and_then(|op| op.timer.take());
        if let Some(timer) = timer {
            let _ = timer.join();
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let Some(op) = state
                .operations
                .get_mut(&work.request.operation_id)
                .filter(|op| op.stream_id == work.stream_id)
            else {
                return;
            };
            if op.terminal.is_some() {
                return;
            }
            op.terminal = Some(final_event.clone());
        }
        // recordFinal precedes the final frame (318-319).
        if let Some(snapshot) = self
            .registry
            .lock()
            .operations
            .get_mut(&work.request.operation_id)
        {
            snapshot.record_final(terminal.outcome, self.clock.now_iso());
        }
        // Main cancellation must not suppress its own terminal. Cleanup has a
        // separately bounded publication budget, never a permission to retry effects.
        let cleanup = CallControl {
            cancellation: Cancellation::default(),
            deadline: Instant::now().checked_add(Duration::from_secs(5)),
        };
        if self.owner.publish(&cleanup, &final_event).is_err() {
            self.retire();
        }
        // The abortControllers entry goes after the final is sent (395-398).
        self.release_active(&work.request.operation_id, &work.stream_id);
        if let Some(op) = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .operations
            .get_mut(&work.request.operation_id)
            .filter(|op| op.stream_id == work.stream_id)
        {
            op.terminal_published = true;
        }
        self.ready.notify();
        if terminal.outcome != ModuleOutcome::ResultUnknown {
            for identity in context
                .identities
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
            {
                self.owner.release_known_calls(identity);
            }
        }
    }
    fn release_active(&self, operation_id: &JsString, stream_id: &JsString) {
        let mut registry = self.registry.lock();
        if registry
            .active
            .get(operation_id)
            .is_some_and(|active| &active.stream_id == stream_id)
        {
            registry.active.remove(operation_id);
        }
    }
    /// The input reached its end. agentLoopSidecarServer.ts serve() handles each
    /// line to completion, and a matching cancel aborts the execution
    /// synchronously, so an execution that settles on its abort alone has
    /// already published its terminal by the time the end of input is read;
    /// everything sent before that is flushed. Only sends attempted after the
    /// end are dropped. Mirror that: give executions whose cancel was already
    /// accepted a bounded chance to publish, let the writer drain what is queued,
    /// and only then retire the connection.
    fn settle_on_input_end(&self) {
        let until = Instant::now()
            .checked_add(INPUT_END_SETTLE)
            .unwrap_or_else(Instant::now);
        loop {
            let generation = self.ready.generation();
            let pending = self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .operations
                .values()
                .any(|op| {
                    matches!(
                        op.cancellation.cause(),
                        Some(CancelCause::Requested(_) | CancelCause::Deadline)
                    ) && !op.terminal_published
                });
            if !pending || self.owner.is_closed() || Instant::now() >= until {
                break;
            }
            self.ready.wait(generation, Some(until));
        }
        self.owner.wait_drained(until);
    }
    pub fn retire(&self) {
        self.owner.close();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut retired = Vec::new();
        // The baseline's external loop module only marks the connection
        // closed (agentLoopSidecarServer.ts:201-204): its executions keep
        // running, stay active for a cancel or status on another connection,
        // and settle on their own terms.
        let external = crate::factory_payload::external_deployment();
        for op in state
            .operations
            .values_mut()
            .filter(|op| op.terminal.is_none() && !external)
        {
            op.cancellation.cancel(CancelCause::TransportClosed);
            let context = ExecutionContext {
                owner: self.owner.clone(),
                request: op.request.clone(),
                stream_id: op.stream_id.clone(),
                sequence: op.sequence.clone(),
                identities: Mutex::new(Vec::new()),
                scope: Arc::default(),
                control: CallControl {
                    cancellation: op.cancellation.clone(),
                    deadline: None,
                },
            };
            op.terminal = Some(context.event(
                "agent.execute.result_unknown".into(),
                JsObject::new(),
                Some(ModuleOutcome::ResultUnknown),
                Some(object([
                    ("code", JsValue::from("RESULT_UNKNOWN")),
                    (
                        "message",
                        JsValue::from("Execution connection retired before a known terminal."),
                    ),
                ])),
            ));
            retired.push((op.request.operation_id.clone(), op.stream_id.clone()));
        }
        state.queue.clear();
        drop(state);
        if !retired.is_empty() {
            let now = self.clock.now_iso();
            let mut registry = self.registry.lock();
            for (operation_id, stream_id) in &retired {
                if registry
                    .active
                    .get(operation_id)
                    .is_some_and(|active| &active.stream_id == stream_id)
                {
                    registry.active.remove(operation_id);
                    if let Some(snapshot) = registry.operations.get_mut(operation_id) {
                        snapshot.record_final(ModuleOutcome::ResultUnknown, now.clone());
                    }
                }
            }
        }
        self.ready.notify();
        // Executions that keep running still own their deadline timers.
        let timers: Vec<_> = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state
                .operations
                .values_mut()
                .filter(|op| op.terminal.is_some() || !external)
                .filter_map(|op| op.timer.take())
                .collect()
        };
        for timer in timers {
            let _ = timer.join();
        }
    }
    /// Blocking reader/writer handles are never joined indefinitely. An incomplete
    /// report requires the owning production process to terminate, not reuse this
    /// server as a cleanly retired connection. GUI/embedding owners need an actual
    /// interruptible IO implementation before treating incomplete shutdown as safe.
    pub fn serve(
        self: &Arc<Self>,
        input: impl BufRead + Send + 'static,
        output: impl Write + Send + 'static,
        grace: Duration,
    ) -> ShutdownReport {
        let (done_tx, done_rx) = mpsc::channel();
        let reader_server = self.clone();
        let reader_done = done_tx.clone();
        let reader = thread::spawn(move || {
            let mut lines = LineReader::new(input);
            while !reader_server.owner.is_closed() {
                let bytes = match lines.next_line() {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        reader_server.settle_on_input_end();
                        break;
                    }
                    Err(_) => break,
                };
                let line = String::from_utf8_lossy(&bytes);
                if line.chars().all(js_whitespace) {
                    continue;
                }
                let result = match parse_json(&line) {
                    Ok(value) => reader_server.receive(&value),
                    Err(_) => reader_server.send_error(
                        "parse",
                        "INVALID_JSON",
                        "Module message is not valid JSON.".into(),
                    ),
                };
                if result.is_err() {
                    break;
                }
            }
            reader_server.retire();
            let _ = reader_done.send(());
        });
        let writer_server = self.clone();
        let writer_done = done_tx.clone();
        let writer = thread::spawn(move || {
            let _ = writer_server.owner.drive_writer(output);
            writer_server.retire();
            let _ = writer_done.send(());
        });
        let worker_server = self.clone();
        let worker = thread::spawn(move || {
            worker_server.worker();
            let _ = done_tx.send(());
        });
        let _ = done_rx.recv();
        self.retire();
        let until = Instant::now()
            .checked_add(grace)
            .unwrap_or_else(Instant::now);
        let mut stalled = false;
        while Instant::now() < until
            && !(reader.is_finished()
                && writer.is_finished()
                && worker.is_finished()
                && self.executions_finished())
        {
            if reader.is_finished()
                && writer.is_finished()
                && worker.is_finished()
                && self.stalled()
            {
                stalled = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let report = ShutdownReport {
            reader_joined: reader.is_finished(),
            writer_joined: writer.is_finished(),
            worker_joined: worker.is_finished() && self.executions_finished(),
            stalled,
        };
        if report.reader_joined {
            let _ = reader.join();
        }
        if report.writer_joined {
            let _ = writer.join();
        }
        if report.worker_joined {
            let _ = worker.join();
        }
        report
    }
    /// The baseline's response envelopes: kind, messageId (`nextId(prefix)`),
    /// inReplyTo, then requestId when the branch writes one, then ok.
    fn response(
        &self,
        prefix: &str,
        request: RequestRef<'_>,
        request_id: Option<&JsString>,
        ok: bool,
    ) -> JsObject {
        let mut response = object([
            ("kind", JsValue::from("response")),
            ("messageId", JsValue::String(self.owner.next_id(prefix))),
            ("inReplyTo", JsValue::String(request.message_id.clone())),
        ]);
        if let Some(id) = request_id {
            response.insert("requestId".into(), JsValue::String(id.clone()));
        }
        response.insert("ok".into(), JsValue::Bool(ok));
        response
    }
    /// A refusal: `final: true, outcome: "failed"` before the code when the
    /// branch ends the call (module_call 158-168, deadline 226-235).
    fn reject(
        &self,
        prefix: &str,
        request: RequestRef<'_>,
        request_id: Option<&JsString>,
        code: &'static str,
        final_failure: bool,
    ) -> io::Result<()> {
        let mut response = self.response(prefix, request, request_id, false);
        if final_failure {
            response.insert("final".into(), JsValue::Bool(true));
            response.insert("outcome".into(), JsValue::from("failed"));
        }
        response.insert("code".into(), JsValue::from(code));
        self.owner.send_control(&JsValue::Object(response))
    }
    /// `parse` for a line that is not JSON (114-123), `validation` for one
    /// validateModuleMessage refuses (125-134).
    fn send_error(&self, prefix: &str, code: &'static str, message: JsString) -> io::Result<()> {
        self.owner.send_control(&JsValue::Object(object([
            ("kind", JsValue::from("error")),
            ("messageId", JsValue::String(self.owner.next_id(prefix))),
            ("code", JsValue::from(code)),
            ("message", JsValue::String(message)),
            ("retryability", JsValue::from("unsafe")),
        ])))
    }
    fn capabilities(&self) -> JsObject {
        let methods = [
            ("execute", true),
            ("cancel", true),
            ("status", true),
            ("resume", false),
            ("ack", false),
        ]
        .map(|(name, enabled)| {
            let mut method = object([
                ("name", JsValue::from(name)),
                ("enabled", JsValue::Bool(enabled)),
            ]);
            if name == "execute" {
                for (key, value) in [
                    ("profiles", JsValue::Array(vec![JsValue::from("streaming")])),
                    ("cancel", JsValue::Bool(true)),
                    ("resumeSupport", JsValue::from("none")),
                    ("retry", JsValue::from("retry_after_status")),
                ] {
                    method.insert(key.into(), value);
                }
            }
            JsValue::Object(method)
        });
        object([
            (
                "capabilitiesVersion",
                JsValue::String(self.owner.binding().capabilities_version.clone()),
            ),
            ("methods", JsValue::Array(methods.into())),
        ])
    }
}
/// readline over the input (agentLoopSidecarServer.ts:108, `crlfDelay:
/// Infinity`): `\n`, `\r\n` and a lone `\r` each end a line, a `\n` right
/// after a `\r` (even in a later read) ends nothing, and a lone `\r` hands its
/// line over at once instead of waiting for more input. The bytes after the
/// last line ending come out as a final line when the input ends.
struct LineReader<R> {
    input: R,
    after_cr: bool,
}
impl<R: BufRead> LineReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            after_cr: false,
        }
    }
    fn next_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let mut started = false;
        loop {
            let available = match self.input.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if available.is_empty() {
                return Ok(started.then_some(line));
            }
            if self.after_cr && available[0] == b'\n' {
                self.after_cr = false;
                self.input.consume(1);
                continue;
            }
            self.after_cr = false;
            started = true;
            match available.iter().position(|b| *b == b'\n' || *b == b'\r') {
                Some(end) => {
                    line.extend_from_slice(&available[..end]);
                    self.after_cr = available[end] == b'\r';
                    self.input.consume(end + 1);
                    return Ok(Some(line));
                }
                None => {
                    let length = available.len();
                    line.extend_from_slice(available);
                    self.input.consume(length);
                }
            }
        }
    }
}
fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    object
}
fn outcome_name(outcome: ModuleOutcome) -> &'static str {
    match outcome {
        ModuleOutcome::Completed => "completed",
        ModuleOutcome::Failed => "failed",
        ModuleOutcome::Cancelled => "cancelled",
        ModuleOutcome::ResultUnknown => "result_unknown",
    }
}
fn outcome_phase(outcome: ModuleOutcome) -> OperationPhase {
    match outcome {
        ModuleOutcome::Completed => OperationPhase::Completed,
        ModuleOutcome::Failed => OperationPhase::Failed,
        ModuleOutcome::Cancelled => OperationPhase::Cancelled,
        ModuleOutcome::ResultUnknown => OperationPhase::Resolving,
    }
}
fn phase_name(phase: OperationPhase) -> &'static str {
    match phase {
        OperationPhase::Running => "running",
        OperationPhase::CancelRequested => "cancel_requested",
        OperationPhase::Completed => "completed",
        OperationPhase::Failed => "failed",
        OperationPhase::Cancelled => "cancelled",
        OperationPhase::Resolving => "resolving",
    }
}
/// The loop's own result for an `error` run carries the execution's last
/// host-module failure (agentLoopSidecarServer.ts:303-339): as
/// `payload.moduleFailure` right after `messages`, and as the terminal error
/// when the result names none.
fn with_module_failure(
    mut terminal: ExecutionTerminal,
    failure: Option<JsObject>,
) -> ExecutionTerminal {
    let Some(failure) = failure else {
        return terminal;
    };
    let errored = terminal
        .payload
        .get("result")
        .and_then(|result| result.get("type"))
        .and_then(JsValue::as_string)
        .is_some_and(|kind| kind.equals_str("error"));
    if !errored {
        return terminal;
    }
    let mut payload = JsObject::new();
    for (key, value) in std::mem::take(&mut terminal.payload).entries() {
        payload.insert(key.clone(), value.clone());
        if key.equals_str("messages") {
            payload.insert("moduleFailure".into(), JsValue::Object(failure.clone()));
        }
    }
    if payload.get("moduleFailure").is_none() {
        payload.insert("moduleFailure".into(), JsValue::Object(failure.clone()));
    }
    terminal.payload = payload;
    if terminal.error.is_none() {
        terminal.error = Some(failure);
    }
    terminal
}
pub(crate) fn failure_terminal(error: PortError) -> ExecutionTerminal {
    let accepted_call = if let PortError::AcceptedUnknown {
        identity,
        last_applied_sequence,
    } = &error
    {
        let mut call = object([
            ("sessionId", JsValue::String(identity.session_id.clone())),
            ("turnId", JsValue::String(identity.turn_id.clone())),
            ("runId", JsValue::String(identity.run_id.clone())),
            (
                "operationId",
                JsValue::String(identity.operation_id.clone()),
            ),
            ("requestId", JsValue::String(identity.request_id.clone())),
            (
                "moduleInstanceId",
                JsValue::String(identity.module_instance_id.clone()),
            ),
            (
                "connectionGeneration",
                JsValue::String(identity.connection_generation.clone()),
            ),
        ]);
        if let Some(key) = &identity.idempotency_key {
            call.insert("idempotencyKey".into(), JsValue::String(key.clone()));
        }
        if let Some(sequence) = last_applied_sequence {
            call.insert("lastAppliedSequence".into(), JsValue::Number(*sequence));
        }
        Some(JsValue::Object(call))
    } else {
        None
    };
    let (outcome, code, message) = match error {
        PortError::AcceptedUnknown { .. } => (
            ModuleOutcome::ResultUnknown,
            Some("RESULT_UNKNOWN".into()),
            "Host capability execution outcome is unknown.".into(),
        ),
        PortError::Cancelled {
            cause: CancelCause::Deadline,
        } => (
            ModuleOutcome::ResultUnknown,
            Some("DEADLINE_EXCEEDED".into()),
            "Module execution deadline exceeded.".into(),
        ),
        PortError::Cancelled { .. } => (
            ModuleOutcome::Cancelled,
            None,
            "Sidecar module call aborted.".into(),
        ),
        PortError::HostFailure { code, message, .. }
        | PortError::ProviderBoundaryOnly { code, message, .. } => {
            (ModuleOutcome::Failed, Some(code), message)
        }
        PortError::KnownTerminalException { code, message, .. } => {
            // This is the proven thrown Error's code, not a raw diagnostic
            // moduleCode. Source's outer catch classifies it before cancellation.
            let outcome = if code
                .as_ref()
                .is_some_and(|code| code.equals_str("RESULT_UNKNOWN"))
            {
                ModuleOutcome::ResultUnknown
            } else {
                ModuleOutcome::Failed
            };
            (outcome, code, message)
        }
        PortError::InvalidResponse { message }
        | PortError::SourceException { message, .. }
        | PortError::KnownConsumerException { message, .. }
        | PortError::TransportBeforeDispatch { message } => (ModuleOutcome::Failed, None, message),
        PortError::NotAdvertised { module, operation } => (
            ModuleOutcome::Failed,
            None,
            format!("Host method {module}.{operation} is not advertised.").into(),
        ),
        PortError::Provider(error) => (
            ModuleOutcome::Failed,
            Some(error.view.code),
            error.view.message,
        ),
    };
    let mut error = object([("message", JsValue::String(message))]);
    if let Some(code) = code {
        error.insert("code".into(), JsValue::String(code));
    }
    if let Some(call) = accepted_call {
        error.insert("acceptedCall".into(), call);
    }
    ExecutionTerminal {
        outcome,
        payload: object([("error", JsValue::Object(error.clone()))]),
        error: Some(error),
    }
}
/// TS agentLoopSidecarServer.ts:252-253 expires nonpositive delays immediately;
/// positive delays pass through setTimeout after capping at 2^31-1 ms. The real
/// Node timer-list probe for #401 confirmed 0.5 -> 1, 1.9 -> 1, 2.9 -> 2 ms.
fn execution_deadline_delay(remaining_ms: f64) -> Duration {
    if remaining_ms <= 0.0 {
        Duration::ZERO
    } else {
        Duration::from_millis(pilotdeck_protocol::timer_delay_ms(
            remaining_ms.min(2_147_483_647.0),
        ))
    }
}

#[cfg(test)]
mod deadline_delay_tests {
    use super::*;

    #[test]
    fn execution_deadline_matches_ts_timer_list_and_immediate_expiry() {
        // Real TS server + Node 22 timer-list values (not just _idleTimeout).
        for (remaining_ms, expected_ms) in [
            (-1.0, 0),
            (0.0, 0),
            (0.5, 1),
            (1.0, 1),
            (1.9, 1),
            (2.9, 2),
            // 封顶在截断之前：2^31-1 本身与 2^31-1+0.5 都落在 2_147_483_647
            (2_147_483_647.0, 2_147_483_647),
            (2_147_483_647.5, 2_147_483_647),
            (2_147_483_648.0, 2_147_483_647),
        ] {
            assert_eq!(
                execution_deadline_delay(remaining_ms),
                Duration::from_millis(expected_ms),
                "remaining_ms={remaining_ms}",
            );
        }
    }
}

#[cfg(test)]
mod line_reader_tests {
    use super::LineReader;
    use std::io::{BufReader, Read};

    /// Hands out the input in the given pieces, one per read.
    struct Pieces(Vec<&'static [u8]>);
    impl Read for Pieces {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let piece = self.0.remove(0);
            buffer[..piece.len()].copy_from_slice(piece);
            Ok(piece.len())
        }
    }

    #[test]
    fn splits_like_readline_with_infinite_crlf_delay() {
        let mut lines = LineReader::new(BufReader::new(Pieces(vec![
            b"a\rb\n",
            b"c\r",
            b"\nd\r\n\r\n",
            b"\n",
            b"e\r",
            b"\r",
            b"tail",
        ])));
        let mut seen = Vec::new();
        while let Some(line) = lines.next_line().unwrap() {
            seen.push(String::from_utf8(line).unwrap());
        }
        assert_eq!(seen, ["a", "b", "c", "d", "", "", "e", "", "tail"]);
    }
}

fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

#[cfg(test)]
mod known_terminal_error_tests {
    use super::*;
    use pilotdeck_protocol::model::BoundarySnapshot;

    #[test]
    fn module_failure_follows_messages_only_for_an_error_result() {
        let failure = object([
            ("code", "PROVIDER_DOWN".into()),
            ("message", "provider down".into()),
        ]);
        let terminal = |kind: &str| ExecutionTerminal {
            outcome: ModuleOutcome::Failed,
            payload: object([
                ("result", JsValue::Object(object([("type", kind.into())]))),
                ("messages", JsValue::Array(vec![])),
                ("seedState", JsValue::Object(JsObject::new())),
            ]),
            error: None,
        };
        let errored = with_module_failure(terminal("error"), Some(failure.clone()));
        let keys: Vec<_> = errored
            .payload
            .entries()
            .iter()
            .map(|(key, _)| key.to_utf8().unwrap())
            .collect();
        assert_eq!(keys, ["result", "messages", "moduleFailure", "seedState"]);
        assert_eq!(errored.error, Some(failure.clone()));
        let max_turns = with_module_failure(terminal("max_turns"), Some(failure));
        assert!(max_turns.payload.get("moduleFailure").is_none());
        assert!(max_turns.error.is_none());
    }

    #[test]
    fn constructed_terminal_error_code_sets_outer_unknown_without_fabricating_acceptance() {
        let terminal = failure_terminal(PortError::KnownTerminalException {
            message: "original terminal message".into(),
            code: Some("RESULT_UNKNOWN".into()),
            boundary: BoundarySnapshot::capture(&JsValue::Null),
        });
        assert_eq!(terminal.outcome, ModuleOutcome::ResultUnknown);
        let error = terminal.error.unwrap();
        assert_eq!(
            error.get("message"),
            Some(&JsValue::String("original terminal message".into()))
        );
        assert_eq!(
            error.get("code"),
            Some(&JsValue::String("RESULT_UNKNOWN".into()))
        );
        assert!(error.get("acceptedCall").is_none());
    }

    #[test]
    fn diagnostic_code_does_not_supply_a_thrown_error_code_or_terminal_property() {
        let boundary = BoundarySnapshot::capture(&JsValue::Object(object([
            ("code", "RESULT_UNKNOWN".into()),
            ("terminal", JsValue::Bool(true)),
        ])));
        for error in [
            PortError::KnownConsumerException {
                message: "ordinary".into(),
                boundary: boundary.clone(),
            },
            PortError::KnownTerminalException {
                message: "terminal".into(),
                code: None,
                boundary: boundary.clone(),
            },
            PortError::KnownTerminalException {
                message: "terminal".into(),
                code: Some("X".into()),
                boundary,
            },
        ] {
            let terminal = failure_terminal(error);
            assert_eq!(terminal.outcome, ModuleOutcome::Failed);
            assert!(terminal.error.unwrap().get("acceptedCall").is_none());
        }
    }
}
