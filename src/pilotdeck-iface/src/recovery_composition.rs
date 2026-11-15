//! Trusted composition root only. Identity is an expectation, not an install receipt.
use crate::{context_codec::InstalledContextOperation, recovery_launch::RecoveryLaunch};
use agentcpm_core::scheduler::adapters::native::NativeSpec;
use agentcpm_host::{
    ExternalBackends, ResidentComposition, SchedulerComponent, SchedulerImplementation,
    TrustedRecoverySetup,
};
use agentcpm_scheduler_protocol::{self as p, context_operations as ops};

/// The root freezes the component and reviewed setup once, before constructing
/// the loop launch. Expected identity alone does not establish installation.
pub struct RecoveryInstallation {
    component: SchedulerComponent,
    setup: TrustedRecoverySetup,
    expected: InstalledContextOperation,
}
impl RecoveryInstallation {
    pub fn new(binding: p::Binding, launch: &RecoveryLaunch) -> p::Result<Self> {
        let (launch_binding, _) = crate::recovery_launch::parse_recovery_args(launch.args())?
            .ok_or_else(|| {
                p::Failure::external("CONTEXT_RECOVERY_CONFIG", "missing launch marker")
            })?;
        if launch_binding != binding.id {
            return Err(p::Failure::external(
                "CONTEXT_RECOVERY_CONFIG",
                "recovery launch binding conflict",
            ));
        }
        let spec = NativeSpec {
            program: launch.program().to_owned(),
            args: launch.args().to_vec(),
            sha256: launch.sha256().into(),
        };
        let component = SchedulerComponent {
            binding: binding.clone(),
            provider: SchedulerImplementation::Native {
                program: spec.program.clone(),
                args: spec.args.clone(),
                sha256: spec.sha256.clone(),
            },
            controller: false,
        };
        let effective = component.effective_binding()?;
        let setup = TrustedRecoverySetup {
            binding_ref: binding.id,
            contract_ref: ops::failure::TRUST_CONTRACT.into(),
            export_name: ops::Operation::RecoverFromModelError.name().into(),
            spec,
            reviewed_configuration: launch.configuration()?,
        };
        let expected = InstalledContextOperation {
            operation: ops::Operation::RecoverFromModelError,
            binding: effective.id,
            revision: effective.revision,
        };
        Ok(Self {
            component,
            setup,
            expected,
        })
    }
    pub fn expected(&self) -> &InstalledContextOperation {
        &self.expected
    }
    /// Consume the same frozen facts. Advertise only after resident startup has
    /// confirmed installation; pass other protected Context roles unchanged.
    pub fn compose(
        self,
        mut external: ExternalBackends,
        mut contexts: Vec<ops::ContextBinding>,
    ) -> p::Result<ResidentComposition> {
        let binding = &self.component.binding.id;
        if contexts.iter().any(|role| {
            role.operation == ops::Operation::RecoverFromModelError || &role.binding_ref == binding
        }) {
            return Err(p::Failure::external(
                "CONTEXT_RECOVERY_CONFIG",
                "recovery role conflict",
            ));
        }
        let assembly = external.scheduler_assembly.as_mut().ok_or_else(|| {
            p::Failure::external("CONTEXT_RECOVERY_CONFIG", "missing scheduler assembly")
        })?;
        if assembly
            .bindings
            .iter()
            .any(|old| &old.binding.id == binding)
        {
            return Err(p::Failure::external(
                "CONTEXT_RECOVERY_CONFIG",
                "duplicate recovery binding",
            ));
        }
        contexts.push(ops::ContextBinding {
            binding_ref: binding.clone(),
            operation: ops::Operation::RecoverFromModelError,
        });
        assembly.bindings.push(self.component);
        Ok(ResidentComposition::new(external)
            .with_context_bindings(contexts)
            .with_trusted_recovery(self.setup))
    }
}
