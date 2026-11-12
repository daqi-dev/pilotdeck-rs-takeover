//! `pilotdeck server [--port N] [--host H]`: the server branch of main()
//! in src/cli/pilotdeck.ts:35-299, over the native gateway host.
// `super`, not `crate`: tests/sdk_signal_exit.rs mounts main.rs as a module.
use super::paths::{self, Env};
use super::{Failure, config, skills, stderr_line, stdout_line};
use pilotdeck_gateway::native_process::{self, ServerOptions};
use std::path::{Path, PathBuf};

/// The agent-loop sidecar ships beside this executable.
const SIDECAR: &str = "pilotdeck-agent-loop-sidecar";

/// TelemetrySender's counters (src/telemetry/sender.ts) as the shutdown bundle
/// logs them. The Rust port has no telemetry collector yet, so nothing is ever
/// queued or sent and the snapshot is the baseline's own with telemetry off
/// (its default). With `telemetry.enabled: true` the baseline's numbers move
/// and these do not.
const TELEMETRY_SNAPSHOT: &str =
    r#"{"queued":0,"sent":0,"sendFailures":0,"retries":0,"dropped":0,"queueDepth":0}"#;

pub fn run(argv: &[String], env: &Env, cwd: &Path) -> Result<u8, Failure> {
    let pilot_home = paths::pilot_home(env, cwd)?;
    // loadPilotConfig({ projectRoot, env }) comes first: a missing or invalid
    // config ends the command before anything is created.
    let loaded = config::load(env, cwd, &pilot_home)?;
    // createTelemetryCollector(): its runtime context's git probes.
    super::telemetry::probe_runtime_context(env, cwd);
    // `if (snapshot.config.proxy?.url) await installGlobalProxy(url)`.
    if let Some(url) = loaded.proxy_url.as_deref() {
        super::proxy::install_from_config(env, url);
    }
    // createLocalGateway(): the legacy bundled-skill migration and its output.
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe));
    let exe_dir = exe.as_deref().and_then(Path::parent);
    let builtin_root = skills::builtin_skills_root(env, cwd, exe_dir)?;
    let migration = skills::migrate_legacy_bundled_skill_copies(&pilot_home, &builtin_root)?;
    if let Some(notice) = migration.notice {
        stdout_line(&notice);
    }
    for warning in migration.warnings {
        stderr_line(&warning);
    }
    let options = ServerOptions {
        token_path: paths::gateway_token_path(env, cwd)?,
        pilot_home,
        project: cwd.to_path_buf(),
        os_home: env.get("HOME").map(PathBuf::from),
        // readStringFlag(argv, "--host") ?? env.PILOTDECK_GATEWAY_HOST
        host: string_flag(argv, "--host").or_else(|| env.string("PILOTDECK_GATEWAY_HOST")),
        // readPort(argv) ?? (Number.isFinite(envPort) ? envPort : 18789)
        port: port_flag(argv)
            .or_else(|| {
                native_process::parse_port(
                    &env.string("PILOTDECK_GATEWAY_PORT").unwrap_or_default(),
                )
            })
            .unwrap_or(native_process::DEFAULT_PORT),
        allow_remote_host: env.string("PILOTDECK_GATEWAY_ALLOW_REMOTE").as_deref() == Some("1"),
        // env.PILOTDECK_GATEWAY_TOKEN || env.PILOTDECK_TOKEN
        token: ["PILOTDECK_GATEWAY_TOKEN", "PILOTDECK_TOKEN"]
            .into_iter()
            .filter_map(|name| env.string(name))
            .find(|token| !token.is_empty()),
        sidecar: exe_dir.unwrap_or(Path::new("")).join(SIDECAR),
        env: env.strings(),
        builtin_skills_root: Some(builtin_root),
        builtin_plugins_root: builtin_plugins_root(env, cwd, exe_dir),
    };
    native_process::serve(options, |listening| {
        stdout_line(&format!("PilotDeck server listening: {}", listening.url));
        stdout_line(&format!("WebSocket: {}", listening.ws_url));
        if let Some(path) = &listening.token_path {
            stdout_line(&format!("Token: {}", path.display()));
        }
    })
    .map_err(|error| Failure::Error(error.to_string()))?;
    // PilotDeckServerShutdownBundle logs this after the listener closed; the
    // SIGINT / SIGTERM handlers then exit 0.
    stdout_line(&format!(
        "[telemetry] shutdown snapshot {TELEMETRY_SNAPSHOT}"
    ));
    Ok(0)
}

