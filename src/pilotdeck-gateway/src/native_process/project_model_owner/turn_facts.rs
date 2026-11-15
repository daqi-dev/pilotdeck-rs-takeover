//! Bounded facts owned by the very project instance bound during session prepare.
//! Registration is not a grant. Every actual model/tool job still requires
//! Kernel's current scoped reservation; no request/session string creates one.
use super::{OwnerRoot, ProjectModelOwner, State, closed};
use crate::{
    host::{ExecuteRequest, HostError, HostErrorKind},
    runtime::{
        project::RuntimeError,
        turn::{
            execution_backend::TurnExecutionFacts,
            kernel_client::{KernelRequestAdmission, KernelTurnRegistration, KernelTurnScope},
        },
    },
};
use agentcpm_host::{TrustedProjectExecutionSource, TrustedProjectReservation};
use agentcpm_iface::model_session::ModelExecutionContext;
use std::sync::{Arc, Mutex, Weak};

// Current Gateway serializes turns. This bounds even an incorrectly composed
// trusted caller; dropping a registration never permits a key to replace a
// still-live entry. Actual active jobs keep their own strong facts below.
const MAX_REGISTERED_TURNS: usize = 64;

#[derive(Clone, PartialEq, Eq)]
struct Origin {
    session: String,
    generation: u32,
    turn: u64,
    input_key: String,
}

pub(super) struct Record {
    origin: Origin,
    owner_epoch: u64,
    facts: TurnExecutionFacts,
    request: ExecuteRequest,
    _root: Arc<OwnerRoot>,
}

/// A concrete registration installed by the same prepare that received owner.
/// It retains already-admitted original data; it does not claim missing Tool,
/// Context, approval, or late-settlement capabilities are installed.
pub struct ProjectTurnAdmission {
    owner: Arc<ProjectModelOwner>,
}
impl ProjectTurnAdmission {
    pub fn new(owner: Arc<ProjectModelOwner>) -> Self {
        Self { owner }
    }
}
impl KernelRequestAdmission for ProjectTurnAdmission {
    fn admit(
        &self,
        request: &ExecuteRequest,
        facts: &TurnExecutionFacts,
        scope: &KernelTurnScope<'_>,
    ) -> Result<Box<dyn KernelTurnRegistration>, HostError> {
        scope.validate_owner(&self.owner)?;
        if request.identity != facts.identity {
            return Err(HostError::new(
                HostErrorKind::Protocol,
                "Turn facts do not belong to the original admitted request.",
            ));
        }
        if let Some(error) = &facts.tools.selection_error {
            return Err(error.clone());
        }
        self.owner
            .register_turn(
                Origin {
                    session: scope.session_id().into(),
                    generation: scope.generation(),
                    turn: scope.turn(),
                    input_key: scope.input_key().into(),
                },
                request.clone(),
                facts.clone(),
            )
            .map(|registration| Box::new(registration) as Box<dyn KernelTurnRegistration>)
            .map_err(|error| HostError::new(HostErrorKind::Session, error.message))
    }
}

struct Registration {
    state: Weak<Mutex<State>>,
    record: Arc<Record>,
}
impl KernelTurnRegistration for Registration {}
impl Drop for Registration {
    fn drop(&mut self) {
        let removed = self.state.upgrade().and_then(|state| {
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            if state
                .turns
                .get(&self.record.origin.input_key)
                .is_some_and(|record| Arc::ptr_eq(record, &self.record))
            {
                state.turns.remove(&self.record.origin.input_key)
            } else {
                None
            }
        });
        // Context/execution/observer destructors must never run under State.
        drop(removed);
    }
}

/// One actual job's immutable borrow. Keep it beside that job's project fork
/// and Kernel reservation until the real scope receipt. Retiring the input
/// rejects new borrows; it does not erase facts already used by an accepted job.
#[derive(Clone)]
pub struct ProjectTurnBorrow {
    record: Arc<Record>,
    source: TrustedProjectExecutionSource,
}
impl ProjectTurnBorrow {
    pub fn facts(&self) -> &TurnExecutionFacts {
        &self.record.facts
    }
    pub fn request(&self) -> &ExecuteRequest {
        &self.record.request
    }
    pub fn source(&self) -> &TrustedProjectExecutionSource {
        &self.source
    }
}

