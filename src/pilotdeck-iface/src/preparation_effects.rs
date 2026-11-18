//! Context preparation notifications, awaiting a real Scheduler lifecycle owner.
//!
//! This adapter constructs the original product payloads and preserves their
//! admission/settlement order. It is intentionally not installed until the
//! host supplies an invocation-bound owner for both hooks and product events.
use pilotdeck_agent::{
    ports::{CallControl, CancelCause, Cancellation, PortError, PortResult, Route},
    turn::TurnInput,
};
use pilotdeck_protocol::{JsObject, JsValue};
use pilotdeck_turn_preparation::PreparationEffects;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// One already-admitted hook operation; polling must never redispatch it.
/// Any wait interrupted while the outcome is not known must return
/// AcceptedUnknown with its original identity, not a generic cancellation.
pub trait PendingNotice: Send {
    fn is_ready(&self) -> bool;
    fn wait(self: Box<Self>, control: &CallControl) -> PortResult<()>;
}
/// Host-owned Scheduler adapter. Implementations must use real registered
/// lifecycle capabilities and ProductController event publication, preserve
/// accepted-unknown identities, and own admitted work after caller drop.
/// `enqueue_event` preserves order, bounded capacity, and the original first
/// event admission fence without waiting for its completion. `finish_events`
/// settles that same queue with the original fixed terminal deadline.
pub trait NoticeOwner: Send + Sync {
    /// Host verifies original operation, prepare ACK, scope and cleanup grant.
    fn authorize_instructions_loaded(
        &self,
        input: &TurnInput,
        control: &CallControl,
    ) -> PortResult<()>;
    /// Host verifies ordinary operation admission; no cleanup exception.
    fn authorize_pre_model(&self, control: &CallControl) -> PortResult<()>;
    fn lifecycle_advertised(&self) -> bool;
    fn event_advertised(&self) -> bool;
    /// Original owned activity channel is independent of event.emit support.
    fn emit_owned_activity(&self, event: &JsObject);
    fn start_lifecycle(
        &self,
        event: &'static str,
        detail: JsObject,
        control: &CallControl,
    ) -> PortResult<Box<dyn PendingNotice>>;
    fn enqueue_event(&self, event: JsObject, control: &CallControl) -> PortResult<()>;
    fn finish_events(&self, control: &CallControl) -> PortResult<()>;
    /// Stop new admission, retire the owner and retain admitted operations for
    /// bounded settlement/reconciliation. Must not replay effects.
    fn retire(&self);
}

