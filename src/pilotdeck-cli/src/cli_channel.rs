//! The default command: `createLocalGateway({ projectRoot: process.cwd() })`
//! then `new CliChannel({ argv }).start(...)` (src/cli/pilotdeck.ts:346-348,
//! src/adapters/channel/cli/CliChannel.ts). Each prompt is one `submitTurn`,
//! using the probed server or local fallback and rendering events as they arrive.
pub(crate) mod remote;

use super::paths::{self, Env};
use super::{Failure, config, probe, server, skills, stderr_line, stdout_line};
use pilotdeck_gateway::channel::cli;
use pilotdeck_gateway::native_process::{LocalGateway, LocalOptions};
use pilotdeck_protocol::{JsObject, JsValue};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const DRAIN_BUDGET: Duration = Duration::from_secs(5);

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
    // The baseline constructs its fallback before probing. A health response
    // alone is insufficient: a refused WebSocket/hello also selects fallback.
    let remote = probe::probe_gateway_server(env, cwd)
        .and_then(|token| Remote::connect("ws://127.0.0.1:18789/ws", &token).ok());
    if remote.is_none() {
        write_stderr("[pilotdeck] gateway server unavailable; using in-process mode.\n");
    }
    let mut channel = Channel {
        gateway: &gateway,
        remote,
        session_key: cli::default_session_key(&cwd.to_string_lossy()),
    };
    let prompt = paths::js_trim(&argv.join(" ")).to_owned();
    let outcome = if prompt.is_empty() {
        channel.run_interactive()
    } else {
        channel.submit_and_render(&prompt)
    };
    // The fixed baseline leaves both clients alive after completion. The
    // authorized CLI hang fix releases them on success, input EOF and error.
    if let Some(remote) = &mut channel.remote {
        remote.runtime.block_on(remote.gateway.close());
    }
    let shutdown = gateway.shutdown(DRAIN_BUDGET);
    outcome.and(shutdown).map_err(Failure::Error)?;
    Ok(0)
}

struct Remote {
    runtime: tokio::runtime::Runtime,
    gateway: remote::RemoteGateway,
}

impl Remote {
    fn connect(url: &str, token: &str) -> Result<Self, String> {
        // The socket reader must keep servicing ping/close while the main
        // thread is blocked waiting for the next interactive input line.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let gateway = runtime.block_on(remote::RemoteGateway::connect(url, token))?;
        Ok(Self { runtime, gateway })
    }
}

struct Channel<'a> {
    gateway: &'a LocalGateway,
    remote: Option<Remote>,
    session_key: String,
}

impl Channel<'_> {
    /// `runInteractive`: one turn per non-blank input line, each followed by
    /// a newline, until the input ends.
    fn run_interactive(&mut self) -> Result<(), String> {
        let mut stdin = std::io::stdin().lock();
        let mut raw = Vec::new();
        let mut skip_lf = false;
        while read_line(&mut stdin, &mut raw, &mut skip_lf).map_err(|error| error.to_string())? {
            let line = String::from_utf8_lossy(&raw);
            let text = paths::js_trim(&line);
            if !text.is_empty() {
                self.submit_and_render(text)?;
                write_stdout("\n");
            }
        }
        Ok(())
    }

    /// `submitAndRender`: `{ sessionKey, channelKey: "cli", projectKey:
    /// undefined, message, canPrompt: false }`, each rendered event written
    /// as it arrives.
    fn submit_and_render(&mut self, message: &str) -> Result<(), String> {
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
        let render = |event| {
            if let Some(text) = cli::render_event(&event) {
                write_stdout(&text);
            }
        };
        match &mut self.remote {
            Some(remote) => remote
                .runtime
                .block_on(remote.gateway.submit_turn(JsValue::Object(input), render)),
            None => self
                .gateway
                .submit_turn(JsValue::Object(input), Arc::new(render)),
        }
    }
}

/// readline emits on CR immediately, even when no LF or EOF has arrived yet.
/// Suppress a following LF so CRLF represents one line across buffer reads.
fn read_line(
    input: &mut impl BufRead,
    line: &mut Vec<u8>,
    skip_lf: &mut bool,
) -> std::io::Result<bool> {
    line.clear();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        if *skip_lf {
            *skip_lf = false;
            if available[0] == b'\n' {
                input.consume(1);
                continue;
            }
        }
        if let Some(end) = available
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
        {
            line.extend_from_slice(&available[..end]);
            *skip_lf = available[end] == b'\r';
            input.consume(end + 1);
            return Ok(true);
        }
        let count = available.len();
        line.extend_from_slice(available);
        input.consume(count);
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

#[cfg(test)]
mod tests {
    use super::read_line;
    use std::io::{BufReader, Cursor};

    #[test]
    fn readline_accepts_cr_lf_crlf_and_unterminated_tail() {
        // A one-byte buffer forces CRLF and UTF-8 across fill_buf boundaries.
        let mut input = BufReader::with_capacity(1, Cursor::new("first\r\n\n中文\rthird\ntail"));
        let mut raw = Vec::new();
        let mut skip_lf = false;
        let mut lines = Vec::new();
        while read_line(&mut input, &mut raw, &mut skip_lf).unwrap() {
            lines.push(String::from_utf8(raw.clone()).unwrap());
        }
        assert_eq!(lines, ["first", "", "中文", "third", "tail"]);
    }

    #[test]
    fn readline_returns_after_cr_without_reading_the_next_chunk() {
        struct FirstChunk;
        impl std::io::Read for FirstChunk {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("read_line uses buffered input");
            }
        }
        impl std::io::BufRead for FirstChunk {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                Ok(b"first\r")
            }
            fn consume(&mut self, amount: usize) {
                assert_eq!(amount, 6);
            }
        }
        let mut raw = Vec::new();
        let mut skip_lf = false;
        assert!(read_line(&mut FirstChunk, &mut raw, &mut skip_lf).unwrap());
        assert_eq!(raw, b"first");
        assert!(skip_lf);
    }
}
