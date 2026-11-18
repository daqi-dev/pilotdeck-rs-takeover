//! Synchronous original AgentEvent publication over the public SDK Controller.
//! Unknown admission/settlement is retained by this call owner; never replayed.
use crate::port_helpers::{CallViewScope, invalid};
use crate::{
    preparation_effects::ManagedPreparationEffects,
    product::{self, PendingProductEvent, ProductEventSettlement},
    sdk_notice_owner::{ActivityAdmission, NoticeAdmission, OwnedActivity},
};
use agentcpm_scheduler_protocol::{CallContext, Client, Failure};
#[cfg(test)]
use pilotdeck_agent::ports::CancelCause;
use pilotdeck_agent::{
    ports::{CallControl, PortError, PortResult},
    turn::TurnEventPort,
};
use pilotdeck_protocol::{JsValue, model::BoundarySnapshot, stringify_json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::runtime::Handle;

struct Unsettled {
    key: String,
    pending: Option<PendingProductEvent>,
    expected_json: String,
    activity: Option<Box<dyn ActivityAdmission>>,
    cause: Option<Failure>,
}
/// Observation references only: Kernel remains the durable event owner.
pub struct EventPublicationLedger {
    context: CallContext,
    controller: String,
    controller_revision: String,
    entries: Mutex<Vec<Unsettled>>,
    next: AtomicU64,
    active: AtomicBool,
}
impl crate::port_helpers::CallViewScope for EventPublicationLedger {
    fn call_view_scope(&self) -> (&CallContext, &str, &str) {
        (&self.context, &self.controller, &self.controller_revision)
    }
}
impl EventPublicationLedger {
    pub fn new(context: CallContext, controller: String, controller_revision: String) -> Self {
        Self {
            context,
            controller,
            controller_revision,
            entries: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
            active: AtomicBool::new(false),
        }
    }
    /// Only observes the existing call; no result or absence authorizes replay.
    pub async fn observe(
        &self,
        client: &dyn Client,
        key: &str,
    ) -> agentcpm_scheduler_protocol::Result<Option<agentcpm_scheduler_protocol::CallView>> {
        let (expected_json, known_call) = {
            let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            let item = entries
                .iter()
                .find(|entry| entry.key == key)
                .ok_or_else(|| {
                    Failure::external("EVENT_NOT_FOUND", "event identity not retained")
                })?;
            (
                item.expected_json.clone(),
                item.pending.as_ref().map(|call| call.call_ref().to_owned()),
            )
        };
        let Some(view) = client.find(key, &self.controller).await? else {
            return Ok(None);
        };
        if known_call.as_ref().is_some_and(|known| known != &view.id)
            || !self.matches_call_view(&view, key)
            || view.invocation.context_ref != product::EVENT_CONTEXT
            || view.invocation.payload.as_str() != Some(expected_json.as_str())
        {
            return Err(Failure::external(
                "EVENT_IDENTITY_MISMATCH",
                "retained event does not match Kernel call",
            ));
        }
        Ok(Some(view))
    }
    /// Only confirmed completion proves delivery. Failed/Cancelled may follow
    /// a published event and remain unknown without a delivery receipt.
    pub async fn reconcile(
        &self,
        client: &dyn Client,
        key: &str,
    ) -> agentcpm_scheduler_protocol::Result<Option<agentcpm_scheduler_protocol::CallView>> {
        let Some(view) = self.observe(client, key).await? else {
            return Ok(None);
        };
        if view.state == agentcpm_scheduler_protocol::State::Completed {
            self.admit_entry(key);
        }
        Ok(Some(view))
    }
    fn set_cause(&self, key: &str, cause: Failure) {
        self.mutate_entry(key, |entry| entry.cause = Some(cause));
    }
    /// Mutate the retained entry for `key`, if present.
    fn mutate_entry(&self, key: &str, f: impl FnOnce(&mut Unsettled)) {
        if let Some(entry) = self
            .locked_entries()
            .iter_mut()
            .find(|entry| entry.key == key)
        {
            f(entry);
        }
    }
    /// Lock the retained entry table.
    fn locked_entries(&self) -> std::sync::MutexGuard<'_, Vec<Unsettled>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn cause(&self, key: &str) -> Option<Failure> {
        self.locked_entries()
            .iter()
            .find(|item| item.key == key)
            .and_then(|item| item.cause.clone())
    }
    /// Remove the retained entry for `key`, admitting any staged activity.
    fn admit_entry(&self, key: &str) {
        let entry = {
            let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            entries
                .iter()
                .position(|item| item.key == key)
                .map(|i| entries.remove(i))
        };
        if let Some(mut entry) = entry
            && let Some(activity) = entry.activity.take()
        {
            activity.admit();
        }
    }
    /// Record the pending call handle for `key` if the entry is still retained.
    fn set_pending(&self, key: &str, pending: PendingProductEvent) {
        self.mutate_entry(key, |entry| entry.pending = Some(pending));
    }
    pub fn unresolved(&self) -> Vec<(String, Option<String>)> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|entry| {
                (
                    entry.key.clone(),
                    entry
                        .pending
                        .as_ref()
                        .map(|call| call.call_ref().to_owned()),
                )
            })
            .collect()
    }
}
pub struct ProductionTurnEvents {
    context: CallContext,
    controller: String,
    client: Arc<dyn Client>,
    control: CallControl,
    runtime: Handle,
    admission: Arc<dyn NoticeAdmission>,
    activity: Arc<dyn OwnedActivity>,
    effects: Arc<Mutex<ManagedPreparationEffects>>,
    unsettled: Arc<EventPublicationLedger>,
}
impl ProductionTurnEvents {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: CallContext,
        controller: String,
        client: Arc<dyn Client>,
        control: CallControl,
        admission: Arc<dyn NoticeAdmission>,
        activity: Arc<dyn OwnedActivity>,
        effects: Arc<Mutex<ManagedPreparationEffects>>,
        unsettled: Arc<EventPublicationLedger>,
    ) -> PortResult<Self> {
        let runtime = Handle::try_current().map_err(|_| invalid("SDK runtime is unavailable"))?;
        if unsettled.controller != controller
            || !crate::port_helpers::same_call_scope(&unsettled.context, &context)
        {
            return Err(invalid("event publication ledger scope mismatch"));
        }
        if unsettled
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(invalid("an event publisher already owns this call"));
        }
        Ok(Self {
            context,
            controller,
            client,
            control,
            runtime,
            admission,
            activity,
            effects,
            unsettled,
        })
    }
    fn unknown(&self, key: &str) -> PortError {
        let mut error = crate::port_helpers::accepted_unknown(&self.context, key);
        if let PortError::AcceptedUnknown { cause, .. } = &mut error {
            *cause = self.unsettled.cause(key).map(|failure| {
                Box::new(pilotdeck_protocol::model::HostDiagnosticCause {
                    code: failure.code,
                    message_key: failure.message_key,
                    params: failure.params,
                })
            });
        }
        error
    }
    /// Retain the pending handle as unresolved and surface the unknown outcome.
    fn unknown_with_pending(&self, key: &str, pending: PendingProductEvent) -> PortError {
        self.unsettled.set_pending(key, pending);
        self.unknown(key)
    }
    fn remove_unsettled(&self, key: &str) {
        self.unsettled
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|item| item.key != key);
    }
    fn soft_cancelled(&self, wait_for_interrupt: bool) -> Option<PortError> {
        soft_cancelled_port_error(&self.control, &self.runtime, wait_for_interrupt)
    }
    /// When the outcome was an explicit soft cancellation, drop the retained key
    /// and surface it.
    fn soft_cancelled_for(&self, key: &str, wait_for_interrupt: bool) -> Option<PortError> {
        let cancelled = self.soft_cancelled(wait_for_interrupt)?;
        self.remove_unsettled(key);
        Some(cancelled)
    }
    fn publish(&mut self, boundary: BoundarySnapshot) -> PortResult<()> {
        if let Some((key, _)) = self.unsettled.unresolved().first() {
            return Err(self.unknown(key));
        }
        self.control.check()?;
        let object = boundary
            .value()
            .as_object()
            .ok_or_else(|| invalid("AgentEvent must be an object"))?;
        let kind = object
            .get("type")
            .and_then(JsValue::as_string)
            .ok_or_else(|| invalid("AgentEvent type is missing"))?;
        if kind.equals_str("turn_completed") {
            self.admission.before_turn_completed(&self.control)?;
            use pilotdeck_turn_preparation::PreparationEffects;
            self.effects
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .finish()?;
        }
        self.admission.event_dispatch(&self.control)?;
        let serial =
            crate::port_helpers::next_sequence(&self.unsettled.next, "event sequence exhausted")?;
        let key = format!("{}:event:{}", self.context.call_ref, serial);
        let encoded = stringify_json(boundary.value())
            .ok()
            .flatten()
            .ok_or_else(|| invalid("AgentEvent cannot be serialized"))?;
        self.control.check()?;
        let activity = self.activity.stage_event(object);
        // Register before awaiting invoke: a dropped/cancelled future is not
        // evidence that the Kernel did not accept the event.
        self.unsettled
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Unsettled {
                key: key.clone(),
                pending: None,
                expected_json: encoded.clone(),
                activity: Some(activity),
                cause: None,
            });
        let admission = self.runtime.block_on(controlled(
            product::admit_event(
                self.client.as_ref(),
                &self.context,
                &self.controller,
                &key,
                encoded,
            ),
            &self.control,
        ));
        let pending = match admission {
            Ok(Ok(pending)) => pending,
            // Admission is not proof of rejection; retain the pre-registered key
            // unless this invocation was explicitly soft-cancelled without ambiguity.
            Ok(Err(failure)) => {
                if is_soft_cancel_settlement_code(failure.code.as_str())
                    && let Some(cancelled) = self.soft_cancelled_for(&key, true)
                {
                    return Err(cancelled);
                }
                self.unsettled.set_cause(&key, failure);
                return Err(self.unknown(&key));
            }
            Err(()) => {
                if let Some(cancelled) = self.soft_cancelled(false) {
                    self.remove_unsettled(&key);
                    return Err(cancelled);
                }
                return Err(self.unknown(&key));
            }
        };
        let outcome = self.runtime.block_on(controlled(
            pending.settlement(self.client.as_ref()),
            &self.control,
        ));
        match outcome {
            Ok(Ok(ProductEventSettlement::Completed)) => {
                self.unsettled.admit_entry(&key);
                Ok(())
            }
            Ok(Ok(ProductEventSettlement::Failed(failure))) => {
                if failure
                    .as_ref()
                    .is_some_and(|item| is_soft_cancel_settlement_code(item.code.as_str()))
                    && let Some(cancelled) = self.soft_cancelled_for(&key, true)
                {
                    return Err(cancelled);
                }
                if let Some(failure) = failure {
                    self.unsettled.set_cause(&key, failure);
                }
                Err(self.unknown_with_pending(&key, pending))
            }
            Ok(Ok(ProductEventSettlement::Cancelled)) => {
                if let Some(cancelled) = self.soft_cancelled_for(&key, true) {
                    return Err(cancelled);
                }
                self.unsettled.set_cause(
                    &key,
                    Failure::external(
                        "CANCELLED",
                        "event publication cancelled before confirmed delivery",
                    ),
                );
                Err(self.unknown_with_pending(&key, pending))
            }
            Err(()) => {
                if let Some(cancelled) = self.soft_cancelled_for(&key, false) {
                    return Err(cancelled);
                }
                Err(self.unknown_with_pending(&key, pending))
            }
            other => {
                if let Ok(Err(ref failure)) = other {
                    if is_soft_cancel_settlement_code(failure.code.as_str())
                        && let Some(cancelled) = self.soft_cancelled_for(&key, true)
                    {
                        return Err(cancelled);
                    }
                    self.unsettled.set_cause(&key, failure.clone());
                }
                Err(self.unknown_with_pending(&key, pending))
            }
        }
    }
}
fn is_soft_cancel_settlement_code(code: &str) -> bool {
    matches!(code, "OUTPUT_CONSUMER_ENDED" | "CALLER_ENDED" | "CANCELLED")
}
fn soft_cancelled_port_error(
    control: &CallControl,
    runtime: &Handle,
    wait_for_interrupt: bool,
) -> Option<PortError> {
    crate::port_helpers::wait_for_soft_cancel(
        runtime,
        || crate::port_helpers::soft_cancel_check(control),
        wait_for_interrupt,
    )
}
impl Drop for ProductionTurnEvents {
    fn drop(&mut self) {
        self.unsettled.active.store(false, Ordering::Release);
    }
}
impl TurnEventPort for ProductionTurnEvents {
    fn drain_owned_activity(&mut self) {
        self.activity.drain();
    }
    fn emit(&mut self, boundary: BoundarySnapshot) -> PortResult<()> {
        self.publish(boundary)
    }
}
async fn controlled<F: std::future::Future>(
    future: F,
    control: &CallControl,
) -> Result<F::Output, ()> {
    tokio::pin!(future);
    loop {
        if control.check().is_err() {
            return Err(());
        }
        tokio::select! { biased; result = &mut future => return Ok(result), _ = tokio::time::sleep(Duration::from_millis(10)) => { if control.check().is_err() { return Err(()); } } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preparation_effects::{NoticeOwner, PendingNotice};
    use pilotdeck_agent::turn::TurnInput;
    use pilotdeck_protocol::JsObject;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Admission {
        denied: bool,
    }
    impl NoticeAdmission for Admission {
        fn instructions_loaded(&self, _: &TurnInput, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn pre_model(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn before_turn_completed(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn event_dispatch(&self, _: &CallControl) -> PortResult<()> {
            if self.denied {
                return Err(invalid("denied before dispatch"));
            }
            Ok(())
        }
    }
    struct Activity(Arc<AtomicUsize>);
    struct ActivityAttempt;
    impl crate::sdk_notice_owner::ActivityAdmission for ActivityAttempt {
        fn admit(self: Box<Self>) {}
    }
    impl OwnedActivity for Activity {
        fn emit(&self, _: &JsObject) {}
        fn drain(&self) {}
        fn stage_event(&self, _: &JsObject) -> Box<dyn ActivityAdmission> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::new(ActivityAttempt)
        }
    }
    struct NoticeFixture;
    impl NoticeOwner for NoticeFixture {
        fn authorize_instructions_loaded(&self, _: &TurnInput, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn authorize_pre_model(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn lifecycle_advertised(&self) -> bool {
            false
        }
        fn event_advertised(&self) -> bool {
            false
        }
        fn emit_owned_activity(&self, _: &JsObject) {}
        fn start_lifecycle(
            &self,
            _: &'static str,
            _: JsObject,
            _: &CallControl,
        ) -> PortResult<Box<dyn PendingNotice>> {
            Err(invalid("unexpected"))
        }
        fn enqueue_event(&self, _: JsObject, _: &CallControl) -> PortResult<()> {
            Err(invalid("unexpected"))
        }
        fn finish_events(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn retire(&self) {}
    }
    fn event() -> BoundarySnapshot {
        BoundarySnapshot::capture(
            &pilotdeck_protocol::parse_json(r#"{"type":"model_delta","message":"x"}"#).unwrap(),
        )
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn admission_denial_precedes_staging_and_dispatch() {
        tokio::task::spawn_blocking(|| {
            let context = crate::tests::context();
            let control = CallControl {
                cancellation: Default::default(),
                deadline: None,
            };
            let activity = Arc::new(AtomicUsize::new(0));
            let ledger = Arc::new(EventPublicationLedger::new(
                context.clone(),
                "controller".into(),
                "rev".into(),
            ));
            let effects = Arc::new(Mutex::new(ManagedPreparationEffects::new(
                Arc::new(NoticeFixture),
                control.clone(),
            )));
            let mut sink = ProductionTurnEvents::new(
                context,
                "controller".into(),
                crate::tests::client(None),
                control,
                Arc::new(Admission { denied: true }),
                Arc::new(Activity(activity.clone())),
                effects,
                ledger.clone(),
            )
            .unwrap();
            assert!(matches!(
                sink.emit(event()),
                Err(PortError::InvalidResponse { .. })
            ));
            assert_eq!(activity.load(Ordering::SeqCst), 0);
            assert!(ledger.unresolved().is_empty());
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn ambiguous_invoke_keeps_identity_and_prevents_replay() {
        tokio::task::spawn_blocking(|| {
            let context = crate::tests::context();
            let control = CallControl {
                cancellation: Default::default(),
                deadline: None,
            };
            let cancellation = control.cancellation.clone();
            let activity = Arc::new(AtomicUsize::new(0));
            let ledger = Arc::new(EventPublicationLedger::new(
                context.clone(),
                "controller".into(),
                "rev".into(),
            ));
            let effects = Arc::new(Mutex::new(ManagedPreparationEffects::new(
                Arc::new(NoticeFixture),
                control.clone(),
            )));
            let mut sink = ProductionTurnEvents::new(
                context.clone(),
                "controller".into(),
                crate::tests::client(None),
                control.clone(),
                Arc::new(Admission { denied: false }),
                Arc::new(Activity(activity.clone())),
                effects.clone(),
                ledger.clone(),
            )
            .unwrap();
            assert!(matches!(
                sink.emit(event()),
                Err(PortError::AcceptedUnknown { .. })
            ));
            assert!(
                ProductionTurnEvents::new(
                    context.clone(),
                    "controller".into(),
                    crate::tests::client(None),
                    control.clone(),
                    Arc::new(Admission { denied: false }),
                    Arc::new(Activity(activity.clone())),
                    effects.clone(),
                    ledger.clone(),
                )
                .is_err()
            );
            cancellation.cancel(pilotdeck_agent::ports::CancelCause::Requested(
                "stop".into(),
            ));
            assert!(matches!(
                sink.emit(event()),
                Err(PortError::AcceptedUnknown { .. })
            ));
            assert_eq!(activity.load(Ordering::SeqCst), 1);
            assert_eq!(ledger.unresolved().len(), 1);
            drop(sink);
            let mut replacement = ProductionTurnEvents::new(
                context,
                "controller".into(),
                crate::tests::client(None),
                control,
                Arc::new(Admission { denied: false }),
                Arc::new(Activity(activity.clone())),
                effects,
                ledger.clone(),
            )
            .unwrap();
            assert!(matches!(
                replacement.emit(event()),
                Err(PortError::AcceptedUnknown { .. })
            ));
            assert_eq!(activity.load(Ordering::SeqCst), 1);
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn changed_state_cannot_acquire_event_publisher() {
        tokio::task::spawn_blocking(|| {
            let context = crate::tests::context();
            let mut other = context.clone();
            other.state.revision += 1;
            let control = CallControl {
                cancellation: Default::default(),
                deadline: None,
            };
            let ledger = Arc::new(EventPublicationLedger::new(
                context.clone(),
                "controller".into(),
                "rev".into(),
            ));
            let effects = Arc::new(Mutex::new(ManagedPreparationEffects::new(
                Arc::new(NoticeFixture),
                control.clone(),
            )));
            let count = Arc::new(AtomicUsize::new(0));
            assert!(
                ProductionTurnEvents::new(
                    other,
                    "controller".into(),
                    crate::tests::client(None),
                    control.clone(),
                    Arc::new(Admission { denied: false }),
                    Arc::new(Activity(count.clone())),
                    effects.clone(),
                    ledger.clone()
                )
                .is_err()
            );
            assert!(
                ProductionTurnEvents::new(
                    context,
                    "controller".into(),
                    crate::tests::client(None),
                    control,
                    Arc::new(Admission { denied: false }),
                    Arc::new(Activity(count)),
                    effects,
                    ledger
                )
                .is_ok()
            );
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn soft_cancelled_inflight_publication_returns_cancelled_without_poisoning_ledger() {
        use agentcpm_iface::BoxFuture;
        use agentcpm_scheduler_protocol as p;
        use serde_json::Value;
        use std::sync::atomic::AtomicBool;

        struct OutputCancelledClient {
            context: CallContext,
            cancellation: pilotdeck_agent::ports::Cancellation,
            invoked: Arc<AtomicBool>,
        }
        impl Client for OutputCancelledClient {
            fn resource(&self, _: &str, _: Value) -> BoxFuture<'_, p::Result<Value>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn publish(
                &self,
                _: &str,
                _: &str,
                _: Value,
                _: p::State,
                _: Option<Failure>,
                _: Option<p::Wait>,
            ) -> BoxFuture<'_, p::Result<()>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn deliver(
                &self,
                _: &str,
                _: p::Destination,
                _: p::Event,
            ) -> BoxFuture<'_, p::Result<()>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn invoke(&self, _: p::Invocation) -> BoxFuture<'_, p::Result<String>> {
                self.invoked.store(true, Ordering::SeqCst);
                Box::pin(async { Ok("event-call-1".into()) })
            }
            fn find(&self, _: &str, _: &str) -> BoxFuture<'_, p::Result<Option<p::CallView>>> {
                Box::pin(async { Ok(None) })
            }
            fn children(&self, _: &str) -> BoxFuture<'_, p::Result<Vec<p::CallView>>> {
                Box::pin(async { Ok(vec![]) })
            }
            fn get(&self, _: &str) -> BoxFuture<'_, p::Result<p::CallView>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn wait(&self, id: &str) -> BoxFuture<'_, p::Result<p::CallView>> {
                let id = id.to_owned();
                let context = self.context.clone();
                let cancellation = self.cancellation.clone();
                Box::pin(async move {
                    cancellation.cancel(CancelCause::Requested("e2e_cancel".into()));
                    let view = p::CallView {
                        id,
                        session_ref: context.session_ref,
                        source_binding: None,
                        parent_session_ref: None,
                        cause_execution_ref: None,
                        binding: p::Binding {
                            id: "controller".into(),
                            revision: "rev".into(),
                            kind: p::Kind::Service,
                            effect: p::Effect::State,
                            permissions: p::Permissions::calls([]),
                        },
                        invocation: p::Invocation::new(
                            "controller",
                            product::EVENT_CONTEXT,
                            "k",
                            "c",
                            Value::Null,
                        ),
                        state: p::State::Failed,
                        result: None,
                        presentation: None,
                        settlement_data: None,
                        settlement_error: None,
                        error: Some(Failure::new(
                            "OUTPUT_CONSUMER_ENDED",
                            "output consumer disconnected",
                        )),
                        waiting: None,
                        cursor: 0,
                        origin: context.origin,
                        interaction: context.interaction,
                        interactions: vec![],
                        cancel_requested: false,
                        stepped: false,
                        security: context.security,
                    };
                    Ok(view)
                })
            }
            fn control(
                &self,
                _: &str,
                _: p::ControlKind,
                _: String,
            ) -> BoxFuture<'_, p::Result<p::ControlOutcome>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn respond(
                &self,
                _: &str,
                _: &str,
                _: Vec<p::Message>,
                _: &str,
            ) -> BoxFuture<'_, p::Result<()>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn read_state(&self, _: &str) -> BoxFuture<'_, p::Result<p::VersionedState>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn write_state(&self, _: p::StateChange) -> BoxFuture<'_, p::Result<u64>> {
                Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
            }
            fn events(&self, _: u64) -> BoxFuture<'_, p::Result<Vec<p::Event>>> {
                Box::pin(async { Ok(vec![]) })
            }
            fn wait_events(&self, _: u64) -> BoxFuture<'_, p::Result<Vec<p::Event>>> {
                Box::pin(async { Ok(vec![]) })
            }
        }

        tokio::task::spawn_blocking(|| {
            let context = crate::tests::context();
            let control = CallControl {
                cancellation: Default::default(),
                deadline: None,
            };
            let activity = Arc::new(AtomicUsize::new(0));
            let ledger = Arc::new(EventPublicationLedger::new(
                context.clone(),
                "controller".into(),
                "rev".into(),
            ));
            let effects = Arc::new(Mutex::new(ManagedPreparationEffects::new(
                Arc::new(NoticeFixture),
                control.clone(),
            )));
            let invoked = Arc::new(AtomicBool::new(false));
            let client = Arc::new(OutputCancelledClient {
                context: context.clone(),
                cancellation: control.cancellation.clone(),
                invoked: invoked.clone(),
            });
            let mut sink = ProductionTurnEvents::new(
                context,
                "controller".into(),
                client,
                control,
                Arc::new(Admission { denied: false }),
                Arc::new(Activity(activity.clone())),
                effects,
                ledger.clone(),
            )
            .unwrap();
            let res = sink.emit(event());
            assert!(
                matches!(
                    res,
                    Err(PortError::Cancelled {
                        cause: CancelCause::Requested(_)
                    })
                ),
                "expected Cancelled, got {res:?}"
            );
            assert!(invoked.load(Ordering::SeqCst));
            assert_eq!(activity.load(Ordering::SeqCst), 1);
            assert!(ledger.unresolved().is_empty());
        })
        .await
        .unwrap();
    }
}
