//! The side effects of the baseline's telemetry runtime context
//! (src/telemetry/context.ts `resolveTelemetryRuntimeContext`), which
//! `createTelemetryCollector` resolves on every `pilotdeck server` start,
//! whether telemetry is enabled or not (src/cli/pilotdeck.ts:41-44).
//!
//! The context itself only feeds uploaded events, and the Rust port does not
//! upload telemetry (the baseline posts to an external collector). What the
//! context does leave behind locally are its two `git` probes: they run with
//! the server's working directory and inherited stderr, so outside a git work
//! tree git's own `fatal: not a git repository ...` lines reach the server's
//! stderr. They are reproduced here by running the same commands the same way.
use super::paths::Env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `execFileSync(..., { timeout: 1500 })` in both probes.
const GIT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Run the probes `resolveTelemetryRuntimeContext` runs, in its order:
/// `resolveDeploymentMode` (context.ts:68-75) reaches the work-tree probe
/// only when the process looks like neither a container nor the desktop
/// app; `resolveAppCommitHash` (context.ts:136-151) runs `git rev-parse
/// --short HEAD` unless a commit hash comes from the environment.
pub fn probe_runtime_context(env: &Env, cwd: &Path) {
    if !looks_like_docker(env) && !looks_like_desktop_installer() {
        run_git(&["rev-parse", "--is-inside-work-tree"], cwd);
    }
    // `env.COMMIT_HASH ?? env.GIT_COMMIT ?? env.PILOTDECK_GIT_SHA ?? env.GIT_SHA`,
    // then `if (fromEnv)`: an empty first value still falls through to git.
    let from_env = ["COMMIT_HASH", "GIT_COMMIT", "PILOTDECK_GIT_SHA", "GIT_SHA"]
        .into_iter()
        .find_map(|name| env.get(name));
    if from_env.is_none_or(|value| value.is_empty()) {
        run_git(&["rev-parse", "--short", "HEAD"], cwd);
    }
}

/// context.ts:77-86.
fn looks_like_docker(env: &Env) -> bool {
    if env.string("DOCKER_CONTAINER").as_deref() == Some("1")
        || env
            .get("KUBERNETES_SERVICE_HOST")
            .is_some_and(|value| !value.is_empty())
    {
        return true;
    }
    if Path::new("/.dockerenv").exists() {
        return true;
    }
    std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroup| {
        let lower = cgroup.to_lowercase();
        ["docker", "containerd", "kubepods", "podman"]
            .iter()
            .any(|needle| lower.contains(needle))
    })
}

/// context.ts:88-92. The baseline's `process.execPath` is the runtime that
/// runs it; only the desktop bundle's runtime lives under `PilotDeck.app`.
/// The Rust executable is the runtime here, so the bundle check applies to
/// it. (The baseline's `pilotdeck.exe` suffix names the Windows desktop
/// runtime, which a plain Windows build of this binary would also match
/// without being one, so it is not carried over.)
fn looks_like_desktop_installer() -> bool {
    std::env::current_exe().is_ok_and(|exe| {
        exe.to_string_lossy()
            .to_lowercase()
            .contains("pilotdeck.app")
    })
}

/// `execFileSync("git", args, { cwd, encoding: "utf8", timeout: 1500 })`:
/// stdout is captured and discarded here (only the context uses it), stderr
/// goes to the parent's stderr, and a failure to spawn, a non-zero exit or
/// the timeout are all swallowed by the baseline's `catch`.
fn run_git(args: &[&str], cwd: &Path) {
    let Ok(mut child) = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    else {
        return;
    };
    drop(child.stdin.take());
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if started.elapsed() >= GIT_TIMEOUT => {
                // Node's timeout sends SIGTERM (`killSignal` default).
                #[cfg(unix)]
                // The pid is our own child, not yet reaped.
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}
