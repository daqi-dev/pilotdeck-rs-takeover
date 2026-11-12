//! `BrowserUseSessionMcpSpecPreparer` (src/cli/BrowserUseSessionMcpSpecPreparer.ts)
//! and `buildBrowserUseArgs` (src/cli/createLocalGateway.ts:1729-1814).
//!
//! Before a session starts its per-session MCP runtime, the stdio spec whose id
//! is `browser-use` gets a session-local output directory (created on disk and
//! used as the process cwd) and the Playwright MCP flags for that directory,
//! the action / navigation timeouts and the browser proxy. Every other spec
//! passes through untouched.
//!
//! The sandboxed variant (`storageRoot`, a `profile` directory and
//! `PILOTDECK_SESSION_BROWSER_PROXY=1`) belongs to the session storage root of
//! the confined execution world, which this host does not provide; specs are
//! always prepared the unconfined way.
use super::protocol::{McpServerSpec, McpTransportSpec};
use std::path::{Path, PathBuf};

const DEFAULT_BROWSER_ACTION_TIMEOUT_MS: f64 = 30_000.0;
const DEFAULT_BROWSER_NAVIGATION_TIMEOUT_MS: f64 = 90_000.0;

/// The inputs `ProjectRuntimeRegistry` hands to the preparer
/// (ProjectRuntimeRegistry.ts:345-351): the runtime's project root and the
/// snapshot's `proxy` section.
#[derive(Clone, Debug, Default)]
pub struct BrowserUsePreparation {
    pub project_root: PathBuf,
    /// `snapshot.config.proxy.url`.
    pub proxy_url: Option<String>,
    /// `snapshot.config.proxy.noProxy`.
    pub proxy_no_proxy: Option<String>,
}

