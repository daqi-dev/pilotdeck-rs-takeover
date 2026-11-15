//! Prepare-time inputs for the explicitly configured Kernel turn backend.
//! The returned owner and catalog must be handed directly to the same
//! connection's authenticated creation/claim/bind operation.
use super::project_model_owner::ProjectModelOwner;
use crate::runtime::project::{RuntimeError, RuntimeLease};
use agentcpm_host::TrustedModelCatalogEntry;
use std::sync::Arc;

pub type ManagedFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
pub type CloseManaged =
    Box<dyn FnOnce() -> ManagedFuture<'static, crate::runtime::project::RuntimeResult> + Send>;

/// An explicit trusted assembly may install one binder. It must use the
/// authenticated Kernel creation/claim/bind chain, and its close callback
/// must close the real Kernel session and await its resource receipt.
pub trait ManagedSessionBinder: Send + Sync {
    /// 必须在启动产品assembly前验证实际安装的Kernel能力和原认证通道。
    /// 不能以静态feature常量、on_ready或缺省Ok替代实际Store CAS证明。
    fn validate_installation(&self) -> ManagedFuture<'_, Result<(), RuntimeError>>;
    fn prepare<'a>(
        &'a self,
        admitted: &'a RuntimeLease,
        context: &'a crate::runtime::session::SessionContext,
    ) -> ManagedFuture<'a, Result<PreparedManagedSession, RuntimeError>>;
}

pub struct PreparedManagedSession {
    pub backend: Arc<dyn crate::runtime::turn::execution_backend::TurnExecutionBackend>,
    pub close: CloseManaged,
}

/// Trusted assembly must derive the approved catalog from the lease's exact
/// immutable project generation and the authenticated session's policy.
/// There is no default catalog or lookup by path, public ID, or request JSON.
pub trait ProjectCatalogPolicy: Send + Sync {
    fn approved_catalog(
        &self,
        lease: &RuntimeLease,
    ) -> Result<Vec<TrustedModelCatalogEntry>, RuntimeError>;
}

/// Authenticated product policy decides which exact provider/model pairs may
/// be presented to this session. The normalized project configuration is a
/// source of candidates, not authorization.
pub trait ProjectCatalogApproval: Send + Sync {
    fn approved(&self, provider: &str, model: &str) -> bool;
}

pub struct SnapshotCatalogPolicy {
    pub approval: Arc<dyn ProjectCatalogApproval>,
}

impl ProjectCatalogPolicy for SnapshotCatalogPolicy {
    fn approved_catalog(
        &self,
        lease: &RuntimeLease,
    ) -> Result<Vec<TrustedModelCatalogEntry>, RuntimeError> {
        let resources = lease
            .runtime()
            .resources()
            .get::<crate::runtime::project::ProjectResources>()
            .ok_or_else(|| RuntimeError::new("Native project resources are missing."))?;
        let snapshot = resources.context().snapshot();
        let config = snapshot
            .get("config")
            .and_then(|value| value.get("model"))
            .ok_or_else(|| RuntimeError::new("Project model configuration is missing."))?;
        catalog_from_config(config, self.approval.as_ref())
    }
}

fn catalog_from_config(
    config: &pilotdeck_protocol::JsValue,
    approval: &dyn ProjectCatalogApproval,
) -> Result<Vec<TrustedModelCatalogEntry>, RuntimeError> {
    use agentcpm_iface::model_session::ModelBinding;
    use pilotdeck_protocol::JsValue;
    let invalid = || RuntimeError::new("Project model catalog is invalid or unavailable.");
    let providers = config
        .get("providers")
        .and_then(JsValue::as_object)
        .ok_or_else(invalid)?;
    let mut catalog = Vec::new();
    for (provider_key, provider) in providers.own_entries() {
        let provider_key = provider_key.to_utf8().map_err(|_| invalid())?;
        if provider_key.is_empty() {
            return Err(invalid());
        }
        let models = provider
            .get("models")
            .and_then(JsValue::as_object)
            .ok_or_else(invalid)?;
        for (model_key, model) in models.own_entries() {
            let model_key = model_key.to_utf8().map_err(|_| invalid())?;
            if model_key.is_empty() {
                return Err(invalid());
            }
            if !approval.approved(&provider_key, &model_key) {
                continue;
            }
            let mut capabilities = Vec::new();
            if let Some(JsValue::Array(input)) =
                model.get("multimodal").and_then(|value| value.get("input"))
            {
                for item in input {
                    let text = item
                        .as_string()
                        .ok_or_else(invalid)?
                        .to_utf8()
                        .map_err(|_| invalid())?;
                    if !capabilities.contains(&text) {
                        capabilities.push(text);
                    }
                }
            }
            catalog.push(TrustedModelCatalogEntry {
                binding: ModelBinding::new(model_key, Some(provider_key.clone()), Vec::new()),
                capabilities,
            });
        }
    }
    if catalog.is_empty() {
        return Err(invalid());
    }
    Ok(catalog)
}

/// Called inside real ProjectSessionFactory.prepare after project.acquire.
/// The caller must immediately use both outputs in the same authenticated
/// Kernel session.create/claim/bind and register the resulting backend's real
/// RPC close and resource receipt in PreparedResources. Failure drops the
/// owner root; no session is created by this helper.
pub fn prepare_inputs(
    admitted: &RuntimeLease,
    policy: &dyn ProjectCatalogPolicy,
) -> Result<(Arc<ProjectModelOwner>, Vec<TrustedModelCatalogEntry>), RuntimeError> {
    let owner = Arc::new(ProjectModelOwner::new(admitted)?);
    let catalog = policy.approved_catalog(admitted)?;
    Ok((owner, catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Only(&'static str);
    impl ProjectCatalogApproval for Only {
        fn approved(&self, provider: &str, _: &str) -> bool {
            provider == self.0
        }
    }

    #[test]
    fn catalog_preserves_provider_scoped_duplicate_models_and_approval() {
        let config = pilotdeck_protocol::parse_json(r#"{"providers":{"a":{"models":{"same":{"multimodal":{"input":["text","image"]}}}},"b":{"models":{"same":{"multimodal":{"input":["text"]}}}}}}"#).unwrap();
        let a = catalog_from_config(&config, &Only("a")).unwrap();
        let b = catalog_from_config(&config, &Only("b")).unwrap();
        assert_eq!(a[0].binding.model, b[0].binding.model);
        assert_ne!(a[0].binding.endpoint_ref, b[0].binding.endpoint_ref);
        assert_eq!(a[0].capabilities, ["text", "image"]);
        assert!(catalog_from_config(&config, &Only("missing")).is_err());
    }
}
