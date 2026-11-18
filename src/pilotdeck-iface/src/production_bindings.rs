//! Native Loop composition. Requires genuine root-installed adapters; no defaults.
use crate::port_helpers::invalid;
use crate::{
    context_codec::{ContextCodec, ContextProofs, InstalledContextOperation},
    initial_read_codec::InitialReadCodec,
    model_session_codec::ModelSessionCodec,
    original::OriginalBindings,
    preparation_effects::{ManagedPreparationEffects, PendingNotice},
    product_event_queue::{EventSettlementLedger, ProductEventQueue},
    production_events::{EventPublicationLedger, ProductionTurnEvents},
    resource_mux::ResourceMux,
    resources::ResourceCodec,
    sdk_notice_owner::{
        ActivityAdmission, LifecycleOwner, NoticeAdmission, OwnedActivity, SdkNoticeOwner,
    },
    tool_batch_codec::ToolBatchCodec,
    tool_batch_owner::SourceToolOwners,
};
use agentcpm_iface::types::TurnInput as IfaceTurnInput;
use agentcpm_scheduler_protocol::{
    CallContext, Client, LOOP_SETTLEMENT_FEATURE, SETTLEMENT_DATA_FEATURE, context_history,
    context_operations, context_operations::Operation, step_scope,
};
use pilotdeck_agent::{
    ports::{CallControl, PortResult},
    turn::{TurnClock, TurnEventPort, TurnInput},
};
use pilotdeck_protocol::JsObject;
use pilotdeck_turn_preparation::PreparationEffects;
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Trusted root creates this from actual negotiated features and installed
/// authorities. Methods are mandatory: absent services cannot become no-ops.
/// The complete original Context operation surface a normal turn requires.
pub(crate) const REQUIRED_TURN_OPERATIONS: [Operation; 5] = [
    Operation::PrepareForModel,
    Operation::ApplyToolResults,
    Operation::RecoverFromModelError,
    Operation::CaptureTurn,
    Operation::TryAutoCompact,
];

