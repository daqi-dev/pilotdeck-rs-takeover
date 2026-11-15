//! Attempt-local cancellation and conservative resource receipts, draft.
use agentcpm_host::TrustedEgressPool;
use agentcpm_iface::BoxFuture;
use agentcpm_iface::env::http_pull::{
    HttpDispatch, HttpExchangeControl, HttpExchangeEnd, HttpExchangeReceipt, HttpResources,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::Waker;
use tokio::sync::{Notify, watch};

pub(crate) struct Attempt {
    receipt: Mutex<HttpExchangeReceipt>,
    changed: Notify,
    stopped: watch::Sender<bool>,
    owners: AtomicUsize,
    pool: Mutex<Option<TrustedEgressPool>>,
    locally_stopped: AtomicBool,
    body_waker: Mutex<Option<Waker>>,
    retained: Mutex<Option<Arc<dyn Send + Sync>>>,
    release_started: AtomicBool,
}

impl Attempt {
    pub(crate) fn new() -> Arc<Self> {
        let (stopped, _) = watch::channel(false);
        Arc::new(Self {
            receipt: Mutex::new(HttpExchangeReceipt {
                dispatch: HttpDispatch::NotDispatched,
                end: None,
                resources: HttpResources::Held,
            }),
            changed: Notify::new(),
            stopped,
            owners: AtomicUsize::new(0),
            pool: Mutex::new(None),
            locally_stopped: AtomicBool::new(false),
            body_waker: Mutex::new(None),
            retained: Mutex::new(None),
            release_started: AtomicBool::new(false),
        })
    }
    pub(crate) fn register_body_waker(&self, waker: &Waker) {
        *self.body_waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(waker.clone());
    }
    pub(crate) fn is_stopped(&self) -> bool {
        self.locally_stopped.load(Ordering::SeqCst)
    }
    /// Retain the real prepared facts and generation lease until the
    /// attempt's owned resources are proven converged. Attach before publish.
    pub(crate) fn retain_facts(&self, facts: Arc<dyn Send + Sync>) {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        assert!(retained.is_none(), "attempt facts may only be bound once");
        *retained = Some(facts);
    }
    pub(crate) fn attach_pool(&self, pool: TrustedEgressPool) {
        let mut current = self.pool.lock().unwrap_or_else(|p| p.into_inner());
        assert!(current.is_none(), "attempt pool may only be bound once");
        *current = Some(pool);
    }
    /// A head future transfers this lease to its returned body. Merely
    /// observing a stopped receipt is not evidence the owner was dropped.
    pub(crate) fn owner(self: &Arc<Self>) -> AttemptOwner {
        let receipt = self.receipt.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            receipt.end.is_none() && receipt.resources != HttpResources::Released,
            "cannot acquire an owner for a completed attempt"
        );
        assert_eq!(
            self.owners.fetch_add(1, Ordering::SeqCst),
            0,
            "attempt permits one transferable owner"
        );
        drop(receipt);
        AttemptOwner(self.clone())
    }
    pub(crate) fn has_owner(&self) -> bool {
        self.owners.load(Ordering::SeqCst) != 0
    }
    pub(crate) fn stop_receiver(&self) -> watch::Receiver<bool> {
        self.stopped.subscribe()
    }
    /// Invoke immediately before Hyper may dispatch, not after headers arrive.
    pub(crate) fn may_dispatch(&self) -> bool {
        let mut receipt = self.receipt.lock().unwrap_or_else(|p| p.into_inner());
        if receipt.end.is_some() || receipt.resources == HttpResources::Released {
            return false;
        }
        receipt.dispatch = HttpDispatch::MayHaveDispatched;
        true
    }
    /// First terminal outcome wins; failures and cancellation never imply
    /// resource release without a separate tracked convergence acknowledgement.
    pub(crate) fn finish(&self, end: HttpExchangeEnd) -> HttpExchangeEnd {
        let mut receipt = self.receipt.lock().unwrap_or_else(|p| p.into_inner());
        if receipt.end.is_none() {
            receipt.end = Some(end);
        }
        let actual = receipt.end.expect("terminal outcome just set");
        drop(receipt);
        self.changed.notify_waiters();
        actual
    }
    /// Called only after attempt-owned body/helper convergence or after the
    /// containing scope pool and all tracked drivers/connects have converged.
    pub(crate) fn resources_converged(&self) -> bool {
        {
            let receipt = self.receipt.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                receipt.end.is_some(),
                "resource convergence requires a terminal outcome"
            );
        }
        // Only one caller drops the retained generation lease; another waiter
        // must not announce Released before that drop has completed.
        if self.release_started.swap(true, Ordering::SeqCst) {
            return self.snapshot().resources == HttpResources::Released;
        }
        let retained = {
            self.retained
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
        };
        drop(retained);
        let mut receipt = self.receipt.lock().unwrap_or_else(|p| p.into_inner());
        receipt.resources = HttpResources::Released;
        drop(receipt);
        self.changed.notify_waiters();
        true
    }
}
pub(crate) struct AttemptOwner(Arc<Attempt>);
impl Drop for AttemptOwner {
    fn drop(&mut self) {
        self.0.owners.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }
}
impl HttpExchangeControl for Attempt {
    fn stop_local(&self) {
        self.locally_stopped.store(true, Ordering::SeqCst);
        self.finish(HttpExchangeEnd::StoppedLocally);
        self.stopped.send_replace(true);
        let waker = self
            .body_waker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    fn snapshot(&self) -> HttpExchangeReceipt {
        self.receipt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn released(&self) -> BoxFuture<'_, HttpExchangeReceipt> {
        Box::pin(async move {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                // Register before the snapshot to prevent a lost wakeup when
                // convergence races the check; Notify supports many waiters.
                notified.as_mut().enable();
                let receipt = self.snapshot();
                if receipt.resources == HttpResources::Released {
                    return receipt;
                }
                if receipt.end == Some(HttpExchangeEnd::Eof) && !self.has_owner() {
                    let pool = self.pool.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    if let Some(pool) = pool {
                        pool.dials_idle().await;
                        // Recheck after awaiting: another owner or dial may
                        // have appeared while the pool was becoming idle.
                        if !self.has_owner()
                            && !pool.has_pending_dials()
                            && self.snapshot().end == Some(HttpExchangeEnd::Eof)
                        {
                            if self.resources_converged() {
                                continue;
                            }
                            notified.await;
                            continue;
                        }
                        if !self.has_owner() && pool.has_pending_dials() {
                            continue;
                        }
                    }
                }
                notified.await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_is_local_and_all_release_waiters_observe_convergence() {
        let attempt = Attempt::new();
        let mut stop = attempt.stop_receiver();
        assert!(attempt.may_dispatch());
        let a = attempt.clone();
        let b = attempt.clone();
        let first = tokio::spawn(async move { a.released().await });
        let second = tokio::spawn(async move { b.released().await });
        attempt.stop_local();
        stop.changed().await.unwrap();
        assert!(*stop.borrow());
        assert!(!attempt.may_dispatch());
        assert_eq!(attempt.snapshot().resources, HttpResources::Held);
        attempt.resources_converged();
        for receipt in [
            first.await.unwrap(),
            second.await.unwrap(),
            attempt.released().await,
        ] {
            assert_eq!(receipt.dispatch, HttpDispatch::MayHaveDispatched);
            assert_eq!(receipt.end, Some(HttpExchangeEnd::StoppedLocally));
            assert_eq!(receipt.resources, HttpResources::Released);
        }
        assert!(!attempt.may_dispatch());
    }

    #[test]
    #[should_panic(expected = "resource convergence requires a terminal outcome")]
    fn active_attempt_cannot_be_marked_released() {
        Attempt::new().resources_converged();
    }

    #[test]
    fn eof_and_failure_are_not_overwritten_by_later_stop() {
        for end in [HttpExchangeEnd::Eof, HttpExchangeEnd::Failed] {
            let attempt = Attempt::new();
            attempt.finish(end);
            attempt.stop_local();
            assert_eq!(attempt.snapshot().end, Some(end));
        }
    }
}
