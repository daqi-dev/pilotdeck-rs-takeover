//! Native Loop composition. Requires genuine root-installed adapters; no defaults.
use crate::{
    context_codec::{ContextCodec, ContextProofs, InstalledContextOperation},
    initial_read_codec::InitialReadCodec,
    model_session_codec::ModelSessionCodec,
    original::OriginalBindings,
    preparation_effects::ManagedPreparationEffects,
    product_event_queue::{EventSettlementLedger, ProductEventQueue},
    production_events::{EventPublicationLedger, ProductionTurnEvents},
    resource_mux::ResourceMux,
    resources::ResourceCodec,
    sdk_notice_owner::{LifecycleOwner, NoticeAdmission, OwnedActivity, SdkNoticeOwner},
    tool_batch_codec::ToolBatchCodec,
    tool_batch_owner::SourceToolOwners,
};
use agentcpm_iface::types::TurnInput as IfaceTurnInput;
use agentcpm_scheduler_protocol::{CallContext, Client, context_operations::Operation};
use pilotdeck_agent::{
    ports::{CallControl, PortError, PortResult},
    turn::{TurnClock, TurnEventPort, TurnInput},
};
use pilotdeck_turn_preparation::PreparationEffects;
use std::sync::{Arc, Mutex};

/// Trusted root creates this from actual negotiated features and installed
/// authorities. Methods are mandatory: absent services cannot become no-ops.
/// Binding and resource-codec construction perform no effects.
pub trait ProductionAdapters: Send + Sync {
    /// Check the fixed startup controller against the actually installed role.
    fn validate_startup(&self, controller: &str, controller_revision: &str) -> PortResult<()>;
    /// Must reject if the actual host lacks model invocation-context, demand,
    /// settlement, late-result and all other required profiles/authorities.
    fn validate_call(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        client: &dyn Client,
    ) -> PortResult<()>;
    /// Retain per-call observation references through real owner settlement.
    /// This is synchronous construction only, never an authority or journal.
    fn retain_observers(
        &self,
        context: &CallContext,
        proofs: Arc<ContextProofs>,
        notices: Arc<EventSettlementLedger>,
        events: Arc<EventPublicationLedger>,
    ) -> PortResult<()>;
    /// Executor owned by the original Native assembly. Correlation comes
    /// from the same source-bound SDK client, never a replacement socket.
    fn tool_executor(
        &self,
        context: &CallContext,
        client: &Arc<dyn Client>,
    ) -> PortResult<tokio::runtime::Handle>;
    /// Exact source aggregate retained by the Native/Kernel late owner, not
    /// merely the worker. It must already be bound to this same Client Arc.
    fn source_tool_owners(
        &self,
        context: &CallContext,
        client: &Arc<dyn Client>,
    ) -> PortResult<Arc<SourceToolOwners>>;
    /// Tool, permission, initialization and other original host operations.
    fn resource_codecs(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        proofs: Arc<ContextProofs>,
    ) -> PortResult<Vec<Box<dyn ResourceCodec>>>;
    /// Supplies the real Kernel-authorized lifecycle, dispatch admission and
    /// owned-activity owners for this call; none can be a silent substitute.
    fn notice_services(
        &self,
        context: &CallContext,
        input: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
        proofs: Arc<ContextProofs>,
    ) -> PortResult<NoticeServices>;
}
pub struct NoticeServices {
    pub admission: Arc<dyn NoticeAdmission>,
    pub lifecycle: Arc<dyn LifecycleOwner>,
    pub activity: Arc<dyn OwnedActivity>,
    /// Actual negotiated event advertisement, not a business payload flag.
    pub event_advertised: bool,
}
pub struct ProductionBindings {
    controller: String,
    controller_revision: String,
    contexts: Vec<InstalledContextOperation>,
    adapters: Arc<dyn ProductionAdapters>,
}
impl ProductionBindings {
    pub fn new(
        controller: String,
        controller_revision: String,
        contexts: Vec<InstalledContextOperation>,
        adapters: Arc<dyn ProductionAdapters>,
    ) -> PortResult<Self> {
        // A normal turn needs the complete original Context surface; a single
        // installed Recovery role must not be advertised as a full runtime.
        let required = [
            Operation::PrepareForModel,
            Operation::ApplyToolResults,
            Operation::RecoverFromModelError,
            Operation::CaptureTurn,
            Operation::TryAutoCompact,
        ];
        if controller.is_empty()
            || controller_revision.is_empty()
            || contexts.len() != required.len()
            || required.iter().any(|op| {
                contexts
                    .iter()
                    .filter(|entry| entry.operation == *op)
                    .count()
                    != 1
            })
            || contexts
                .iter()
                .any(|entry| entry.binding.is_empty() || entry.revision.is_empty())
            || contexts.iter().enumerate().any(|(i, entry)| {
                contexts[..i]
                    .iter()
                    .any(|other| other.binding == entry.binding)
            })
        {
            return Err(invalid("complete, unique Context bindings are required"));
        }
        Ok(Self {
            controller,
            controller_revision,
            contexts,
            adapters,
        })
    }
}
fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: message.into(),
    }
}
struct Scoped {
    context: CallContext,
    input: IfaceTurnInput,
    client: Arc<dyn Client>,
    contexts: Vec<InstalledContextOperation>,
    controller: String,
    adapters: Arc<dyn ProductionAdapters>,
    proofs: Arc<ContextProofs>,
    events: Arc<EventSettlementLedger>,
    publication: Arc<EventPublicationLedger>,
    prepared_notices: Mutex<Option<PreparedNotices>>,
}
struct PreparedNotices {
    effects: Arc<Mutex<ManagedPreparationEffects>>,
    admission: Arc<dyn NoticeAdmission>,
    activity: Arc<dyn OwnedActivity>,
}
impl OriginalBindings for ProductionBindings {
    fn supports_context_call(&self) -> bool {
        true
    }
    fn bind_context_call(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
        proofs: Arc<ContextProofs>,
    ) -> PortResult<Option<Arc<dyn OriginalBindings>>> {
        control.check()?;
        let scope = client
            .step_scope()
            .ok_or_else(|| invalid("an actual step-scoped client is required"))?;
        if input.execution.run_id != context.call_ref
            || !scope.matches(
                context,
                context.interaction.turn,
                &input.execution.operation_id,
            )
        {
            return Err(invalid("step-scoped client identity mismatch"));
        }
        self.adapters
            .validate_call(context, input, client.as_ref())?;
        let notices = Arc::new(EventSettlementLedger::new(
            context.clone(),
            self.controller.clone(),
            self.controller_revision.clone(),
        ));
        let publication = Arc::new(EventPublicationLedger::new(
            context.clone(),
            self.controller.clone(),
            self.controller_revision.clone(),
        ));
        self.adapters.retain_observers(
            context,
            proofs.clone(),
            notices.clone(),
            publication.clone(),
        )?;
        Ok(Some(Arc::new(Scoped {
            context: context.clone(),
            input: input.clone(),
            client,
            contexts: self.contexts.clone(),
            controller: self.controller.clone(),
            adapters: self.adapters.clone(),
            proofs,
            events: notices,
            publication,
            prepared_notices: Mutex::new(None),
        })))
    }
    fn resources(&self, _: &CallContext, _: &IfaceTurnInput) -> PortResult<Box<dyn ResourceCodec>> {
        Err(invalid("production binding must be scoped"))
    }
    fn effects(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        Err(invalid("production binding must be scoped"))
    }
    fn events(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        Err(invalid("production binding must be scoped"))
    }
    fn clock(&self, _: &CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        Err(invalid("production binding must be scoped"))
    }
}
impl Scoped {
    fn check(&self, context: &CallContext) -> PortResult<()> {
        if context.call_ref != self.context.call_ref
            || context.session_ref != self.context.session_ref
            || context.binding != self.context.binding
            || context.invocation != self.context.invocation
            || context.state != self.context.state
            || context.origin != self.context.origin
            || context.interaction != self.context.interaction
            || context.security != self.context.security
            || context.attempt != self.context.attempt
        {
            return Err(invalid("production call scope changed"));
        }
        Ok(())
    }
}
impl OriginalBindings for Scoped {
    fn resources(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>> {
        self.check(context)?;
        if input != &self.input {
            return Err(invalid("production input changed"));
        }
        let model = ModelSessionCodec::new_with_local_resource_facts(context.clone())
            .with_consumer_demand()
            .with_invocation_context();
        let context_codec =
            ContextCodec::new(context.clone(), self.contexts.clone(), self.proofs.clone())?;
        let mut codecs = self
            .adapters
            .resource_codecs(context, input, self.proofs.clone())?;
        let executor = self.adapters.tool_executor(context, &self.client)?;
        let (tool, owner) = ToolBatchCodec::new(context.clone(), self.client.clone(), executor)?;
        let owners = self.adapters.source_tool_owners(context, &self.client)?;
        if !owners.matches_client(&self.client) {
            return Err(invalid("tool source client changed"));
        }
        // SDK registers the actual source owner before the codec is exposed.
        // Its ticket is not a Kernel or Host resource-release receipt.
        owners
            .register_native(&self.client)
            .map_err(|_| invalid("native source custody registration failed"))?;
        owners
            .retain(owner)
            .map_err(|_| invalid("tool source cannot retain its owner"))?;
        codecs.push(Box::new(tool));
        codecs.push(Box::new(InitialReadCodec::new(self.client.as_ref())));
        codecs.push(Box::new(model));
        codecs.push(Box::new(context_codec));
        Ok(Box::new(ResourceMux::new(codecs)?))
    }
    fn effects(
        &self,
        context: &CallContext,
        input: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        self.check(context)?;
        let services = self.adapters.notice_services(
            context,
            input,
            client.clone(),
            control,
            self.proofs.clone(),
        )?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| invalid("SDK runtime is unavailable"))?;
        let queue = ProductEventQueue::new(
            client,
            context.clone(),
            self.controller.clone(),
            runtime,
            control.clone(),
            self.events.clone(),
            services.admission.clone(),
        )?;
        let owner = Arc::new(SdkNoticeOwner::new(
            services.admission.clone(),
            services.lifecycle,
            services.activity.clone(),
            queue,
            services.event_advertised,
        ));
        let effects = Arc::new(Mutex::new(ManagedPreparationEffects::new(
            owner,
            control.clone(),
        )));
        let mut slot = self
            .prepared_notices
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return Err(invalid("preparation notices already initialized"));
        }
        *slot = Some(PreparedNotices {
            effects: effects.clone(),
            admission: services.admission,
            activity: services.activity,
        });
        Ok(effects)
    }
    fn events(
        &self,
        context: &CallContext,
        input: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        self.check(context)?;
        let _ = input;
        let slot = self
            .prepared_notices
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let state = slot
            .as_ref()
            .ok_or_else(|| invalid("preparation notices are not initialized"))?;
        Ok(Box::new(ProductionTurnEvents::new(
            context.clone(),
            self.controller.clone(),
            client,
            control.clone(),
            state.admission.clone(),
            state.activity.clone(),
            state.effects.clone(),
            self.publication.clone(),
        )?))
    }
    fn clock(&self, context: &CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        self.check(context)?;
        Ok(Box::new(crate::production_clock::ProductionTurnClock))
    }
}