impl ProjectModelOwner {
    fn register_turn(
        &self,
        origin: Origin,
        request: ExecuteRequest,
        facts: TurnExecutionFacts,
    ) -> Result<Registration, RuntimeError> {
        if origin.input_key.is_empty() || origin.session.is_empty() || origin.turn == 0 {
            return Err(RuntimeError::new(
                "Authenticated turn registration is incomplete.",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let root = state.root.clone().ok_or_else(closed)?;
        if state.turns.len() >= MAX_REGISTERED_TURNS {
            return Err(RuntimeError::new(
                "Project turn registration capacity exceeded.",
            ));
        }
        if state.turns.contains_key(&origin.input_key) {
            return Err(RuntimeError::new(
                "Project input key is already registered.",
            ));
        }
        let record = Arc::new(Record {
            origin,
            owner_epoch: state.epoch,
            facts,
            request,
            _root: root,
        });
        state
            .turns
            .insert(record.origin.input_key.clone(), record.clone());
        Ok(Registration {
            state: Arc::downgrade(&self.state),
            record,
        })
    }

    /// Only a real Host reservation can obtain a model job's turn facts.
    /// The reservation itself proves session/principal/job/token/source; the
    /// model context is checked by the caller's actual reserve_project path.
    pub(crate) fn borrow_model_turn(
        &self,
        reservation: &TrustedProjectReservation,
        context: &ModelExecutionContext,
    ) -> Result<ProjectTurnBorrow, RuntimeError> {
        let actual = reservation
            .owner::<ProjectModelOwner>()
            .ok_or_else(closed)?;
        if !std::ptr::eq(actual, self) {
            return Err(RuntimeError::new(
                "Model job belongs to a different project owner.",
            ));
        }
        reservation.revalidate().map_err(|_| closed())?;
        let source = reservation.source().ok_or_else(closed)?;
        let record = self.borrow_origin(
            context.session_id(),
            source.input_idempotency_key(),
            source.turn(),
            source.generation(),
        )?;
        Ok(ProjectTurnBorrow {
            record,
            source: source.clone(),
        })
    }

    fn borrow_origin(
        &self,
        session: &str,
        key: &str,
        turn: u64,
        generation: u32,
    ) -> Result<Arc<Record>, RuntimeError> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.root.is_none() {
            return Err(closed());
        }
        let record = state.turns.get(key).ok_or_else(closed)?;
        if record.owner_epoch != state.epoch
            || record.origin.session != session
            || record.origin.turn != turn
            || record.origin.generation != generation
        {
            return Err(closed());
        }
        Ok(record.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostFuture, modules::capability};
    use crate::runtime::turn::execution_backend::TurnToolFacts;
    use pilotdeck_protocol::{JsObject, JsString, JsValue, admission::ExecutionIdentity};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Ports {
        owner: Weak<Mutex<State>>,
        dropped_without_lock: Arc<AtomicBool>,
        calls: Arc<Mutex<Vec<JsValue>>>,
    }
    impl Drop for Ports {
        fn drop(&mut self) {
            let free = self
                .owner
                .upgrade()
                .is_some_and(|owner| owner.try_lock().is_ok());
            self.dropped_without_lock.store(free, Ordering::SeqCst);
        }
    }
    impl capability::ToolCatalog for Ports {
        fn list(&self) -> HostFuture<Vec<Arc<dyn capability::ToolDefinition>>> {
            Box::pin(async { panic!("fact registration must not query tools") })
        }
    }
    impl capability::ToolExecution for Ports {
        fn execute_all(
            &self,
            _: Vec<capability::ToolCall>,
            _: capability::ToolInvocationContext,
        ) -> HostFuture<Vec<JsObject>> {
            Box::pin(async { panic!("fact registration must not execute tools") })
        }
    }
    impl capability::ToolContext for Ports {
        fn prepare(&self, call: JsValue) -> HostFuture<capability::ToolInvocationContext> {
            self.calls.lock().unwrap().push(call);
            Box::pin(async {
                Ok(capability::ToolInvocationContext {
                    runtime: Arc::new(()),
                    execution: Arc::new(()),
                })
            })
        }
    }
    impl capability::CapabilityResultObserver for Ports {
        fn on_capability_results(&self, _: Vec<JsObject>) -> HostFuture<()> {
            Box::pin(async { panic!("fact registration must not invent tool results") })
        }
    }
    fn owner() -> ProjectModelOwner {
        let returned = Arc::new(AtomicBool::new(false));
        ProjectModelOwner {
            state: Arc::new(Mutex::new(State {
                epoch: 0,
                root: Some(Arc::new(OwnerRoot {
                    lease: None,
                    returned: returned.clone(),
                })),
                returned,
                turns: Default::default(),
            })),
        }
    }
    fn origin(key: &str, turn: u64) -> Origin {
        Origin {
            session: "kernel-session".into(),
            generation: 3,
            turn,
            input_key: key.into(),
        }
    }
    fn fixture(
        owner: &ProjectModelOwner,
    ) -> (
        ExecuteRequest,
        TurnExecutionFacts,
        Arc<AtomicBool>,
        Arc<Mutex<Vec<JsValue>>>,
    ) {
        let dropped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(vec![]));
        let ports = Arc::new(Ports {
            owner: Arc::downgrade(&owner.state),
            dropped_without_lock: dropped.clone(),
            calls: calls.clone(),
        });
        let identity = ExecutionIdentity {
            session_id: "product-session".into(),
            turn_id: "product-turn".into(),
            run_id: "original-run".into(),
            operation_id: "original-operation".into(),
            request_id: "original-request".into(),
        };
        let mut payload = JsObject::new();
        payload.insert("originalUndefined".into(), JsValue::Undefined);
        payload.insert(
            "businessId".into(),
            JsValue::String(JsString::from_utf16(vec![0xd800, 0x41])),
        );
        let request = ExecuteRequest {
            identity: identity.clone(),
            payload,
            idempotency_key: Some("original-key".into()),
            operation_deadline: None,
        };
        let facts = TurnExecutionFacts {
            identity,
            model: Default::default(),
            turn_signal: crate::runtime::hooks::Cancellation::new(),
            tools: TurnToolFacts {
                execution: ports.clone(),
                context: ports.clone(),
                observer: ports,
                checkpoint: Default::default(),
                selection_error: None,
            },
        };
        (request, facts, dropped, calls)
    }
    #[test]
    fn duplicate_and_cross_input_generation_or_session_do_not_borrow() {
        let owner = owner();
        let (request, facts, _, calls) = fixture(&owner);
        let first = owner
            .register_turn(origin("input-1", 7), request.clone(), facts.clone())
            .unwrap();
        assert!(
            owner
                .register_turn(origin("input-1", 7), request, facts)
                .is_err()
        );
        for (session, key, turn, generation) in [
            ("kernel-session", "missing", 7, 3),
            ("other", "input-1", 7, 3),
            ("kernel-session", "input-1", 8, 3),
            ("kernel-session", "input-1", 7, 4),
        ] {
            assert!(owner.borrow_origin(session, key, turn, generation).is_err());
        }
        assert!(calls.lock().unwrap().is_empty());
        drop(first);
        assert!(
            owner
                .borrow_origin("kernel-session", "input-1", 7, 3)
                .is_err()
        );
    }
    #[test]
    fn already_borrowed_facts_hold_context_and_root_after_retirement() {
        let owner = owner();
        let (request, facts, dropped, _) = fixture(&owner);
        let registration = owner
            .register_turn(origin("input", 7), request, facts)
            .unwrap();
        let job = owner
            .borrow_origin("kernel-session", "input", 7, 3)
            .unwrap();
        owner.close();
        assert!(!owner.is_released());
        assert!(
            owner
                .borrow_origin("kernel-session", "input", 7, 3)
                .is_err()
        );
        drop(registration);
        assert!(!owner.is_released());
        assert!(!dropped.load(Ordering::SeqCst));
        drop(job);
        assert!(owner.is_released());
        assert!(
            dropped.load(Ordering::SeqCst),
            "last real handle destructor must run outside owner lock"
        );
    }
    #[test]
    fn stale_registration_drop_does_not_remove_reused_slot() {
        let owner = owner();
        let (request, facts, _, _) = fixture(&owner);
        let first = owner
            .register_turn(origin("input", 7), request.clone(), facts.clone())
            .unwrap();
        let stale = Registration {
            state: first.state.clone(),
            record: first.record.clone(),
        };
        drop(first);
        let second = owner
            .register_turn(origin("input", 8), request, facts)
            .unwrap();
        drop(stale);
        assert!(owner.borrow_origin("kernel-session", "input", 8, 3).is_ok());
        assert!(
            owner
                .borrow_origin("kernel-session", "input", 7, 3)
                .is_err()
        );
        drop(second);
    }
    #[tokio::test]
    async fn original_handles_and_full_js_value_are_preserved_without_reconstruction() {
        let owner = owner();
        let (request, facts, _, calls) = fixture(&owner);
        let original = request.clone();
        let context = facts.tools.context.clone();
        let checkpoint = facts.tools.checkpoint.clone();
        let registration = owner
            .register_turn(origin("input", 7), request, facts)
            .unwrap();
        let held = owner
            .borrow_origin("kernel-session", "input", 7, 3)
            .unwrap();
        assert_eq!(held.request.payload, original.payload);
        assert!(Arc::ptr_eq(&held.facts.tools.context, &context));
        assert!(Arc::ptr_eq(
            &held.facts.tools.checkpoint.read_file_state,
            &checkpoint.read_file_state
        ));
        let full_call = JsValue::Object(original.payload);
        held.facts
            .tools
            .context
            .prepare(full_call.clone())
            .await
            .unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), &[full_call]);
        drop(registration);
    }
    #[test]
    fn registry_is_bounded_and_capacity_returns_when_registration_retires() {
        let owner = owner();
        let (request, facts, _, _) = fixture(&owner);
        let mut registrations = vec![];
        for n in 0..MAX_REGISTERED_TURNS {
            registrations.push(
                owner
                    .register_turn(
                        origin(&format!("input-{n}"), n as u64 + 1),
                        request.clone(),
                        facts.clone(),
                    )
                    .unwrap(),
            );
        }
        assert!(
            owner
                .register_turn(origin("overflow", 100), request.clone(), facts.clone())
                .is_err()
        );
        drop(registrations.pop());
        assert!(
            owner
                .register_turn(origin("replacement", 100), request, facts)
                .is_ok()
        );
    }
}
