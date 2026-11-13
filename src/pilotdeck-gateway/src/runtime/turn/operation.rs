//! The session's durable operation ledger for an explicitly deployed stdio
//! sidecar (`PILOTDECK_AGENT_LOOP_TRANSPORT=stdio`).
//!
//! The baseline composes a `SessionAgentLoopOperationLedger` for every session
//! (`AgentSessionRuntimeBundle.ts:143-150`), but only an external loop's
//! sidecar client writes to it (`agentLoopSidecarClient.ts:586-632`,
//! `780-798`, `1053-1072`): `agent_loop_operation_started` right before
//! execute is sent, `agent_loop_operation_accepted` when the sidecar accepts
//! the stream, and `agent_loop_operation_terminal` with the projected result
//! once its final event arrives. The gateway's default in-process loop has no
//! such records, so the native deployment (which runs the same loop in a
//! child process here) never installs this ledger.
use crate::{
    host::{
        HostError, HostErrorKind,
        modules::tool_context::{HostToolCheckpoint, serialize_seed_state},
    },
    runtime::project::RuntimeError,
    storage::{
        KnownTerminal, LedgerError, OperationIdentity, OperationResolution, SessionOperationLedger,
        TranscriptSink, TranscriptWriter, UnknownTerminal,
    },
};
use pilotdeck_protocol::{
    JsObject, JsString, JsValue,
    admission::{ExecutionIdentity, SidecarBinding},
    module::{ModuleBinding, ModuleOutcome},
};
use std::sync::{Arc, Mutex};

/// Ledger writes go through the session's own transcript writer, as TS's
/// `transcript.recordSessionEvent` does.
struct WriterSink(Arc<TranscriptWriter>);

impl TranscriptSink for WriterSink {
    fn append(
        &mut self,
        session_id: &JsString,
        turn_id: &JsString,
        event: JsObject,
    ) -> crate::storage::Result<()> {
        self.0
            .record_session_event(session_id, turn_id, event)
            .map_err(|error| LedgerError::Invalid(error.to_string()))
    }
}

/// Session-lifetime state of the sidecar runner: the ledger, the session's
/// host tool checkpoint (`AgentLoopSidecarRunner.seedState`, kept across turns
/// by `this.seedState = hostToolCheckpoint.snapshot()`), and the read grants
/// that turn inputs added to it.
pub struct SessionOperations {
    ledger: Mutex<SessionOperationLedger<WriterSink>>,
    checkpoint: HostToolCheckpoint,
    granted_read_files: Mutex<Vec<JsString>>,
}

impl SessionOperations {
    /// A new session ledger over the session's writer. Restored entries only
    /// matter to `recover`, which looks an operation up by its id; every turn
    /// here executes under a fresh turn id, so none is ever found.
    pub fn new(
        session_id: JsString,
        writer: Arc<TranscriptWriter>,
        checkpoint: HostToolCheckpoint,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            ledger: Mutex::new(
                SessionOperationLedger::new(session_id, WriterSink(writer), &[])
                    .map_err(|error| RuntimeError::new(error.to_string()))?,
            ),
            checkpoint,
            granted_read_files: Mutex::default(),
        })
    }

    /// `new HostToolCheckpoint(turnSeedState, input.allowedReadFiles)`
    /// (`agentLoopSidecarClient.ts:340`): the turn's checkpoint is the
    /// session's seed plus the turn's own read grants.
    pub(super) fn begin_turn(
        self: &Arc<Self>,
        identity: ExecutionIdentity,
        idempotency_key: Option<JsString>,
        options: &JsValue,
    ) -> Arc<TurnOperation> {
        let mut granted = self.granted_read_files.lock().unwrap();
        if let Some(JsValue::Array(paths)) = options.get("allowedReadFiles") {
            for path in paths.iter().filter_map(JsValue::as_string) {
                if !granted.contains(path) {
                    granted.push(path.clone());
                }
            }
        }
        Arc::new(TurnOperation {
            session: self.clone(),
            identity,
            idempotency_key,
            binding: Mutex::default(),
            stream_id: Mutex::default(),
        })
    }

    /// `serializeAgentLoopSeedStateProjection(hostToolCheckpoint.snapshot())`
    /// (`withHostToolCheckpoint`, agentLoopSidecarClient.ts:1033-1035): the
    /// host tools' read and write state as the turn ends.
    fn seed(&self) -> Result<JsValue, RuntimeError> {
        let mut seed = self
            .checkpoint
            .snapshot()
            .map_err(|error| RuntimeError::new(error.message))?;
        let mut allowed = seed.allowed_read_files.take().unwrap_or_default();
        for path in self.granted_read_files.lock().unwrap().iter() {
            if !allowed.contains(path) {
                allowed.push(path.clone());
            }
        }
        seed.allowed_read_files = Some(allowed);
        Ok(JsValue::Object(serialize_seed_state(&seed)))
    }
}

