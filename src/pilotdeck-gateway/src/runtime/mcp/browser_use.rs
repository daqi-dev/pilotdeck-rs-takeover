//! The per-session `browser-use` MCP process configuration
//! (`cli/BrowserUseSessionMcpSpecPreparer.ts` with `buildBrowserUseArgs`,
//! `cli/createLocalGateway.ts:1729-1813`).
//!
//! The builtin `browser-use` plugin declares a per-session stdio server that
//! runs `@playwright/mcp`. Before a session starts it, the baseline gives it a
//! session-local output directory — `<projectRoot>/.pilotdeck/
//! browser_screenshots/<sanitized session key>`, created on the spot — as its
//! working directory and `--output-dir`, plus the action / navigation
//! timeouts and the browser proxy flags. Nothing else about the browser is
//! the host's: the Playwright process itself does the automation. The
//! sandboxed-session variant (an nsjail `storageRoot`) is not reproduced:
//! Rust has no such provider.

use std::path::Path;

use pilotdeck_protocol::{JsString, JsValue};

use super::protocol::{McpServerSpec, McpTransportSpec};
use crate::runtime::project::{RuntimeError, RuntimeJsError};

/// `DEFAULT_BROWSER_ACTION_TIMEOUT_MS` (createLocalGateway.ts:1576).
const DEFAULT_ACTION_TIMEOUT_MS: f64 = 30_000.0;
/// `DEFAULT_BROWSER_NAVIGATION_TIMEOUT_MS` (createLocalGateway.ts:1577).
const DEFAULT_NAVIGATION_TIMEOUT_MS: f64 = 90_000.0;

/// What one session's preparation reads besides the specs.
pub struct BrowserUseSessionContext<'a> {
    /// `runtime.projectRoot`, absolute.
    pub project_root: &'a Path,
    pub session_key: &'a JsString,
    /// `runtime.snapshot.config.proxy` (`{url, noProxy?}` or absent).
    pub config_proxy: Option<&'a JsValue>,
    /// The process environment (`options.env`).
    pub env: &'a (dyn Fn(&str) -> Option<String> + Sync),
}

/// `BrowserUseSessionMcpSpecPreparer.prepare`: every stdio spec with id
/// `browser-use` gets its output directory (created recursively, like
/// `mkdirSync(path, {recursive: true})`), that directory as `cwd`, and the
/// `buildBrowserUseArgs` flags; other specs pass through. A directory that
/// cannot be created fails the session's tool composition with Node's
/// `mkdir` error.
pub fn prepare_browser_use_specs(
    specs: Vec<McpServerSpec>,
    context: &BrowserUseSessionContext<'_>,
) -> Result<Vec<McpServerSpec>, RuntimeError> {
    specs
        .into_iter()
        .map(|spec| {
            let McpTransportSpec::Stdio {
                command,
                args,
                env,
                per_session,
                ..
            } = &spec.transport
            else {
                return Ok(spec);
            };
            if spec.id != "browser-use" {
                return Ok(spec);
            }
            let output_dir = output_dir(context)?;
            crate::node_io::mkdir_recursive(Path::new(&output_dir)).map_err(|error| {
                let message = error.to_string();
                let code = crate::node_io::lookup(&error.source).map(|(code, _)| code.to_string());
                RuntimeError::new(&message).with_js_error(RuntimeJsError {
                    name: "Error".into(),
                    code,
                    message: message.into(),
                    requires_host: false,
                })
            })?;
            let args = build_browser_use_args(
                args.clone().unwrap_or_default(),
                &output_dir,
                context.env,
                context.config_proxy,
            );
            Ok(McpServerSpec {
                transport: McpTransportSpec::Stdio {
                    command: command.clone(),
                    args: Some(args),
                    env: env.clone(),
                    cwd: Some(output_dir),
                    per_session: *per_session,
                },
                ..spec
            })
        })
        .collect()
}

/// `join(projectRoot, ".pilotdeck", "browser_screenshots",
/// sanitizeSessionIdForPath(sessionKey))`; `join` normalizes the result.
fn output_dir(context: &BrowserUseSessionContext<'_>) -> Result<String, RuntimeError> {
    let session = crate::storage::sanitize_session_id(context.session_key);
    let joined = format!(
        "{}/.pilotdeck/browser_screenshots/{session}",
        context.project_root.to_string_lossy()
    );
    super::registry::resolve_path(&joined).map_err(|error| RuntimeError::new(error.to_string()))
}

