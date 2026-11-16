//! Explicit opt-in scoped resident Kernel profile and `ManagedSessionBinder`
//! for `fb54c74a8`-compatible hosts (atomic scheduler + scoped tool/model/context
//! features, without `LOOP_SETTLEMENT_FEATURE`).
use super::{
    DeploymentKernelConfig, KernelTurnOptions,
    managed_project_session::{
        ManagedFuture, ManagedSessionBinder, PreparedManagedSession, ProjectCatalogPolicy,
        prepare_inputs,
    },
};
use crate::runtime::{
    project::{RuntimeError, RuntimeLease},
    session::SessionContext,
    turn::kernel_client::{
        KernelConnection,
        scoped::{ResidentSignalState, ScopedSessionConfig, prepare_scoped_session},
    },
};
use agentcpm_scheduler_protocol as p;
use std::{sync::Arc, time::Duration};

pub const KERNEL_EXECUTION_PROFILE_ENV: &str = "PILOTDECK_KERNEL_EXECUTION_PROFILE";

/// Explicit Kernel execution profile selection.
///
/// The default remains strict `FullLate` (`LOOP_SETTLEMENT_FEATURE` +
/// `SETTLEMENT_DATA_FEATURE`). `ScopedResident` must be explicitly opted into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KernelExecutionProfile {
    #[default]
    FullLate,
    ScopedResident,
    Legacy,
}

/// Fail-closed parser for `PILOTDECK_KERNEL_EXECUTION_PROFILE`.
pub fn parse_kernel_profile(value: Option<&str>) -> Result<KernelExecutionProfile, RuntimeError> {
    match parse_explicit_kernel_profile(value)? {
        None => Ok(KernelExecutionProfile::FullLate),
        Some(profile) => Ok(profile),
    }
}

/// Fail-closed parser that distinguishes unset (`None`) from explicit profile overrides.
pub fn parse_explicit_kernel_profile(
    value: Option<&str>,
) -> Result<Option<KernelExecutionProfile>, RuntimeError> {
    match value {
        None => Ok(None),
        Some("full-late") => Ok(Some(KernelExecutionProfile::FullLate)),
        Some("scoped-resident") => Ok(Some(KernelExecutionProfile::ScopedResident)),
        Some("legacy") => Ok(Some(KernelExecutionProfile::Legacy)),
        Some(_) => Err(RuntimeError::new(
            "Kernel execution profile (PILOTDECK_KERNEL_EXECUTION_PROFILE) must be full-late, scoped-resident, or legacy.",
        )),
    }
}

/// Read and validate `PILOTDECK_KERNEL_EXECUTION_PROFILE` from an explicit env map.
pub fn kernel_profile_from_env(
    env: &crate::config::Env,
) -> Result<KernelExecutionProfile, RuntimeError> {
    parse_kernel_profile(env.get(KERNEL_EXECUTION_PROFILE_ENV).map(String::as_str))
}

/// Explicit profile descriptor for `fb54c74a8`-compatible scoped resident execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScopedResidentKernelProfile;

impl ScopedResidentKernelProfile {
    pub const REQUIRED_FEATURES: &'static [&'static str] = &[
        p::INPUT_LIMITS_FEATURE,
        p::OUTPUT_DEMAND_FEATURE,
        p::tool_batch::FEATURE,
        p::model_session_resources::FEATURE,
        p::context_history::FEATURE,
    ];

    pub fn required_features() -> Vec<String> {
        Self::REQUIRED_FEATURES
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect()
    }

    pub const fn claims_full_late_settlement() -> bool {
        false
    }

    pub const fn profile(self) -> KernelExecutionProfile {
        KernelExecutionProfile::ScopedResident
    }

    pub fn validate_required_features(features: &[String]) -> Result<(), RuntimeError> {
        if features
            .iter()
            .any(|feature| feature == p::LOOP_SETTLEMENT_FEATURE)
        {
            return Err(RuntimeError::new(
                "ScopedResident profile does not claim LOOP_SETTLEMENT_FEATURE; use KernelSessionBinder for FullLate.",
            ));
        }
        for required in Self::REQUIRED_FEATURES {
            if !features.iter().any(|feature| feature == required) {
                return Err(RuntimeError::new(format!(
                    "ScopedResident profile requires `{required}`."
                )));
            }
        }
        Ok(())
    }
}

