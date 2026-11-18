//! 受信产品组装入口。普通Gateway沿固定基线的单token部署域，身份由显式配置和真实UDS取证绑定。
use super::managed_project_session::{
    ManagedFuture, ManagedSessionBinder, PreparedManagedSession, ProjectCatalogPolicy,
    prepare_inputs,
};
use super::project_model_owner::ProjectModelOwner;
use crate::host::HostFuture;
use crate::runtime::{
    project::{RuntimeError, RuntimeLease},
    session::SessionContext,
    turn::kernel_client::{KernelConnection, KernelExecutionOptions},
};
use std::sync::Arc;

/// 这是组装方的受信能力，不是请求参数。多主体部署必须按真实认证来源安装
/// 对应供应者，不能把 SessionContext 的字符串或 body.principal 转成身份。
pub trait AuthenticatedKernelConnections: Send + Sync {
    fn connect(&self) -> HostFuture<KernelConnection>;
}
pub type KernelTurnOptions = Arc<
    dyn Fn(&SessionContext, Arc<ProjectModelOwner>) -> Result<KernelExecutionOptions, RuntimeError>
        + Send
        + Sync,
>;

/// 一个已选定的认证部署作用域。只支持明确配置的主体；不声称自动继承远端租户。
pub struct KernelSessionBinder {
    pub connections: Arc<dyn AuthenticatedKernelConnections>,
    pub project_sessions: agentcpm_host::TrustedProjectSessions,
    pub catalog: Arc<dyn ProjectCatalogPolicy>,
    pub turn_options: KernelTurnOptions,
    #[cfg_attr(not(feature = "kernel-resident"), allow(dead_code))]
    pub(crate) resident_shutdown:
        Option<Arc<crate::runtime::turn::kernel_client::scoped::ResidentSignalState>>,
}
impl ManagedSessionBinder for KernelSessionBinder {
    fn backend_owner(&self) -> pilotdeck_session_migration::BackendOwner {
        pilotdeck_session_migration::BackendOwner::KernelFullLate
    }
    fn validate_installation(&self) -> ManagedFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move {
            // 真实握手核实际已安装scheduler的CAS/feature与认证身份；不创建会话或owner。
            let _connection = self
                .connections
                .connect()
                .await
                .map_err(|e| RuntimeError::new(e.message))?;
            Ok(())
        })
    }
    fn prepare<'a>(
        &'a self,
        admitted: &'a RuntimeLease,
        context: &'a SessionContext,
    ) -> ManagedFuture<'a, Result<PreparedManagedSession, RuntimeError>> {
        Box::pin(async move {
            let connection = self
                .connections
                .connect()
                .await
                .map_err(|e| RuntimeError::new(e.message))?;
            let (owner, catalog) = prepare_inputs(admitted, self.catalog.as_ref())?;
            let options = (self.turn_options)(context, owner.clone())?;
            let backend = connection
                .create_project_session(&self.project_sessions, owner, catalog, options)
                .await
                .map_err(|e| RuntimeError::new(e.message))?;
            if let Some(state) = &self.resident_shutdown {
                backend.set_resident_shutdown(state.clone());
            }
            let close = backend.clone();
            Ok(PreparedManagedSession {
                backend,
                close: Box::new(move || {
                    Box::pin(async move {
                        close
                            .close()
                            .await
                            .map_err(|e| RuntimeError::new(e.message))
                    })
                }),
            })
        })
    }
}

