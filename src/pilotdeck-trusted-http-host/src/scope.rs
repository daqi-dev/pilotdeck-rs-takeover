//! Job/generation-bound HTTP pools and shutdown ownership, draft.
use crate::{
    attempt::Attempt,
    bridge::ScopeDialer,
    prepared::{PreparedGrant, Revocable},
};
use agentcpm_host::{ManagedModelLifecycle, TrustedEgressDialer, TrustedEgressPool};
use agentcpm_iface::BoxFuture;
use agentcpm_iface::env::http_pull::{
    HttpExchangeControl, HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPreparationIssuer,
    HttpPullContext, HttpRequestPreparation, HttpResources, HttpScopeCleanup,
};
use pilotdeck_http_transport::{
    executor::ScopeExecutor,
    transport::{HyperModelTransport, NetworkConfig, ProxySettings},
};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

pub(crate) struct Scope {
    pub(crate) pool: TrustedEgressPool,
    context: HttpPullContext,
    pub(crate) executor: ScopeExecutor,
    client: Mutex<Option<HyperModelTransport>>,
    attempts: Mutex<ScopeAttempts>,
    stop_completion: StopCompletion,
}
struct ScopeAttempts {
    stopped: bool,
    values: Vec<Arc<Attempt>>,
    grants: Vec<Weak<dyn Revocable>>,
    preparations: usize,
}
/// One caller owns all revocation/destructor work. Duplicate stops may return
/// early but cannot make cleanup report Released until the owner finishes.
#[derive(Default)]
struct StopCompletion {
    started: AtomicBool,
    finished: AtomicBool,
}
impl StopCompletion {
    fn run_once(&self, cleanup: impl FnOnce()) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        cleanup();
        self.finished.store(true, Ordering::SeqCst);
    }
    fn resources(&self) -> HttpResources {
        if self.finished.load(Ordering::SeqCst) {
            HttpResources::Released
        } else {
            HttpResources::Held
        }
    }
}

impl Scope {
    pub(crate) fn client(&self) -> Option<HyperModelTransport> {
        self.client
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub(crate) fn register(&self, attempt: Arc<Attempt>) -> Result<(), HttpFailure> {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        if attempts.stopped {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Cancelled,
                "model scope stopped",
            ));
        }
        attempt.attach_pool(self.pool.clone());
        attempts.values.push(attempt);
        Ok(())
    }
    /// Must register before issuing a token, so close cannot miss a prepared
    /// token even when no HTTP attempt has started yet.
    pub(crate) fn begin_preparation(self: &Arc<Self>) -> Result<PreparationOwner, HttpFailure> {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        if attempts.stopped {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Cancelled,
                "model scope stopped",
            ));
        }
        attempts.preparations += 1;
        Ok(PreparationOwner(self.clone()))
    }
    pub(crate) fn register_revocable<T: Revocable + 'static>(
        &self,
        item: &Arc<T>,
    ) -> Result<(), HttpFailure> {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        if attempts.stopped {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Cancelled,
                "model scope stopped",
            ));
        }
        attempts.grants.retain(|item| item.strong_count() > 0);
        let item: Arc<dyn Revocable> = item.clone();
        attempts.grants.push(Arc::downgrade(&item));
        Ok(())
    }
    pub(crate) fn register_grant(&self, grant: &Arc<PreparedGrant>) -> Result<(), HttpFailure> {
        self.register_revocable(grant)
    }
    /// Registration and capability issuance linearize with stop on one lock.
    pub(crate) fn issue_grant(
        &self,
        issuer: &HttpPreparationIssuer,
        grant: Arc<PreparedGrant>,
    ) -> Result<HttpRequestPreparation, HttpFailure> {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        if attempts.stopped {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Cancelled,
                "model scope stopped",
            ));
        }
        attempts.grants.retain(|item| item.strong_count() > 0);
        let item: Arc<dyn Revocable> = grant.clone();
        attempts.grants.push(Arc::downgrade(&item));
        Ok(issuer.issue(grant))
    }
    /// Fence new owners/registrations without waking callbacks. Called while
    /// the manager linearizes close; resource cleanup follows outside locks.
    fn fence(&self) {
        self.attempts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
    }
    pub(crate) fn stop(&self) {
        self.stop_completion.run_once(|| {
            let (attempts, grants) = {
                let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
                attempts.stopped = true;
                (
                    attempts.values.clone(),
                    attempts
                        .grants
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect::<Vec<_>>(),
                )
            };
            for grant in grants {
                grant.revoke();
            }
            for attempt in attempts {
                attempt.stop_local();
            }
            let client = { self.client.lock().unwrap_or_else(|p| p.into_inner()).take() };
            drop(client);
            self.pool.stop_local();
            self.executor.stop();
        });
    }
    pub(crate) fn is_released(&self) -> bool {
        if self.stop_completion.resources() != HttpResources::Released
            || !self.pool.is_released()
            || !self.executor.is_released()
        {
            return false;
        }
        let attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values
            .clone();
        for attempt in attempts {
            if !attempt.has_owner() && attempt.snapshot().end.is_some() {
                attempt.resources_converged();
            }
        }
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        attempts.values.retain(|attempt| {
            attempt.snapshot().resources != agentcpm_iface::env::http_pull::HttpResources::Released
        });
        attempts.values.is_empty() && attempts.preparations == 0
    }
}

