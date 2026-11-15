//! Source-bound custody for accepted tool batches. This is not a new grant:
//! Kernel associates cleanup with the PREPARE receipt on this exact client.
use agentcpm_scheduler_protocol::{
    Client,
    provider::ResourceCall,
    tool_batch::{self, Query, Receipt, Resources},
};
use pilotdeck_agent::ports::{CallControl, ExecutionIdentity, HostCall};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    task::{Context, Poll, Waker},
};
use tokio::{runtime::Handle, sync::watch, task::JoinHandle};
const MAX_OWNED: usize = 128;

#[derive(Default)]
struct State {
    closed: bool,
    in_flight: usize,
    unknown: bool,
    records: Vec<Arc<Batch>>,
    recovery_tasks: Vec<JoinHandle<()>>,
    observations: Vec<Arc<ToolObservation>>,
}
/// Original source facts retained with the SDK reader ticket. This is an
/// observation handle, not a new authority or a replay key.
pub struct ToolObservation {
    ticket: ResourceCall,
    call: HostCall,
    control: CallControl,
    identity: ExecutionIdentity,
    batch_ref: Option<String>,
    late: AtomicBool,
    reconciling: AtomicBool,
    consumption: AtomicU8,
    recovery: AtomicU8,
}
impl ToolObservation {
    pub fn ticket(&self) -> &ResourceCall {
        &self.ticket
    }
    pub fn call(&self) -> &HostCall {
        &self.call
    }
    pub fn control(&self) -> &CallControl {
        &self.control
    }
    pub fn identity(&self) -> &ExecutionIdentity {
        &self.identity
    }
    pub fn batch_ref(&self) -> Option<&str> {
        self.batch_ref.as_deref()
    }
}
struct Inner {
    client: Arc<dyn Client>,
    runtime: Handle,
    state: Mutex<State>,
    stop: watch::Sender<bool>,
    source_gate: Mutex<Option<Arc<Mutex<bool>>>>,
}
/// Original Native source owns this registry beyond worker completion, until
/// every associated batch and observation has actually converged. This is an
/// ownership aggregate, not an authorization or a session-id registry.
pub struct SourceToolOwners {
    client: Arc<dyn Client>,
    state: Mutex<SourceToolState>,
    gate: Arc<Mutex<bool>>,
    native_ticket: Mutex<Option<agentcpm_scheduler_protocol::provider::NativeSourceOwnerTicket>>,
}
#[derive(Default)]
struct SourceToolState {
    closed: bool,
    owners: Vec<Arc<ToolBatchOwner>>,
}
impl SourceToolOwners {
    pub fn new(client: Arc<dyn Client>) -> Arc<Self> {
        Arc::new(Self {
            client,
            state: Mutex::new(SourceToolState::default()),
            gate: Arc::new(Mutex::new(false)),
            native_ticket: Mutex::new(None),
        })
    }
    pub fn matches_client(&self, client: &Arc<dyn Client>) -> bool {
        Arc::ptr_eq(&self.client, client)
    }
    /// Register the same aggregate on the original validated SDK source before
    /// any codec can issue PREPARE. The returned observation is only an owner
    /// report; Kernel must independently join its actual Host/step resources.
    pub fn register_native(
        self: &Arc<Self>,
        client: &Arc<dyn Client>,
    ) -> Result<
        agentcpm_scheduler_protocol::provider::NativeSourceOwnerTicket,
        agentcpm_scheduler_protocol::Failure,
    > {
        if !self.matches_client(client) {
            return Err(agentcpm_scheduler_protocol::Failure::new(
                "INPUT_IDENTITY",
                "tool source client changed",
            ));
        }
        let source: Arc<dyn agentcpm_scheduler_protocol::provider::NativeSourceOwner> =
            self.clone();
        let ticket = client.retain_native_source_owner(source)?;
        *self.native_ticket.lock().unwrap_or_else(|e| e.into_inner()) = Some(ticket.clone());
        Ok(ticket)
    }
    pub fn retain(&self, owner: Arc<ToolBatchOwner>) -> Result<(), &'static str> {
        if !Arc::ptr_eq(&self.client, &owner.0.client) {
            return Err("tool owner belongs to a different source client");
        }
        let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if *gate {
            return Err("tool source closed or full");
        }
        {
            let mut owner_gate = owner
                .0
                .source_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if owner_gate
                .as_ref()
                .is_some_and(|other| !Arc::ptr_eq(other, &self.gate))
            {
                return Err("tool owner belongs to another source");
            }
            *owner_gate = Some(self.gate.clone());
        }
        let candidates = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owners
            .clone();
        let finished: Vec<_> = candidates
            .into_iter()
            .filter(|entry| entry.is_released())
            .collect();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut retired = Vec::new();
        state.owners.retain(|entry| {
            if finished.iter().any(|done| Arc::ptr_eq(done, entry)) {
                retired.push(entry.clone());
                false
            } else {
                true
            }
        });
        let result = if state.closed || state.owners.len() >= MAX_OWNED {
            Err("tool source closed or full")
        } else if state.owners.iter().any(|entry| Arc::ptr_eq(entry, &owner)) {
            Err("tool owner already retained")
        } else {
            state.owners.push(owner);
            Ok(())
        };
        drop(state);
        drop(retired);
        drop(finished);
        drop(gate);
        result
    }
    /// Normal completion prevents new registration, without stopping siblings.
    pub fn seal(&self) {
        let owners = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            *gate = true;
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state.owners.clone()
        };
        for owner in owners {
            owner.seal();
        }
    }
    /// Actual source stop, not a worker-return signal, cancels its own batches.
    pub fn stop(&self) {
        let owners = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            *gate = true;
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state.owners.clone()
        };
        for owner in owners {
            owner.stop();
        }
    }
    pub fn is_released(&self) -> bool {
        let owners = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.closed {
                return false;
            }
            state.owners.clone()
        };
        owners.iter().all(|owner| owner.is_released())
    }
}
// The SDK source registry retains this exact aggregate. The result is an
// owner-reported fact; Kernel still combines it with its own receipts.
impl agentcpm_scheduler_protocol::provider::NativeSourceOwner for SourceToolOwners {
    fn seal(&self) {
        SourceToolOwners::seal(self);
    }
    fn stop(&self) {
        SourceToolOwners::stop(self);
    }
    fn is_released(&self) -> bool {
        SourceToolOwners::is_released(self)
    }
}
pub struct ToolBatchOwner(Arc<Inner>);
struct Batch {
    initial: Receipt,
    released: AtomicBool,
    joined: AtomicBool,
    task: Mutex<Option<JoinHandle<()>>>,
}
pub(crate) struct BatchToken {
    owner: Arc<Inner>,
    batch: Arc<Batch>,
    handed_off: bool,
}
pub(crate) struct PrepareGuard {
    owner: Arc<Inner>,
    settled: bool,
    ticket: Option<ResourceCall>,
}

