//! Frozen startup facts for the source-built Native Loop; not authority itself.
use crate::context_codec::InstalledContextOperation;
use agentcpm_scheduler_protocol::context_operations::Operation;
use pilotdeck_agent::ports::{PortError, PortResult};
use serde::{Deserialize, Serialize};

const MARKER: &str = "--pilotdeck-native-loop-v1";
const PROFILE: &str = "pilotdeck.native-loop.configuration.v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ContextRole {
    operation: Operation,
    binding: String,
    revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Configuration {
    profile: String,
    controller: String,
    controller_revision: String,
    process_cwd: String,
    contexts: Vec<ContextRole>,
}
/// Root constructs once from actual installed roles, never an Execute payload.
/// Kernel independently authenticates the executable, feature set and calls.
pub struct NativeLoopLaunch {
    args: Vec<String>,
}
impl NativeLoopLaunch {
    pub fn new(
        controller: String,
        controller_revision: String,
        process_cwd: String,
        contexts: &[InstalledContextOperation],
    ) -> PortResult<Self> {
        let config = Configuration {
            profile: PROFILE.into(),
            controller,
            controller_revision,
            process_cwd,
            contexts: contexts
                .iter()
                .map(|v| ContextRole {
                    operation: v.operation,
                    binding: v.binding.clone(),
                    revision: v.revision.clone(),
                })
                .collect(),
        };
        validate(&config)?;
        let json = serde_json::to_string(&config).map_err(|_| invalid())?;
        Ok(Self {
            args: vec![MARKER.into(), json],
        })
    }
    pub fn args(&self) -> &[String] {
        &self.args
    }
}
/// Immutable startup configuration is only a lookup hint to real root-installed
/// services; parsing does not authenticate the source or grant capabilities.
pub struct NativeLoopConfiguration {
    pub controller: String,
    pub controller_revision: String,
    pub process_cwd: String,
    pub contexts: Vec<InstalledContextOperation>,
}
pub fn parse(args: &[String]) -> PortResult<NativeLoopConfiguration> {
    if args.len() != 2 || args[0] != MARKER {
        return Err(invalid());
    }
    let config: Configuration = serde_json::from_str(&args[1]).map_err(|_| invalid())?;
    validate(&config)?;
    Ok(NativeLoopConfiguration {
        controller: config.controller,
        controller_revision: config.controller_revision,
        process_cwd: config.process_cwd,
        contexts: config
            .contexts
            .into_iter()
            .map(|v| InstalledContextOperation {
                operation: v.operation,
                binding: v.binding,
                revision: v.revision,
            })
            .collect(),
    })
}
fn invalid() -> PortError {
    PortError::InvalidResponse {
        message: "invalid Native Loop startup configuration".into(),
    }
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0')
}
fn validate(c: &Configuration) -> PortResult<()> {
    let required = [
        Operation::PrepareForModel,
        Operation::ApplyToolResults,
        Operation::RecoverFromModelError,
        Operation::CaptureTurn,
        Operation::TryAutoCompact,
    ];
    if c.profile != PROFILE
        || !valid_id(&c.controller)
        || !valid_id(&c.controller_revision)
        || !valid_id(&c.process_cwd)
        || !std::path::Path::new(&c.process_cwd).is_absolute()
        || c.contexts.len() != required.len()
        || required
            .iter()
            .any(|op| c.contexts.iter().filter(|v| v.operation == *op).count() != 1)
        || c.contexts
            .iter()
            .any(|v| !valid_id(&v.binding) || !valid_id(&v.revision))
        || c.contexts
            .iter()
            .enumerate()
            .any(|(i, v)| c.contexts[..i].iter().any(|old| old.binding == v.binding))
    {
        return Err(invalid());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn roles() -> Vec<InstalledContextOperation> {
        [
            Operation::PrepareForModel,
            Operation::ApplyToolResults,
            Operation::RecoverFromModelError,
            Operation::CaptureTurn,
            Operation::TryAutoCompact,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, operation)| InstalledContextOperation {
            operation,
            binding: format!("binding-{i}"),
            revision: format!("revision-{i}"),
        })
        .collect()
    }
    #[test]
    fn startup_roles_round_trip_without_business_payload() {
        let launch = NativeLoopLaunch::new(
            "controller".into(),
            "controller-rev".into(),
            "/project".into(),
            &roles(),
        )
        .unwrap();
        let parsed = parse(launch.args()).unwrap();
        assert_eq!(parsed.controller, "controller");
        assert_eq!(parsed.contexts.len(), 5);
        assert_eq!(parsed.contexts[2].revision, "revision-2");
    }
    #[test]
    fn incomplete_duplicate_and_extra_config_fail_closed() {
        let mut roles = roles();
        roles.pop();
        assert!(
            NativeLoopLaunch::new(
                "controller".into(),
                "controller-rev".into(),
                "/project".into(),
                &roles
            )
            .is_err()
        );
        let mut roles = self::roles();
        roles[1].binding = roles[0].binding.clone();
        assert!(
            NativeLoopLaunch::new(
                "controller".into(),
                "controller-rev".into(),
                "/project".into(),
                &roles
            )
            .is_err()
        );
        let launch = NativeLoopLaunch::new(
            "controller".into(),
            "controller-rev".into(),
            "/project".into(),
            &self::roles(),
        )
        .unwrap();
        let mut args = launch.args().to_vec();
        args.push("extra".into());
        assert!(parse(&args).is_err());
    }
}