/// `buildBrowserUseArgs(baseArgs, outputDir, env, configProxy)`.
pub fn build_browser_use_args(
    base_args: Vec<String>,
    output_dir: &str,
    env: &dyn Fn(&str) -> Option<String>,
    config_proxy: Option<&JsValue>,
) -> Vec<String> {
    let mut args = base_args;
    append_cli_arg(&mut args, "--output-dir", output_dir.to_string());
    let timeout = |names: [&str; 2], fallback: f64| {
        let value = names
            .iter()
            .find_map(|name| read_positive_integer_env(env(name).as_deref()))
            .unwrap_or(fallback);
        pilotdeck_protocol::number::number_to_string(value)
    };
    append_cli_arg(
        &mut args,
        "--timeout-action",
        timeout(
            [
                "PILOTDECK_BROWSER_TIMEOUT_ACTION_MS",
                "PILOTDECK_BROWSER_ACTION_TIMEOUT_MS",
            ],
            DEFAULT_ACTION_TIMEOUT_MS,
        ),
    );
    append_cli_arg(
        &mut args,
        "--timeout-navigation",
        timeout(
            [
                "PILOTDECK_BROWSER_TIMEOUT_NAVIGATION_MS",
                "PILOTDECK_BROWSER_NAVIGATION_TIMEOUT_MS",
            ],
            DEFAULT_NAVIGATION_TIMEOUT_MS,
        ),
    );
    let config_value = |name: &str| {
        config_proxy
            .and_then(|proxy| proxy.get(name))
            .and_then(JsValue::as_string)
            .and_then(|value| clean(Some(String::from_utf16_lossy(value.units()))))
    };
    if let Some((server, source)) = resolve_proxy_server(env, config_value("url")) {
        append_cli_arg(&mut args, "--proxy-server", server);
        let bypass = clean(env("PILOTDECK_BROWSER_PROXY_BYPASS")).unwrap_or_else(|| {
            let no_proxy = clean(env("no_proxy")).or_else(|| clean(env("NO_PROXY")));
            let config_no_proxy = (source == ProxySource::Config)
                .then(|| config_value("noProxy"))
                .flatten();
            [no_proxy, config_no_proxy]
                .into_iter()
                .flatten()
                .chain(
                    ["localhost", "127.0.0.1", "host.docker.internal"]
                        .into_iter()
                        .map(String::from),
                )
                .collect::<Vec<_>>()
                .join(",")
        });
        append_cli_arg(&mut args, "--proxy-bypass", bypass);
    }
    args
}

/// `appendCliArg`: a flag already given (`--flag` or `--flag=…`) is kept.
fn append_cli_arg(args: &mut Vec<String>, flag: &str, value: String) {
    let prefix = format!("{flag}=");
    if args
        .iter()
        .any(|arg| arg == flag || arg.starts_with(&prefix))
    {
        return;
    }
    args.push(flag.to_string());
    args.push(value);
}

/// `readPositiveIntegerEnv` (LocalGatewayBootConfig.ts:63-68).
fn read_positive_integer_env(value: Option<&str>) -> Option<f64> {
    let parsed = super::client::parse_int(crate::metadata::js_trim(value?));
    (parsed.is_finite() && parsed > 0.0).then(|| parsed.floor())
}

