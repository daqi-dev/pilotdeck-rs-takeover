//! Explicit product assembly for Host's managed model setup. It contains no
//! project credentials and does not choose a principal or a default authority.
use super::managed_project_jobs::{ProjectJobs, ProjectTurnAuthority, SnapshotJobPolicy};
use agentcpm_host::{
    ManagedModelTransport, TrustedModelBindingResolver, TrustedModelCatalogEntry,
    TrustedModelServices, TrustedModelSetup, TrustedProjectSessions,
};
use agentcpm_iface::{
    env::http_pull::HttpPullTransport,
    model_session::ModelSessionFactory,
    types::{Error, ErrorCategory},
};
use pilotdeck_http_transport::transport::ProxySettings;
use pilotdeck_model_iface::SharedPilotDeckModelFactory;
use pilotdeck_trusted_http_host::TrustedHttpRuntime;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

struct Assembled {
    runtime: Arc<TrustedHttpRuntime>,
    resolver: TrustedModelBindingResolver,
    projects: TrustedProjectSessions,
}

pub struct GatewayManagedModelSetup {
    proxy: Option<ProxySettings>,
    authority: Arc<dyn ProjectTurnAuthority>,
    executor: tokio::runtime::Handle,
    assembled: OnceLock<Assembled>,
    claimed: AtomicBool,
}
impl GatewayManagedModelSetup {
    /// All inputs come from the explicitly configured trusted product root.
    /// The proxy option is the same startup configuration used by Gateway.
    pub fn new(
        proxy: Option<ProxySettings>,
        authority: Arc<dyn ProjectTurnAuthority>,
        executor: tokio::runtime::Handle,
    ) -> Self {
        Self {
            proxy,
            authority,
            executor,
            assembled: OnceLock::new(),
            claimed: AtomicBool::new(false),
        }
    }
    /// Available after transport construction. This is not a Host-ready or
    /// full installation receipt; callers must wait for Host readiness and
    /// the returned service still verifies the live Host at each operation.
    pub fn project_sessions(&self) -> Result<TrustedProjectSessions, Error> {
        self.assembled
            .get()
            .map(|assembled| assembled.projects.clone())
            .ok_or_else(setup_error)
    }
}
impl TrustedModelSetup for GatewayManagedModelSetup {
    fn uses_project_sessions(&self) -> bool {
        true
    }
    fn catalog(&self) -> Result<Vec<TrustedModelCatalogEntry>, Error> {
        Ok(Vec::new())
    }
    fn transport(&self, services: TrustedModelServices) -> Result<ManagedModelTransport, Error> {
        // Win the one-time claim before constructing any runtime or network
        // configuration. A panic leaves setup unavailable rather than
        // permitting an untracked duplicate lifecycle.
        claim_setup(&self.claimed)?;
        let resolver = services.bindings.clone();
        let projects = services.project_sessions();
        let runtime = Arc::new(TrustedHttpRuntime::new(services, self.proxy.clone()));
        let transport = runtime.managed_transport();
        self.assembled
            .set(Assembled {
                runtime,
                resolver,
                projects,
            })
            .map_err(|_| setup_error())?;
        Ok(transport)
    }
    fn factory(
        &self,
        transport: Arc<dyn HttpPullTransport>,
    ) -> Result<Arc<dyn ModelSessionFactory>, Error> {
        let assembled = self.assembled.get().ok_or_else(setup_error)?;
        let policy = Arc::new(SnapshotJobPolicy {
            runtime: assembled.runtime.clone(),
            authority: self.authority.clone(),
        });
        let jobs = Arc::new(ProjectJobs::new(
            assembled.runtime.clone(),
            assembled.resolver.clone(),
            policy,
        ));
        Ok(Arc::new(SharedPilotDeckModelFactory {
            jobs,
            transport,
            executor: self.executor.clone(),
        }))
    }
}
fn setup_error() -> Error {
    Error::new(
        ErrorCategory::Internal,
        -32603,
        "managed model assembly is unavailable",
    )
}

fn claim_setup(claimed: &AtomicBool) -> Result<(), Error> {
    claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| setup_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_setup_claims_construct_at_most_one_runtime() {
        let claim = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let claim = claim.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                claim_setup(&claim).is_ok()
            }));
        }
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        assert!(claim_setup(&claim).is_err());
    }
}
