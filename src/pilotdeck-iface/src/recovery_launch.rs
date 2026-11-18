//! Single-source Recovery Native launch facts.
//! The Kernel, not this descriptor, decides whether a provider is trusted.
use crate::recovery_binding::{RecoveryPolicy, serve_recovery};
use agentcpm_scheduler_protocol::{
    Failure, Result, context_history::OpaquePayload, context_operations::failure,
};
use pilotdeck_context_runtime::compaction::recovery::ContextOverflowRecovery;
use std::path::{Path, PathBuf};

/// CLI flag selecting the model-error Recovery capability provider mode.
pub const RECOVERY_FLAG: &str = "--pilotdeck-context-recovery-v1";

const MARKER: &str = RECOVERY_FLAG;

/// Construct once at the trusted composition root; feed these exact facts to
/// both the Kernel Native spec and its trusted Recovery registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryLaunch {
    program: PathBuf,
    sha256: String,
    args: Vec<String>,
}
impl RecoveryLaunch {
    pub fn new(
        program: PathBuf,
        sha256: String,
        binding: String,
        policy: RecoveryPolicy,
    ) -> Result<Self> {
        if !program.is_absolute()
            || !valid_digest(&sha256)
            || binding.is_empty()
            || binding.contains('\0')
        {
            return Err(invalid());
        }
        let mut args = vec![MARKER.into(), binding];
        match policy {
            RecoveryPolicy::Configured(value) => {
                args.push("configured".into());
                args.push(format!(
                    "{:016x}",
                    value.truncate_first_keep_ratio.to_bits()
                ));
                args.push(format!(
                    "{:016x}",
                    value.truncate_second_keep_ratio.to_bits()
                ));
            }
            RecoveryPolicy::LegacyFallback { first_keep_ratio } => {
                args.push("fallback".into());
                args.push(format!("{:016x}", first_keep_ratio.to_bits()));
            }
        }
        Ok(Self {
            program,
            sha256,
            args,
        })
    }
    pub fn program(&self) -> &Path {
        &self.program
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn args(&self) -> &[String] {
        &self.args
    }
    /// Identity helpers use the Kernel SDK profile; they grant no authority.
    pub fn args_digest(&self) -> Result<String> {
        failure::args_digest(&self.args)
    }
    /// Derived from the same argv consumed by the provider. Kernel digesting
    /// binds both fields; only this trusted root code establishes their product
    /// semantic relationship, and the Kernel does not interpret the bytes.
    pub fn configuration(&self) -> Result<OpaquePayload> {
        let (_, policy) = parse_recovery_args(&self.args)?.ok_or_else(invalid)?;
        let mut bytes = Vec::new();
        match policy {
            RecoveryPolicy::Configured(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&value.truncate_first_keep_ratio.to_bits().to_be_bytes());
                bytes.extend_from_slice(&value.truncate_second_keep_ratio.to_bits().to_be_bytes());
            }
            RecoveryPolicy::LegacyFallback { first_keep_ratio } => {
                bytes.push(0);
                bytes.extend_from_slice(&first_keep_ratio.to_bits().to_be_bytes());
            }
        }
        Ok(OpaquePayload {
            profile: "pilotdeck.recovery.configuration.v1".into(),
            bytes,
        })
    }
    pub fn configuration_digest(&self) -> Result<String> {
        failure::configuration_digest(&self.configuration()?)
    }
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn invalid() -> Failure {
    Failure::external(
        "CONTEXT_RECOVERY_CONFIG",
        "invalid Recovery launch configuration",
    )
}
fn bits(value: &str) -> Result<f64> {
    if value.len() != 16
        || !value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid());
    }
    u64::from_str_radix(value, 16)
        .map(f64::from_bits)
        .map_err(|_| invalid())
}
/// Return None only when the dedicated marker is absent. Once selected,
/// malformed or extra arguments fail closed and never pick default policy.
pub fn parse_recovery_args(args: &[String]) -> Result<Option<(String, RecoveryPolicy)>> {
    if args.first().map(String::as_str) != Some(MARKER) {
        return Ok(None);
    }
    if args
        .get(1)
        .is_none_or(|binding| binding.is_empty() || binding.contains('\0'))
    {
        return Err(invalid());
    }
    let policy = match args.get(2).map(String::as_str) {
        Some("configured") if args.len() == 5 => {
            RecoveryPolicy::Configured(ContextOverflowRecovery {
                truncate_first_keep_ratio: bits(&args[3])?,
                truncate_second_keep_ratio: bits(&args[4])?,
            })
        }
        Some("fallback") if args.len() == 4 => RecoveryPolicy::LegacyFallback {
            first_keep_ratio: bits(&args[3])?,
        },
        _ => return Err(invalid()),
    };
    Ok(Some((args[1].clone(), policy)))
}
/// Called from a real product binary's explicit subcommand dispatch. This
/// wrapper neither creates a NativeSpec nor registers a trusted role.
pub async fn dispatch_recovery(args: &[String]) -> Result<bool> {
    let Some((binding, policy)) = parse_recovery_args(args)? else {
        return Ok(false);
    };
    serve_recovery(binding, policy).await?;
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_roundtrips_exact_bits_and_explicit_binding() {
        let nan = f64::from_bits(0x7ff8000000000011);
        let launch = RecoveryLaunch::new(
            "/opt/pilotdeck/provider".into(),
            "a".repeat(64),
            "binding-1".into(),
            RecoveryPolicy::Configured(ContextOverflowRecovery {
                truncate_first_keep_ratio: -0.0,
                truncate_second_keep_ratio: nan,
            }),
        )
        .unwrap();
        let (binding, policy) = parse_recovery_args(launch.args()).unwrap().unwrap();
        assert_eq!(binding, "binding-1");
        let RecoveryPolicy::Configured(policy) = policy else {
            panic!("wrong mode")
        };
        assert_eq!(
            policy.truncate_first_keep_ratio.to_bits(),
            (-0.0f64).to_bits()
        );
        assert_eq!(policy.truncate_second_keep_ratio.to_bits(), nan.to_bits());
        assert_eq!(launch.program(), Path::new("/opt/pilotdeck/provider"));
        assert_eq!(launch.sha256(), "a".repeat(64));
    }
    #[test]
    fn malformed_selected_command_fails_closed() {
        let launch = RecoveryLaunch::new(
            "/provider".into(),
            "b".repeat(64),
            "id".into(),
            RecoveryPolicy::LegacyFallback {
                first_keep_ratio: 0.5,
            },
        )
        .unwrap();
        let mut args = launch.args().to_vec();
        args.push("extra".into());
        assert!(parse_recovery_args(&args).is_err());
        args.pop();
        args[1] = "bad\0binding".into();
        assert!(parse_recovery_args(&args).is_err());
        args[1] = "id".into();
        args[3] = "3fe000000000000G".into();
        assert!(parse_recovery_args(&args).is_err());
        assert!(
            RecoveryLaunch::new(
                "relative".into(),
                "b".repeat(64),
                "id".into(),
                RecoveryPolicy::LegacyFallback {
                    first_keep_ratio: 0.5
                }
            )
            .is_err()
        );
        assert!(parse_recovery_args(&["other".into()]).unwrap().is_none());
    }

    #[test]
    fn versioned_digest_matches_independent_json_fixture() {
        let launch = RecoveryLaunch::new(
            "/provider".into(),
            "b".repeat(64),
            "id".into(),
            RecoveryPolicy::LegacyFallback {
                first_keep_ratio: 0.5,
            },
        )
        .unwrap();
        assert_eq!(
            launch.args_digest().unwrap(),
            "5c735ab9802dd7dc781c70fdd8c4d2e644a0f45201d8385f0a6817815b1997b0"
        );
        assert_eq!(
            launch.configuration_digest().unwrap(),
            "0b2cb053f224a02fbebd703dd593e7958d9fc58e0fbf80073fbffd447d7e4dc7"
        );
        assert_eq!(
            launch.configuration().unwrap().bytes,
            vec![0, 63, 224, 0, 0, 0, 0, 0, 0]
        );
        let other_binding = RecoveryLaunch::new(
            "/provider".into(),
            "b".repeat(64),
            "other".into(),
            RecoveryPolicy::LegacyFallback {
                first_keep_ratio: 0.5,
            },
        )
        .unwrap();
        assert_ne!(
            launch.args_digest().unwrap(),
            other_binding.args_digest().unwrap()
        );
        assert_eq!(
            launch.configuration_digest().unwrap(),
            other_binding.configuration_digest().unwrap()
        );
        let other_ratio = RecoveryLaunch::new(
            "/provider".into(),
            "b".repeat(64),
            "id".into(),
            RecoveryPolicy::LegacyFallback {
                first_keep_ratio: 0.25,
            },
        )
        .unwrap();
        assert_ne!(
            launch.configuration_digest().unwrap(),
            other_ratio.configuration_digest().unwrap()
        );
    }
}