/// One turn's transport attempt in the session ledger.
pub(super) struct TurnOperation {
    session: Arc<SessionOperations>,
    identity: ExecutionIdentity,
    idempotency_key: Option<JsString>,
    binding: Mutex<Option<ModuleBinding>>,
    stream_id: Mutex<Option<JsString>>,
}

impl TurnOperation {
    fn operation_identity(&self) -> Option<OperationIdentity> {
        let binding = self.binding.lock().unwrap().clone()?;
        Some(OperationIdentity {
            run_id: self.identity.run_id.clone(),
            operation_id: self.identity.operation_id.clone(),
            request_id: self.identity.request_id.clone(),
            session_id: self.identity.session_id.clone(),
            turn_id: self.identity.turn_id.clone(),
            binding,
            idempotency_key: self.idempotency_key.clone(),
        })
    }

    /// `operationLedger.start(this.operationIdentity())`.
    pub(super) fn started(&self, binding: &SidecarBinding) -> Result<(), HostError> {
        *self.binding.lock().unwrap() = Some(ModuleBinding {
            module_instance_id: binding.module_instance_id.clone(),
            connection_generation: binding.connection_generation.clone(),
        });
        let identity = self.operation_identity().expect("bound operation");
        self.session
            .ledger
            .lock()
            .unwrap()
            .start(&identity)
            .map_err(ledger_failure)
    }

    /// `operationLedger.accept(this.acceptedOperation())`.
    pub(super) fn accepted(&self, stream_id: &JsString) -> Result<(), HostError> {
        *self.stream_id.lock().unwrap() = Some(stream_id.clone());
        let Some(identity) = self.operation_identity() else {
            return Err(HostError::new(
                HostErrorKind::Protocol,
                "Sidecar execute started without a handshake binding.",
            ));
        };
        self.session
            .ledger
            .lock()
            .unwrap()
            .accept(&identity, stream_id)
            .map_err(ledger_failure)
    }

    /// `recordKnownTerminal(terminal, outcome)` with the projected terminal
    /// (`turn_completed`'s `result` and `messages`) and the host checkpoint.
    pub(super) fn terminal(
        &self,
        outcome: ModuleOutcome,
        last_applied_sequence: f64,
        completed: &JsValue,
    ) -> Result<(), RuntimeError> {
        let Some(identity) = self.operation_identity() else {
            return Err(RuntimeError::new(
                "Sidecar execute started without a handshake binding.",
            ));
        };
        let messages = match completed.get("messages") {
            Some(JsValue::Array(messages)) => messages.clone(),
            _ => Vec::new(),
        };
        let terminal = KnownTerminal {
            identity,
            stream_id: self.stream_id.lock().unwrap().clone(),
            last_applied_sequence,
            resolution: OperationResolution {
                outcome,
                result: completed
                    .get("result")
                    .cloned()
                    .unwrap_or(JsValue::Undefined),
                messages,
                seed_state: Some(self.session.seed()?),
            },
            code: None,
            error: None,
        };
        self.session
            .ledger
            .lock()
            .unwrap()
            .terminal(&terminal)
            .map_err(|error| RuntimeError::new(error.to_string()))
    }
}

impl TurnOperation {
    /// `operationLedger.resultUnknown` for a transport interruption: the
    /// accepted stream, the last event cursor, `TRANSPORT_INTERRUPTED` and
    /// `serializeTransportError(error)` (agentLoopSidecarClient.ts:1003-1013,
    /// 1775-1784). A write failure is dropped.
    pub(super) fn result_unknown(
        &self,
        stream_id: &JsString,
        last_applied_sequence: f64,
        message: &str,
    ) {
        let Some(identity) = self.operation_identity() else {
            return;
        };
        let terminal = UnknownTerminal {
            identity,
            stream_id: stream_id.clone(),
            last_applied_sequence: Some(last_applied_sequence),
            code: Some("TRANSPORT_INTERRUPTED".into()),
            error: Some(crate::metadata::object([("message", message.into())])),
        };
        let _ = self
            .session
            .ledger
            .lock()
            .unwrap()
            .result_unknown(&terminal);
    }
}

fn ledger_failure(error: LedgerError) -> HostError {
    HostError::new(HostErrorKind::Callback, error.to_string())
}

/// `resolveAgentLoopDeploymentProfile`'s transport selection
/// (`AgentLoopDeploymentProfile.ts:130-134`) names an external loop: stdio
/// or tcp.
pub fn explicit_external(transport: Option<&str>) -> bool {
    transport.is_some_and(|value| {
        let value = value.trim();
        value.eq_ignore_ascii_case("stdio") || value.eq_ignore_ascii_case("tcp")
    })
}

/// The module outcome a final event declares.
pub(super) fn final_outcome(frame: &JsValue) -> Option<ModuleOutcome> {
    let outcome = frame.get("outcome")?.as_string()?;
    [
        ("completed", ModuleOutcome::Completed),
        ("failed", ModuleOutcome::Failed),
        ("cancelled", ModuleOutcome::Cancelled),
    ]
    .into_iter()
    .find(|(name, _)| outcome.equals_str(name))
    .map(|(_, outcome)| outcome)
}
