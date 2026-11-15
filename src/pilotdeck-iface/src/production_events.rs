//! Synchronous original AgentEvent publication over the public SDK Controller.
//! Unknown admission/settlement is retained by this call owner; never replayed.
use crate::{
    preparation_effects::ManagedPreparationEffects,
    product::{self, PendingProductEvent, ProductEventSettlement},
    sdk_notice_owner::{ActivityAdmission, NoticeAdmission, OwnedActivity},
};
use agentcpm_scheduler_protocol::{CallContext, Client, Failure};
use pilotdeck_agent::{
    ports::{CallControl, ExecutionIdentity, PortError, PortResult},
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
            || view.session_ref != self.context.session_ref
            || view.parent_session_ref.as_deref() != Some(self.context.session_ref.as_str())
            || view.binding.id != self.controller
            || view.binding.revision != self.controller_revision
            || view.source_binding.as_deref() != Some(self.context.binding.id.as_str())
            || view.cause_execution_ref.as_deref() != Some(self.context.call_ref.as_str())
            || view.invocation.parent_execution_ref.as_deref()
                != Some(self.context.call_ref.as_str())
            || view.invocation.context_ref != product::EVENT_CONTEXT
            || view.invocation.idempotency_key != key
            || view.invocation.correlation_id != self.context.invocation.correlation_id
            || view.invocation.payload.as_str() != Some(expected_json.as_str())
            || view.origin != self.context.interaction
            || view.interaction != self.context.interaction
            || view.security != self.context.security
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
        Ok(Some(view))
    }
    fn set_cause(&self, key: &str, cause: Failure) {
        if let Some(item) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter_mut()
            .find(|item| item.key == key)
        {
            item.cause = Some(cause);
        }
    }
    fn cause(&self, key: &str) -> Option<Failure> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|item| item.key == key)
            .and_then(|item| item.cause.clone())
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
        let original = &unsettled.context;
        if unsettled.controller != controller
            || original.call_ref != context.call_ref
            || original.session_ref != context.session_ref
            || original.binding != context.binding
            || original.invocation != context.invocation
            || original.state != context.state
            || original.origin != context.origin
            || original.interaction != context.interaction
            || original.security != context.security
            || original.attempt != context.attempt
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
        PortError::AcceptedUnknown {
            identity: Box::new(ExecutionIdentity {
                session_id: self.context.session_ref.as_str().into(),
                turn_id: self.context.interaction.turn.to_string().into(),
                run_id: self.context.call_ref.as_str().into(),
                operation_id: self.context.invocation.correlation_id.as_str().into(),
                request_id: self.context.call_ref.as_str().into(),
                idempotency_key: Some(key.into()),
                module_instance_id: self.context.binding.id.as_str().into(),
                connection_generation: self.context.interaction.generation.to_string().into(),
            }),
            last_applied_sequence: None,
            cause: self.unsettled.cause(key).map(|failure| {
                Box::new(pilotdeck_protocol::model::HostDiagnosticCause {
                    code: failure.code,
                    message_key: failure.message_key,
                    params: failure.params,
                })
            }),
        }
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
        let mut current = self.unsettled.next.load(Ordering::Acquire);
        let serial = loop {
            let next = current
                .checked_add(1)
                .ok_or_else(|| invalid("event sequence exhausted"))?;
            match self.unsettled.next.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break next,
                Err(observed) => current = observed,
            }
        };
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
            // Admission is not proof of rejection; retain the pre-registered key.
            Ok(Err(failure)) => {
                self.unsettled.set_cause(&key, failure);
                return Err(self.unknown(&key));
            }
            Err(()) => return Err(self.unknown(&key)),
        };
        let outcome = self.runtime.block_on(controlled(
            pending.settlement(self.client.as_ref()),
            &self.control,
        ));
        match outcome {
            Ok(Ok(ProductEventSettlement::Completed)) => {
                let entry = {
                    let mut entries = self
                        .unsettled
                        .entries
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
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
                Ok(())
            }
            Ok(Ok(ProductEventSettlement::Failed(failure))) => {
                if let Some(failure) = failure {
                    self.unsettled.set_cause(&key, failure);
                }
                if let Some(item) = self
                    .unsettled
                    .entries
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter_mut()
                    .find(|item| item.key == key)
                {
                    item.pending = Some(pending);
                }
                Err(self.unknown(&key))
            }
            Ok(Ok(ProductEventSettlement::Cancelled)) => {
                self.unsettled.set_cause(
                    &key,
                    Failure::external(
                        "CANCELLED",
                        "event publication cancelled before confirmed delivery",
                    ),
                );
                if let Some(item) = self
                    .unsettled
                    .entries
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter_mut()
                    .find(|item| item.key == key)
                {
                    item.pending = Some(pending);
                }
                Err(self.unknown(&key))
            }
            other => {
                if let Ok(Err(ref failure)) = other {
                    self.unsettled.set_cause(&key, failure.clone());
                }
                if let Some(item) = self
                    .unsettled
                    .entries
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter_mut()
                    .find(|item| item.key == key)
                {
                    item.pending = Some(pending);
                }
                Err(self.unknown(&key))
            }
        }
    }
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
fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: message.into(),
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
}
