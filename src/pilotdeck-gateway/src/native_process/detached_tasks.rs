//! Structured producers for original Promise.all siblings. This proves only
//! this group's task handoffs have stopped; it is not a Host resource receipt.
use crate::host::{HostError, HostErrorKind, HostFuture};
use pilotdeck_protocol::JsObject;
use std::sync::{Arc, Mutex};
use tokio::{runtime::Handle, task::JoinHandle};

struct Pending<T: Send + Sync + 'static>(Mutex<Option<(HostFuture<JsObject>, ProducerGuard<T>)>>);
struct State<T: Send + Sync + 'static> {
    root_closed: bool,
    producers: usize,
    tasks: Vec<JoinHandle<()>>,
    slots: Vec<Arc<Pending<T>>>,
    unknown: bool,
    completed: bool,
}
struct Inner<T: Send + Sync + 'static> {
    // Keeps the exact batch/turn owner alive. The enclosing Host control must
    // additionally hold the real project fork and source reservation.
    owner: Mutex<Option<T>>,
    executor: Handle,
    state: Mutex<State<T>>,
}
pub(crate) struct ProducerGroup<T: Send + Sync + 'static>(Arc<Inner<T>>);
impl<T: Send + Sync + 'static> Clone for ProducerGroup<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
/// No Clone, public constructor, or conversion from a caller-provided ID. A
/// gate may share an Arc of this guard; its producer survives the last clone.
pub(crate) struct ProducerGuard<T: Send + Sync + 'static> {
    inner: Arc<Inner<T>>,
    root: bool,
}

impl<T: Send + Sync + 'static> ProducerGroup<T> {
    pub(crate) fn new(owner: T, executor: Handle) -> (Self, ProducerGuard<T>) {
        let inner = Arc::new(Inner {
            owner: Mutex::new(Some(owner)),
            executor,
            state: Mutex::new(State {
                root_closed: false,
                producers: 1,
                tasks: Vec::new(),
                slots: Vec::new(),
                unknown: false,
                completed: false,
            }),
        });
        (Self(inner.clone()), ProducerGuard { inner, root: true })
    }

    /// Forbids new root work. The primary gate's existing root guard remains
    /// counted until its last Arc reference is actually dropped; previously
    /// minted child guards may continue handing off already-started work.
    pub(crate) fn close_root(&self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .root_closed = true;
    }

    /// Monotone task-only fact, under the same lock as mint, handoff and guard
    /// drop. Host still must join all tool/approval/helper/resource receipts and
    /// wait for actual fork, reservation and owner destruction before Released.
    pub(crate) fn tasks_drained(&self) -> bool {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.completed {
            return true;
        }
        state.tasks.retain(|task| !task.is_finished());
        state.slots.retain(|slot| {
            slot.0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
        });
        if state.root_closed
            && state.producers == 0
            && state.tasks.is_empty()
            && state.slots.is_empty()
            && !state.unknown
        {
            state.completed = true;
        }
        state.completed
    }

    /// A separately owned cleanup task may take the source lease only after
    /// the producer fence has latched. Taking it is not a release receipt.
    pub(crate) fn take_drained_owner(&self) -> Option<T> {
        if !self.tasks_drained() {
            return None;
        }
        self.0
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// The caller already holds a registered producer while using this. The
    /// owner is private to this source and cannot be selected by a string.
    pub(crate) fn with_open_owner<R>(&self, operation: impl FnOnce(&mut T) -> R) -> Option<R> {
        let state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.root_closed || state.completed {
            return None;
        }
        let mut owner = self
            .0
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        owner.as_mut().map(operation)
    }
}