impl ToolBatchOwner {
    pub(crate) fn new(client: Arc<dyn Client>, runtime: Handle) -> Arc<Self> {
        Arc::new(Self(Arc::new(Inner {
            client,
            runtime,
            state: Mutex::new(State::default()),
            stop: watch::channel(false).0,
            source_gate: Mutex::new(None),
        })))
    }
    /// Reserve ownership before PREPARE can be sent. A dropped unresolved guard
    /// is Unknown, never evidence that the write was rejected or released.
    pub(crate) fn begin_prepare(&self) -> Result<PrepareGuard, &'static str> {
        let source_gate = self
            .0
            .source_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let gate = source_gate
            .as_ref()
            .map(|value| value.lock().unwrap_or_else(|e| e.into_inner()));
        if gate.as_ref().is_some_and(|closed| **closed) {
            return Err("tool source closed");
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut retired = Vec::new();
        state.records.retain(|record| {
            if record.joined() {
                retired.push(record.clone());
                false
            } else {
                true
            }
        });
        if state.closed || state.records.len().saturating_add(state.in_flight) >= MAX_OWNED {
            drop(state);
            drop(retired);
            return Err("tool batch owner closed or full");
        }
        state.in_flight += 1;
        let guard = PrepareGuard {
            owner: self.0.clone(),
            settled: false,
            ticket: None,
        };
        drop(state);
        drop(retired);
        Ok(guard)
    }
    pub(crate) fn track_observation(
        &self,
        ticket: ResourceCall,
        call: HostCall,
        control: CallControl,
        identity: ExecutionIdentity,
        batch_ref: Option<String>,
    ) -> Result<(), &'static str> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        // A ticket admitted before stop must still be accounted for when its
        // reply arrives after the registration fence has closed.
        if state.observations.len() >= MAX_OWNED
            || state
                .observations
                .iter()
                .any(|item| item.ticket.request_id() == ticket.request_id())
        {
            state.unknown = true;
            return Err("tool observation cannot be retained");
        }
        state.observations.push(Arc::new(ToolObservation {
            ticket,
            call,
            control,
            identity,
            batch_ref,
            late: AtomicBool::new(false),
            reconciling: AtomicBool::new(false),
            consumption: AtomicU8::new(0),
            recovery: AtomicU8::new(0),
        }));
        Ok(())
    }
    /// Only the retained original call owner should use this for late
    /// reconciliation. No value here can mint or replay a Host operation.
    pub fn retained_observations(&self) -> Vec<Arc<ToolObservation>> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observations
            .iter()
            .filter(|item| item.late.load(Ordering::Acquire))
            .cloned()
            .collect()
    }
    /// Reconcile one original observation. The trusted call owner must first
    /// consume or persist the actual SDK response using the retained call facts;
    /// failure leaves the original slot and its release fence intact.
    pub async fn reconcile_observation<F>(
        &self,
        observation: &Arc<ToolObservation>,
        consume: F,
    ) -> Result<(), agentcpm_scheduler_protocol::Failure>
    where
        F: FnOnce(
            &ToolObservation,
            &agentcpm_scheduler_protocol::provider::ResourceResponse,
        ) -> Result<(), agentcpm_scheduler_protocol::Failure>,
    {
        self.0
            .client
            .validate_observed_resource(&observation.ticket)?;
        if !observation.late.load(Ordering::Acquire)
            || observation
                .reconciling
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(agentcpm_scheduler_protocol::Failure::new(
                "INPUT_IDENTITY",
                "tool observation is not available for reconciliation",
            ));
        }
        struct ReconcileGuard<'a>(&'a AtomicBool);
        impl Drop for ReconcileGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = ReconcileGuard(&observation.reconciling);
        let tracked = self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observations
            .iter()
            .any(|item| Arc::ptr_eq(item, observation));
        if !tracked {
            return Err(agentcpm_scheduler_protocol::Failure::new(
                "INPUT_IDENTITY",
                "tool observation is not retained by this owner",
            ));
        }
        let response = observation.ticket.response().await?;
        match observation.consumption.load(Ordering::Acquire) {
            0 => {
                // A failed or panicking callback can have partially persisted
                // the result. Fence automatic re-execution before entering it.
                observation.consumption.store(2, Ordering::Release);
                consume(observation, &response)?;
                observation.consumption.store(1, Ordering::Release);
            }
            1 => {} // Prior consumption succeeded; retry only the ACK.
            _ => {
                return Err(agentcpm_scheduler_protocol::Failure::new(
                    "OUTCOME_UNKNOWN",
                    "tool observation consumption is unconfirmed",
                ));
            }
        }
        observation.ticket.acknowledge_response().await?;
        forget_observation(&self.0, &observation.ticket);
        Ok(())
    }
    pub(crate) fn observation_needs_consumer(&self, ticket: &ResourceCall) {
        mark_late_observation(&self.0, ticket);
    }
    pub(crate) fn observation_acknowledged(&self, ticket: &ResourceCall) {
        forget_observation(&self.0, ticket);
    }
    pub(crate) fn recover_status(&self, ticket: ResourceCall, already_consumed: bool) {
        let owner = self.0.clone();
        {
            let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(item) = state
                .observations
                .iter()
                .find(|item| item.ticket.request_id() == ticket.request_id())
            else {
                return;
            };
            let kind = if already_consumed { 1 } else { 2 };
            if item
                .recovery
                .compare_exchange(0, kind, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            if !already_consumed {
                item.late.store(true, Ordering::Release);
            }
            state.in_flight += 1;
        }
        let task_owner = owner.clone();
        let runtime = owner.runtime.clone();
        let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.spawn(async move {
                if ticket.response().await.is_err() {
                    task_owner
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .unknown = true;
                } else if already_consumed && ticket.acknowledge_response().await.is_ok() {
                    forget_observation(&task_owner, &ticket);
                } else if already_consumed {
                    task_owner
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .unknown = true;
                }
            })
        }));
        let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
        match spawned {
            Ok(task) => state.recovery_tasks.push(task),
            Err(_) => {
                state.in_flight -= 1;
                state.unknown = true;
            }
        }
    }
    /// Normal completion fences new PREPARE but leaves original detached
    /// siblings running; the observer only reads status until real release.
    pub fn seal(&self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
    }
    /// A real local stop also asks Host to close only these retained batches.
    pub fn stop(&self) {
        self.seal();
        self.0.stop.send_replace(true);
    }
    /// True only after the registration fence, all sent PREPAREs are accounted
    /// for, each real Host receipt says Released, and each observer joins.
    pub fn is_released(&self) -> bool {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut i = 0;
        let mut retired = Vec::new();
        while i < state.recovery_tasks.len() {
            if !state.recovery_tasks[i].is_finished() {
                i += 1;
                continue;
            }
            let mut context = Context::from_waker(Waker::noop());
            match Pin::new(&mut state.recovery_tasks[i]).poll(&mut context) {
                Poll::Ready(result) => {
                    let task = state.recovery_tasks.swap_remove(i);
                    state.in_flight -= 1;
                    if result.is_err() {
                        state.unknown = true;
                    }
                    retired.push(task);
                }
                Poll::Pending => i += 1,
            }
        }
        let released = state.closed
            && state.in_flight == 0
            && !state.unknown
            && state.observations.is_empty()
            && state.records.iter().all(|record| record.joined());
        drop(state);
        drop(retired);
        released
    }
}
fn mark_late_observation(owner: &Arc<Inner>, ticket: &ResourceCall) {
    let state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(item) = state
        .observations
        .iter()
        .find(|item| item.ticket.request_id() == ticket.request_id())
    {
        item.late.store(true, Ordering::Release);
    }
}
fn forget_observation(owner: &Arc<Inner>, ticket: &ResourceCall) {
    let removed = {
        let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .observations
            .iter()
            .position(|item| item.ticket.request_id() == ticket.request_id())
            .map(|i| state.observations.swap_remove(i))
    };
    drop(removed);
}
impl PrepareGuard {
    pub(crate) fn attach(&mut self, ticket: ResourceCall) -> Result<(), &'static str> {
        if self.ticket.is_some() {
            return Err("prepare observation already attached");
        }
        self.ticket = Some(ticket);
        Ok(())
    }
    /// Used solely for an affirmative pre-write rejection (or unsent request).
    pub(crate) fn rejected(mut self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight -= 1;
        self.settled = true;
    }
    /// The codec passes the actual successfully decoded response, not an
    /// externally supplied id. Store it before starting the observer.
    pub(crate) fn received(mut self, receipt: Receipt) -> Result<BatchToken, &'static str> {
        let mut state = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        if receipt.batch_ref.is_empty()
            || state
                .records
                .iter()
                .any(|entry| entry.initial.batch_ref == receipt.batch_ref)
        {
            state.unknown = true;
            state.in_flight -= 1;
            self.settled = true;
            return Err("invalid or duplicate batch receipt");
        }
        let batch = Arc::new(Batch {
            initial: receipt,
            released: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            task: Mutex::new(None),
        });
        state.records.push(batch.clone());
        state.in_flight -= 1;
        self.settled = true;
        Ok(BatchToken {
            owner: self.owner.clone(),
            batch,
            handed_off: false,
        })
    }
}
impl BatchToken {
    /// Transfers a known parent result to the caller; observing the real
    /// resource receipt continues without closing original sibling work.
    pub(crate) fn handoff(mut self) -> Result<(), &'static str> {
        self.handed_off = true;
        start_observer(&self.owner, &self.batch, false)
    }
}
impl Drop for BatchToken {
    fn drop(&mut self) {
        if !self.handed_off {
            let _ = start_observer(&self.owner, &self.batch, true);
        }
    }
}
fn start_observer(
    owner: &Arc<Inner>,
    batch: &Arc<Batch>,
    close_on_drop: bool,
) -> Result<(), &'static str> {
    let target = batch.clone();
    let client = owner.client.clone();
    let mut stop = owner.stop.subscribe();
    let spawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        owner.runtime.spawn(async move {
            let query = Query { batch_ref: target.initial.batch_ref.clone() };
            let mut first = Some(target.initial.clone());
            loop {
                let closing = close_on_drop || *stop.borrow();
                let result = if let Some(receipt) = first.take() { Ok(receipt) } else if closing { tool_batch::close(client.as_ref(), &query).await } else {
                    tokio::select! {
                        result = tool_batch::status(client.as_ref(), &query) => result,
                        changed = stop.changed() => { if changed.is_err() { break; } else { continue; } }
                    }
                };
                if let Ok(receipt) = result {
                    if receipt.batch_ref != query.batch_ref { break; }
                    if receipt.resources == Resources::Released {
                        if !receipt.closed || matches!(receipt.state, tool_batch::State::Preparing | tool_batch::State::Running) { break; }
                        target.released.store(true, Ordering::Release);
                        break;
                    }
                }
                tokio::select! { _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}, changed = stop.changed() => { if changed.is_err() { break; } } }
            }
        })
    }));
    match spawn {
        Ok(task) => {
            *batch.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
            Ok(())
        }
        Err(_) => {
            owner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unknown = true;
            Err("tool batch observer could not start")
        }
    }
}
impl Drop for PrepareGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if let Some(ticket) = self.ticket.take() {
            // Transfer the existing in-flight count before spawning. The new
            // guard retains it through the original slot response and ACK.
            self.settled = true;
            let guard = PrepareGuard {
                owner: self.owner.clone(),
                settled: false,
                ticket: None,
            };
            let runtime = self.owner.runtime.clone();
            // A second counted lease prevents release until the recovery task's
            // JoinHandle, including its remaining destructors, really joins.
            {
                self.owner
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .in_flight += 1;
            }
            let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.spawn(async move {
                    let response = match ticket.response().await {
                        Ok(value) => value,
                        Err(_) => return,
                    };
                    match response.into_result() {
                        Ok(value) => {
                            let Ok(receipt) = serde_json::from_value::<Receipt>(value) else {
                                return;
                            };
                            // PREPARE itself may already contain the terminal
                            // business result. Retain its original reader ticket
                            // and call facts for the actual late consumer; never
                            // ACK merely because cleanup custody was established.
                            mark_late_observation(&guard.owner, &ticket);
                            if let Ok(token) = guard.received(receipt) {
                                drop(token);
                            }
                        }
                        Err(failure)
                            if matches!(
                                failure.code.as_str(),
                                "CAPABILITY_DENIED"
                                    | "POLICY_DENIED"
                                    | "DELEGATION_DENIED"
                                    | "SESSION_FORBIDDEN"
                                    | "AUTHORITY_REVOKED"
                                    | "CAPABILITY_PARAMS"
                            ) =>
                        {
                            if ticket.acknowledge_response().await.is_ok() {
                                forget_observation(&guard.owner, &ticket);
                                guard.rejected();
                            }
                        }
                        Err(_) => {}
                    }
                })
            }));
            let mut state = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
            match spawned {
                Ok(task) => state.recovery_tasks.push(task),
                Err(_) => {
                    state.in_flight -= 1;
                    state.unknown = true;
                }
            }
            return;
        }
        let mut state = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight -= 1;
        state.unknown = true;
    }
}
impl Batch {
    fn joined(&self) -> bool {
        if !self.released.load(Ordering::Acquire) {
            return false;
        }
        if self.joined.load(Ordering::Acquire) {
            return true;
        }
        let Ok(mut slot) = self.task.try_lock() else {
            return false;
        };
        let Some(task) = slot.as_mut() else {
            return self.joined.load(Ordering::Acquire);
        };
        if !task.is_finished() {
            return false;
        }
        let mut context = Context::from_waker(Waker::noop());
        match Pin::new(task).poll(&mut context) {
            Poll::Ready(Ok(())) => {
                drop(slot.take());
                self.joined.store(true, Ordering::Release);
                true
            }
            Poll::Ready(Err(_)) => {
                drop(slot.take());
                false
            }
            Poll::Pending => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_scheduler_protocol::tool_batch::State as BatchState;

    #[tokio::test]
    async fn source_owner_rejects_other_client_and_waits_for_original_prepare() {
        let client = crate::tests::client(Some("pending"));
        let source = SourceToolOwners::new(client.clone());
        let foreign = ToolBatchOwner::new(crate::tests::client(Some("pending")), Handle::current());
        assert!(source.retain(foreign).is_err());
        let owner = ToolBatchOwner::new(client, Handle::current());
        let pending = owner.begin_prepare().unwrap();
        source.retain(owner.clone()).unwrap();
        source.seal();
        assert!(!source.is_released());
        assert!(source.retain(owner).is_err());
        pending.rejected();
        assert!(source.is_released());
    }

    #[tokio::test]
    async fn source_gate_closure_prevents_late_prepare() {
        let client = crate::tests::client(Some("pending"));
        let source = SourceToolOwners::new(client.clone());
        let owner = ToolBatchOwner::new(client, Handle::current());
        source.retain(owner.clone()).unwrap();
        let mut gate = source.gate.lock().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let rendezvous = barrier.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            rendezvous.wait();
            let _ = tx.send(owner.begin_prepare().is_err());
        });
        barrier.wait();
        *gate = true;
        drop(gate);
        assert!(rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
        source.seal();
        assert!(source.is_released());
    }

    #[tokio::test]
    async fn unresolved_prepare_stays_unknown_even_after_fence() {
        let owner = ToolBatchOwner::new(crate::tests::client(Some("pending")), Handle::current());
        let pending = owner.begin_prepare().unwrap();
        owner.seal();
        assert!(!owner.is_released());
        drop(pending);
        assert!(!owner.is_released());
        assert!(owner.begin_prepare().is_err());
    }

    #[tokio::test]
    async fn receipt_arriving_after_stop_is_owned_until_observer_joins() {
        let owner = ToolBatchOwner::new(crate::tests::client(Some("pending")), Handle::current());
        let pending = owner.begin_prepare().unwrap();
        owner.stop();
        assert!(!owner.is_released());
        pending
            .received(Receipt {
                batch_ref: "actual-fixture-batch".into(),
                state: BatchState::Completed,
                result: None,
                resources: Resources::Released,
                closed: true,
            })
            .unwrap()
            .handoff()
            .unwrap();
        for _ in 0..100 {
            if owner.is_released() {
                assert!(owner.is_released());
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("released receipt observer did not join");
    }

    #[tokio::test]
    async fn affirmative_rejection_does_not_leave_phantom_resource() {
        let owner = ToolBatchOwner::new(crate::tests::client(Some("pending")), Handle::current());
        owner.begin_prepare().unwrap().rejected();
        owner.seal();
        assert!(owner.is_released());
    }
}