/// 仅由组装程序读取的部署配置，不是Gateway或module-host请求字段。
/// 原普通Gateway/WS/API共享一个部署token，本配置给该安全域明确的Kernel身份。
/// module-host的四字段主体必须另行受信映射，不能使用本配置将其折叠为部署主体。
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentKernelConfig {
    pub socket: std::path::PathBuf,
    pub expected_uid: u32,
    pub principal: String,
    pub tenant: String,
    pub rpc_timeout_ms: u64,
}
impl DeploymentKernelConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if !self.socket.is_absolute()
            || self.rpc_timeout_ms == 0
            || self.principal.trim().is_empty()
            || self.principal.trim() != self.principal
            || self.tenant.trim().is_empty()
            || self.tenant.trim() != self.tenant
        {
            return Err(RuntimeError::new(
                "Kernel deployment requires an absolute UDS socket, explicit nonempty principal/tenant, and a positive RPC timeout.",
            ));
        }
        Ok(())
    }
    /// 正式宿主启动入口；不设置per-listener声明，不关闭任一strict门。
    pub fn host_options(
        &self,
        startup: agentcpm_host::StartupOptions,
    ) -> Result<agentcpm_host::EmbedOptions, RuntimeError> {
        self.validate()?;
        let mut options = agentcpm_host::EmbedOptions::new(
            startup,
            agentcpm_host::EmbedTransport::Uds(self.socket.clone()),
        );
        options.trusted_uds_identity = Some(
            agentcpm_host::TrustedUdsIdentityMapping::new(
                self.expected_uid,
                self.principal.clone(),
                self.tenant.clone(),
            )
            .map_err(|error| RuntimeError::new(error.to_string()))?,
        );
        // EmbedOptions默认两门均开启；这里也明确写入，避免配置含糊。
        options.require_attested_transport = true;
        options.require_attested_tenant = true;
        Ok(options)
    }
    fn verify(
        &self,
        identity: &crate::runtime::turn::kernel_client::AttestedConnection,
    ) -> Result<(), crate::host::HostError> {
        if identity.principal() != self.principal || identity.tenant() != Some(self.tenant.as_str())
        {
            return Err(crate::host::HostError::new(
                crate::host::HostErrorKind::Protocol,
                "Kernel authenticated connection does not match the explicitly configured deployment identity.",
            ));
        }
        Ok(())
    }
}
impl AuthenticatedKernelConnections for DeploymentKernelConfig {
    fn connect(&self) -> HostFuture<KernelConnection> {
        let config = self.clone();
        Box::pin(async move {
            config.validate().map_err(|error| {
                crate::host::HostError::new(crate::host::HostErrorKind::Session, error.message)
            })?;
            let connection = KernelConnection::connect_uds(
                &config.socket,
                std::time::Duration::from_millis(config.rpc_timeout_ms),
            )
            .await?;
            config.verify(connection.attested())?;
            Ok(connection)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn deployment() -> DeploymentKernelConfig {
        DeploymentKernelConfig {
            socket: std::path::PathBuf::from("/isolated/kernel.sock"),
            expected_uid: 501,
            principal: "gateway-deployment".into(),
            tenant: "pilotdeck".into(),
            rpc_timeout_ms: 1000,
        }
    }
    #[test]
    fn deployment_requires_explicit_identity_and_preserves_both_strict_gates() {
        let config = deployment();
        let options = config
            .host_options(agentcpm_host::StartupOptions {
                config: None,
                data_dir: "/isolated/data".into(),
                components_dir: "/isolated/components".into(),
            })
            .unwrap();
        assert!(options.require_attested_transport);
        assert!(options.require_attested_tenant);
        assert!(options.transport_attested_principal.is_none());
        assert!(!options.assume_single_principal);
        assert!(options.trusted_uds_identity.is_some());
        for bad in [
            DeploymentKernelConfig {
                tenant: String::new(),
                ..deployment()
            },
            DeploymentKernelConfig {
                principal: " gateway".into(),
                ..deployment()
            },
            DeploymentKernelConfig {
                socket: "relative.sock".into(),
                ..deployment()
            },
            DeploymentKernelConfig {
                rpc_timeout_ms: 0,
                ..deployment()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
    #[test]
    fn request_principal_and_relaxation_are_not_deployment_configuration() {
        let good = r#"{"socket":"/isolated/kernel.sock","expected_uid":501,"principal":"gateway","tenant":"deployment","rpc_timeout_ms":1000}"#;
        assert!(serde_json::from_str::<DeploymentKernelConfig>(good).is_ok());
        for field in [
            r#""require_attested_tenant":false"#,
            r#""body_principal":{"tenantId":"other"}"#,
            r#""assume_single_principal":true"#,
        ] {
            let bad = format!("{},{} }}", good.trim_end_matches('}'), field);
            assert!(serde_json::from_str::<DeploymentKernelConfig>(&bad).is_err());
        }
    }
}
