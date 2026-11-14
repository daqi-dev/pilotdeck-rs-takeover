//! The side effects of the baseline's telemetry runtime context
//! (src/telemetry/context.ts `resolveTelemetryRuntimeContext`), which
//! `createTelemetryCollector` resolves on every `pilotdeck server` start,
//! whether telemetry is enabled or not (src/cli/pilotdeck.ts:41-44).
//!
//! The context itself only feeds uploaded events, and the Rust port does not
//! upload telemetry (the baseline posts to an external collector). What the
//! context does leave behind locally are its two `git` probes: they run with
//! the server's working directory and their stderr is passed on to the
//! server's, so outside a git work tree git's own `fatal: not a git
//! repository ...` lines reach the server's stderr. They are reproduced here
//! by running the same commands and passing their stderr on in the same order.
use super::paths::Env;
use pilotdeck_protocol::{JsObject, JsValue};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `execFileSync(..., { timeout: 1500 })` in both probes.
const GIT_TIMEOUT: Duration = Duration::from_millis(1500);
/// The longest pause between two looks at a running probe.
const GIT_POLL_CAP: Duration = Duration::from_millis(5);

/// Run the probes `resolveTelemetryRuntimeContext` runs, in its order:
/// `resolveDeploymentMode` (context.ts:68-75) reaches the work-tree probe
/// only when the process looks like neither a container nor the desktop
/// app; `resolveAppCommitHash` (context.ts:136-151) runs `git rev-parse
/// --short HEAD` unless a commit hash comes from the environment.
///
/// Neither probe reads the other's result, so both run at once. Each one's
/// stderr is collected and written to ours once both are done, in the
/// baseline's order: `execFileSync` without a `stdio` option pipes the
/// child's stderr and writes what it collected to the parent's after the
/// child ended (Node's `child_process.js`, `inheritStderr`).
pub fn probe_runtime_context(env: &Env, cwd: &Path) {
    let mut probes: Vec<&[&str]> = Vec::new();
    if !looks_like_docker(env) && !looks_like_desktop_installer() {
        probes.push(&["rev-parse", "--is-inside-work-tree"]);
    }
    // `env.COMMIT_HASH ?? env.GIT_COMMIT ?? env.PILOTDECK_GIT_SHA ?? env.GIT_SHA`,
    // then `if (fromEnv)`: an empty first value still falls through to git.
    let from_env = ["COMMIT_HASH", "GIT_COMMIT", "PILOTDECK_GIT_SHA", "GIT_SHA"]
        .into_iter()
        .find_map(|name| env.get(name));
    if from_env.is_none_or(|value| value.is_empty()) {
        probes.push(&["rev-parse", "--short", "HEAD"]);
    }
    let outputs: Vec<Vec<u8>> = std::thread::scope(|scope| {
        let running: Vec<_> = probes
            .iter()
            .map(|args| scope.spawn(move || run_git(args, cwd)))
            .collect();
        running
            .into_iter()
            .map(|probe| probe.join().unwrap_or_default())
            .collect()
    });
    let mut stderr = std::io::stderr().lock();
    for output in outputs {
        let _ = stderr.write_all(&output);
    }
    let _ = stderr.flush();
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
/// is collected and returned for the caller to write to ours, and a failure
/// to spawn, a non-zero exit or the timeout are all swallowed by the
/// baseline's `catch`.
fn run_git(args: &[&str], cwd: &Path) -> Vec<u8> {
    let Ok(mut child) = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return Vec::new();
    };
    drop(child.stdin.take());
    let started = Instant::now();
    // A reader drains stderr while the child runs, so a chatty child never
    // blocks on a full pipe.
    let (sender, collected) = std::sync::mpsc::channel();
    if let Some(mut pipe) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sender.send(bytes);
        });
    }
    // The probe sits on the server's start path, so its exit is noticed
    // promptly: the poll starts at 100 µs and backs off to the 5 ms cap.
    let mut pause = Duration::from_micros(100);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
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
                break;
            }
            Ok(None) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(GIT_POLL_CAP);
            }
        }
    }
    // A descendant that kept the pipe open does not hold the start past the
    // probe's own bound.
    let left = GIT_TIMEOUT.saturating_sub(started.elapsed());
    collected.recv_timeout(left).unwrap_or_default()
}