impl<T: Send + Sync + 'static> ProducerGuard<T> {
    pub(crate) fn mark_unknown(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unknown = true;
    }
    /// Mint one child producer only from a live guard. The guard must be put in
    /// the per-task gate before the task is first polled; no batch/call string
    /// can create a guard. Child producers may continue after root close.
    pub(crate) fn child(&self) -> Result<Self, HostError> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.completed || (self.root && state.root_closed) {
            return Err(unknown());
        }
        state.producers += 1;
        Ok(Self {
            inner: self.inner.clone(),
            root: false,
        })
    }

    /// The future has already been polled. Under the same lock, mint its task
    /// guard and store the future before scheduling it. Failed/late handoff
    /// never discards it: keep it owned and report Unknown if the root was
    /// already closed or the executor panics.
    pub(crate) fn handoff(&self, future: HostFuture<JsObject>) -> Result<(), HostError> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A live guard prevents completed=true, including while waiting for
        // this lock. Retain a late root handoff and do not certify convergence.
        let late = state.completed || (self.root && state.root_closed);
        if late {
            state.unknown = true;
        }
        state.producers += 1;
        let child = ProducerGuard {
            inner: self.inner.clone(),
            root: false,
        };
        let slot = Arc::new(Pending(Mutex::new(Some((future, child)))));
        state.slots.push(slot.clone());
        let spawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.inner.executor.spawn(async move {
                let work = slot
                    .0
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some((future, producer)) = work {
                    let _producer = producer;
                    let _ = future.await;
                }
            })
        }));
        match spawn {
            Ok(task) => {
                state.tasks.push(task);
                if late { Err(unknown()) } else { Ok(()) }
            }
            Err(_) => {
                state.unknown = true;
                Err(unknown())
            }
        }
    }
}
impl<T: Send + Sync + 'static> Drop for ProducerGuard<T> {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        debug_assert!(state.producers > 0);
        state.producers -= 1;
    }
}
fn unknown() -> HostError {
    HostError::new(
        HostErrorKind::ResultUnknown,
        "tool producer ownership is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_is_taken_only_after_all_registered_producers_exit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (group, root) = ProducerGroup::new(42_u32, runtime.handle().clone());
        let child = root.child().unwrap();
        group.close_root();
        drop(root);
        assert!(group.take_drained_owner().is_none());
        drop(child);
        assert_eq!(group.take_drained_owner(), Some(42));
        assert!(group.take_drained_owner().is_none());
    }

    #[test]
    fn close_prevents_new_owner_checkout() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (group, root) = ProducerGroup::new(42_u32, runtime.handle().clone());
        assert_eq!(group.with_open_owner(|value| *value), Some(42));
        group.close_root();
        assert!(
            group
                .with_open_owner(|_| panic!("closed scope performed checkout"))
                .is_none()
        );
        drop(root);
        assert_eq!(group.take_drained_owner(), Some(42));
    }
    #[tokio::test]
    async fn shared_gate_reference_keeps_root_producer_counted() {
        let (group, root) = ProducerGroup::new(Arc::new(()), Handle::current());
        let gate = Arc::new(root);
        let in_flight = gate.clone();
        group.close_root();
        drop(gate);
        assert!(!group.tasks_drained());
        drop(in_flight);
        assert!(group.tasks_drained());
        assert!(group.tasks_drained());
        // No guard remains, and ProducerGroup exposes no API to mint another.
    }

    #[tokio::test]
    async fn root_close_cannot_overtake_a_handoff_caller_holding_the_gate() {
        let (group, root) = ProducerGroup::new(Arc::new(()), Handle::current());
        let root = Arc::new(root);
        let in_flight = root.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let (executed_tx, executed_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            continue_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            in_flight
                .handoff(Box::pin(async move {
                    let _ = executed_tx.send(());
                    Ok(JsObject::new())
                }))
                .is_err()
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        group.close_root();
        drop(root);
        assert!(!group.tasks_drained());
        continue_tx.send(()).unwrap();
        assert!(thread.join().unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(1), executed_rx)
            .await
            .unwrap()
            .unwrap();
        // The late, already-polled work was still handed off, but this is a
        // violated root boundary and must never latch a task-release fact.
        assert!(!group.tasks_drained());
    }

    #[tokio::test]
    async fn child_can_handoff_after_root_close_and_latch_is_monotone() {
        let (group, root) = ProducerGroup::new(Arc::new(()), Handle::current());
        let child = root.child().unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        group.close_root();
        assert!(root.child().is_err());
        drop(root);
        assert!(!group.tasks_drained());
        child
            .handoff(Box::pin(async move {
                let _ = receive.await;
                Ok(JsObject::new())
            }))
            .unwrap();
        drop(child);
        assert!(!group.tasks_drained());
        send.send(()).unwrap();
        for _ in 0..100 {
            if group.tasks_drained() {
                assert!(group.tasks_drained());
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("tracked sibling never finished")
    }

    #[tokio::test]
    async fn late_root_handoff_remains_owned_and_never_claims_drained() {
        let (group, root) = ProducerGroup::new(Arc::new(()), Handle::current());
        group.close_root();
        let (send, receive) = tokio::sync::oneshot::channel();
        let result = root.handoff(Box::pin(async move {
            let _ = send.send(());
            Ok(JsObject::new())
        }));
        assert!(result.is_err());
        drop(root);
        tokio::time::timeout(std::time::Duration::from_secs(1), receive)
            .await
            .unwrap()
            .unwrap();
        assert!(!group.tasks_drained());
    }

    #[test]
    fn executor_shutdown_keeps_unpolled_handoff_owned() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (group, root) = ProducerGroup::new(Arc::new(()), runtime.handle().clone());
        drop(runtime);
        let _ = root.handoff(Box::pin(async {
            std::future::pending::<()>().await;
            Ok(JsObject::new())
        }));
        group.close_root();
        drop(root);
        assert!(!group.tasks_drained());
    }
}