/// Fail closed unless the client is step-scoped to this exact turn.
fn require_step_scope(
    client: &dyn Client,
    context: &CallContext,
    input: &IfaceTurnInput,
) -> PortResult<()> {
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
    Ok(())
}

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
        let required = REQUIRED_TURN_OPERATIONS;
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
        require_step_scope(client.as_ref(), context, input)?;
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
        if !crate::port_helpers::same_call_scope(context, &self.context) {
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

/// Host features strictly required by PilotDeck's FullLate production Native Loop.
pub const REQUIRED_PRODUCTION_HOST_FEATURES: &[&str] = &[
    SETTLEMENT_DATA_FEATURE,
    LOOP_SETTLEMENT_FEATURE,
    step_scope::FEATURE,
    context_history::FEATURE,
    context_operations::FEATURE,
    context_operations::failure::FEATURE,
];

struct RetainedObservers {
    proofs: Arc<ContextProofs>,
    _notices: Arc<EventSettlementLedger>,
    _events: Arc<EventPublicationLedger>,
}

/// Production [`ProductionAdapters`] backed by real step-scoped resource codecs,
/// 5-role [`ContextCodec`], source-owned tool custody (`SourceToolOwners`),
/// `ProductController` event publication (`ProductionTurnEvents`), and
/// `SdkNoticeOwner` with strict Kernel host-feature negotiation checks.
#[derive(Default)]
pub struct DefaultProductionAdapters {
    expected_controller: Option<(String, String)>,
    observers: Mutex<BTreeMap<String, RetainedObservers>>,
    tool_owners: Mutex<BTreeMap<String, Arc<SourceToolOwners>>>,
}

impl DefaultProductionAdapters {
    /// Construct production adapters accepting any well-formed controller identity.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct production adapters pinned to a fixed `(controller, revision)` pair.
    pub fn for_controller(controller: String, controller_revision: String) -> Self {
        Self {
            expected_controller: Some((controller, controller_revision)),
            observers: Mutex::new(BTreeMap::new()),
            tool_owners: Mutex::new(BTreeMap::new()),
        }
    }

    /// Snapshot retained [`ContextProofs`] for a given call reference, if present.
    pub fn retained_proofs(&self, call_ref: &str) -> Option<Arc<ContextProofs>> {
        self.observers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(call_ref)
            .map(|entry| entry.proofs.clone())
    }
}

fn valid_startup_id(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0')
}

impl ProductionAdapters for DefaultProductionAdapters {
    fn validate_startup(&self, controller: &str, controller_revision: &str) -> PortResult<()> {
        if !valid_startup_id(controller) || !valid_startup_id(controller_revision) {
            return Err(invalid(
                "controller and controller_revision must be non-empty",
            ));
        }
        if let Some((expected_controller, expected_revision)) = &self.expected_controller
            && (controller != expected_controller || controller_revision != expected_revision)
        {
            return Err(invalid("controller installation mismatch"));
        }
        Ok(())
    }

    fn validate_call(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        client: &dyn Client,
    ) -> PortResult<()> {
        let negotiated: HashSet<String> = client.host_features().into_iter().collect();
        for required in REQUIRED_PRODUCTION_HOST_FEATURES {
            if !negotiated.contains(*required) {
                return Err(invalid(&format!(
                    "host lacks required production feature: {required}"
                )));
            }
        }
        require_step_scope(client, context, input)?;
        tokio::runtime::Handle::try_current().map_err(|_| invalid("SDK runtime is unavailable"))?;
        Ok(())
    }

    fn retain_observers(
        &self,
        context: &CallContext,
        proofs: Arc<ContextProofs>,
        notices: Arc<EventSettlementLedger>,
        events: Arc<EventPublicationLedger>,
    ) -> PortResult<()> {
        if context.call_ref.is_empty() || context.session_ref.is_empty() {
            return Err(invalid("call identity is required to retain observers"));
        }
        let mut guard = self.observers.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(
            context.call_ref.clone(),
            RetainedObservers {
                proofs,
                _notices: notices,
                _events: events,
            },
        );
        Ok(())
    }

    fn tool_executor(
        &self,
        _context: &CallContext,
        _client: &Arc<dyn Client>,
    ) -> PortResult<tokio::runtime::Handle> {
        tokio::runtime::Handle::try_current().map_err(|_| invalid("SDK runtime is unavailable"))
    }

    fn source_tool_owners(
        &self,
        context: &CallContext,
        client: &Arc<dyn Client>,
    ) -> PortResult<Arc<SourceToolOwners>> {
        let mut guard = self.tool_owners.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = guard.get(&context.call_ref) {
            if !existing.matches_client(client) {
                return Err(invalid("tool source client changed"));
            }
            return Ok(existing.clone());
        }
        let owners = SourceToolOwners::new(client.clone());
        guard.insert(context.call_ref.clone(), owners.clone());
        Ok(owners)
    }

    fn resource_codecs(
        &self,
        context: &CallContext,
        _input: &IfaceTurnInput,
        _proofs: Arc<ContextProofs>,
    ) -> PortResult<Vec<Box<dyn ResourceCodec>>> {
        if context.call_ref.is_empty() || context.session_ref.is_empty() {
            return Err(invalid("call identity is required for resource codecs"));
        }
        Ok(Vec::new())
    }

    fn notice_services(
        &self,
        context: &CallContext,
        _input: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
        _proofs: Arc<ContextProofs>,
    ) -> PortResult<NoticeServices> {
        control.check()?;
        let scope = client
            .step_scope()
            .ok_or_else(|| invalid("notice services require an active step-scoped client"))?;
        if scope.session_ref != context.session_ref
            || scope.call_ref != context.call_ref
            || scope.interaction != context.interaction
        {
            return Err(invalid("notice services step-scope mismatch"));
        }
        Ok(NoticeServices {
            admission: Arc::new(DefaultNoticeAdmission {
                context: context.clone(),
                client,
            }),
            lifecycle: Arc::new(DefaultLifecycleOwner {
                retired: AtomicBool::new(false),
            }),
            activity: Arc::new(DefaultOwnedActivity::default()),
            event_advertised: true,
        })
    }
}

struct DefaultNoticeAdmission {
    context: CallContext,
    client: Arc<dyn Client>,
}

impl DefaultNoticeAdmission {
    fn check_scope(&self, control: &CallControl) -> PortResult<()> {
        control.check()?;
        let scope = self
            .client
            .step_scope()
            .ok_or_else(|| invalid("notice dispatch lost step scope"))?;
        if scope.session_ref != self.context.session_ref
            || scope.call_ref != self.context.call_ref
            || scope.interaction != self.context.interaction
        {
            return Err(invalid("notice dispatch step scope mismatch"));
        }
        Ok(())
    }
}

impl NoticeAdmission for DefaultNoticeAdmission {
    fn instructions_loaded(&self, input: &TurnInput, control: &CallControl) -> PortResult<()> {
        if input.session_id.is_empty() || input.turn_id.is_empty() {
            return Err(invalid(
                "instructions_loaded requires session_id and turn_id",
            ));
        }
        self.check_scope(control)
    }

    fn pre_model(&self, control: &CallControl) -> PortResult<()> {
        self.check_scope(control)
    }

    fn before_turn_completed(&self, control: &CallControl) -> PortResult<()> {
        self.check_scope(control)
    }

    fn event_dispatch(&self, control: &CallControl) -> PortResult<()> {
        self.check_scope(control)
    }
}

struct DefaultLifecycleOwner {
    retired: AtomicBool,
}

impl LifecycleOwner for DefaultLifecycleOwner {
    fn advertised(&self) -> bool {
        false
    }

    fn start(
        &self,
        _event: &'static str,
        _detail: JsObject,
        _control: &CallControl,
    ) -> PortResult<Box<dyn PendingNotice>> {
        let _ = self.retired.load(Ordering::Acquire);
        Err(invalid("host lifecycle capability is not installed"))
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct DefaultOwnedActivityInner {
    retired: AtomicBool,
    emitted: Mutex<Vec<JsObject>>,
}

impl DefaultOwnedActivityInner {
    fn emit_event(&self, event: &JsObject) {
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.emitted.lock().unwrap_or_else(|p| p.into_inner());
        if guard.len() >= 1024 {
            guard.remove(0);
        }
        guard.push(event.clone());
    }
}

#[derive(Default)]
struct DefaultOwnedActivity(Arc<DefaultOwnedActivityInner>);

struct DefaultActivityAdmission {
    event: JsObject,
    inner: Arc<DefaultOwnedActivityInner>,
    consumed: AtomicBool,
}

impl ActivityAdmission for DefaultActivityAdmission {
    fn admit(self: Box<Self>) {
        if !self.consumed.swap(true, Ordering::AcqRel) {
            self.inner.emit_event(&self.event);
        }
    }
}

impl OwnedActivity for DefaultOwnedActivity {
    fn emit(&self, event: &JsObject) {
        self.0.emit_event(event);
    }

    fn drain(&self) {
        self.0.retired.store(true, Ordering::Release);
        self.0
            .emitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    fn stage_event(&self, event: &JsObject) -> Box<dyn ActivityAdmission> {
        Box::new(DefaultActivityAdmission {
            event: event.clone(),
            inner: Arc::clone(&self.0),
            consumed: AtomicBool::new(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_iface::BoxFuture;
    use agentcpm_scheduler_protocol::{
        CallView, ControlKind, ControlOutcome, Destination, Event, Failure, Invocation,
        Result as PResult, State, StateChange, VersionedState, Wait, step_scope::StepRef,
    };
    use pilotdeck_agent::ports::{CancelCause, Cancellation};

    struct TestClient {
        features: Vec<String>,
        scope: Option<StepRef>,
    }

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    impl Client for TestClient {
        fn step_scope(&self) -> Option<StepRef> {
            self.scope.clone()
        }
        fn host_features(&self) -> Vec<String> {
            self.features.clone()
        }
        fn resource(
            &self,
            _: &str,
            _: serde_json::Value,
        ) -> BoxFuture<'_, PResult<serde_json::Value>> {
            unused()
        }
        fn publish(
            &self,
            _: &str,
            _: &str,
            _: serde_json::Value,
            _: State,
            _: Option<Failure>,
            _: Option<Wait>,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<CallView>>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<CallView>>> {
            unused()
        }
        fn get(&self, _: &str) -> BoxFuture<'_, PResult<CallView>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<CallView>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<agentcpm_scheduler_protocol::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    fn all_production_features() -> Vec<String> {
        REQUIRED_PRODUCTION_HOST_FEATURES
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect()
    }

    fn matching_scope(ctx: &CallContext, operation_id: &str) -> StepRef {
        StepRef {
            grant: "grant-1".into(),
            session_ref: ctx.session_ref.clone(),
            call_ref: ctx.call_ref.clone(),
            driver_generation: 1,
            operation_id: operation_id.into(),
            interaction: ctx.interaction.clone(),
        }
    }

    #[tokio::test]
    async fn default_production_adapters_enforces_features_scope_startup_and_observers() {
        let adapters = DefaultProductionAdapters::for_controller(
            "controller".into(),
            "controller-rev-1".into(),
        );
        adapters
            .validate_startup("controller", "controller-rev-1")
            .unwrap();
        assert!(
            adapters
                .validate_startup("other", "controller-rev-1")
                .is_err()
        );
        assert!(adapters.validate_startup("", "controller-rev-1").is_err());

        let ctx = crate::tests::context();
        let iface_input = IfaceTurnInput::new(
            vec![],
            agentcpm_iface::types::ExecutionIdentity::new(ctx.call_ref.clone(), "op-prod-1"),
        );

        // Missing any one of the 6 required host features must reject validate_call.
        for drop_idx in 0..REQUIRED_PRODUCTION_HOST_FEATURES.len() {
            let mut partial = all_production_features();
            partial.remove(drop_idx);
            let client = TestClient {
                features: partial,
                scope: Some(matching_scope(&ctx, "op-prod-1")),
            };
            assert!(adapters.validate_call(&ctx, &iface_input, &client).is_err());
        }

        // Missing step_scope fails closed.
        let no_scope = TestClient {
            features: all_production_features(),
            scope: None,
        };
        assert!(
            adapters
                .validate_call(&ctx, &iface_input, &no_scope)
                .is_err()
        );

        // Valid features + matching step_scope passes and builds notice/observer/tool owners.
        let valid_client: Arc<dyn Client> = Arc::new(TestClient {
            features: all_production_features(),
            scope: Some(matching_scope(&ctx, "op-prod-1")),
        });
        adapters
            .validate_call(&ctx, &iface_input, valid_client.as_ref())
            .unwrap();

        let proofs = Arc::new(ContextProofs::default());
        let notices = Arc::new(EventSettlementLedger::new(
            ctx.clone(),
            "controller".into(),
            "controller-rev-1".into(),
        ));
        let events = Arc::new(EventPublicationLedger::new(
            ctx.clone(),
            "controller".into(),
            "controller-rev-1".into(),
        ));
        adapters
            .retain_observers(&ctx, proofs.clone(), notices, events)
            .unwrap();
        assert!(adapters.retained_proofs(&ctx.call_ref).is_some());

        let owners1 = adapters.source_tool_owners(&ctx, &valid_client).unwrap();
        let owners2 = adapters.source_tool_owners(&ctx, &valid_client).unwrap();
        assert!(Arc::ptr_eq(&owners1, &owners2));

        let foreign_client: Arc<dyn Client> = Arc::new(TestClient {
            features: all_production_features(),
            scope: Some(matching_scope(&ctx, "op-prod-1")),
        });
        assert!(adapters.source_tool_owners(&ctx, &foreign_client).is_err());

        let control = CallControl {
            cancellation: Cancellation::default(),
            deadline: None,
        };
        let turn_input = TurnInput {
            session_id: "sess-1".into(),
            turn_id: "turn-1".into(),
            messages: vec![],
            max_turns: None,
            max_budget_usd: None,
            task_budget_usd: None,
            initial_task_budget_spent_usd: None,
            base_permission_mode: None,
            allow_plan_mode_tools: false,
            can_elicit: false,
            model_override: None,
        };
        let services = adapters
            .notice_services(&ctx, &turn_input, valid_client, &control, proofs)
            .unwrap();
        assert!(services.event_advertised);
        assert!(!services.lifecycle.advertised());
        assert!(
            services
                .admission
                .instructions_loaded(&turn_input, &control)
                .is_ok()
        );
        services.admission.pre_model(&control).unwrap();
        services.admission.before_turn_completed(&control).unwrap();
        services.admission.event_dispatch(&control).unwrap();
        let mut obj = JsObject::new();
        obj.insert("type".into(), "probe".into());
        services.activity.stage_event(&obj).admit();
        services.activity.drain();

        control
            .cancellation
            .cancel(CancelCause::Requested("stop".into()));
        assert!(services.admission.event_dispatch(&control).is_err());
    }
}
