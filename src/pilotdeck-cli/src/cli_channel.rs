//! The default command: `createLocalGateway({ projectRoot: process.cwd() })`
//! then `new CliChannel({ argv }).start(...)` (src/cli/pilotdeck.ts:346-348,
//! src/adapters/channel/cli/CliChannel.ts). The gateway runs in this process;
//! each prompt is one `submitTurn`, rendered to stdout as it streams.
use super::paths::{self, Env};
use super::{Failure, config, probe, server, skills, stderr_line, stdout_line};
use pilotdeck_gateway::channel::cli;
use pilotdeck_gateway::native_process::{LocalGateway, LocalOptions};
use pilotdeck_protocol::{JsObject, JsValue};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn run(argv: &[String], env: &Env, cwd: &Path) -> Result<u8, Failure> {
    let pilot_home = paths::pilot_home(env, cwd)?;
    // Node's import.meta.url is the realpath of the entry module; match it so a
    // symlinked launcher still probes beside the real install root.
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
    config::check(env, cwd, &pilot_home)?;
    telemetry_git_probes(env, cwd);
    let gateway = LocalGateway::start(LocalOptions {
        pilot_home,
        project: cwd.to_path_buf(),
        os_home: env.get("HOME").map(PathBuf::from),
        sidecar: exe_dir.unwrap_or(Path::new("")).join(server::SIDECAR),
        env: env.strings(),
        builtin_skills_root: Some(builtin_root),
        builtin_plugins_root: server::builtin_plugins_root(env, cwd, exe_dir),
    })
    .map_err(|error| Failure::Error(error.to_string()))?;
    // resolveGateway: the fixed-address probe comes after the local gateway
    // exists. A reachable server would take every turn over WebSocket; its
    // address cannot be pointed at an isolated server, so that branch is not
    // ported.
    if probe::probe_gateway_server(env, cwd).is_some() {
        return Err(Failure::Todo(
            "the default CliChannel against a running server",
        ));
    }
    write_stderr("[pilotdeck] gateway server unavailable; using in-process mode.\n");
    let channel = Channel {
        gateway,
        session_key: cli::default_session_key(&cwd.to_string_lossy()),
    };
    let prompt = paths::js_trim(&argv.join(" ")).to_owned();
    let outcome = if prompt.is_empty() {
        channel.run_interactive()
    } else {
        channel.submit_and_render(&prompt)
    };
    // main().catch: the thrown message, then the process lives on.
    if let Err(message) = outcome {
        stderr_line(&message);
    }
    // The baseline's in-process gateway keeps its event loop alive, so the
    // command never exits on its own once the input is done: it waits until
    // it is killed. Keep the gateway (and its sessions) up the same way.
    loop {
        std::thread::park();
    }
}

struct Channel {
    gateway: LocalGateway,
    session_key: String,
}

impl Channel {
    /// `runInteractive`: one turn per non-blank input line, each followed by
    /// a newline, until the input ends.
    fn run_interactive(&self) -> Result<(), String> {
        let mut stdin = std::io::stdin().lock();
        let mut raw = Vec::new();
        loop {
            raw.clear();
            let read = stdin.read_until(b'\n', &mut raw).unwrap_or(0);
            if read == 0 {
                return Ok(());
            }
            if raw.last() == Some(&b'\n') {
                raw.pop();
                if raw.last() == Some(&b'\r') {
                    raw.pop();
                }
            }
            // readline also ends a line at a lone `\r`.
            for line in String::from_utf8_lossy(&raw).split('\r') {
                let text = paths::js_trim(line);
                if text.is_empty() {
                    continue;
                }
                self.submit_and_render(text)?;
                write_stdout("\n");
            }
        }
    }

    /// `submitAndRender`: `{ sessionKey, channelKey: "cli", projectKey:
    /// undefined, message, canPrompt: false }`, each rendered event written
    /// as it arrives.
    fn submit_and_render(&self, message: &str) -> Result<(), String> {
        let mut input = JsObject::new();
        input.insert(
            "sessionKey".into(),
            JsValue::String(self.session_key.as_str().into()),
        );
        input.insert(
            "channelKey".into(),
            JsValue::String(cli::CHANNEL_KEY.into()),
        );
        input.insert("message".into(), JsValue::String(message.into()));
        input.insert("canPrompt".into(), JsValue::Bool(false));
        self.gateway.submit_turn(
            JsValue::Object(input),
            Arc::new(|event| {
                if let Some(text) = cli::render_event(&event) {
                    write_stdout(&text);
                }
            }),
        )
    }
}

/// The local gateway resolves the telemetry runtime context even with
/// telemetry disabled (src/telemetry/context.ts): outside a container it asks
/// `git rev-parse --is-inside-work-tree` whether this is a source checkout,
/// and without a commit hash in the environment `git rev-parse --short HEAD`.
/// Both run in the working directory with stdout captured and stderr
/// inherited, so outside a git checkout each leaves git's `fatal:` line on
/// this process's stderr. Their answers feed only telemetry, which the port
/// does not send.
fn telemetry_git_probes(env: &Env, cwd: &Path) {
    let set = |name: &str| env.get(name).is_some_and(|value| !value.is_empty());
    let git = |args: &[&str]| {
        let _ = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .output();
    };
    // looksLikeDocker, then looksLikeDesktopInstaller (an Electron runtime or
    // a `pilotdeck.app` / `pilotdeck.exe` executable, never this binary).
    let docker = env
        .get("DOCKER_CONTAINER")
        .is_some_and(|value| value == "1")
        || set("KUBERNETES_SERVICE_HOST")
        || Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroup| {
            let cgroup = cgroup.to_lowercase();
            ["docker", "containerd", "kubepods", "podman"]
                .iter()
                .any(|name| cgroup.contains(name))
        });
    if !docker {
        git(&["rev-parse", "--is-inside-work-tree"]);
    }
    // `COMMIT_HASH ?? GIT_COMMIT ?? PILOTDECK_GIT_SHA ?? GIT_SHA`, then
    // `if (fromEnv)`: an empty first value still falls through to git.
    let from_env = ["COMMIT_HASH", "GIT_COMMIT", "PILOTDECK_GIT_SHA", "GIT_SHA"]
        .iter()
        .find_map(|name| env.get(name));
    if from_env.is_none_or(|value| value.is_empty()) {
        git(&["rev-parse", "--short", "HEAD"]);
    }
}

fn write_stdout(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn write_stderr(text: &str) {
    let mut out = std::io::stderr().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}