impl BrowserUsePreparation {
    /// `prepare({ projectRoot, sessionKey, proxy, specs })`. `session_dir` is
    /// `sanitizeSessionIdForPath(sessionKey)`; `env` is the gateway's
    /// `process.env`.
    pub fn prepare(
        &self,
        specs: Vec<McpServerSpec>,
        session_dir: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> std::io::Result<Vec<McpServerSpec>> {
        specs
            .into_iter()
            .map(|mut spec| {
                if spec.id != "browser-use" {
                    return Ok(spec);
                }
                let McpTransportSpec::Stdio { args, cwd, .. } = &mut spec.transport else {
                    return Ok(spec);
                };
                let output_dir = self
                    .project_root
                    .join(".pilotdeck")
                    .join("browser_screenshots")
                    .join(session_dir);
                std::fs::create_dir_all(&output_dir)?;
                let output_dir = path_string(&output_dir);
                let base = args.take().unwrap_or_default();
                *args = Some(build_browser_use_args(
                    base,
                    &output_dir,
                    env,
                    self.proxy_url.as_deref(),
                    self.proxy_no_proxy.as_deref(),
                ));
                *cwd = Some(output_dir);
                Ok(spec)
            })
            .collect()
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `buildBrowserUseArgs(baseArgs, outputDir, env, configProxy)`.
pub fn build_browser_use_args(
    base: Vec<String>,
    output_dir: &str,
    env: &dyn Fn(&str) -> Option<String>,
    config_proxy_url: Option<&str>,
    config_no_proxy: Option<&str>,
) -> Vec<String> {
    let timeout = |primary: &str, legacy: &str, default: f64| {
        let value = read_positive_integer_env(env(primary).as_deref())
            .or_else(|| read_positive_integer_env(env(legacy).as_deref()))
            .unwrap_or(default);
        pilotdeck_protocol::number::number_to_string(value)
    };
    let mut args = base;
    append_cli_arg(&mut args, "--output-dir", output_dir);
    append_cli_arg(
        &mut args,
        "--timeout-action",
        &timeout(
            "PILOTDECK_BROWSER_TIMEOUT_ACTION_MS",
            "PILOTDECK_BROWSER_ACTION_TIMEOUT_MS",
            DEFAULT_BROWSER_ACTION_TIMEOUT_MS,
        ),
    );
    append_cli_arg(
        &mut args,
        "--timeout-navigation",
        &timeout(
            "PILOTDECK_BROWSER_TIMEOUT_NAVIGATION_MS",
            "PILOTDECK_BROWSER_NAVIGATION_TIMEOUT_MS",
            DEFAULT_BROWSER_NAVIGATION_TIMEOUT_MS,
        ),
    );
    if let Some((server, source)) = resolve_proxy_server(env, config_proxy_url) {
        append_cli_arg(&mut args, "--proxy-server", &server);
        let bypass = resolve_proxy_bypass(env, config_no_proxy, source);
        if !bypass.is_empty() {
            append_cli_arg(&mut args, "--proxy-bypass", &bypass);
        }
    }
    args
}

/// `appendCliArg`: an existing `flag` or `flag=…` wins.
fn append_cli_arg(args: &mut Vec<String>, flag: &str, value: &str) {
    let prefix = format!("{flag}=");
    if args
        .iter()
        .any(|arg| arg == flag || arg.starts_with(&prefix))
    {
        return;
    }
    args.push(flag.to_owned());
    args.push(value.to_owned());
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProxySource {
    BrowserEnv,
    Env,
    Config,
}

fn resolve_proxy_server(
    env: &dyn Fn(&str) -> Option<String>,
    config_url: Option<&str>,
) -> Option<(String, ProxySource)> {
    if let Some(explicit) = clean(env("PILOTDECK_BROWSER_PROXY_SERVER")) {
        if ["0", "false", "off", "none", "direct"]
            .iter()
            .any(|word| explicit.eq_ignore_ascii_case(word))
        {
            return None;
        }
        return Some((explicit, ProxySource::BrowserEnv));
    }
    let from_env = clean(env("PILOTDECK_BROWSER_PROXY_FROM_ENV")).unwrap_or_default();
    if ["1", "true", "on", "yes"]
        .iter()
        .any(|word| from_env.eq_ignore_ascii_case(word))
    {
        let proxy = [
            "PILOTDECK_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "http_proxy",
            "HTTP_PROXY",
        ]
        .iter()
        .find_map(|name| clean(env(name)));
        if let Some(proxy) = proxy {
            return Some((proxy, ProxySource::Env));
        }
    }
    clean(config_url.map(str::to_owned)).map(|url| (url, ProxySource::Config))
}

fn resolve_proxy_bypass(
    env: &dyn Fn(&str) -> Option<String>,
    config_no_proxy: Option<&str>,
    source: ProxySource,
) -> String {
    if let Some(explicit) = clean(env("PILOTDECK_BROWSER_PROXY_BYPASS")) {
        return explicit;
    }
    let no_proxy = clean(env("no_proxy")).or_else(|| clean(env("NO_PROXY")));
    let config = if source == ProxySource::Config {
        clean(config_no_proxy.map(str::to_owned))
    } else {
        None
    };
    [no_proxy, config]
        .into_iter()
        .flatten()
        .chain(
            ["localhost", "127.0.0.1", "host.docker.internal"]
                .into_iter()
                .map(str::to_owned),
        )
        .collect::<Vec<_>>()
        .join(",")
}

/// `cleanEnvValue`: `value?.trim()`, empty as unset. JS `trim` strips the
/// ECMAScript whitespace set, which `char::is_whitespace` plus BOM covers.
fn clean(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// `readPositiveIntegerEnv` (LocalGatewayBootConfig.ts:62-67):
/// `Number.parseInt(value.trim(), 10)`, kept when finite and positive.
fn read_positive_integer_env(value: Option<&str>) -> Option<f64> {
    let value = value?.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (negative, rest) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let number: f64 = rest[..end].parse().ok()?;
    (!negative && number.is_finite() && number > 0.0).then_some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn build(
        base: &[&str],
        pairs: &'static [(&'static str, &'static str)],
        config: Option<&str>,
        no_proxy: Option<&str>,
    ) -> Vec<String> {
        build_browser_use_args(
            base.iter().map(|s| (*s).to_owned()).collect(),
            "/out",
            &env_of(pairs),
            config,
            no_proxy,
        )
    }

    #[test]
    fn defaults_append_output_dir_and_timeouts() {
        assert_eq!(
            build(&["entry.js", "--browser", "chromium"], &[], None, None),
            [
                "entry.js",
                "--browser",
                "chromium",
                "--output-dir",
                "/out",
                "--timeout-action",
                "30000",
                "--timeout-navigation",
                "90000"
            ]
        );
    }

    #[test]
    fn existing_flags_win_and_timeouts_follow_parse_int() {
        let args = build(
            &["--output-dir=/mine", "--timeout-navigation", "5"],
            &[
                ("PILOTDECK_BROWSER_TIMEOUT_ACTION_MS", " 0x10 "),
                ("PILOTDECK_BROWSER_ACTION_TIMEOUT_MS", "1500.9ms"),
            ],
            None,
            None,
        );
        assert_eq!(
            args,
            [
                "--output-dir=/mine",
                "--timeout-navigation",
                "5",
                "--timeout-action",
                "1500"
            ]
        );
        let args = build(
            &[],
            &[("PILOTDECK_BROWSER_TIMEOUT_NAVIGATION_MS", "-3")],
            None,
            None,
        );
        assert_eq!(args[5], "90000");
    }

    #[test]
    fn proxy_sources_and_bypass_follow_ts_precedence() {
        let explicit = build(
            &[],
            &[
                ("PILOTDECK_BROWSER_PROXY_SERVER", " http://b:1 "),
                ("PILOTDECK_BROWSER_PROXY_FROM_ENV", "yes"),
                ("HTTPS_PROXY", "http://e:2"),
                ("NO_PROXY", "x.test"),
            ],
            Some("http://c:3"),
            Some("cfg.test"),
        );
        assert_eq!(
            &explicit[6..],
            [
                "--proxy-server",
                "http://b:1",
                "--proxy-bypass",
                "x.test,localhost,127.0.0.1,host.docker.internal"
            ]
        );
        let off = build(
            &[],
            &[("PILOTDECK_BROWSER_PROXY_SERVER", "Direct")],
            Some("http://c:3"),
            None,
        );
        assert_eq!(off.len(), 6);
        let from_env = build(
            &[],
            &[
                ("PILOTDECK_BROWSER_PROXY_FROM_ENV", "ON"),
                ("https_proxy", " "),
                ("HTTPS_PROXY", "http://e:2"),
                ("PILOTDECK_BROWSER_PROXY_BYPASS", "only.test"),
            ],
            None,
            None,
        );
        assert_eq!(
            &from_env[6..],
            [
                "--proxy-server",
                "http://e:2",
                "--proxy-bypass",
                "only.test"
            ]
        );
        let config = build(&[], &[], Some(" http://c:3 "), Some("cfg.test"));
        assert_eq!(
            &config[6..],
            [
                "--proxy-server",
                "http://c:3",
                "--proxy-bypass",
                "cfg.test,localhost,127.0.0.1,host.docker.internal"
            ]
        );
    }

    #[test]
    fn prepare_touches_only_browser_use_stdio() {
        let root = std::env::temp_dir().join(format!("pd-browser-use-{}", std::process::id()));
        let preparation = BrowserUsePreparation {
            project_root: root.clone(),
            ..Default::default()
        };
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
        let prepared = preparation
            .prepare(vec![stdio("other"), stdio("browser-use")], "cli-s", &|_| {
                None
            })
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