/// The local half of `TelemetrySender` (src/telemetry/sender.ts) that runs
/// whether telemetry is enabled or not: the constructor restores the queue
/// file (`restoreQueue`, :143-165), the shutdown bundle logs the counters
/// (`PilotDeckServerShutdownBundle.ts:36`), and `shutdown()` writes the queue
/// back or removes the file when it is empty (`persistQueue`, :167-183).
/// Nothing is ever enqueued or uploaded here: the Rust port does not send
/// telemetry, so with `telemetry.enabled: true` the baseline's counters move
/// and these do not.
pub struct TelemetryQueue {
    path: std::path::PathBuf,
    items: Vec<JsValue>,
}

impl TelemetryQueue {
    /// `resolveTelemetryConfig` (collector.ts:142-144): `ANALYTICS_QUEUE_FILE`
    /// resolved against the working directory when set (an empty value counts
    /// as unset), otherwise `<pilotHome>/telemetry/queue.jsonl`.
    pub fn restore(env: &Env, cwd: &Path, pilot_home: &Path) -> Self {
        let path = match env.string("ANALYTICS_QUEUE_FILE") {
            Some(value) if !value.is_empty() => super::paths::resolve(cwd, Path::new(&value)),
            _ => pilot_home.join("telemetry").join("queue.jsonl"),
        };
        let mut items = Vec::new();
        if let Ok(raw) = std::fs::read_to_string(&path) {
            for line in raw.split('\n').map(super::paths::js_trim) {
                if line.is_empty() {
                    continue;
                }
                let Ok(parsed) = pilotdeck_protocol::parse_json(line) else {
                    continue;
                };
                let Some(event) = parsed.get("event") else {
                    continue;
                };
                if !event.get("eventId").is_some_and(js_truthy) {
                    continue;
                }
                // `Math.max(0, Number(parsed.attempts) || 0)`.
                let attempts = js_number(parsed.get("attempts"));
                let attempts = if attempts.is_nan() || attempts == 0.0 {
                    0.0
                } else {
                    attempts.max(0.0)
                };
                let mut item = JsObject::new();
                item.insert("event".into(), event.clone());
                item.insert("attempts".into(), JsValue::Number(attempts));
                items.push(JsValue::Object(item));
            }
        }
        Self { path, items }
    }

    /// `JSON.stringify(sender.snapshot())`: nothing was queued, sent or
    /// dropped in this process; the depth is what the restore brought back.
    pub fn snapshot(&self) -> String {
        format!(
            r#"{{"queued":0,"sent":0,"sendFailures":0,"retries":0,"dropped":0,"queueDepth":{}}}"#,
            self.items.len()
        )
    }

    /// `persistQueue`: every error is swallowed.
    pub fn persist(&self) {
        if self.items.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let lines: Vec<String> = self
            .items
            .iter()
            .filter_map(|item| pilotdeck_protocol::stringify_json(item).ok().flatten())
            .collect();
        let _ = std::fs::write(&self.path, format!("{}\n", lines.join("\n")));
    }
}

fn js_truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(number) => *number != 0.0 && !number.is_nan(),
        JsValue::String(text) => !text.is_empty(),
        JsValue::Array(_) | JsValue::Object(_) => true,
    }
}

/// `Number(value)` for the JSON values a queue line can carry. An array goes
/// through its string form: `[]` is `""` (0), a one-element array is that
/// element's string, anything longer has a comma (NaN).
fn js_number(value: Option<&JsValue>) -> f64 {
    match value {
        None | Some(JsValue::Undefined) => f64::NAN,
        Some(JsValue::Null) => 0.0,
        Some(JsValue::Bool(value)) => f64::from(u8::from(*value)),
        Some(JsValue::Number(number)) => *number,
        Some(JsValue::String(text)) => {
            let text = String::from_utf16_lossy(text.units());
            string_to_number(super::paths::js_trim(&text))
        }
        Some(JsValue::Array(items)) => match items.as_slice() {
            [] => 0.0,
            [JsValue::Null] => 0.0,
            [JsValue::Bool(_)] | [JsValue::Object(_)] => f64::NAN,
            [single] => js_number(Some(single)),
            _ => f64::NAN,
        },
        Some(JsValue::Object(_)) => f64::NAN,
    }
}

/// ECMAScript StringToNumber on an already trimmed string.
fn string_to_number(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = text.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            return digits
                .chars()
                .try_fold(0.0_f64, |acc, ch| {
                    ch.to_digit(radix)
                        .map(|digit| acc * f64::from(radix) + f64::from(digit))
                })
                .unwrap_or(f64::NAN);
        }
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let valid = unsigned
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_digit() || ch == '.')
        && unsigned
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | 'e' | 'E' | '+' | '-'));
    if !valid {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}
