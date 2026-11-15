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
    /// Fork and snapshot happen outside the owner mutex. Rechecking both the
    /// generation identity and epoch makes close win any race before commit.
    pub(crate) fn checkout(
        &self,
        expected_owner: &ProjectModelOwner,
    ) -> Result<ProjectModelLease, RuntimeError> {
        self.checkout_inner(expected_owner, || {})
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

fn closed() -> RuntimeError {
    RuntimeError::new("Session project generation is closed.")
}