pub struct ManagedPreparationEffects {
    owner: Arc<dyn NoticeOwner>,
    control: CallControl,
    pending: Vec<Box<dyn PendingNotice>>,
    terminal_deadline: Option<Instant>,
    terminal_error: Option<PortError>,
}
impl ManagedPreparationEffects {
    /// Retain terminal ownership errors and surface the failure.
    fn reject<T>(&mut self, error: PortError) -> PortResult<T> {
        if ownership_error(&error) {
            self.terminal_error = Some(error.clone());
        }
        Err(error)
    }
    pub fn new(owner: Arc<dyn NoticeOwner>, control: CallControl) -> Self {
        Self {
            owner,
            control,
            pending: Vec::new(),
            terminal_deadline: None,
            terminal_error: None,
        }
    }
    fn lifecycle(
        &mut self,
        event: &'static str,
        detail: JsObject,
        control: &CallControl,
    ) -> PortResult<()> {
        if let Some(error) = &self.terminal_error {
            return Err(error.clone());
        }
        let mut waiting = Vec::new();
        let mut calls = std::mem::take(&mut self.pending).into_iter();
        while let Some(call) = calls.next() {
            if call.is_ready() {
                if let Err(error) = call.wait(&self.control)
                    && ownership_error(&error)
                {
                    waiting.extend(calls);
                    self.pending = waiting;
                    self.terminal_error = Some(error.clone());
                    return Err(error);
                }
            } else {
                waiting.push(call);
            }
        }
        self.pending = waiting;
        if self.owner.lifecycle_advertised() {
            if self.pending.len() >= 64 {
                return Err(PortError::InvalidResponse {
                    message: "Advisory lifecycle pending-call capacity exhausted.".into(),
                });
            }
            match self.owner.start_lifecycle(event, detail, control) {
                Ok(call) => self.pending.push(call),
                Err(error) if ownership_error(&error) => {
                    self.terminal_error = Some(error.clone());
                    return Err(error);
                }
                Err(_) => {}
            }
        }
        Ok(())
    }
}
impl PreparationEffects for ManagedPreparationEffects {
    fn notice(&mut self, input: &TurnInput, has_system_prompt: bool) -> PortResult<()> {
        if let Some(error) = &self.terminal_error {
            return Err(error.clone());
        }
        if let Some(cause @ (CancelCause::OwnerRetired | CancelCause::TransportClosed)) =
            self.control.cancellation.cause()
        {
            return Err(PortError::Cancelled { cause });
        }
        // The owner must grant this bounded cleanup admission after confirmed
        // prepare; a fresh token here is not itself an authorization grant.
        let accepted = CallControl {
            cancellation: Cancellation::default(),
            deadline: Some(Instant::now() + Duration::from_secs(5)),
        };
        if let Err(error) = self.owner.authorize_instructions_loaded(input, &accepted) {
            return self.reject(error);
        }
        let mut detail = JsObject::new();
        detail.insert("hasSystemPrompt".into(), JsValue::Bool(has_system_prompt));
        self.lifecycle("InstructionsLoaded", detail, &accepted)?;
        let mut event = JsObject::new();
        event.insert("type".into(), "instructions_loaded".into());
        event.insert(
            "sessionId".into(),
            JsValue::String(input.session_id.clone()),
        );
        event.insert("turnId".into(), JsValue::String(input.turn_id.clone()));
        event.insert("hasSystemPrompt".into(), JsValue::Bool(has_system_prompt));
        self.owner.emit_owned_activity(&event);
        if self.owner.event_advertised()
            && let Err(error) = self.owner.enqueue_event(event, &accepted)
        {
            return self.reject(error);
        }
        Ok(())
    }
    fn before_model_request(&mut self, route: &Route) -> PortResult<()> {
        if let Some(error) = &self.terminal_error {
            return Err(error.clone());
        }
        self.control.check()?;
        if let Err(error) = self.owner.authorize_pre_model(&self.control) {
            return self.reject(error);
        }
        let mut detail = JsObject::new();
        detail.insert("provider".into(), JsValue::String(route.provider.clone()));
        detail.insert("model".into(), JsValue::String(route.model.clone()));
        self.lifecycle("PreModelRequest", detail, &self.control.clone())
    }
    fn finish(&mut self) -> PortResult<()> {
        let prior = self.terminal_error.clone();
        if prior.is_some() {
            self.owner.retire();
        }
        let deadline = *self.terminal_deadline.get_or_insert_with(|| {
            let bound = Instant::now() + Duration::from_secs(5);
            self.control
                .deadline
                .map_or(bound, |prior| prior.min(bound))
        });
        let control = CallControl {
            cancellation: self.control.cancellation.clone(),
            deadline: Some(deadline),
        };
        let mut first = prior;
        if let Err(error) = self.owner.finish_events(&control)
            && first.is_none()
        {
            first = Some(error);
        }
        let mut calls = std::mem::take(&mut self.pending).into_iter();
        while let Some(call) = calls.next() {
            if let Err(error) = call.wait(&control)
                && ownership_error(&error)
            {
                self.pending.extend(calls);
                if first.is_none() {
                    first = Some(error);
                }
                break;
            }
        }
        self.terminal_error = first.clone();
        first.map_or(Ok(()), Err)
    }
}
fn ownership_error(error: &PortError) -> bool {
    matches!(
        error,
        PortError::AcceptedUnknown { .. }
            | PortError::TransportBeforeDispatch { .. }
            | PortError::Cancelled {
                cause: CancelCause::OwnerRetired | CancelCause::TransportClosed
            }
    )
}

