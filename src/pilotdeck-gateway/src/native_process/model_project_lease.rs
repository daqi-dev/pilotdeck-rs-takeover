//! Scope-owned project fork and Host reservation. Registered before the fork;
//! no Drop or task-abort is treated as a completed project release.
use super::project_model_owner::{ProjectModelLease, ProjectTurnBorrow};
use agentcpm_host::TrustedProjectReservation;
use agentcpm_iface::{BoxFuture, env::http_pull::HttpResources};
use pilotdeck_trusted_http_host::{ScopedLifecycle, TrustedProjectLease};
use std::sync::{Arc, Mutex};
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};

pub(crate) trait ReservationHold: Send + Sync {}
impl ReservationHold for TrustedProjectReservation {}
#[cfg(test)]
impl ReservationHold for () {}

struct State {
    building: bool,
    stopped: bool,
    child: Option<Arc<ProjectModelLease>>,
    turn: Option<ProjectTurnBorrow>,
    reservation: Option<Arc<dyn ReservationHold>>,
}
struct Inner {
    state: Mutex<State>,
    changed: Notify,
}
pub(crate) struct ScopeProjectLease {
    inner: Arc<Inner>,
    executor: Handle,
    task: Mutex<Option<JoinHandle<bool>>>,
    outcome: std::sync::atomic::AtomicU8,
}
pub(crate) struct Building(Arc<ScopeProjectLease>);
impl Drop for Building {
    fn drop(&mut self) {
        self.0
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .building = false;
        self.0.inner.changed.notify_waiters();
    }
}
impl ScopeProjectLease {
    pub fn new(reservation: Arc<dyn ReservationHold>, executor: Handle) -> (Arc<Self>, Building) {
        let owner = Arc::new(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    building: true,
                    stopped: false,
                    child: None,
                    turn: None,
                    reservation: Some(reservation),
                }),
                changed: Notify::new(),
            }),
            executor,
            task: Mutex::new(None),
            outcome: std::sync::atomic::AtomicU8::new(0),
        });
        (owner.clone(), Building(owner))
    }
    pub fn install_turn(&self, turn: ProjectTurnBorrow) {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        assert!(state.building && state.turn.is_none());
        state.turn = Some(turn);
    }
    pub fn install(&self, child: ProjectModelLease) -> Arc<ProjectModelLease> {
        let child = Arc::new(child);
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        assert!(state.building && state.child.is_none());
        state.child = Some(child.clone());
        child
    }
    fn poll_join(&self) {
        use std::{
            future::Future,
            pin::Pin,
            sync::atomic::Ordering,
            task::{Context, Poll, Waker},
        };
        let mut slot = self.task.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(task) = slot.as_mut()
            && task.is_finished()
        {
            let mut context = Context::from_waker(Waker::noop());
            if let Poll::Ready(result) = Pin::new(task).poll(&mut context) {
                slot.take();
                self.outcome.store(
                    if matches!(result, Ok(true)) { 1 } else { 2 },
                    Ordering::SeqCst,
                );
            }
        }
    }
}
impl TrustedProjectLease for ScopeProjectLease {}
impl ScopedLifecycle for ScopeProjectLease {
    fn stop(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
        self.inner.changed.notify_waiters();
    }
    fn io_drained(&self) {
        let mut slot = self.task.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() || self.outcome.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return;
        }
        let inner = self.inner.clone();
        *slot = Some(self.executor.spawn(async move { cleanup(inner).await }));
    }
    fn resources(&self) -> HttpResources {
        self.poll_join();
        match self.outcome.load(std::sync::atomic::Ordering::SeqCst) {
            1 => HttpResources::Released,
            2 => HttpResources::Unknown,
            _ => HttpResources::Held,
        }
    }
    fn released(&self) -> BoxFuture<'_, HttpResources> {
        Box::pin(async move {
            loop {
                match self.resources() {
                    HttpResources::Released => return HttpResources::Released,
                    HttpResources::Unknown => return HttpResources::Unknown,
                    _ => {}
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
    }
}
async fn cleanup(inner: Arc<Inner>) -> bool {
    loop {
        let notified = inner.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let ready = {
            let state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stopped && !state.building
        };
        if ready {
            break;
        }
        notified.await;
    }
    // The builder holds another reservation Arc until every local borrow,
    // including its turn/root reference, has left. Keep our reservation alive
    // while waiting; otherwise a root drop could start disposal after the fork
    // had incorrectly been declared released.
    loop {
        let sole = {
            let state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state
                .reservation
                .as_ref()
                .is_some_and(|value| Arc::strong_count(value) == 1)
        };
        if sole {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Release turn-scoped extra owners before the project fork. No destructor
    // executes under the state mutex.
    let turn = {
        inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .turn
            .take()
    };
    drop(turn);
    let mut lease = loop {
        let child = {
            let mut state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            if state
                .child
                .as_ref()
                .is_some_and(|child| Arc::strong_count(child) == 1)
            {
                state.child.take()
            } else if state.child.is_none() {
                break None;
            } else {
                None
            }
        };
        if let Some(child) = child {
            match Arc::try_unwrap(child) {
                Ok(child) => break Some(child),
                Err(child) => {
                    inner.state.lock().unwrap_or_else(|p| p.into_inner()).child = Some(child);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    if let Some(lease) = &mut lease
        && let Some(runtime) = lease.begin_release()
        && runtime.wait_disposed().await.is_err()
    {
        return false;
    }
    drop(lease);
    let reservation = {
        inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reservation
            .take()
    };
    drop(reservation);
    true
}
