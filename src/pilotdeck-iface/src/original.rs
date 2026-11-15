//! Full original factory admission, shared with the existing transport.
use crate::{AdmittedTurn, AdmittedTurnFactory, resources::ResourceCodec};
use agentcpm_iface::types::TurnInput as IfaceTurnInput;
use agentcpm_scheduler_protocol::{CallContext, Client};
use pilotdeck_agent::{
    ports::*,
    turn::{TurnClock, TurnEventPort, TurnInput},
};
use pilotdeck_protocol::{JsString, JsValue, model::BoundarySnapshot};
use pilotdeck_turn_preparation::{
    OriginalTurnPreparation, PreparationEffects,
    execute::{FactoryPayloadDecoder, FactoryPayloadError, OriginalFactoryPayloadDecoder},
    initialization,
};
use std::sync::{Arc, Mutex};

/// Explicit product composition. A binding provides the real resource codec and
/// event/notification delivery; there is no default text-only or silent no-op.
/// Effectful implementations must use the current Scheduler's bound resources.
pub trait OriginalBindings: Send + Sync {
    fn resources(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>>;
    fn effects(
        &self,
        context: &CallContext,
        input: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>>;
    fn events(
        &self,
        context: &CallContext,
        input: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>>;
    fn clock(&self, context: &CallContext) -> PortResult<Box<dyn TurnClock + Send>>;
}

pub struct OriginalPayloadAdmission {
    process_cwd: JsString,
    bindings: Arc<dyn OriginalBindings>,
}

impl OriginalPayloadAdmission {
    pub fn new(process_cwd: JsString, bindings: Arc<dyn OriginalBindings>) -> Self {
        Self {
            process_cwd,
            bindings,
        }
    }
}

fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: message.into(),
    }
}

impl AdmittedTurnFactory for OriginalPayloadAdmission {
    fn resource_codec(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>> {
        self.bindings.resources(context, input)
    }

    fn prepare(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        host: &mut dyn HostPort,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<AdmittedTurn> {
        control.check()?;
        // Scheduler payload is an opaque application value. Carry the original
        // serialized boundary as text so lone UTF-16 escapes survive the SDK's
        // serde_json layer. The compatibility entry owns this serialization.
        let raw = context.invocation.payload.as_str().ok_or_else(|| {
            invalid(
                "PilotDeck invocation payload must contain the original execute boundary JSON text",
            )
        })?;
        let boundary = pilotdeck_protocol::parse_json(raw)
            .map_err(|_| invalid("PilotDeck execute boundary is not valid JSON"))?;
        let decoded = OriginalFactoryPayloadDecoder
            .decode(&boundary, &self.process_cwd)
            .map_err(|error| {
                let message = match error {
                    FactoryPayloadError::NativeGraphRequired { message }
                    | FactoryPayloadError::SourceTypeError { message }
                    | FactoryPayloadError::InvalidSubagentModel { message }
                    | FactoryPayloadError::InvalidEnvelope { message } => message,
                };
                PortError::SourceException {
                    message,
                    boundary: BoundarySnapshot::capture(&boundary),
                }
            })?;
        let initialized = initialization::initialize(&decoded, host, control)?;
        let execution = ExecutionHints {
            run_id: context.call_ref.as_str().into(),
            operation_id: input.execution.operation_id.as_str().into(),
            idempotency_key: input.execution.idempotency_key.as_deref().map(Into::into),
            operation_deadline: boundary
                .get("operationDeadline")
                .and_then(JsValue::as_string)
                .cloned(),
        };
        // Preserve the old initialization order: these bindings are created only
        // after PlanTodo/metadata reads and restored-state admission succeed.
        let effects = self
            .bindings
            .effects(context, &decoded.input, client.clone(), control)?;
        let preparation = OriginalTurnPreparation::new(
            initialized.state,
            initialized.model_state,
            initialization::model_hints(&decoded.input, execution.clone()),
            decoded.methods,
            effects,
        );
        let tool_hints = initialization::tool_hints(&decoded.config, &decoded.input, execution);
        let initial = decoded
            .input
            .messages
            .iter()
            .map(|message| pilotdeck_turn_preparation::admit_message(message.value()))
            .collect::<PortResult<Vec<_>>>()?;
        let events = self
            .bindings
            .events(context, &decoded.input, client, control)?;
        let clock = self.bindings.clock(context)?;
        Ok(AdmittedTurn {
            config: decoded.config,
            input: decoded.input,
            initial,
            preparation: Box::new(preparation),
            file_state: initialized.file_state,
            events,
            clock,
            tool_hints,
        })
    }
}