impl Drop for ManagedPreparationEffects {
    fn drop(&mut self) {
        self.owner.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_agent::ports::ExecutionIdentity;
    use std::sync::Mutex;
    type Log = Arc<Mutex<Vec<String>>>;
    struct Pending {
        log: Log,
        label: &'static str,
        failure: Option<PortError>,
    }
    impl PendingNotice for Pending {
        fn is_ready(&self) -> bool {
            false
        }
        fn wait(self: Box<Self>, _: &CallControl) -> PortResult<()> {
            self.log
                .lock()
                .unwrap()
                .push(format!("settle:{}", self.label));
            self.failure.map_or(Ok(()), Err)
        }
    }
    struct Owner {
        log: Log,
        event: bool,
        hook_failure: Option<PortError>,
    }
    impl NoticeOwner for Owner {
        fn authorize_instructions_loaded(&self, _: &TurnInput, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn authorize_pre_model(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn lifecycle_advertised(&self) -> bool {
            true
        }
        fn event_advertised(&self) -> bool {
            self.event
        }
        fn emit_owned_activity(&self, event: &JsObject) {
            assert_eq!(
                event.get("type"),
                Some(&JsValue::from("instructions_loaded"))
            );
            self.log.lock().unwrap().push("activity".into());
        }
        fn start_lifecycle(
            &self,
            event: &'static str,
            _: JsObject,
            _: &CallControl,
        ) -> PortResult<Box<dyn PendingNotice>> {
            self.log.lock().unwrap().push(format!("start:{event}"));
            Ok(Box::new(Pending {
                log: self.log.clone(),
                label: event,
                failure: self.hook_failure.clone(),
            }))
        }
        fn enqueue_event(&self, _: JsObject, _: &CallControl) -> PortResult<()> {
            self.log.lock().unwrap().push("event".into());
            Ok(())
        }
        fn finish_events(&self, _: &CallControl) -> PortResult<()> {
            self.log.lock().unwrap().push("finish-events".into());
            Ok(())
        }
        fn retire(&self) {
            self.log.lock().unwrap().push("retire".into());
        }
    }
    fn input() -> TurnInput {
        TurnInput {
            session_id: "session".into(),
            turn_id: "turn".into(),
            messages: vec![],
            max_turns: None,
            max_budget_usd: None,
            task_budget_usd: None,
            initial_task_budget_spent_usd: None,
            base_permission_mode: None,
            allow_plan_mode_tools: false,
            can_elicit: false,
            model_override: None,
        }
    }
    fn control() -> CallControl {
        CallControl {
            cancellation: Cancellation::default(),
            deadline: None,
        }
    }
    fn unknown() -> PortError {
        PortError::AcceptedUnknown {
            identity: Box::new(ExecutionIdentity {
                session_id: "session".into(),
                turn_id: "turn".into(),
                run_id: "run".into(),
                operation_id: "operation".into(),
                request_id: "request".into(),
                idempotency_key: None,
                module_instance_id: "binding".into(),
                connection_generation: "generation".into(),
            }),
            last_applied_sequence: None,
            cause: None,
        }
    }
    #[test]
    fn activity_is_independent_and_events_settle_before_hooks() {
        let log = Log::default();
        let owner = Arc::new(Owner {
            log: log.clone(),
            event: false,
            hook_failure: None,
        });
        let mut effects = ManagedPreparationEffects::new(owner, control());
        effects.notice(&input(), true).unwrap();
        effects
            .before_model_request(&Route {
                provider: "p".into(),
                model: "m".into(),
            })
            .unwrap();
        effects.finish().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                "start:InstructionsLoaded",
                "activity",
                "start:PreModelRequest",
                "finish-events",
                "settle:InstructionsLoaded",
                "settle:PreModelRequest"
            ]
        );
    }
    #[test]
    fn unknown_remains_sticky_on_repeated_finish() {
        let log = Log::default();
        let owner = Arc::new(Owner {
            log: log.clone(),
            event: true,
            hook_failure: Some(unknown()),
        });
        let mut effects = ManagedPreparationEffects::new(owner, control());
        effects.notice(&input(), false).unwrap();
        assert!(matches!(
            effects.finish(),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert!(matches!(
            effects.before_model_request(&Route {
                provider: "p".into(),
                model: "m".into()
            }),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert!(matches!(
            effects.finish(),
            Err(PortError::AcceptedUnknown { .. })
        ));
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| *entry == "settle:InstructionsLoaded")
                .count(),
            1
        );
    }
    #[test]
    fn retired_owner_prevents_new_notice() {
        let log = Log::default();
        let owner = Arc::new(Owner {
            log: log.clone(),
            event: true,
            hook_failure: None,
        });
        let ctl = control();
        ctl.cancellation.cancel(CancelCause::OwnerRetired);
        let mut effects = ManagedPreparationEffects::new(owner, ctl);
        assert!(matches!(
            effects.notice(&input(), true),
            Err(PortError::Cancelled {
                cause: CancelCause::OwnerRetired
            })
        ));
        assert!(log.lock().unwrap().is_empty());
    }
}