pub(crate) struct PreparationOwner(Arc<Scope>);
impl Drop for PreparationOwner {
    fn drop(&mut self) {
        let mut attempts = self.0.attempts.lock().unwrap_or_else(|p| p.into_inner());
        attempts.preparations -= 1;
    }
}
impl HttpScopeCleanup for Scope {
    fn resources(&self) -> HttpResources {
        if self.is_released() {
            HttpResources::Released
        } else {
            HttpResources::Held
        }
    }
    fn released(&self) -> BoxFuture<'_, HttpResources> {
        Box::pin(async move {
            // Pool's async wait covers background DNS and owned sockets.
            // Hyper JoinHandles have no clonable waiter; periodically recheck
            // their actual completion and outstanding attempt owners.
            self.pool.released().await;
            while !self.is_released() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            HttpResources::Released
        })
    }
}

fn registry_covers_material(registry: &agentcpm_core::guard::SecretRegistry, value: &str) -> bool {
    value != "***" && registry.redact(value) == "***"
}

/// A tombstone and in-flight creation fence for one Host-verified context.
struct CreationGate {
    context: HttpPullContext,
    closed: AtomicBool,
    active: AtomicUsize,
    changed: Notify,
}
impl CreationGate {
    fn new(context: HttpPullContext) -> Arc<Self> {
        Arc::new(Self {
            context,
            closed: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            changed: Notify::new(),
        })
    }
    fn idle(&self) -> bool {
        self.active.load(Ordering::SeqCst) == 0
    }
    async fn wait_idle(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.idle() {
                return;
            }
            notified.await;
        }
    }
}
struct CreationOwner(Arc<CreationGate>);
pub(crate) struct CreationReservation {
    gate: Arc<CreationGate>,
    _owner: CreationOwner,
}
impl CreationReservation {
    /// A continuation fence is derived only from an already counted owner.
    pub(crate) fn fork(&self) -> Self {
        self.gate.active.fetch_add(1, Ordering::SeqCst);
        Self {
            gate: self.gate.clone(),
            _owner: CreationOwner(self.gate.clone()),
        }
    }
}
impl Drop for CreationOwner {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }
}
fn same_context(a: &HttpPullContext, b: &HttpPullContext) -> bool {
    a.session_id == b.session_id
        && a.model_session_id == b.model_session_id
        && a.job_id == b.job_id
        && a.principal == b.principal
        && a.deadline == b.deadline
        && a.cancel.same_source(&b.cancel)
}
struct CombinedCleanup {
    gate: Arc<CreationGate>,
    scope: Option<Arc<Scope>>,
}
impl HttpScopeCleanup for CombinedCleanup {
    fn resources(&self) -> HttpResources {
        if self.gate.idle() && self.scope.as_ref().is_none_or(|scope| scope.is_released()) {
            HttpResources::Released
        } else {
            HttpResources::Held
        }
    }
    fn released(&self) -> BoxFuture<'_, HttpResources> {
        Box::pin(async move {
            self.gate.wait_idle().await;
            if let Some(scope) = &self.scope {
                scope.released().await;
            }
            HttpResources::Released
        })
    }
}
struct State {
    stopped: bool,
    scopes: Vec<Arc<Scope>>,
    gates: Vec<Arc<CreationGate>>,
}
impl State {
    fn begin_creation(
        &mut self,
        context: &HttpPullContext,
    ) -> Result<(Arc<CreationGate>, CreationOwner), HttpFailure> {
        if self.stopped {
            return Err(stopped());
        }
        let gate = match self
            .gates
            .iter()
            .find(|gate| same_context(&gate.context, context))
        {
            Some(gate) => gate.clone(),
            None => {
                let gate = CreationGate::new(context.clone());
                self.gates.push(gate.clone());
                gate
            }
        };
        if gate.closed.load(Ordering::SeqCst) {
            return Err(stopped());
        }
        gate.active.fetch_add(1, Ordering::SeqCst);
        Ok((gate.clone(), CreationOwner(gate)))
    }
    fn close_context(
        &mut self,
        context: &HttpPullContext,
    ) -> (Arc<CreationGate>, Option<Arc<Scope>>) {
        let gate = match self
            .gates
            .iter()
            .find(|gate| same_context(&gate.context, context))
        {
            Some(gate) => gate.clone(),
            None => {
                let gate = CreationGate::new(context.clone());
                self.gates.push(gate.clone());
                gate
            }
        };
        gate.closed.store(true, Ordering::SeqCst);
        let scope = self
            .scopes
            .iter()
            .find(|scope| same_context(&scope.context, context))
            .cloned();
        (gate, scope)
    }
}
pub(crate) struct ScopeManager {
    dialer: TrustedEgressDialer,
    network: NetworkConfig,
    state: Mutex<State>,
}
impl ScopeManager {
    /// The trusted caller supplies its explicit project/turn secret inventory.
    /// Add the exact frozen proxy material through the existing parser's
    /// coverage traversal, rather than re-reading environment or re-parsing.
    pub(crate) fn secret_registry(
        &self,
        mut values: Vec<String>,
    ) -> Arc<agentcpm_core::guard::SecretRegistry> {
        self.network.proxy_secrets_covered(|value| {
            values.push(value.to_owned());
            true
        });
        Arc::new(agentcpm_core::guard::SecretRegistry::new(
            values
                .into_iter()
                .enumerate()
                .map(|(index, value)| (format!("managed/{index}"), value)),
        ))
    }
    pub(crate) fn new(dialer: TrustedEgressDialer, proxy: Option<ProxySettings>) -> Arc<Self> {
        Arc::new(Self {
            dialer,
            network: NetworkConfig::from_settings(proxy.as_ref()),
            state: Mutex::new(State {
                stopped: false,
                scopes: Vec::new(),
                gates: Vec::new(),
            }),
        })
    }
    /// Creation is pure metadata; the bridge validates context and mints the
    /// pool identity. Never key authorization on caller-supplied strings.
    pub(crate) fn proxy_secrets_covered(
        &self,
        registry: &agentcpm_core::guard::SecretRegistry,
    ) -> bool {
        self.network
            .proxy_secrets_covered(|value| registry_covers_material(registry, value))
    }
    pub(crate) fn reserve_creation(
        &self,
        context: &HttpPullContext,
    ) -> Result<CreationReservation, HttpFailure> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (gate, owner) = state.begin_creation(context)?;
        Ok(CreationReservation {
            gate,
            _owner: owner,
        })
    }
    pub(crate) fn scope(
        &self,
        context: HttpPullContext,
    ) -> Result<(Arc<Scope>, PreparationOwner), HttpFailure> {
        let reservation = self.reserve_creation(&context)?;
        self.scope_reserved(context, reservation)
    }
    pub(crate) fn scope_reserved(
        &self,
        context: HttpPullContext,
        reservation: CreationReservation,
    ) -> Result<(Arc<Scope>, PreparationOwner), HttpFailure> {
        if !same_context(&reservation.gate.context, &context) {
            return Err(stopped());
        }
        let CreationReservation {
            gate,
            _owner: _creation,
        } = reservation;
        if gate.closed.load(Ordering::SeqCst) {
            return Err(stopped());
        }
        let pool = self.dialer.pool(context.clone())?;
        let scope = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.stopped || gate.closed.load(Ordering::SeqCst) {
                drop(state);
                pool.stop_local();
                return Err(stopped());
            }
            if let Some(scope) = state
                .scopes
                .iter()
                .find(|scope| scope.pool.same_scope(&pool))
            {
                scope.clone()
            } else {
                let executor = ScopeExecutor::default();
                let client = HyperModelTransport::for_scope(
                    Arc::new(ScopeDialer(pool.clone())),
                    &self.network,
                    executor.clone(),
                );
                let scope = Arc::new(Scope {
                    pool,
                    context,
                    executor,
                    client: Mutex::new(Some(client)),
                    attempts: Mutex::new(ScopeAttempts {
                        stopped: false,
                        values: Vec::new(),
                        grants: Vec::new(),
                        preparations: 0,
                    }),
                    stop_completion: StopCompletion::default(),
                });
                state.scopes.push(scope.clone());
                scope
            }
        };
        // Transfer the creation fence to a scope preparation owner without a
        // gap; caller holds it until its secrets or attempt are registered.
        let preparation = scope.begin_preparation()?;
        Ok((scope, preparation))
    }

    /// Host wrapper has already verified the original context even after
    /// revocation. Empty close still records a tombstone and fences pending
    /// creations; it does not mint a pool or perform network I/O.
    pub(crate) fn cleanup_scope(
        &self,
        context: &HttpPullContext,
    ) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        let (gate, scope) = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let (gate, scope) = state.close_context(context);
            if let Some(scope) = &scope {
                scope.fence();
            }
            (gate, scope)
        };
        if let Some(scope) = &scope {
            scope.stop();
        }
        Ok(Arc::new(CombinedCleanup { gate, scope }))
    }
}
fn stopped() -> HttpFailure {
    HttpFailure::new(
        HttpFailurePhase::Scope,
        HttpFailureKind::Cancelled,
        "model scope stopped",
    )
}
impl ManagedModelLifecycle for ScopeManager {
    fn stop(&self) {
        let scopes = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stopped = true;
            for gate in &state.gates {
                gate.closed.store(true, Ordering::SeqCst);
            }
            for scope in &state.scopes {
                scope.fence();
            }
            state.scopes.clone()
        };
        for scope in scopes {
            scope.stop();
        }
    }
    fn is_released(&self) -> bool {
        let scopes = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if !state.stopped || state.gates.iter().any(|gate| !gate.idle()) {
                return false;
            }
            state.scopes.clone()
        };
        let released: Vec<_> = scopes
            .into_iter()
            .filter(|scope| scope.is_released())
            .collect();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .scopes
            .retain(|scope| !released.iter().any(|done| Arc::ptr_eq(scope, done)));
        state.scopes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_core::guard::SecretRegistry;
    #[test]
    fn proxy_material_requires_full_redaction_not_partial_match() {
        let prefix = SecretRegistry::new([("test/a".into(), "abc".into())]);
        let suffix = SecretRegistry::new([("test/a".into(), "def".into())]);
        let full = SecretRegistry::new([("test/a".into(), "abcdef".into())]);
        assert!(!registry_covers_material(&prefix, "abcdef"));
        assert!(!registry_covers_material(&suffix, "abcdef"));
        assert!(registry_covers_material(&full, "abcdef"));
    }
    fn context() -> HttpPullContext {
        HttpPullContext {
            session_id: "s".into(),
            model_session_id: "m".into(),
            job_id: "j".into(),
            principal: "p".into(),
            deadline: None,
            cancel: Default::default(),
        }
    }
    #[tokio::test]
    async fn empty_close_fences_inflight_creation_and_prevents_late_scope() {
        let context = context();
        let mut state = State {
            stopped: false,
            scopes: Vec::new(),
            gates: Vec::new(),
        };
        let (gate, creating) = state.begin_creation(&context).unwrap();
        let outer = CreationReservation {
            gate: gate.clone(),
            _owner: creating,
        };
        let inner = outer.fork();
        let (closed, scope) = state.close_context(&context);
        assert!(Arc::ptr_eq(&gate, &closed));
        assert!(scope.is_none());
        let cleanup = Arc::new(CombinedCleanup {
            gate: closed,
            scope: None,
        });
        assert_eq!(cleanup.resources(), HttpResources::Held);
        assert!(state.begin_creation(&context).is_err());
        let a = cleanup.clone();
        let b = cleanup.clone();
        let first = tokio::spawn(async move { a.released().await });
        let second = tokio::spawn(async move { b.released().await });
        drop(inner);
        assert_eq!(cleanup.resources(), HttpResources::Held);
        drop(outer);
        assert_eq!(first.await.unwrap(), HttpResources::Released);
        assert_eq!(second.await.unwrap(), HttpResources::Released);
        assert_eq!(cleanup.resources(), HttpResources::Released);
        assert!(state.begin_creation(&context).is_err());
        // Same strings without the original cancel identity do not select or
        // reopen the original tombstone. Host guard still authorizes new jobs.
        assert!(state.begin_creation(&self::context()).is_ok());
    }
    #[test]
    fn close_before_any_creation_still_makes_a_tombstone() {
        let context = context();
        let mut state = State {
            stopped: false,
            scopes: Vec::new(),
            gates: Vec::new(),
        };
        let (gate, scope) = state.close_context(&context);
        assert!(scope.is_none());
        assert!(gate.idle());
        assert!(state.begin_creation(&context).is_err());
    }
    #[test]
    fn repeated_stop_cannot_release_while_the_first_destructor_is_blocked() {
        use crate::prepared::{Revocable, ScopeOwned};
        use std::sync::mpsc;
        struct BlockingDrop {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Drop for BlockingDrop {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let shell = ScopeOwned::new(BlockingDrop {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let completion = Arc::new(StopCompletion::default());
        let owner = completion.clone();
        let thread = std::thread::spawn(move || owner.run_once(|| shell.revoke()));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        completion.run_once(|| panic!("duplicate stop must not own cleanup"));
        assert_eq!(completion.resources(), HttpResources::Held);
        release_tx.send(()).unwrap();
        thread.join().unwrap();
        assert_eq!(completion.resources(), HttpResources::Released);
    }
}
