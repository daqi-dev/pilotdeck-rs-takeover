//! `agentcpm-pd` three-entry CLI shell (`WP3`, `007 §4–§6`).
//!
//! Entries:
//! 1. canonical argv commands driven by the embedded 285-operation command
//!    tree (`commands.rs`);
//! 2. `api invoke <methodId> --input-json <file|->` sharing the same registry
//!    and schemas;
//! 3. the long-lived `api connect` pd-connect/1 stdio engine (`connect.rs`).
//!
//! Business operations are dispatched over `/v1/ws` through `agentcpm-pd-client`;
//! local lifecycle commands (`instance init`, `server serve/start/status/stop`)
//! call the Gateway `public_v1::bootstrap` library when the `embedded-gateway`
//! feature is enabled.

pub mod commands;
pub mod connect;

use std::io::IsTerminal;

use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, EXIT_FAILED, EXIT_OK, EXIT_USAGE, EffectStatus, RetrySpec,
};

pub const BIN_NAME: &str = "agentcpm-pd";

/// Output formats from `--output` (command tree `outputPolicy`: JSON stdout is
/// exactly one envelope; NDJSON is one versioned frame per line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    Ndjson,
}

impl OutputFormat {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "ndjson" => Ok(Self::Ndjson),
            other => Err(format!(
                "invalid --output value `{other}` (expected text, json, or ndjson)"
            )),
        }
    }

    /// Resolves the unary output format: explicit choice, else `text` on a
    /// terminal and `json` for machine (non-tty) callers.
    pub fn resolve_unary(preferred: Option<Self>) -> Self {
        preferred.unwrap_or(if std::io::stdout().is_terminal() {
            Self::Text
        } else {
            Self::Json
        })
    }

    /// Resolves the follow-stream output format: NDJSON frames; `--output json`
    /// is a usage error because a stream is not one envelope.
    pub fn resolve_stream(preferred: Option<Self>) -> Result<Self, String> {
        match preferred {
            Some(Self::Json) => Err(
                "--output json requires a unary command; follow streams emit NDJSON frames"
                    .to_owned(),
            ),
            Some(other) => Ok(other),
            None => Ok(Self::Ndjson),
        }
    }
}

/// Global connection/output options shared by every entry (command tree
/// `globalOptions`; one stdin serves at most one input).
#[derive(Clone, Debug, Default)]
pub struct GlobalOptions {
    pub target: Option<String>,
    pub output: Option<OutputFormat>,
    pub input_json: Option<String>,
    pub request_key: Option<String>,
    pub timeout_ms: Option<u64>,
    pub non_interactive: bool,
    pub no_start: bool,
    pub secret_output: bool,
}

/// Table-driven description of one global option.
#[derive(Clone, Copy, Debug)]
pub struct GlobalOptSpec {
    pub name: &'static str,
    pub takes_value: bool,
    pub help: &'static str,
}

pub const GLOBAL_OPTIONS: &[GlobalOptSpec] = &[
    GlobalOptSpec {
        name: "--target",
        takes_value: true,
        help: "local or remote target profile; remote failures never fall back to local",
    },
    GlobalOptSpec {
        name: "--output",
        takes_value: true,
        help: "output format: text|json|ndjson (default: text on a terminal, json otherwise, ndjson for follow streams)",
    },
    GlobalOptSpec {
        name: "--input-json",
        takes_value: true,
        help: "UTF-8 JSON object file or `-` (stdin); mutually exclusive with business options",
    },
    GlobalOptSpec {
        name: "--request-key",
        takes_value: true,
        help: "stable idempotency key across retries; generated and persisted when omitted",
    },
    GlobalOptSpec {
        name: "--timeout",
        takes_value: true,
        help: "observation or request deadline in milliseconds; never cancels background runs",
    },
    GlobalOptSpec {
        name: "--non-interactive",
        takes_value: false,
        help: "never read TTY prompts; answer interaction items via public commands",
    },
    GlobalOptSpec {
        name: "--no-start",
        takes_value: false,
        help: "forbid local gateway auto-start",
    },
    GlobalOptSpec {
        name: "--secret-output",
        takes_value: false,
        help: "newly created secrets are written to explicit new files, never stdout or logs",
    },
];

pub fn is_global_option(name: &str) -> bool {
    GLOBAL_OPTIONS.iter().any(|spec| spec.name == name)
}

