//! Test-only controlled P2/P3 Scheduler driver; never the production backend.
//! The product factory supplies the real lease and LiveSession supplies facts.
//! Delegates directly to the shared non-test scoped driver in `scoped.rs`.
use super::{
    scoped::{ScopedSessionConfig, prepare_scoped_session},
    *,
};
use crate::native_process::{
    ManagedFuture, ManagedSessionBinder, PreparedManagedSession, ProjectCatalogPolicy,
};
use crate::runtime::{
    project::{RuntimeError, RuntimeLease},
    session::SessionContext,
};

#[derive(Clone)]
pub(crate) struct FixtureBinder {
    pub socket: std::path::PathBuf,
    pub expected_principal: String,
    pub expected_tenant: String,
    pub timeout: Duration,
    pub required_features: Vec<String>,
    pub projects: agentcpm_host::TrustedProjectSessions,
    pub catalog: Arc<dyn ProjectCatalogPolicy>,
    pub loop_binding: p::Binding,
    pub controller_binding: p::Binding,
}

impl FixtureBinder {
    async fn connect(&self) -> Result<KernelConnection, RuntimeError> {
        let connection = KernelConnection::connect_scoped_attested(
            &self.socket,
            self.timeout,
            None,
            &self.required_features,
            &self.loop_binding.id,
            &self.controller_binding.id,
        )
        .await
        .map_err(|e| RuntimeError::new(e.message))?;
        if connection.attested().principal() != self.expected_principal
            || connection.attested().tenant() != Some(self.expected_tenant.as_str())
        {
            return Err(RuntimeError::new(
                "Fixture authenticated identity mismatch.",
            ));
        }
        Ok(connection)
    }
}

impl ManagedSessionBinder for FixtureBinder {
    fn validate_installation(&self) -> ManagedFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move { self.connect().await.map(drop) })
    }

    fn prepare<'a>(
        &'a self,
        admitted: &'a RuntimeLease,
        _: &'a SessionContext,
    ) -> ManagedFuture<'a, Result<PreparedManagedSession, RuntimeError>> {
        Box::pin(async move {
            let (owner, catalog) =
                crate::native_process::prepare_inputs(admitted, self.catalog.as_ref())?;
            let binder = self.clone();
            let config = ScopedSessionConfig {
                loop_binding_id: binder.loop_binding.id.clone(),
                controller_binding_id: binder.controller_binding.id.clone(),
                expected_loop_binding: Some(binder.loop_binding.clone()),
                expected_controller_binding: Some(binder.controller_binding.clone()),
                module_id: "p2-controlled-loop".into(),
                timeout: binder.timeout,
                consumer_demand: false,
                resident_shutdown: None,
            };
            let projects = binder.projects.clone();
            prepare_scoped_session(
                async move { binder.connect().await },
                projects,
                owner,
                catalog,
                config,
            )
            .await
        })
    }
}
