//! Rejected legacy entries (`cli.compat.update`, `cli.compat.sidecar-*`,
//! `cli.compat.rust-gateway-bin`): fail locally with an explicit error code
//! and a one-line migration pointer, write to stderr, exit 1. These paths
//! never run Node/Web scripts, never execute arbitrary JS, never listen on
//! legacy ports, and never start the old gateway.

use std::ffi::OsStr;

use crate::{EXIT_FAILED, SHIM_NAME};

pub const CODE_LEGACY_WEB_UPDATE: &str = "legacy_web_update_unsupported";
pub const CODE_UNSUPPORTED_RUNTIME_ENTRY: &str = "unsupported_runtime_entry";
pub const CODE_UNSUPPORTED_PROFILE: &str = "unsupported_profile";

/// One local rejection: error code, reason, and a migration pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: &'static str,
    pub reason: String,
    pub guidance: String,
}

impl Rejection {
    /// Renders the stderr diagnostic (two lines, both prefixed).
    pub fn render(&self) -> String {
        format!(
            "{SHIM_NAME}: {} (error: {})\n{SHIM_NAME}: migration: {}\n",
            self.reason, self.code, self.guidance
        )
    }

    /// Writes the diagnostic to stderr and returns the exit code (1).
    pub fn emit(&self) -> i32 {
        eprint!("{}", self.render());
        EXIT_FAILED
    }
}

/// `pilotdeck update [argv]`: the legacy web updater is a different product.
pub fn update() -> Rejection {
    Rejection {
        code: CODE_LEGACY_WEB_UPDATE,
        reason:
            "the legacy `update` command forwarded argv to the Node/Web updater, which is not part of agentcpm-pd"
                .to_owned(),
        guidance: "update your installation with the agentcpm-pd release process instead; this shim never runs Node/Web scripts".to_owned(),
    }
}

/// `pilotdeck-agent-loop-sidecar` (default stdio): not provided by the shim.
pub fn sidecar_stdio() -> Rejection {
    Rejection {
        code: CODE_UNSUPPORTED_RUNTIME_ENTRY,
        reason: "the legacy `pilotdeck-agent-loop-sidecar` stdio runtime entry is not provided by this shim".to_owned(),
        guidance: "module stdio protocol is delivered as an independent runtime contract, not through the product CLI; see `agentcpm-pd help`".to_owned(),
    }
}

/// sidecar + `PILOTDECK_AGENT_LOOP_TCP_*`: never implicitly listens on the
/// legacy port.
pub fn sidecar_tcp() -> Rejection {
    Rejection {
        code: CODE_UNSUPPORTED_RUNTIME_ENTRY,
        reason: "the legacy sidecar TCP listener (`PILOTDECK_AGENT_LOOP_TCP_HOST/PORT`) is not provided by this shim and no legacy port is opened".to_owned(),
        guidance: "deploy explicit TCP module endpoints through the agentcpm-pd runtime; see `agentcpm-pd help`".to_owned(),
    }
}

/// sidecar + `PILOTDECK_AGENT_LOOP_FACTORY=<js>`: arbitrary JS is never
/// executed.
pub fn sidecar_factory() -> Rejection {
    Rejection {
        code: CODE_UNSUPPORTED_PROFILE,
        reason: "the legacy `PILOTDECK_AGENT_LOOP_FACTORY` imported an arbitrary JS module; this shim does not execute JS".to_owned(),
        guidance: "use explicitly supported extension profiles of agentcpm-pd; see `agentcpm-pd help`".to_owned(),
    }
}

/// `pilotdeck-gateway [--native]` / `PILOTDECK_GATEWAY_MODE=proxy`: the
/// legacy gateway binary is not started by the shim.
pub fn gateway_bin() -> Rejection {
    Rejection {
        code: CODE_UNSUPPORTED_RUNTIME_ENTRY,
        reason: "the legacy `pilotdeck-gateway` binary (including `PILOTDECK_GATEWAY_MODE=proxy`) is not started by this shim".to_owned(),
        guidance: "run the production gateway through `agentcpm-pd server serve` or the registered service; see `agentcpm-pd help`".to_owned(),
    }
}

/// Legacy environment variables consumed by the rejected runtime entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LegacyEnv {
    pub gateway_mode: Option<String>,
    pub agent_loop_factory: Option<String>,
    pub agent_loop_tcp_host: Option<String>,
    pub agent_loop_tcp_port: Option<String>,
}

impl LegacyEnv {
    /// Reads the live process environment (kept separate from the pure
    /// routing function below so tests stay hermetic).
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
        Self {
            gateway_mode: var("PILOTDECK_GATEWAY_MODE"),
            agent_loop_factory: var("PILOTDECK_AGENT_LOOP_FACTORY"),
            agent_loop_tcp_host: var("PILOTDECK_AGENT_LOOP_TCP_HOST"),
            agent_loop_tcp_port: var("PILOTDECK_AGENT_LOOP_TCP_PORT"),
        }
    }

    fn tcp_requested(&self) -> bool {
        self.agent_loop_tcp_host
            .as_deref()
            .is_some_and(|v| !v.is_empty())
            || self
                .agent_loop_tcp_port
                .as_deref()
                .is_some_and(|v| !v.is_empty())
    }

    fn factory_requested(&self) -> bool {
        self.agent_loop_factory
            .as_deref()
            .is_some_and(|v| !v.is_empty())
    }
}