fn parse_bool_flag(name: &str, value: Option<&str>) -> Result<bool, String> {
    match value {
        None => Ok(true),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => Err(format!("option `{name}` does not accept value `{other}`")),
    }
}

/// Applies one global option occurrence; `value` is the inline (`--opt=v`) or
/// following-token value, already stripped of the option name.
pub fn apply_global_option(
    globals: &mut GlobalOptions,
    name: &str,
    value: Option<&str>,
) -> Result<(), String> {
    match name {
        "--target" => {
            globals.target = Some(
                value
                    .ok_or_else(|| format!("option `{name}` requires a value"))?
                    .to_owned(),
            );
        }
        "--output" => {
            let raw = value.ok_or_else(|| format!("option `{name}` requires a value"))?;
            globals.output = Some(OutputFormat::parse(raw)?);
        }
        "--input-json" => {
            globals.input_json = Some(
                value
                    .ok_or_else(|| format!("option `{name}` requires a value"))?
                    .to_owned(),
            );
        }
        "--request-key" => {
            globals.request_key = Some(
                value
                    .ok_or_else(|| format!("option `{name}` requires a value"))?
                    .to_owned(),
            );
        }
        "--timeout" => {
            let raw = value.ok_or_else(|| format!("option `{name}` requires a value"))?;
            let ms = raw.parse::<u64>().map_err(|_| {
                format!("option `{name}` expects a positive millisecond value, got `{raw}`")
            })?;
            if ms == 0 {
                return Err(format!("option `{name}` must be greater than zero"));
            }
            globals.timeout_ms = Some(ms);
        }
        "--non-interactive" => globals.non_interactive = parse_bool_flag(name, value)?,
        "--no-start" => globals.no_start = parse_bool_flag(name, value)?,
        "--secret-output" => globals.secret_output = parse_bool_flag(name, value)?,
        other => return Err(format!("unknown global option `{other}`")),
    }
    Ok(())
}

/// Renders a value for the given output format (text = pretty JSON for humans;
/// json/ndjson = one compact line per envelope).
pub fn render_value(format: OutputFormat, value: &serde_json::Value) -> String {
    match format {
        OutputFormat::Text => {
            let mut out = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into());
            out.push('\n');
            out
        }
        OutputFormat::Json | OutputFormat::Ndjson => {
            let mut out = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
            out.push('\n');
            out
        }
    }
}

/// Renders one structured stderr diagnostic line; machine modes keep stdout a
/// single clean envelope.
pub fn render_diag(format: Option<OutputFormat>, message: &str) -> String {
    match format {
        Some(OutputFormat::Json) | Some(OutputFormat::Ndjson) => {
            let mut line = serde_json::to_string(&serde_json::json!({
                "level": "error",
                "message": message,
            }))
            .unwrap_or_else(|_| format!("error: {message}"));
            line.push('\n');
            line
        }
        _ => format!("{BIN_NAME}: {message}\n"),
    }
}

/// Maps a client failure to the contract exit code (`007 §6.8`: 0 success,
/// 1 known failure, 2 usage, 3 result unknown, 130 interrupted).
pub fn exit_code_for(err: &agentcpm_pd_client::error::ClientError) -> i32 {
    err.exit_code_for_outcome()
}

/// Builds a `CAPABILITY_UNAVAILABLE` contract error for commands that are
/// registered but not served by this build.
pub fn capability_unavailable(
    method_id: &str,
    reason: &str,
) -> agentcpm_pd_client::error::ClientError {
    agentcpm_pd_client::error::ClientError::Contract(
        agentcpm_pd_protocol::errors::ContractError::capability_unavailable(method_id, reason),
    )
}

/// Splits `--name` / `--name=value` into `(name, inline_value)`.
pub fn split_long_option(token: &str) -> (String, Option<String>) {
    let body = token.strip_prefix("--").unwrap_or(token);
    match body.split_once('=') {
        Some((name, value)) => (format!("--{name}"), Some(value.to_owned())),
        None => (format!("--{body}"), None),
    }
}

/// Reads a `--input-json` / `--text-file` source: a path or `-` for stdin.
pub fn read_input_source(path: &str) -> Result<String, String> {
    use std::io::Read;
    if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("failed to read `{path}`: {e}"))
    }
}

