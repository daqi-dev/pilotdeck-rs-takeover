//! Entry modules: one file per legacy entry group, dispatched from
//! [`crate::run`]. Local entries (`help`, `reject`, `usage_errors`) never
//! start a gateway and never read a home directory; orchestration entries
//! receive an [`EntryCtx`] (an [`crate::executor::Executor`] plus the
//! explicitly resolved shim home) and perform every business call through
//! the executor.

pub mod chat_search;
pub mod cron;
pub mod guardian;
pub mod help;
pub mod reject;
pub mod resume;
pub mod run;
pub mod server;
pub mod sessions;
pub mod settings;
pub mod setup;
pub mod skills;
pub mod usage_errors;

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{self, CompatConfig, ConfigError};
use crate::executor::Executor;
use crate::{EXIT_FAILED, EXIT_RESULT_UNKNOWN, SHIM_NAME};

/// Context handed to every orchestration entry: the executor plus the
/// resolved shim home. Production resolves the home from the environment;
/// tests inject an isolated temporary directory (the real `~/.pilotdeck`
/// and `~/.agentcpm-pd` are never touched).
pub struct EntryCtx<'a> {
    executor: &'a mut dyn Executor,
    home: Result<PathBuf, ConfigError>,
}

impl<'a> EntryCtx<'a> {
    /// Production construction: the home comes from `AGENTCPM_PD_HOME`.
    pub fn from_env(executor: &'a mut dyn Executor) -> Self {
        let home = config::resolve_home(std::env::var_os(config::HOME_ENV).as_deref());
        Self { executor, home }
    }

    /// Test construction with an isolated home directory.
    pub fn with_home(executor: &'a mut dyn Executor, home: &Path) -> Self {
        Self {
            executor,
            home: Ok(home.to_path_buf()),
        }
    }

    /// The executor for business calls.
    pub fn executor(&mut self) -> &mut dyn Executor {
        self.executor
    }

    /// The resolved home, or `configuration_required` already emitted
    /// (`Err(exit code)`). Help/manifest/rejection paths never call this.
    pub fn require_home(&self) -> Result<&Path, i32> {
        match &self.home {
            Ok(home) => Ok(home),
            Err(e) => {
                eprintln!("{SHIM_NAME}: {e}");
                Err(EXIT_FAILED)
            }
        }
    }

    /// Loads the shim config; a missing file is an empty config, an invalid
    /// document is a surfaced failure (`Err(exit code)`).
    pub fn load_config(&self, home: &Path) -> Result<CompatConfig, i32> {
        match config::load(home) {
            Ok(Some(config)) => Ok(config),
            Ok(None) => Ok(CompatConfig::default()),
            Err(e) => {
                eprintln!("{SHIM_NAME}: {e}");
                Err(EXIT_FAILED)
            }
        }
    }

    /// Loads the legacy→new session mapping table (missing file = empty).
    pub fn load_sessions(&self, home: &Path) -> Result<crate::state::SessionMap, i32> {
        match crate::state::SessionMap::load(home) {
            Ok(map) => Ok(map),
            Err(message) => {
                eprintln!("{SHIM_NAME}: compat session mapping failed to load: {message}");
                Err(EXIT_FAILED)
            }
        }
    }
}

/// Emits a business failure (a `failed` envelope) faithfully and returns the
/// matching exit code: `RESULT_UNKNOWN` maps to exit 3, everything else to
/// exit 1. The error code and message are passed through verbatim —
/// `CAPABILITY_UNAVAILABLE` from a pending-S3 operation is surfaced, never
/// swallowed or turned into fake success.
pub(crate) fn surface_failure(method: &str, error: &crate::executor::ErrorPayload) -> i32 {
    eprintln!(
        "{SHIM_NAME}: {method} failed: {}: {}",
        error.code, error.message
    );
    if error.code == "RESULT_UNKNOWN" {
        EXIT_RESULT_UNKNOWN
    } else {
        EXIT_FAILED
    }
}

