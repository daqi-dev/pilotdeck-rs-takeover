//! Actual product event queue over the public Scheduler SDK.
//! Lifecycle hook capability and trusted cleanup admission are separate.
use crate::product::{self, PendingProductEvent, ProductEventSettlement};
use agentcpm_scheduler_protocol::{CallContext, Client, Failure};
use pilotdeck_agent::ports::{CallControl, ExecutionIdentity, PortError, PortResult};
use pilotdeck_protocol::{JsObject, JsValue};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

struct Notice {
    key: String,
    encoded: String,
    control: CallControl,
    admitted: Option<mpsc::Sender<()>>,
}
struct Unresolved {
    key: String,
    pending: Option<PendingProductEvent>,
    never_dispatched: bool,
    expected_json: Option<String>,
}
/// Operation-owned ledger must outlive the queue; it retains admitted handles.
pub struct EventSettlementLedger {
    entries: Mutex<Vec<Unresolved>>,
    next: AtomicUsize,
    active: AtomicBool,
    context: CallContext,
    controller: String,
    controller_revision: String,
}
#[derive(Debug)]
pub enum EventReconciliation {
    Pending,
    NeverDispatched,
    Known(Box<agentcpm_scheduler_protocol::CallView>),
}
impl EventSettlementLedger {
    pub fn new(context: CallContext, controller: String, controller_revision: String) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            next: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            context,
            controller,
            controller_revision,
        }
    }
    /// Read only; absence is Pending because an interrupted admission may land late.
    pub async fn reconcile(
        &self,
        client: &dyn Client,
        key: &str,
    ) -> agentcpm_scheduler_protocol::Result<EventReconciliation> {
        let (never_dispatched, expected_json, call_ref) = {
            let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            let entry = entries
                .iter()
                .find(|entry| entry.key == key)
                .ok_or_else(|| {
                    Failure::external("NOTICE_NOT_FOUND", "notification identity not retained")
                })?;
            (
                entry.never_dispatched,
                entry.expected_json.clone(),
                entry
                    .pending
                    .as_ref()
                    .map(|pending| pending.call_ref().to_owned()),
            )
        };
        if never_dispatched {
            return Ok(EventReconciliation::NeverDispatched);
        }
        let Some(view) = client.find(key, &self.controller).await? else {
            return Ok(EventReconciliation::Pending);
        };
        if call_ref.as_ref().is_some_and(|id| id != &view.id)
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
            || view.invocation.payload.as_str() != expected_json.as_deref()
            || view.interaction != self.context.interaction
            || view.origin != self.context.interaction
            || view.security != self.context.security
        {
            return Err(Failure::external(
                "NOTICE_IDENTITY_MISMATCH",
                "retained notice does not match the authoritative call",
            ));
        }
        Ok(EventReconciliation::Known(Box::new(view)))
    }
    fn mark_not_dispatched(&self, key: &str) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter_mut()
            .find(|entry| entry.key == key)
        {
            entry.never_dispatched = true;
        }
    }
    fn attach_pending(&self, key: &str, pending: PendingProductEvent) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter_mut()
            .find(|entry| entry.key == key)
        {
            entry.pending = Some(pending);
        }
    }
    pub fn entries(&self) -> Vec<(String, Option<String>, bool)> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|entry| {
                (
                    entry.key.clone(),
                    entry.pending.as_ref().map(|p| p.call_ref().to_owned()),
                    entry.never_dispatched,
                )
            })
            .collect()
    }
}
/// Holds the worker; the host separately retains the settlement ledger.
pub struct ProductEventQueue {
    context: CallContext,
    sender: Option<SyncSender<Notice>>,
    outstanding: Arc<AtomicUsize>,
    retired: Arc<AtomicBool>,
    finish_deadline: Arc<Mutex<Option<Instant>>>,
    unresolved: Arc<EventSettlementLedger>,
    worker: Option<JoinHandle<PortResult<()>>>,
    terminal_error: Mutex<Option<PortError>>,
}
impl ProductEventQueue {
    pub fn new(
        client: Arc<dyn Client>,
        context: CallContext,
        controller: String,
        runtime: Handle,
        operation_control: CallControl,
        unresolved: Arc<EventSettlementLedger>,
        admission: Arc<dyn crate::sdk_notice_owner::NoticeAdmission>,
    ) -> PortResult<Self> {
        if unresolved.context.call_ref != context.call_ref
            || unresolved.context.session_ref != context.session_ref
            || unresolved.controller != controller
            || unresolved.context.interaction != context.interaction
            || unresolved.context.binding != context.binding
            || unresolved.context.invocation != context.invocation
            || unresolved.context.state != context.state
            || unresolved.context.origin != context.origin
            || unresolved.context.security != context.security
            || unresolved.context.attempt != context.attempt
        {
            return Err(PortError::InvalidResponse {
                message: "Event settlement ledger scope mismatch.".into(),
            });
        }
        if unresolved
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PortError::InvalidResponse {
                message: "An event queue already owns this operation.".into(),
            });
        }
        let (sender, receiver) = mpsc::sync_channel::<Notice>(64);
        let outstanding = Arc::new(AtomicUsize::new(0));
        let retired = Arc::new(AtomicBool::new(false));
        let finish_deadline = Arc::new(Mutex::new(None));
        let retained_context = context.clone();
        let event_control = CallControl {
            cancellation: Default::default(),
            deadline: operation_control.deadline,
        };
        let worker = {
            let count = outstanding.clone();
            let stop = retired.clone();
            let terminal = finish_deadline.clone();
            let facts = unresolved.clone();
            thread::Builder::new()
                .name("pilotdeck-sdk-notices".into())
                .spawn(move || {
                    let mut error = None;
                    for notice in receiver {
                        if stop.load(Ordering::Acquire) || error.is_some() {
                            facts.mark_not_dispatched(&notice.key);
                            if error.is_none() {
                                error = Some(PortError::Cancelled {
                                    cause: pilotdeck_agent::ports::CancelCause::OwnerRetired,
                                });
                            }
                            if let Some(admitted) = notice.admitted {
                                let _ = admitted.send(());
                            }
                            count.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        }
                        let key = notice.key.clone();
                        if let Err(denied) = admission.event_dispatch(&notice.control) {
                            facts.mark_not_dispatched(&key);
                            error = Some(denied);
                            if let Some(admitted) = notice.admitted {
                                let _ = admitted.send(());
                            }
                            count.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        }
                        let encoded = notice.encoded;
                        let admission = runtime.block_on(async {
                            wait_controlled(
                                product::admit_event(
                                    client.as_ref(),
                                    &context,
                                    &controller,
                                    &key,
                                    encoded,
                                ),
                                &notice.control,
                                &stop,
                                &terminal,
                            )
                            .await
                        });
                        if let Some(admitted) = notice.admitted {
                            let _ = admitted.send(());
                        }
                        match admission {
                            Ok(Ok(pending)) => {
                                let result = runtime.block_on(async {
                                    wait_controlled(
                                        pending.settlement(client.as_ref()),
                                        &event_control,
                                        &stop,
                                        &terminal,
                                    )
                                    .await
                                });
                                match result {
                                    Ok(Ok(ProductEventSettlement::Completed)) => {}
                                    other => {
                                        facts.attach_pending(&key, pending);
                                        let cause = match other {
                                            Ok(Err(failure))
                                            | Ok(Ok(ProductEventSettlement::Failed(Some(
                                                failure,
                                            )))) => Some(failure),
                                            Ok(Ok(ProductEventSettlement::Cancelled)) => {
                                                Some(Failure::external(
                                                    "CANCELLED",
                                                    "notice cancelled before confirmed delivery",
                                                ))
                                            }
                                            _ => None,
                                        };
                                        error = Some(unknown_with_cause(&context, &key, cause));
                                    }
                                }
                            }
                            other => {
                                let cause = match other {
                                    Ok(Err(failure)) => Some(failure),
                                    _ => None,
                                };
                                error = Some(unknown_with_cause(&context, &key, cause));
                            }
                        }
                        count.fetch_sub(1, Ordering::AcqRel);
                    }
                    error.map_or(Ok(()), Err)
                })
                .map_err(|error| {
                    unresolved.active.store(false, Ordering::Release);
                    PortError::InvalidResponse {
                        message: format!("Event notice worker could not start: {error}").into(),
                    }
                })?
        };
        Ok(Self {
            context: retained_context,
            sender: Some(sender),
            outstanding,
            retired,
            finish_deadline,
            unresolved,
            worker: Some(worker),
            terminal_error: Mutex::new(None),
        })
    }
    pub fn enqueue(&self, event: JsObject, control: &CallControl) -> PortResult<()> {
        if self.retired.load(Ordering::Acquire) {
            return Err(PortError::Cancelled {
                cause: pilotdeck_agent::ports::CancelCause::OwnerRetired,
            });
        }
        control.check()?;
        let mut current = self.unresolved.next.load(Ordering::Acquire);
        let ordinal = loop {
            let next = current
                .checked_add(1)
                .ok_or_else(|| PortError::InvalidResponse {
                    message: "Event notice sequence exhausted.".into(),
                })?;
            match self.unresolved.next.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break next,
                Err(observed) => current = observed,
            }
        };
        let key = format!("{}:notice:{}", self.context.call_ref, ordinal);
        let encoded = pilotdeck_protocol::stringify_json(&JsValue::Object(event))
            .ok()
            .flatten()
            .ok_or_else(|| PortError::InvalidResponse {
                message: "Event notice could not be encoded.".into(),
            })?;
        let idle = self.outstanding.fetch_add(1, Ordering::AcqRel) == 0;
        let (admitted, receive) = mpsc::channel();
        self.unresolved
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Unresolved {
                key: key.clone(),
                pending: None,
                never_dispatched: false,
                expected_json: Some(encoded.clone()),
            });
        let notice = Notice {
            key: key.clone(),
            encoded,
            control: control.clone(),
            admitted: idle.then_some(admitted),
        };
        let result = self
            .sender
            .as_ref()
            .ok_or_else(|| PortError::InvalidResponse {
                message: "Event notice queue is closed.".into(),
            })
            .and_then(|sender| {
                sender
                    .try_send(notice)
                    .map_err(|error| PortError::InvalidResponse {
                        message: match error {
                            mpsc::TrySendError::Full(_) => {
                                "Advisory event pending-notice capacity exhausted."
                            }
                            mpsc::TrySendError::Disconnected(_) => {
                                "Event notice worker terminated before queue admission."
                            }
                        }
                        .into(),
                    })
            });
        if let Err(error) = result {
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
            self.unresolved
                .entries
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|entry| entry.key != key);
            return Err(error);
        }
        if idle {
            let timeout = control.deadline.map_or(Duration::from_secs(5), |d| {
                d.saturating_duration_since(Instant::now())
            });
            if receive.recv_timeout(timeout).is_err() {
                self.retired.store(true, Ordering::Release);
                let error = unknown(&self.context, &key);
                *self
                    .terminal_error
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(error.clone());
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn finish(&mut self, control: &CallControl) -> PortResult<()> {
        let prior = self
            .terminal_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let deadline = control
            .deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(5));
        self.finish_deadline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert(deadline);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let result = worker
                .join()
                .map_err(|_| PortError::InvalidResponse {
                    message: "Event notice worker panicked.".into(),
                })
                .and_then(|value| value);
            if let Err(error) = result {
                let mut saved = self
                    .terminal_error
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let retained = saved.get_or_insert(error).clone();
                return Err(retained);
            }
        }
        prior.map_or(Ok(()), Err)
    }
    /// May be called without acquiring the queue mutex to interrupt a waiter.
    pub fn retirement_signal(&self) -> Arc<AtomicBool> {
        self.retired.clone()
    }
    pub fn retire(&mut self) {
        self.retired.store(true, Ordering::Release);
        self.sender.take();
    }
}
impl Drop for ProductEventQueue {
    fn drop(&mut self) {
        self.retire();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.unresolved.active.store(false, Ordering::Release);
    }
}
async fn wait_controlled<F: std::future::Future>(
    future: F,
    control: &CallControl,
    retired: &AtomicBool,
    terminal: &Mutex<Option<Instant>>,
) -> Result<F::Output, ()> {
    tokio::pin!(future);
    loop {
        if retired.load(Ordering::Acquire)
            || control.check().is_err()
            || terminal
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some_and(|d| Instant::now() >= d)
        {
            return Err(());
        }
        tokio::select! { result = &mut future => return Ok(result), () = tokio::time::sleep(Duration::from_millis(10)) => {} }
    }
}
fn unknown_with_cause(context: &CallContext, key: &str, failure: Option<Failure>) -> PortError {
    let mut error = unknown(context, key);
    if let (PortError::AcceptedUnknown { cause, .. }, Some(failure)) = (&mut error, failure) {
        *cause = Some(Box::new(pilotdeck_protocol::model::HostDiagnosticCause {
            code: failure.code,
            message_key: failure.message_key,
            params: failure.params,
        }));
    }
    error
}
fn unknown(context: &CallContext, key: &str) -> PortError {
    PortError::AcceptedUnknown {
        identity: Box::new(ExecutionIdentity {
            session_id: context.session_ref.as_str().into(),
            turn_id: context.interaction.turn.to_string().into(),
            run_id: context.call_ref.as_str().into(),
            operation_id: context.invocation.correlation_id.as_str().into(),
            request_id: context.call_ref.as_str().into(),
            idempotency_key: Some(key.into()),
            module_instance_id: context.binding.id.as_str().into(),
            connection_generation: context.interaction.generation.to_string().into(),
        }),
        last_applied_sequence: None,
        cause: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestAdmission;
    impl crate::sdk_notice_owner::NoticeAdmission for TestAdmission {
        fn instructions_loaded(
            &self,
            _: &pilotdeck_agent::turn::TurnInput,
            _: &CallControl,
        ) -> PortResult<()> {
            Ok(())
        }
        fn pre_model(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn event_dispatch(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn before_turn_completed(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_is_sticky_and_key_is_owned_by_ledger() {
        let runtime = Handle::current();
        tokio::task::spawn_blocking(move || {
            let context = crate::tests::context();
            let client = crate::tests::client(None);
            let ledger = Arc::new(EventSettlementLedger::new(
                context.clone(),
                "controller".into(),
                "controller-rev".into(),
            ));
            let control = CallControl {
                cancellation: Default::default(),
                deadline: Some(Instant::now() + Duration::from_secs(2)),
            };
            let mut first = ProductEventQueue::new(
                client.clone(),
                context.clone(),
                "controller".into(),
                runtime.clone(),
                control.clone(),
                ledger.clone(),
                Arc::new(TestAdmission),
            )
            .unwrap();
            assert!(
                ProductEventQueue::new(
                    client,
                    context,
                    "controller".into(),
                    runtime,
                    control.clone(),
                    ledger.clone(),
                    Arc::new(TestAdmission),
                )
                .is_err()
            );
            let mut event = JsObject::new();
            event.insert("type".into(), "instructions_loaded".into());
            first.enqueue(event, &control).unwrap();
            assert!(matches!(
                first.finish(&control),
                Err(PortError::AcceptedUnknown { .. })
            ));
            assert!(matches!(
                first.finish(&control),
                Err(PortError::AcceptedUnknown { .. })
            ));
            let entries = ledger.entries();
            assert_eq!(entries.len(), 1);
            assert!(entries[0].0.ends_with(":notice:1"));
            assert!(!entries[0].2);
        })
        .await
        .unwrap();
    }
    struct DenyAdmission;
    impl crate::sdk_notice_owner::NoticeAdmission for DenyAdmission {
        fn instructions_loaded(
            &self,
            _: &pilotdeck_agent::turn::TurnInput,
            _: &CallControl,
        ) -> PortResult<()> {
            Ok(())
        }
        fn pre_model(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
        fn event_dispatch(&self, _: &CallControl) -> PortResult<()> {
            Err(PortError::InvalidResponse {
                message: "denied before invoke".into(),
            })
        }
        fn before_turn_completed(&self, _: &CallControl) -> PortResult<()> {
            Ok(())
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_notice_rechecks_authority_before_invoke() {
        let runtime = Handle::current();
        tokio::task::spawn_blocking(move || {
            let context = crate::tests::context();
            let ledger = Arc::new(EventSettlementLedger::new(
                context.clone(),
                "controller".into(),
                "rev".into(),
            ));
            let control = CallControl {
                cancellation: Default::default(),
                deadline: Some(Instant::now() + Duration::from_secs(2)),
            };
            let mut queue = ProductEventQueue::new(
                crate::tests::client(None),
                context,
                "controller".into(),
                runtime,
                control.clone(),
                ledger.clone(),
                Arc::new(DenyAdmission),
            )
            .unwrap();
            let mut event = JsObject::new();
            event.insert("type".into(), "instructions_loaded".into());
            queue.enqueue(event, &control).unwrap();
            assert!(matches!(
                queue.finish(&control),
                Err(PortError::InvalidResponse { .. })
            ));
            let entries = ledger.entries();
            assert_eq!(entries.len(), 1);
            assert!(entries[0].2);
        })
        .await
        .unwrap();
    }
}
