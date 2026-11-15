//! Full original factory admission, shared with the existing transport.
use crate::{
    AdmittedTurn, AdmittedTurnFactory, CallAssembly, context_codec::ContextProofs,
    resources::ResourceCodec,
};
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
    /// Explicit trusted composition choice; legacy implementations stay on the
    /// existing path and do not require the full Execute envelope here.
    fn supports_context_call(&self) -> bool {
        false
    }
    /// Trusted composition can bind a fresh implementation to this execution's
    /// unique proofs owner. None leaves the legacy binding path unchanged.
    /// This hook must only construct the owner: it must not issue effects or
    /// consume history. Scoped resource codec construction is likewise pure;
    /// the original execute identity is validated before any HostPort read.
    fn bind_context_call(
        &self,
        _context: &CallContext,
        _input: &IfaceTurnInput,
        _client: Arc<dyn Client>,
        _control: &CallControl,
        _proofs: Arc<ContextProofs>,
    ) -> PortResult<Option<Arc<dyn OriginalBindings>>> {
        Ok(None)
    }
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
    product_identity: bool,
}

impl OriginalPayloadAdmission {
    pub fn new(process_cwd: JsString, bindings: Arc<dyn OriginalBindings>) -> Self {
        Self {
            process_cwd,
            bindings,
            product_identity: false,
        }
    }
}

fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: message.into(),
    }
}

struct OriginalCallAssembly {
    admission: OriginalPayloadAdmission,
    context: CallContext,
    input: IfaceTurnInput,
    client: Arc<dyn Client>,
    // Retain the unique owner through codec, preparation, events and cleanup.
    _proofs: Arc<ContextProofs>,
}
impl CallAssembly for OriginalCallAssembly {
    fn resource_codec(&mut self) -> PortResult<Box<dyn ResourceCodec>> {
        self.admission.resource_codec(&self.context, &self.input)
    }
    fn prepare(
        &mut self,
        host: &mut dyn HostPort,
        control: &CallControl,
    ) -> PortResult<AdmittedTurn> {
        self.admission.prepare(
            &self.context,
            &self.input,
            host,
            self.client.clone(),
            control,
        )
    }
}
impl AdmittedTurnFactory for OriginalPayloadAdmission {
    fn bind_call(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Option<Box<dyn CallAssembly>>> {
        control.check()?;
        if !self.bindings.supports_context_call() {
            return Ok(None);
        }
        // Validate the original product identity before any scoped construction.
        let raw = context
            .invocation
            .payload
            .as_str()
            .ok_or_else(|| invalid("missing original execute boundary"))?;
        let boundary = pilotdeck_protocol::parse_json(raw)
            .map_err(|_| invalid("invalid original execute boundary"))?;
        let envelope = pilotdeck_protocol::wire::EnvelopeRef::decode(&boundary)
            .map_err(|_| invalid("invalid original execute envelope"))?;
        if !matches!(
            envelope.message,
            pilotdeck_protocol::wire::MessageRef::Request(pilotdeck_protocol::wire::RequestRef {
                body: pilotdeck_protocol::wire::RequestBodyRef::Execute { .. },
                ..
            })
        ) {
            return Err(invalid("original boundary is not an execute request"));
        }
        let proofs = Arc::new(ContextProofs::default());
        let Some(bindings) = self.bindings.bind_context_call(
            context,
            input,
            client.clone(),
            control,
            proofs.clone(),
        )?
        else {
            return Err(invalid("scoped context binding was not created"));
        };
        Ok(Some(Box::new(OriginalCallAssembly {
            admission: OriginalPayloadAdmission {
                process_cwd: self.process_cwd.clone(),
                bindings,
                product_identity: true,
            },
            context: context.clone(),
            input: input.clone(),
            client,
            _proofs: proofs,
        })))
    }
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
        // The scoped product path must carry the original Execute wire identity.
        // These values are business identifiers only; Kernel CallContext remains
        // the separate authority for resource admission and reconciliation.
        let (run_id, operation_id, idempotency_key) = if self.product_identity {
            use pilotdeck_protocol::wire::{EnvelopeRef, MessageRef, RequestBodyRef};
            let envelope = EnvelopeRef::decode(&boundary)
                .map_err(|_| invalid("invalid original execute envelope"))?;
            let MessageRef::Request(request) = envelope.message else {
                return Err(invalid("original boundary is not a request"));
            };
            let RequestBodyRef::Execute { identity, .. } = request.body else {
                return Err(invalid("original boundary is not an execute request"));
            };
            (
                identity.run_id.clone(),
                identity.operation_id.clone(),
                boundary
                    .get("idempotencyKey")
                    .and_then(JsValue::as_string)
                    .cloned(),
            )
        } else {
            (
                context.call_ref.as_str().into(),
                input.execution.operation_id.as_str().into(),
                input.execution.idempotency_key.as_deref().map(Into::into),
            )
        };
        let initialized = initialization::initialize(&decoded, host, control)?;
        let execution = ExecutionHints {
            run_id,
            operation_id,
            idempotency_key,
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