/// `cleanEnvValue`: trimmed, `undefined` when empty.
fn clean(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = crate::metadata::js_trim(&value);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProxySource {
    BrowserEnv,
    Env,
    Config,
}

/// `resolveBrowserProxyServer`.
fn resolve_proxy_server(
    env: &dyn Fn(&str) -> Option<String>,
    config_url: Option<String>,
) -> Option<(String, ProxySource)> {
    let matches_any =
        |value: &str, words: &[&str]| words.iter().any(|word| value.eq_ignore_ascii_case(word));
    if let Some(explicit) = clean(env("PILOTDECK_BROWSER_PROXY_SERVER")) {
        if matches_any(&explicit, &["0", "false", "off", "none", "direct"]) {
            return None;
        }
        return Some((explicit, ProxySource::BrowserEnv));
    }
    let from_env = clean(env("PILOTDECK_BROWSER_PROXY_FROM_ENV")).unwrap_or_default();
    if matches_any(&from_env, &["1", "true", "on", "yes"]) {
        let env_proxy = [
            "PILOTDECK_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "http_proxy",
            "HTTP_PROXY",
        ]
        .iter()
        .find_map(|name| clean(env(name)));
        if let Some(server) = env_proxy {
            return Some((server, ProxySource::Env));
        }
    }
    config_url.map(|server| (server, ProxySource::Config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    fn base() -> Vec<String> {
        [
            "cli.js",
            "--browser",
            "chromium",
            "--allow-unrestricted-file-access",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn default_args_append_output_dir_and_timeouts() {
        let args = build_browser_use_args(
            base(),
            "/p/.pilotdeck/browser_screenshots/s",
            &env_of(&[]),
            None,
        );
        assert_eq!(
            args[4..],
            [
                "--output-dir",
                "/p/.pilotdeck/browser_screenshots/s",
                "--timeout-action",
                "30000",
                "--timeout-navigation",
                "90000"
            ]
        );
    }

    #[test]
    fn existing_flags_and_environment_timeouts_win() {
        let mut given = base();
        given.push("--output-dir=/elsewhere".into());
        let env = env_of(&[
            ("PILOTDECK_BROWSER_TIMEOUT_ACTION_MS", " -5"),
            ("PILOTDECK_BROWSER_ACTION_TIMEOUT_MS", " 12.9s"),
            ("PILOTDECK_BROWSER_TIMEOUT_NAVIGATION_MS", "0x10"),
        ]);
        let args = build_browser_use_args(given, "/out", &env, None);
        assert_eq!(
            args[4..],
            [
                "--output-dir=/elsewhere",
                "--timeout-action",
                "12",
                "--timeout-navigation",
                "90000"
            ]
        );
    }

    #[test]
    fn proxy_sources_and_bypass() {
        let config =
            pilotdeck_protocol::parse_json(r#"{"url":" http://cfg:1 ","noProxy":"cfg.local"}"#)
                .unwrap();
        let tail = |env: &dyn Fn(&str) -> Option<String>, config: Option<&JsValue>| {
            build_browser_use_args(Vec::new(), "/o", env, config)[6..].to_vec()
        };
        assert_eq!(
            tail(&env_of(&[("no_proxy", "a.local")]), Some(&config)),
            [
                "--proxy-server",
                "http://cfg:1",
                "--proxy-bypass",
                "a.local,cfg.local,localhost,127.0.0.1,host.docker.internal"
            ]
        );
        assert_eq!(
            tail(
                &env_of(&[
                    ("PILOTDECK_BROWSER_PROXY_FROM_ENV", "YES"),
                    ("HTTPS_PROXY", "http://env:2")
                ]),
                Some(&config)
            ),
            [
                "--proxy-server",
                "http://env:2",
                "--proxy-bypass",
                "localhost,127.0.0.1,host.docker.internal"
            ]
        );
        assert_eq!(
            tail(
                &env_of(&[
                    ("PILOTDECK_BROWSER_PROXY_SERVER", "http://b:3"),
                    ("PILOTDECK_BROWSER_PROXY_BYPASS", " x ")
                ]),
                None
            ),
            ["--proxy-server", "http://b:3", "--proxy-bypass", "x"]
        );
        assert!(
            tail(
                &env_of(&[("PILOTDECK_BROWSER_PROXY_SERVER", "Direct")]),
                Some(&config)
            )
            .is_empty()
        );
        // The explicit browser proxy beats PROXY_FROM_ENV and the config; the
        // config's noProxy only joins the bypass when the config is the source.
        assert_eq!(
            tail(
                &env_of(&[
                    ("PILOTDECK_BROWSER_PROXY_SERVER", " http://b:1 "),
                    ("PILOTDECK_BROWSER_PROXY_FROM_ENV", "yes"),
                    ("HTTPS_PROXY", "http://e:2"),
                    ("NO_PROXY", "x.test"),
                ]),
                Some(&config)
            ),
            [
                "--proxy-server",
                "http://b:1",
                "--proxy-bypass",
                "x.test,localhost,127.0.0.1,host.docker.internal"
            ]
        );
        // A blank lower-case variable is skipped, not taken as the proxy.
        assert_eq!(
            tail(
                &env_of(&[
                    ("PILOTDECK_BROWSER_PROXY_FROM_ENV", "ON"),
                    ("https_proxy", " "),
                    ("HTTPS_PROXY", "http://e:2"),
                ]),
                None
            ),
            [
                "--proxy-server",
                "http://e:2",
                "--proxy-bypass",
                "localhost,127.0.0.1,host.docker.internal"
            ]
        );
    }

    #[test]
    fn prepare_touches_only_browser_use_stdio() {
        let root = std::env::temp_dir().join(format!("pd-browser-use-{}", std::process::id()));
        let stdio = |id: &str| McpServerSpec {
            id: id.into(),
            transport: McpTransportSpec::Stdio {
                command: "node".into(),
                args: Some(vec!["entry.js".into()]),
                env: None,
                cwd: None,
                per_session: true,
            },
            call_timeout_ms: None,
        };
        let session_key = JsString::from("cli/s/");
        let prepared = prepare_browser_use_specs(
            vec![stdio("other"), stdio("browser-use")],
            &BrowserUseSessionContext {
                project_root: &root,
                session_key: &session_key,
                config_proxy: None,
                env: &|_| None,
            },
        )
        .expect("prepare");
        assert_eq!(prepared[0], stdio("other"));
        let McpTransportSpec::Stdio { args, cwd, .. } = &prepared[1].transport else {
            panic!("stdio");
        };
        let out = root.join(".pilotdeck/browser_screenshots/cli-s");
        assert!(out.is_dir());
        assert_eq!(cwd.as_deref(), Some(out.to_str().unwrap()));
        assert_eq!(
            args.as_ref().unwrap()[1..3],
            [
                "--output-dir".to_owned(),
                out.to_string_lossy().into_owned()
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