/// Emits a transport-level failure of one invocation.
pub(crate) fn surface_exec_error(method: &str, error: &crate::executor::ExecError) -> i32 {
    eprintln!("{SHIM_NAME}: {method} failed: {error}");
    EXIT_FAILED
}

/// Invokes one operation and unwraps its result, surfacing any failure the
/// standard way (`Err(exit code)`).
pub(crate) fn call_op(ctx: &mut EntryCtx<'_>, method: &str, input: &Value) -> Result<Value, i32> {
    match ctx.executor().invoke(method, input) {
        Ok(resp) => unwrap_result(method, &resp),
        Err(e) => Err(surface_exec_error(method, &e)),
    }
}

/// [`call_op`] with an explicit persistent request key (idempotent writes).
pub(crate) fn call_op_keyed(
    ctx: &mut EntryCtx<'_>,
    method: &str,
    input: &Value,
    request_key: &str,
) -> Result<Value, i32> {
    match ctx.executor().invoke_keyed(method, input, request_key) {
        Ok(resp) => unwrap_result(method, &resp),
        Err(e) => Err(surface_exec_error(method, &e)),
    }
}

/// Turns a response envelope into its result payload.
pub(crate) fn unwrap_result(method: &str, resp: &crate::executor::Response) -> Result<Value, i32> {
    if resp.is_success() {
        Ok(resp.result.clone().unwrap_or(Value::Null))
    } else {
        match &resp.error {
            Some(error) => Err(surface_failure(method, error)),
            None => {
                eprintln!("{SHIM_NAME}: {method} failed without an error payload");
                Err(EXIT_FAILED)
            }
        }
    }
}

/// Reports old fields that cannot be projected onto the new contract
/// (one stderr line; stdout output stays clean). The mapping data requires
/// unprojectable fields to be listed, never silently dropped.
pub(crate) fn note_dropped(context: &str, fields: &[&str]) {
    if !fields.is_empty() {
        eprintln!(
            "{SHIM_NAME}: note: {context}: not projectable onto the new contract: {}",
            fields.join(", ")
        );
    }
}

/// Option specification for the tiny shared argv parser.
pub(crate) struct OptSpec {
    /// Options that carry a value (`--name value` / `--name=value`).
    pub values: &'static [&'static str],
    /// Boolean flags.
    pub flags: &'static [&'static str],
}

/// Result of the shared argv parser.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub positionals: Vec<String>,
    pub flags: Vec<String>,
    pub values: Vec<(String, String)>,
}

impl Parsed {
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn values_of(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// Minimal long-option parser shared by the entry modules. Unknown options,
/// values on valueless flags, and missing values are usage errors (message
/// without the `pilotdeck:` prefix; callers emit via `usage_errors::usage`).
pub(crate) fn parse_cli(rest: &[String], spec: &OptSpec) -> Result<Parsed, String> {
    let mut out = Parsed::default();
    let mut i = 0;
    while i < rest.len() {
        let token = &rest[i];
        if token == "--" {
            out.positionals.extend(rest[i + 1..].iter().cloned());
            break;
        }
        let Some(body) = token.strip_prefix("--") else {
            out.positionals.push(token.clone());
            i += 1;
            continue;
        };
        let (name, inline) = match body.split_once('=') {
            Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
            None => (body.to_owned(), None),
        };
        // Spec entries carry their `--` prefixes; compare the full form.
        let long = format!("--{name}");
        if spec.values.contains(&long.as_str()) {
            let value = match inline {
                Some(v) => v,
                None => match rest.get(i + 1) {
                    Some(v) if !v.starts_with('-') => {
                        i += 1;
                        v.clone()
                    }
                    _ => return Err(format!("option {long} requires a value")),
                },
            };
            out.values.push((long, value));
        } else if spec.flags.contains(&long.as_str()) {
            if inline.is_some() {
                return Err(format!("option {long} does not take a value"));
            }
            out.flags.push(long);
        } else {
            return Err(format!("unknown option {long}"));
        }
        i += 1;
    }
    Ok(out)
}
