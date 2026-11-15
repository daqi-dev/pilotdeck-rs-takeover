//! Draft: trusted session-owner project-generation handoff.
//!
//! This module is not installed until Kernel supplies an authenticated
//! session-owner handoff. A token is a capability, not a session identifier.

mod turn_facts;
pub use turn_facts::{ProjectTurnAdmission, ProjectTurnBorrow};

use crate::runtime::project::{ProjectResources, RuntimeError, RuntimeLease};
use pilotdeck_protocol::JsValue;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

struct State {
    epoch: u64,
    root: Option<Arc<OwnerRoot>>,
    returned: Arc<AtomicBool>,
    turns: std::collections::BTreeMap<String, Arc<turn_facts::Record>>,
}

struct OwnerRoot {
    lease: Option<RuntimeLease>,
    returned: Arc<AtomicBool>,
}
impl Drop for OwnerRoot {
    fn drop(&mut self) {
        drop(self.lease.take());
        self.returned.store(true, Ordering::Release);
    }
}

/// Kept only by the trusted product session owner. Closing revokes issuance;
/// already issued job leases remain alive until their own resource receipt.
pub struct ProjectModelOwner {
    state: Arc<Mutex<State>>,
}

/// Opaque identity to be carried only through the future authenticated Host
/// handoff. It is never derived from a path, request, or public session ID.
#[derive(Clone)]
pub(crate) struct ProjectOwnerToken {
    state: Weak<Mutex<State>>,
    epoch: u64,
}

pub struct ProjectModelLease(RuntimeLease);

impl pilotdeck_trusted_http_host::TrustedProjectLease for ProjectModelLease {}

impl ProjectModelOwner {
    /// Called during real ProjectSessionFactory.prepare with its admitted lease.
    pub(crate) fn new(admitted: &RuntimeLease) -> Result<Self, RuntimeError> {
        let root = admitted.fork()?;
        let returned = Arc::new(AtomicBool::new(false));
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                epoch: 0,
                root: Some(Arc::new(OwnerRoot {
                    lease: Some(root),
                    returned: returned.clone(),
                })),
                returned,
                turns: Default::default(),
            })),
        })
    }

    /// This must only be installed by the trusted session owner into Kernel's
    /// future authenticated binding, never accepted from an ordinary caller.
    pub(crate) fn token(&self) -> Result<ProjectOwnerToken, RuntimeError> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.root.is_none() {
            return Err(closed());
        }
        Ok(ProjectOwnerToken {
            state: Arc::downgrade(&self.state),
            epoch: state.epoch,
        })
    }

    pub(crate) fn close(&self) {
        let (root, turns) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.epoch = state.epoch.wrapping_add(1);
            (state.root.take(), std::mem::take(&mut state.turns))
        };
        // Dropping real context/tool handles can execute destructors. Never
        // run these under the owner mutex. Existing scope borrows retain root.
        drop(turns);
        drop(root);
    }

    /// True only after the root lease destructor completed, including any
    /// checkout that temporarily cloned the root. Job forks are separately
    /// covered by their Host reservations and scope receipts.
    pub(crate) fn is_released(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.root.is_none() && state.returned.load(Ordering::Acquire)
    }
}

impl Drop for ProjectModelOwner {
    fn drop(&mut self) {
        self.close();
    }
}

impl agentcpm_host::TrustedProjectOwner for ProjectModelOwner {
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
    fn revoke(&self) {
        self.close();
    }
    fn is_released(&self) -> bool {
        ProjectModelOwner::is_released(self)
    }
}

impl ProjectOwnerToken {
    /// The managed scope must register every successfully forked lease even
    /// when revocation wins the subsequent epoch check. Returning the child
    /// with a false validity bit lets that scope release it with a receipt.
    pub(crate) fn checkout_for_managed(
        &self,
        expected_owner: &ProjectModelOwner,
    ) -> Result<(ProjectModelLease, bool), RuntimeError> {
        self.checkout_for_managed_inner(expected_owner, || {})
    }