/// Authenticated scoped resident deployment binder (`fb54c74a8`-compatible).
/// Validates peer UID, per-connection UDS attestation, principal/tenant, atomic
/// scheduler, installed `default_loop_ref`/`input_controller_ref`, and the exact
/// `ScopedResidentKernelProfile::REQUIRED_FEATURES`.
#[derive(Clone)]
pub struct ScopedResidentSessionBinder {
    pub deployment: DeploymentKernelConfig,
    pub project_sessions: agentcpm_host::TrustedProjectSessions,
    pub catalog: Arc<dyn ProjectCatalogPolicy>,
    pub turn_options: KernelTurnOptions,
    pub loop_binding: String,
    pub controller_binding: String,
    pub required_features: Vec<String>,
    #[cfg_attr(not(feature = "kernel-resident"), allow(dead_code))]
    pub(crate) resident_shutdown: Option<Arc<ResidentSignalState>>,
}

impl ScopedResidentSessionBinder {
    pub fn new(
        deployment: DeploymentKernelConfig,
        project_sessions: agentcpm_host::TrustedProjectSessions,
        catalog: Arc<dyn ProjectCatalogPolicy>,
        turn_options: KernelTurnOptions,
        loop_binding: impl Into<String>,
        controller_binding: impl Into<String>,
    ) -> Self {
        Self {
            deployment,
            project_sessions,
            catalog,
            turn_options,
            loop_binding: loop_binding.into(),
            controller_binding: controller_binding.into(),
            required_features: ScopedResidentKernelProfile::required_features(),
            resident_shutdown: None,
        }
    }

    #[cfg(feature = "kernel-resident")]
    pub(crate) fn with_resident_shutdown(mut self, state: Arc<ResidentSignalState>) -> Self {
        self.resident_shutdown = Some(state);
        self
    }

    fn validate_config(&self) -> Result<(), RuntimeError> {
        validate_scoped_config(
            &self.deployment,
            &self.loop_binding,
            &self.controller_binding,
            &self.required_features,
        )
    }

    async fn connect(&self) -> Result<KernelConnection, RuntimeError> {
        connect_attested_with_config(
            &self.deployment,
            &self.loop_binding,
            &self.controller_binding,
            &self.required_features,
        )
        .await
    }
}

fn validate_scoped_config(
    deployment: &DeploymentKernelConfig,
    loop_binding: &str,
    controller_binding: &str,
    required_features: &[String],
) -> Result<(), RuntimeError> {
    deployment.validate()?;
    if loop_binding.is_empty() || controller_binding.is_empty() {
        return Err(RuntimeError::new(
            "ScopedResidentSessionBinder requires non-empty loop and controller bindings.",
        ));
    }
    ScopedResidentKernelProfile::validate_required_features(required_features)
}

async fn connect_attested_with_config(
    deployment: &DeploymentKernelConfig,
    loop_binding: &str,
    controller_binding: &str,
    required_features: &[String],
) -> Result<KernelConnection, RuntimeError> {
    validate_scoped_config(
        deployment,
        loop_binding,
        controller_binding,
        required_features,
    )?;
    let connection = KernelConnection::connect_scoped_attested(
        &deployment.socket,
        Duration::from_millis(deployment.rpc_timeout_ms),
        Some(deployment.expected_uid),
        required_features,
        loop_binding,
        controller_binding,
    )
    .await
    .map_err(|e| RuntimeError::new(e.message))?;
    let identity = connection.attested();
    if identity.principal() != deployment.principal
        || identity.tenant() != Some(deployment.tenant.as_str())
    {
        return Err(RuntimeError::new(
            "Kernel authenticated connection does not match the explicitly configured deployment identity.",
        ));
    }
    Ok(connection)
}