/// Legacy multi-call binary names rejected before argv dispatch.
pub const SIDECAR_BIN_NAME: &str = "pilotdeck-agent-loop-sidecar";
pub const GATEWAY_BIN_NAME: &str = "pilotdeck-gateway";

/// Base name of the invoked program (e.g. `pilotdeck`,
/// `pilotdeck-agent-loop-sidecar`).
pub fn invoked_name(arg0: &OsStr) -> String {
    let s = arg0.to_string_lossy();
    match s.rsplit_once('/') {
        Some((_, base)) => base.to_owned(),
        None => s.into_owned(),
    }
}

/// Routes a legacy binary invocation to its rejection, based on the invoked
/// binary name and the legacy environment. The `pilotdeck` name itself is
/// never rejected here (its only rejected subcommand is `update`, dispatched
/// from argv). Environment guards are deliberately scoped to the legacy
/// binary names: they must not reject legitimate `pilotdeck` invocations from
/// a shell that happens to carry old sidecar variables.
pub fn bin_name_rejection(
    bin_name: Option<&str>,
    _argv: &[String],
    env: &LegacyEnv,
) -> Option<Rejection> {
    match bin_name {
        Some(SIDECAR_BIN_NAME) => Some(if env.factory_requested() {
            sidecar_factory()
        } else if env.tcp_requested() {
            sidecar_tcp()
        } else {
            sidecar_stdio()
        }),
        Some(GATEWAY_BIN_NAME) => Some(gateway_bin()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rejection_carries_code_reason_and_guidance() {
        for r in [
            update(),
            sidecar_stdio(),
            sidecar_tcp(),
            sidecar_factory(),
            gateway_bin(),
        ] {
            let rendered = r.render();
            assert!(rendered.contains("(error: "), "code missing: {rendered}");
            assert!(
                rendered.contains("migration: "),
                "guidance missing: {rendered}"
            );
            assert!(!r.reason.is_empty() && !r.guidance.is_empty());
        }
    }

    #[test]
    fn rejection_codes_match_the_case_ledger() {
        assert_eq!(update().code, "legacy_web_update_unsupported");
        assert_eq!(sidecar_stdio().code, "unsupported_runtime_entry");
        assert_eq!(sidecar_tcp().code, "unsupported_runtime_entry");
        assert_eq!(sidecar_factory().code, "unsupported_profile");
        assert_eq!(gateway_bin().code, "unsupported_runtime_entry");
    }

    #[test]
    fn emit_returns_exit_1() {
        assert_eq!(update().emit(), EXIT_FAILED);
    }

    #[test]
    fn sidecar_bin_name_routes_by_env_specificity() {
        let argv = vec!["--stdio".to_owned()];
        // Plain stdio.
        let env = LegacyEnv::default();
        assert_eq!(
            bin_name_rejection(Some(SIDECAR_BIN_NAME), &argv, &env),
            Some(sidecar_stdio())
        );
        // TCP env wins over stdio default.
        let env = LegacyEnv {
            agent_loop_tcp_port: Some("7100".to_owned()),
            ..LegacyEnv::default()
        };
        assert_eq!(
            bin_name_rejection(Some(SIDECAR_BIN_NAME), &argv, &env),
            Some(sidecar_tcp())
        );
        // Factory env wins over everything (never execute arbitrary JS).
        let env = LegacyEnv {
            agent_loop_factory: Some("./plugin.js".to_owned()),
            agent_loop_tcp_port: Some("7100".to_owned()),
            ..LegacyEnv::default()
        };
        assert_eq!(
            bin_name_rejection(Some(SIDECAR_BIN_NAME), &argv, &env),
            Some(sidecar_factory())
        );
        // Empty values do not count as requests.
        let env = LegacyEnv {
            agent_loop_factory: Some(String::new()),
            agent_loop_tcp_host: Some(String::new()),
            agent_loop_tcp_port: Some(String::new()),
            ..LegacyEnv::default()
        };
        assert_eq!(
            bin_name_rejection(Some(SIDECAR_BIN_NAME), &argv, &env),
            Some(sidecar_stdio())
        );
    }

    #[test]
    fn gateway_bin_name_is_rejected_and_pilotdeck_is_not() {
        let env = LegacyEnv::default();
        assert_eq!(
            bin_name_rejection(Some(GATEWAY_BIN_NAME), &[], &env),
            Some(gateway_bin())
        );
        assert_eq!(bin_name_rejection(Some("pilotdeck"), &[], &env), None);
        assert_eq!(bin_name_rejection(None, &[], &env), None);
    }

    #[test]
    fn legacy_env_guards_do_not_leak_into_the_pilotdeck_name() {
        // Even with legacy sidecar env vars set, the `pilotdeck` name routes
        // through normal argv dispatch.
        let env = LegacyEnv {
            agent_loop_factory: Some("x.js".to_owned()),
            gateway_mode: Some("proxy".to_owned()),
            ..LegacyEnv::default()
        };
        assert_eq!(bin_name_rejection(Some("pilotdeck"), &[], &env), None);
    }

    #[test]
    fn invoked_name_takes_the_basename() {
        assert_eq!(
            invoked_name(OsStr::new("/usr/local/bin/pilotdeck-agent-loop-sidecar")),
            SIDECAR_BIN_NAME
        );
        assert_eq!(
            invoked_name(OsStr::new("pilotdeck-gateway")),
            GATEWAY_BIN_NAME
        );
        assert_eq!(invoked_name(OsStr::new("pilotdeck")), "pilotdeck");
    }
}