    fn checkout_for_managed_inner(
        &self,
        expected_owner: &ProjectModelOwner,
        after_fork: impl FnOnce(),
    ) -> Result<(ProjectModelLease, bool), RuntimeError> {
        let owner = self.state.upgrade().ok_or_else(closed)?;
        if !Arc::ptr_eq(&owner, &expected_owner.state) {
            return Err(closed());
        }
        let root = {
            let state = owner.lock().unwrap_or_else(|e| e.into_inner());
            if state.epoch != self.epoch {
                return Err(closed());
            }
            state.root.clone().ok_or_else(closed)?
        };
        let child = root.lease.as_ref().expect("live owner root").fork()?;
        after_fork();
        let valid = {
            let state = owner.lock().unwrap_or_else(|e| e.into_inner());
            state.epoch == self.epoch
                && state
                    .root
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &root))
        };
        Ok((ProjectModelLease(child), valid))
    }

    /// Fork and snapshot happen outside the owner mutex. Rechecking both the
    /// generation identity and epoch makes close win any race before commit.
    pub(crate) fn checkout(
        &self,
        expected_owner: &ProjectModelOwner,
    ) -> Result<ProjectModelLease, RuntimeError> {
        self.checkout_inner(expected_owner, || {})
    }

    /// Register the real fork with its already-installed scope owner before
    /// the epoch recheck can fail. The callback owns the child on every path.
    pub(crate) fn checkout_tracked(
        &self,
        expected_owner: &ProjectModelOwner,
        install: impl FnOnce(ProjectModelLease) -> Arc<ProjectModelLease>,
    ) -> Result<Arc<ProjectModelLease>, RuntimeError> {
        let owner = self.state.upgrade().ok_or_else(closed)?;
        if !Arc::ptr_eq(&owner, &expected_owner.state) {
            return Err(closed());
        }
        let root = {
            let state = owner.lock().unwrap_or_else(|e| e.into_inner());
            if state.epoch != self.epoch {
                return Err(closed());
            }
            state.root.clone().ok_or_else(closed)?
        };
        let child = install(ProjectModelLease(
            root.lease.as_ref().expect("live owner root").fork()?,
        ));
        let valid = {
            let state = owner.lock().unwrap_or_else(|e| e.into_inner());
            state.epoch == self.epoch
                && state
                    .root
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &root))
        };
        if !valid {
            return Err(closed());
        }
        Ok(child)
    }
    #[cfg(test)]
    pub(crate) fn checkout_with_hook(
        &self,
        expected_owner: &ProjectModelOwner,
        after_clone: impl FnOnce(),
    ) -> Result<ProjectModelLease, RuntimeError> {
        self.checkout_inner(expected_owner, after_clone)
    }

    fn checkout_inner(
        &self,
        expected_owner: &ProjectModelOwner,
        after_clone: impl FnOnce(),
    ) -> Result<ProjectModelLease, RuntimeError> {
        let owner = self.state.upgrade().ok_or_else(closed)?;
        if !Arc::ptr_eq(&owner, &expected_owner.state) {
            return Err(closed());
        }
        let root = {
            let state = owner.lock().unwrap_or_else(|e| e.into_inner());
            if state.epoch != self.epoch {
                return Err(closed());
            }
            state.root.clone().ok_or_else(closed)?
        };
        after_clone();
        let child = root.lease.as_ref().expect("live owner root").fork()?;
        let valid = {
            let state = owner.lock().unwrap_or_else(|e| e.into_inner());
            state.epoch == self.epoch
                && state
                    .root
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &root))
        };
        if !valid {
            drop(child);
            return Err(closed());
        }
        Ok(ProjectModelLease(child))
    }
}

impl ProjectModelLease {
    pub(crate) async fn release(&mut self) -> Result<(), RuntimeError> {
        self.0.release().await
    }
    /// Read the router from this exact fork; no registry/path re-acquisition.
    pub(crate) fn router(
        &self,
    ) -> Result<Arc<crate::runtime::model::router::RouterRuntime>, RuntimeError> {
        project_router(&self.0)
    }
    pub(crate) fn begin_release(&mut self) -> Option<Arc<crate::runtime::project::ProjectRuntime>> {
        self.0.begin_release()
    }
    /// Secret-bearing. Only trusted assembly can use this value; do not hand it
    /// to ordinary model runtime or log it. The lease pins this exact snapshot.
    pub(crate) fn private_model_config(&self) -> Result<JsValue, RuntimeError> {
        let resources = self
            .0
            .runtime()
            .resources()
            .get::<ProjectResources>()
            .ok_or_else(|| RuntimeError::new("Native project resources are missing."))?;
        Ok(resources
            .context()
            .snapshot()
            .get("config")
            .and_then(|config| config.get("model"))
            .cloned()
            .unwrap_or(JsValue::Undefined))
    }
}

pub(crate) fn project_router(
    lease: &RuntimeLease,
) -> Result<Arc<crate::runtime::model::router::RouterRuntime>, RuntimeError> {
    use crate::runtime::project::{Resource, model::RouterResources};
    let resources = lease
        .runtime()
        .resources()
        .get::<ProjectResources>()
        .ok_or_else(|| RuntimeError::new("Native project resources are missing."))?;
    let context = resources.context();
    let family = context
        .resource(Resource::Router)
        .ok_or_else(|| RuntimeError::new("Native project router is missing."))?;
    let router = family
        .as_any()
        .downcast_ref::<RouterResources>()
        .ok_or_else(|| RuntimeError::new("Native project router has an unexpected type."))?;
    Ok(router.router.clone())
}
fn closed() -> RuntimeError {
    RuntimeError::new("Session project generation is closed.")
}

#[cfg(test)]
mod managed_checkout_tests {
    use super::*;
    use crate::runtime::project::{
        ProjectRuntimeRegistry, ResourceBundle, RuntimeResult, RuntimeState,
    };
    use std::path::{Path, PathBuf};

    #[tokio::test]
    async fn revoke_after_fork_returns_child_for_real_release() {
        let registry = ProjectRuntimeRegistry::new(
            PathBuf::from("/managed-checkout-fixture"),
            Arc::new(|_: &Path, _: &mut ResourceBundle| -> RuntimeResult { Ok(()) }),
        );
        let admitted = registry.acquire(None).unwrap();
        let runtime = admitted.runtime().clone();
        let owner = ProjectModelOwner::new(&admitted).unwrap();
        let token = owner.token().unwrap();
        let (mut child, valid) = token
            .checkout_for_managed_inner(&owner, || owner.close())
            .unwrap();
        assert!(!valid);
        // The successfully forked child remains owned after revocation; no
        // cancelled checkout silently drops it as an untracked async cleanup.
        assert_eq!(runtime.lease_count(), 2);
        drop(admitted);
        registry.invalidate(None);
        assert_eq!(runtime.lease_count(), 1);
        child.release().await.unwrap();
        assert_eq!(runtime.state(), RuntimeState::Disposed);
    }
}