impl ManagedSessionBinder for ScopedResidentSessionBinder {
    fn backend_owner(&self) -> pilotdeck_session_migration::BackendOwner {
        pilotdeck_session_migration::BackendOwner::KernelScopedResident
    }
    fn validate_installation(&self) -> ManagedFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move { self.connect().await.map(drop) })
    }

    fn prepare<'a>(
        &'a self,
        admitted: &'a RuntimeLease,
        context: &'a SessionContext,
    ) -> ManagedFuture<'a, Result<PreparedManagedSession, RuntimeError>> {
        Box::pin(async move {
            self.validate_config()?;
            let (owner, catalog) = prepare_inputs(admitted, self.catalog.as_ref())?;
            let options = (self.turn_options)(context, owner.clone())?;
            if options.loop_binding != self.loop_binding
                || options.controller_binding != self.controller_binding
                || options.module_id.is_empty()
                || options.cleanup_timeout.is_zero()
            {
                return Err(RuntimeError::new(
                    "Scoped Kernel turn options do not match the installed Native bindings or cleanup budget.",
                ));
            }
            let binder = self.clone();
            let projects = binder.project_sessions.clone();
            let config = ScopedSessionConfig {
                loop_binding_id: binder.loop_binding.clone(),
                controller_binding_id: binder.controller_binding.clone(),
                expected_loop_binding: None,
                expected_controller_binding: None,
                module_id: options.module_id,
                timeout: options.cleanup_timeout,
                consumer_demand: true,
                resident_shutdown: binder.resident_shutdown.clone(),
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn parse_kernel_profile_defaults_to_full_late_and_fails_closed_on_unknown() {
        assert_eq!(
            parse_kernel_profile(None).unwrap(),
            KernelExecutionProfile::FullLate
        );
        assert_eq!(
            parse_kernel_profile(Some("full-late")).unwrap(),
            KernelExecutionProfile::FullLate
        );
        assert_eq!(
            parse_kernel_profile(Some("scoped-resident")).unwrap(),
            KernelExecutionProfile::ScopedResident
        );
        assert_eq!(
            parse_kernel_profile(Some("legacy")).unwrap(),
            KernelExecutionProfile::Legacy
        );
        assert_eq!(parse_explicit_kernel_profile(None).unwrap(), None);
        assert_eq!(
            parse_explicit_kernel_profile(Some("legacy")).unwrap(),
            Some(KernelExecutionProfile::Legacy)
        );
        assert_eq!(
            ScopedResidentKernelProfile.profile(),
            KernelExecutionProfile::ScopedResident
        );
        for bad in [
            "",
            " ",
            "scoped",
            "full_late",
            "scoped_resident",
            "ScopedResident",
            " full-late ",
        ] {
            assert!(
                parse_kernel_profile(Some(bad)).is_err(),
                "unexpected acceptance for `{bad}`"
            );
        }

        let mut env = crate::config::Env::new();
        assert_eq!(
            kernel_profile_from_env(&env).unwrap(),
            KernelExecutionProfile::FullLate
        );
        env.insert(
            KERNEL_EXECUTION_PROFILE_ENV.into(),
            "scoped-resident".into(),
        );
        assert_eq!(
            kernel_profile_from_env(&env).unwrap(),
            KernelExecutionProfile::ScopedResident
        );
        env.insert(KERNEL_EXECUTION_PROFILE_ENV.into(), "bogus".into());
        assert!(kernel_profile_from_env(&env).is_err());
    }

    #[test]
    fn scoped_profile_requires_fb54c74a8_features_and_never_claims_full_late() {
        assert!(!ScopedResidentKernelProfile::claims_full_late_settlement());
        let features = ScopedResidentKernelProfile::required_features();
        for expected in [
            p::INPUT_LIMITS_FEATURE,
            p::OUTPUT_DEMAND_FEATURE,
            p::tool_batch::FEATURE,
            p::model_session_resources::FEATURE,
            p::context_history::FEATURE,
        ] {
            assert!(features.iter().any(|f| f == expected), "missing {expected}");
        }
        assert!(
            !features
                .iter()
                .any(|f| f == p::LOOP_SETTLEMENT_FEATURE || f == p::SETTLEMENT_DATA_FEATURE)
        );
        assert!(ScopedResidentKernelProfile::validate_required_features(&features).is_ok());

        let mut with_full_late = features.clone();
        with_full_late.push(p::LOOP_SETTLEMENT_FEATURE.into());
        assert!(ScopedResidentKernelProfile::validate_required_features(&with_full_late).is_err());

        for idx in 0..features.len() {
            let mut omitted = features.clone();
            omitted.remove(idx);
            assert!(ScopedResidentKernelProfile::validate_required_features(&omitted).is_err());
        }
    }

    async fn serve_once_initialize(socket: &std::path::Path, init_result: Value) {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap() > 0 {
                let req: Value = serde_json::from_str(&line).unwrap();
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": init_result,
                });
                writer
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
            }
        });
    }

    #[tokio::test]
    async fn scoped_binder_validates_uid_attestation_bindings_and_features_without_loop_settlement()
    {
        let short_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::path::PathBuf::from("/tmp").join(format!("sb-{}", &short_id[..12]));
        std::fs::create_dir_all(&dir).unwrap();
        let uid = rustix::process::geteuid().as_raw();
        let features = ScopedResidentKernelProfile::required_features();
        let good_result = json!({
            "abi_version": 2,
            "transport_security_required": true,
            "transport_security": {
                "kind": "unix",
                "attested": true,
                "principal_scope": "per-connection",
                "principal": "gateway",
                "tenant": "pilotdeck",
            },
            "server_capabilities": {
                "scheduler": {
                    "protocol": p::PROTOCOL,
                    "atomic_journal": true,
                    "default_loop_ref": "pilotdeck",
                    "input_controller_ref": "controller",
                    "features": features.clone(),
                }
            }
        });

        let sock_ok = dir.join("ok.s");
        serve_once_initialize(&sock_ok, good_result.clone()).await;
        let deployment = DeploymentKernelConfig {
            socket: sock_ok,
            expected_uid: uid,
            principal: "gateway".into(),
            tenant: "pilotdeck".into(),
            rpc_timeout_ms: 2000,
        };
        connect_attested_with_config(&deployment, "pilotdeck", "controller", &features)
            .await
            .expect("valid fb54c74a8 capability set without LOOP_SETTLEMENT_FEATURE must pass");

        // Wrong tenant fails closed.
        let sock_tenant = dir.join("tenant.s");
        serve_once_initialize(&sock_tenant, good_result.clone()).await;
        let bad_tenant = DeploymentKernelConfig {
            socket: sock_tenant,
            tenant: "other".into(),
            ..deployment.clone()
        };
        assert!(
            connect_attested_with_config(&bad_tenant, "pilotdeck", "controller", &features)
                .await
                .is_err()
        );

        // Wrong UID fails closed.
        let sock_uid = dir.join("uid.s");
        serve_once_initialize(&sock_uid, good_result).await;
        let bad_uid = DeploymentKernelConfig {
            socket: sock_uid,
            expected_uid: uid.wrapping_add(1),
            ..deployment.clone()
        };
        assert!(
            connect_attested_with_config(&bad_uid, "pilotdeck", "controller", &features)
                .await
                .is_err()
        );

        // Missing context_history feature fails closed.
        let mut missing_history = json!({
            "abi_version": 2,
            "transport_security_required": true,
            "transport_security": {
                "kind": "unix",
                "attested": true,
                "principal_scope": "per-connection",
                "principal": "gateway",
                "tenant": "pilotdeck",
            },
            "server_capabilities": {
                "scheduler": {
                    "protocol": p::PROTOCOL,
                    "atomic_journal": true,
                    "default_loop_ref": "pilotdeck",
                    "input_controller_ref": "controller",
                    "features": [
                        p::INPUT_LIMITS_FEATURE,
                        p::OUTPUT_DEMAND_FEATURE,
                        p::tool_batch::FEATURE,
                        p::model_session_resources::FEATURE,
                    ],
                }
            }
        });
        let sock_missing = dir.join("missing.s");
        serve_once_initialize(&sock_missing, missing_history.clone()).await;
        let bad_features = DeploymentKernelConfig {
            socket: sock_missing,
            ..deployment.clone()
        };
        assert!(
            connect_attested_with_config(&bad_features, "pilotdeck", "controller", &features)
                .await
                .is_err()
        );

        // Explicitly claiming LOOP_SETTLEMENT_FEATURE in scoped config fails closed.
        missing_history["server_capabilities"]["scheduler"]["features"] = json!(features);
        let sock_full = dir.join("full.s");
        serve_once_initialize(&sock_full, missing_history).await;
        let mut disallowed = features;
        disallowed.push(p::LOOP_SETTLEMENT_FEATURE.into());
        assert!(
            connect_attested_with_config(
                &DeploymentKernelConfig {
                    socket: sock_full,
                    ..deployment
                },
                "pilotdeck",
                "controller",
                &disallowed,
            )
            .await
            .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
