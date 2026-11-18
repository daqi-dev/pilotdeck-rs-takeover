//! Host-minted, revocable request preparation. Creation belongs exclusively to
//! the trusted model-header service; ordinary drivers only carry the token.
use crate::chain::ChainState;
use agentcpm_core::guard::SecretRegistry;
use agentcpm_host::TrustedModelBinding;
use agentcpm_iface::env::http_pull::{
    HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPullContext,
};
use std::sync::{Arc, Mutex};

/// The lease is supplied by the trusted product assembly and keeps the exact
/// admitted project generation alive; it must not be a caller's project path.
pub trait TrustedProjectLease: Send + Sync {}

pub(crate) type OriginCheck = Arc<dyn Fn(&hyper::Uri) -> Result<(), HttpFailure> + Send + Sync>;

pub(crate) struct PreparedFacts {
    pub(crate) chain: Arc<ChainState>,
    pub(crate) registry: Arc<SecretRegistry>,
    /// Retains the admitted generation until the attempt truly releases.
    pub(crate) _lease: Arc<dyn TrustedProjectLease>,
    pub(crate) authorize_origin: OriginCheck,
}

/// Token clones refer to this revocable shell. Closing the scope clears its
/// reference to secrets and lease, but each live attempt retains its own facts
/// until its request/body ownership actually ends.
pub(crate) trait Revocable: Send + Sync {
    fn revoke(&self);
}

/// Private revocable shell; ordinary owners may retain the shell after close,
/// but not the secret facts or project lease that were inside it.
pub(crate) struct ScopeOwned<T: Send + Sync> {
    inner: Mutex<Option<Arc<T>>>,
}
impl<T: Send + Sync> ScopeOwned<T> {
    pub(crate) fn new(value: T) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Some(Arc::new(value))),
        })
    }
    pub(crate) fn checkout(&self) -> Result<Arc<T>, HttpFailure> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(denied)
    }
}
impl<T: Send + Sync> Revocable for ScopeOwned<T> {
    fn revoke(&self) {
        let value = { self.inner.lock().unwrap_or_else(|p| p.into_inner()).take() };
        drop(value);
    }
}

pub(crate) struct PreparedGrant {
    context: HttpPullContext,
    binding: TrustedModelBinding,
    facts: Mutex<Option<Arc<PreparedFacts>>>,
}
impl PreparedGrant {
    pub(crate) fn new(
        context: HttpPullContext,
        binding: TrustedModelBinding,
        facts: PreparedFacts,
    ) -> Arc<Self> {
        Arc::new(Self {
            context,
            binding,
            facts: Mutex::new(Some(Arc::new(facts))),
        })
    }
    pub(crate) fn checkout(
        &self,
        context: &HttpPullContext,
        binding: &TrustedModelBinding,
    ) -> Result<Arc<PreparedFacts>, HttpFailure> {
        let same = self.context.session_id == context.session_id
            && self.context.model_session_id == context.model_session_id
            && self.context.job_id == context.job_id
            && self.context.principal == context.principal
            && self.context.cancel.same_source(&context.cancel)
            && self.context.deadline == context.deadline
            && &self.binding == binding;
        if !same {
            return Err(denied());
        }
        self.facts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(denied)
    }
}
impl Revocable for PreparedGrant {
    fn revoke(&self) {
        let facts = { self.facts.lock().unwrap_or_else(|p| p.into_inner()).take() };
        drop(facts);
    }
}
fn denied() -> HttpFailure {
    HttpFailure::new(
        HttpFailurePhase::Scope,
        HttpFailureKind::Denied,
        "invalid or revoked HTTP preparation",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn revocation_clears_the_shell_but_preserves_an_active_checkout() {
        let releases = Arc::new(AtomicUsize::new(0));
        let shell = ScopeOwned::new(Lease(releases.clone()));
        let driver_clone = shell.clone();
        let active = shell.checkout().unwrap();
        shell.revoke();
        assert!(driver_clone.checkout().is_err());
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        drop(active);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        // Keeping ordinary shell clones alive cannot keep the lease alive.
        drop(driver_clone);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }
}