/// The builtin plugins directory. The baseline has no such setting: it scans
/// the directory its compiled `loadBuiltinPlugins` module sits in
/// (`dist/src/extension/plugins/builtin`). The Rust layout follows the bundled
/// skills convention (`skills::builtin_skills_root`): a non-empty
/// `PILOTDECK_BUNDLED_PLUGINS_DIR` wins, otherwise the first existing
/// `builtin-plugins` directory one or two levels above the executable. Unlike
/// skills there is no working-directory fallback — plugins can declare
/// processes to start, so nothing is loaded from wherever the server happens
/// to be launched. `None` ships no builtin plugins, which is what the
/// baseline does when its scan fails.
///
/// The directory is reported with symlinks resolved: the baseline's is the
/// location of a loaded module, which Node always gives as a real path, and
/// that path shows up in the skills section of the system prompt.
fn builtin_plugins_root(env: &Env, cwd: &Path, exe_dir: Option<&Path>) -> Option<PathBuf> {
    let real = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    if let Some(dir) = env
        .get("PILOTDECK_BUNDLED_PLUGINS_DIR")
        .filter(|dir| !dir.is_empty())
    {
        return Some(real(paths::resolve(cwd, Path::new(dir))));
    }
    let exe_dir = exe_dir?;
    [
        exe_dir.join("../builtin-plugins"),
        exe_dir.join("../../builtin-plugins"),
    ]
    .into_iter()
    .map(|candidate| paths::resolve(cwd, &candidate))
    .find(|candidate| candidate.is_dir())
    .map(real)
}

/// readPort (pilotdeck.ts:517-525): the argument after the first `--port`,
/// whatever it looks like, read with parseInt; not finite means absent.
fn port_flag(argv: &[String]) -> Option<f64> {
    let index = argv.iter().position(|arg| arg == "--port")?;
    native_process::parse_port(argv.get(index + 1)?)
}

/// readStringFlag (pilotdeck.ts:527-534): the argument after the first
/// occurrence, unless it is missing, empty or starts with `--`.
fn string_flag(argv: &[String], flag: &str) -> Option<String> {
    let index = argv.iter().position(|arg| arg == flag)?;
    argv.get(index + 1)
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn builtin_plugins_root_is_the_override_then_the_install_layout() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("pdrs-plugins-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let exe_dir = base.join("stage/bin");
        let cwd = base.join("work");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(cwd.join("rel")).unwrap();
        // Nothing shipped and no override: no builtin plugins, and never the
        // working directory.
        std::fs::create_dir_all(cwd.join("builtin-plugins")).unwrap();
        assert_eq!(
            builtin_plugins_root(&Env::from_pairs(&[]), &cwd, Some(&exe_dir)),
            None
        );
        assert_eq!(
            builtin_plugins_root(&Env::from_pairs(&[]), &cwd, None),
            None
        );
        // The install layout: beside `bin`.
        std::fs::create_dir_all(base.join("stage/builtin-plugins")).unwrap();
        let empty = Env::from_pairs(&[("PILOTDECK_BUNDLED_PLUGINS_DIR", "")]);
        assert_eq!(
            builtin_plugins_root(&empty, &cwd, Some(&exe_dir)),
            Some(base.join("stage/builtin-plugins"))
        );
        // A non-empty override wins, resolved against the working directory.
        let env = Env::from_pairs(&[("PILOTDECK_BUNDLED_PLUGINS_DIR", "rel")]);
        assert_eq!(
            builtin_plugins_root(&env, &cwd, Some(&exe_dir)),
            Some(cwd.join("rel"))
        );
        // A missing override directory is kept as named (nothing loads from it).
        let env =
            Env::from_pairs(&[("PILOTDECK_BUNDLED_PLUGINS_DIR", "/nonexistent/pdrs-plugins")]);
        assert_eq!(
            builtin_plugins_root(&env, &cwd, Some(&exe_dir)),
            Some(PathBuf::from("/nonexistent/pdrs-plugins"))
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn flags_are_read_like_the_baseline_helpers() {
        assert_eq!(
            port_flag(&args(&["server", "--port", "38513"])),
            Some(38513.0)
        );
        assert_eq!(
            port_flag(&args(&["server", "--port", "1x", "--port", "2"])),
            Some(1.0)
        );
        assert_eq!(port_flag(&args(&["server", "--port", "abc"])), None);
        assert_eq!(port_flag(&args(&["server", "--port"])), None);
        assert_eq!(port_flag(&args(&["server"])), None);
        assert_eq!(
            string_flag(&args(&["server", "--host", "localhost"]), "--host").as_deref(),
            Some("localhost")
        );
        assert_eq!(
            string_flag(&args(&["server", "--host", "--port"]), "--host"),
            None
        );
        assert_eq!(
            string_flag(&args(&["server", "--host", ""]), "--host"),
            None
        );
        assert_eq!(string_flag(&args(&["server", "--host"]), "--host"), None);
    }
}