/// Builds the tokio runtime and dispatches one CLI invocation, returning the
/// process exit code.
pub fn run<I: IntoIterator<Item = String>>(args: I) -> i32 {
    let args: Vec<String> = args.into_iter().collect();
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        eprintln!("{BIN_NAME}: failed to build the async runtime");
        return EXIT_FAILED;
    };
    runtime.block_on(run_entry(args))
}

async fn run_entry(args: Vec<String>) -> i32 {
    match commands::parse_entry(&args) {
        Ok(commands::ParsedEntry::Help) => {
            print!("{}", commands::help_text());
            EXIT_OK
        }
        Ok(commands::ParsedEntry::Version) => {
            println!(
                "{BIN_NAME} {} (contract {})",
                env!("CARGO_PKG_VERSION"),
                agentcpm_pd_protocol::registry::OperationRegistry::global()
                    .registry_digest()
                    .chars()
                    .take(12)
                    .collect::<String>()
            );
            EXIT_OK
        }
        Ok(commands::ParsedEntry::ApiConnect {
            globals,
            log_format,
        }) => connect::run_connect(globals, log_format).await,
        Ok(commands::ParsedEntry::ApiInvoke { globals, method_id }) => {
            commands::run_api_invoke(globals, method_id).await
        }
        Ok(commands::ParsedEntry::Command {
            globals,
            path,
            opts,
        }) => commands::run_canonical(globals, path, opts).await,
        Err(message) => {
            eprintln!("{BIN_NAME}: usage error: {message}");
            eprintln!("run `{BIN_NAME} --help` for the command list");
            EXIT_USAGE
        }
    }
}

/// Convenience: an `INVALID_ARGUMENT`-style usage contract error.
pub fn usage_contract_error(message: &str) -> agentcpm_pd_protocol::errors::ContractError {
    agentcpm_pd_protocol::errors::ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_options_table_applies_and_validates() {
        let mut globals = GlobalOptions::default();
        apply_global_option(&mut globals, "--target", Some("local")).unwrap();
        apply_global_option(&mut globals, "--output", Some("json")).unwrap();
        apply_global_option(&mut globals, "--timeout", Some("1500")).unwrap();
        apply_global_option(&mut globals, "--no-start", None).unwrap();
        apply_global_option(&mut globals, "--non-interactive", Some("false")).unwrap();
        assert_eq!(globals.target.as_deref(), Some("local"));
        assert_eq!(globals.output, Some(OutputFormat::Json));
        assert_eq!(globals.timeout_ms, Some(1500));
        assert!(globals.no_start);
        assert!(!globals.non_interactive);

        assert!(apply_global_option(&mut globals, "--output", Some("yaml")).is_err());
        assert!(apply_global_option(&mut globals, "--timeout", Some("0")).is_err());
        assert!(apply_global_option(&mut globals, "--timeout", Some("fast")).is_err());
        assert!(apply_global_option(&mut globals, "--no-start", Some("maybe")).is_err());
        assert!(apply_global_option(&mut globals, "--target", None).is_err());
        assert!(apply_global_option(&mut globals, "--bogus", None).is_err());
    }

    #[test]
    fn output_format_defaults_follow_tty_and_streams() {
        assert_eq!(
            OutputFormat::resolve_stream(None).unwrap(),
            OutputFormat::Ndjson
        );
        assert_eq!(
            OutputFormat::resolve_stream(Some(OutputFormat::Text)).unwrap(),
            OutputFormat::Text
        );
        assert!(OutputFormat::resolve_stream(Some(OutputFormat::Json)).is_err());
        // resolve_unary depends on the test tty state; assert only the explicit path.
        assert_eq!(
            OutputFormat::resolve_unary(Some(OutputFormat::Ndjson)),
            OutputFormat::Ndjson
        );
    }

    #[test]
    fn diag_and_value_rendering_stay_single_envelope() {
        let v = serde_json::json!({"a": 1});
        assert!(render_value(OutputFormat::Json, &v).ends_with("}\n"));
        assert!(render_value(OutputFormat::Text, &v).contains("  \"a\": 1"));
        assert!(render_diag(Some(OutputFormat::Json), "boom").starts_with("{\"level\":\"error\""));
        assert!(render_diag(None, "boom").contains(BIN_NAME));
    }
}
