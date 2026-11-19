//! Executor layer: the only path through which the shim performs business
//! calls, always via the `agentcpm-pd` public entry (`api invoke`,
//! `cli.contract.compatibility-adapter`: the adapter only calls the same new
//! product API). No direct database access, no legacy runtime fallback.
//!
//! `AgentcpmPdInvoke` spawns `agentcpm-pd api invoke <methodId> --input-json -`
//! as a subprocess, writes the input JSON on stdin and reads one contract
//! response envelope from stdout. `api invoke` is unary, so no streaming
//! passthrough is needed here.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

/// Environment hook overriding the target binary (test / distribution hook).
/// When set it is used as-is; the shim never falls back to a PATH search
/// behind an explicit override.
pub const TARGET_BIN_ENV: &str = "AGENTCPM_PD_COMPAT_TARGET_BIN";

/// The public CLI the shim targets.
pub const TARGET_BIN_NAME: &str = "agentcpm-pd";

/// Contract response envelope version string (informational for the shim).
pub const OPERATION_VERSION: &str = "agentcpm-pd.operation/1";

/// Envelope outcome discriminant (`$defs.response`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Accepted,
    Failed,
}

impl Outcome {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Succeeded | Self::Accepted)
    }
}

/// Contract error payload (`$defs.error`), slimmed to the fields the shim
/// projects: `code`, `message`, and optional `details`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Option<Value>,
}

/// Slim deserialization of the contract response envelope: `outcome` with
/// `result`, or `error { code, message, details }`.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    pub outcome: Outcome,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<ErrorPayload>,
}

impl Response {
    /// A minimal successful response (FakeExecutor default).
    pub fn succeeded_empty() -> Self {
        Self {
            version: Some(OPERATION_VERSION.to_owned()),
            request_id: Some("test".to_owned()),
            outcome: Outcome::Succeeded,
            result: Some(Value::Null),
            error: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.outcome.is_success()
    }
}

/// Transport-level failure of an invocation (a contract `outcome: failed`
/// envelope is NOT an `ExecError`; it is returned as `Ok(Response)` so entry
/// code can project it faithfully).
#[derive(Debug)]
pub enum ExecError {
    /// The `agentcpm-pd` binary cannot be found; the shim refuses to guess a
    /// fallback backend.
    TargetNotFound { searched: String },
    /// Spawning or feeding the target binary failed.
    Spawn {
        program: String,
        source: std::io::Error,
    },
    /// The child did not finish within the invocation deadline and its whole
    /// process group was killed. Without this the shim blocks forever on a
    /// hung target — a state the SIGINT guardian cannot observe (it only
    /// wakes on shim death), so a hung child must be the shim's own
    /// failure.
    Timeout { program: String, timeout: Duration },
    /// The child exited without producing a parseable envelope (e.g. its own
    /// usage error); its stderr is carried for surfacing.
    Exit { code: Option<i32>, stderr: String },
    /// Stdout was not a valid contract response envelope.
    InvalidStdout { message: String },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotFound { searched } => write!(
                f,
                "target binary `{TARGET_BIN_NAME}` not found ({searched}); install agentcpm-pd or set {TARGET_BIN_ENV}"
            ),
            Self::Spawn { program, source } => {
                write!(f, "failed to run target binary `{program}`: {source}")
            }
            Self::Timeout { program, timeout } => write!(
                f,
                "target binary `{program}` made no progress for {timeout:?}; its process \
                 group was killed"
            ),
            Self::Exit { code, stderr } => write!(
                f,
                "target exited with code {} without a response envelope; stderr: {}",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_owned()),
                stderr.trim()
            ),
            Self::InvalidStdout { message } => {
                write!(
                    f,
                    "target stdout was not a contract response envelope: {message}"
                )
            }
        }
    }
}

impl std::error::Error for ExecError {}

/// The single business-call interface of the shim.
pub trait Executor {
    fn invoke(&mut self, method_id: &str, input: &Value) -> Result<Response, ExecError>;

    /// Invokes with an explicit persistent request key (`--request-key`),
    /// used for `idempotentSubmit` writes whose acceptance the shim must be
    /// able to retry without double effects (`run.submit` etc.).
    fn invoke_keyed(
        &mut self,
        method_id: &str,
        input: &Value,
        request_key: &str,
    ) -> Result<Response, ExecError> {
        let _ = request_key;
        self.invoke(method_id, input)
    }

    /// Selects the target profile passed to the CLI (`--target`); `None`
    /// keeps the CLI's own default (the local instance under
    /// `AGENTCPM_PD_HOME`). Entries resolve the target explicitly before
    /// any business call.
    fn set_target(&mut self, target: Option<String>) {
        let _ = target;
    }

    /// Whether this executor performs real subprocess calls (production) or
    /// is a test fake. Gates side processes such as the SIGINT guardian.
    fn is_live(&self) -> bool {
        true
    }
}

/// Default wall-clock budget for one `agentcpm-pd api invoke` subprocess.
/// Every compat operation is either sync-fast or returns an async receipt,
/// so a child that finishes nothing within this bound is hung (stuck
/// gateway, dead pipe) and is killed instead of blocking the shim forever.
pub const DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_secs(120);

/// Watchdog poll granularity (how quickly a completed invocation cancels
/// the pending kill).
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Production executor over the `agentcpm-pd` public CLI.
///
/// Binary resolution happens lazily on the first invocation (entries that
/// fail before any operation must not fail with "target not found" first):
/// an explicit override, then the [`TARGET_BIN_ENV`] hook, then a PATH
/// search for `agentcpm-pd`; no silent fallback. Every invocation carries
/// [`DEFAULT_INVOKE_TIMEOUT`] (overridable via [`AgentcpmPdInvoke::with_invoke_timeout`]).
#[derive(Debug)]
pub struct AgentcpmPdInvoke {
    override_bin: Option<PathBuf>,
    resolved: Option<PathBuf>,
    target: Option<String>,
    invoke_timeout: Duration,
}

impl Default for AgentcpmPdInvoke {
    fn default() -> Self {
        Self {
            override_bin: None,
            resolved: None,
            target: None,
            invoke_timeout: DEFAULT_INVOKE_TIMEOUT,
        }
    }
}

impl AgentcpmPdInvoke {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pins the target binary (tests, distributions); never falls back.
    pub fn with_bin(path: impl Into<PathBuf>) -> Self {
        Self {
            override_bin: Some(path.into()),
            resolved: None,
            target: None,
            invoke_timeout: DEFAULT_INVOKE_TIMEOUT,
        }
    }

    /// Pins the per-invocation wall-clock budget (tests use fast values).
    pub fn with_invoke_timeout(mut self, timeout: Duration) -> Self {
        self.invoke_timeout = timeout;
        self
    }

    /// Resolves the target binary, memoizing the result.
    fn resolve(&mut self) -> Result<&Path, ExecError> {
        if let Some(bin) = &self.override_bin {
            return Ok(bin);
        }
        if self.resolved.is_none() {
            self.resolved = Some(resolve_target_bin(
                std::env::var_os(TARGET_BIN_ENV),
                std::env::var_os("PATH"),
            )?);
        }
        Ok(self.resolved.as_deref().expect("just resolved"))
    }
}

impl AgentcpmPdInvoke {
    /// Shared subprocess path: `agentcpm-pd [--target T] api invoke <methodId>
    /// [--request-key K] --input-json -` with the input JSON on stdin.
    fn invoke_impl(
        &mut self,
        method_id: &str,
        input: &Value,
        request_key: Option<&str>,
    ) -> Result<Response, ExecError> {
        let bin = self.resolve()?.to_owned();
        let payload = serde_json::to_string(input).map_err(|e| ExecError::Spawn {
            program: bin.display().to_string(),
            source: std::io::Error::other(e),
        })?;

        let mut cmd = Command::new(&bin);
        // Explicit target profile goes first (global option); the CLI's own
        // default (local instance under AGENTCPM_PD_HOME) applies when unset.
        if let Some(target) = &self.target {
            cmd.arg("--target").arg(target);
        }
        cmd.arg("api").arg("invoke").arg(method_id);
        if let Some(key) = request_key {
            cmd.arg("--request-key").arg(key);
        }
        cmd.arg("--input-json").arg("-");
        // The target runs in its own process group: a timeout kill must
        // reach the target AND anything it spawned on our behalf (an
        // auto-started gateway) without touching this shim or its SIGINT
        // guardian, which share the shim's group.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ExecError::Spawn {
                program: bin.display().to_string(),
                source: e,
            })?;

        // Feed stdin from a helper thread so a large input cannot deadlock
        // against the child's stdout.
        let mut stdin = child.stdin.take().expect("stdin piped");
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(payload.as_bytes());
        });

        // Drain stdout/stderr from reader threads: a blocking
        // `wait_with_output` would hang the shim forever on a child that
        // never exits, and the readers keep collecting output while the
        // watchdog decides.
        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");
        let out_handle = std::thread::spawn(move || read_pipe_to_end(&mut stdout_pipe));
        let err_handle = std::thread::spawn(move || read_pipe_to_end(&mut stderr_pipe));

        // Watchdog: on deadline, kill the child's whole process group. The
        // main thread still reaps the child below (no zombie).
        let completed = Arc::new(AtomicBool::new(false));
        let watchdog = spawn_kill_watchdog(child.id(), self.invoke_timeout, completed.clone());

        let status = child.wait().map_err(|e| ExecError::Spawn {
            program: bin.display().to_string(),
            source: e,
        })?;
        completed.store(true, Ordering::Release);
        let _ = writer.join();
        let killed = watchdog.join().unwrap_or(false);

        let stdout = String::from_utf8_lossy(&out_handle.join().unwrap_or_default())
            .trim()
            .to_owned();
        let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).into_owned();

        if killed {
            return Err(ExecError::Timeout {
                program: bin.display().to_string(),
                timeout: self.invoke_timeout,
            });
        }
        if stdout.is_empty() {
            return Err(ExecError::Exit {
                code: status.code(),
                stderr,
            });
        }
        serde_json::from_str(&stdout).map_err(|e| ExecError::InvalidStdout {
            message: format!("{e}; stdout: {:.200}", stdout),
        })
    }
}

/// Reads one pipe to the end; whatever a (possibly killed) child wrote so
/// far is still collected.
fn read_pipe_to_end(pipe: &mut impl std::io::Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(pipe, &mut buf);
    buf
}

/// Spawns the invocation watchdog: it sleeps until `timeout` has passed
/// and, unless the invocation completed by then, SIGKILLs the child's
/// whole process group. Returns whether the kill was actually delivered —
/// a group that already exited on its own is a natural completion, not a
/// timeout.
fn spawn_kill_watchdog(
    pid: u32,
    timeout: Duration,
    completed: Arc<AtomicBool>,
) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if completed.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(WATCHDOG_POLL_INTERVAL);
        }
        if completed.load(Ordering::Acquire) {
            return false;
        }
        kill_process_group(pid)
    })
}

/// SIGKILLs a whole process group (the child is its own leader thanks to
/// `process_group(0)`), reaching grandchildren the target spawned. This
/// crate forbids unsafe code, so the signal goes through `/bin/kill`
/// (base-system on the supported unixes) with a PATH `kill` fallback
/// instead of kill(2).
#[cfg(unix)]
fn kill_process_group(pid: u32) -> bool {
    let group = format!("-{pid}");
    for program in ["/bin/kill", "kill"] {
        let delivered = Command::new(program)
            .arg("-9")
            .arg("--")
            .arg(&group)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if delivered {
            return true;
        }
    }
    false
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) -> bool {
    // No portable group kill without unsafe code; the child is left to its
    // own devices on non-unix platforms (the crate is unix-oriented
    // elsewhere too).
    false
}

impl Executor for AgentcpmPdInvoke {
    fn invoke(&mut self, method_id: &str, input: &Value) -> Result<Response, ExecError> {
        self.invoke_impl(method_id, input, None)
    }

    fn invoke_keyed(
        &mut self,
        method_id: &str,
        input: &Value,
        request_key: &str,
    ) -> Result<Response, ExecError> {
        self.invoke_impl(method_id, input, Some(request_key))
    }

    fn set_target(&mut self, target: Option<String>) {
        self.target = target;
    }
}

/// Pure resolution used by [`AgentcpmPdInvoke::resolve`] (parameters instead
/// of live env so tests stay hermetic).
///
/// Order: explicit env hook value as-is (no fallback behind an override);
/// otherwise the first `agentcpm-pd` executable on the PATH; otherwise a
/// clear [`ExecError::TargetNotFound`].
pub fn resolve_target_bin(
    env_value: Option<OsString>,
    path_var: Option<OsString>,
) -> Result<PathBuf, ExecError> {
    if let Some(value) = env_value.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    match find_executable(TARGET_BIN_NAME, path_var) {
        Some(found) => Ok(found),
        None => Err(ExecError::TargetNotFound {
            searched: "PATH".to_owned(),
        }),
    }
}

/// Searches a PATH-style variable for an executable file with `name`.
fn find_executable(name: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    let path_var = path_var?;
    let separator = if cfg!(windows) { ';' } else { ':' };
    for dir in path_var.to_string_lossy().split(separator) {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// One recorded business call.
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub input: Value,
    pub request_key: Option<String>,
}

/// Test executor: records the call sequence and replays canned results
/// (default: a minimal successful envelope when the queue runs dry).
#[derive(Debug, Default)]
pub struct FakeExecutor {
    pub calls: Vec<Call>,
    pub queued: VecDeque<Result<Response, ExecError>>,
    pub target: Option<String>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(mut self, result: Result<Response, ExecError>) -> Self {
        self.queued.push_back(result);
        self
    }

    /// Recorded method ids, in call order.
    pub fn methods(&self) -> Vec<&str> {
        self.calls.iter().map(|c| c.method.as_str()).collect()
    }
}

impl Executor for FakeExecutor {
    fn invoke(&mut self, method_id: &str, input: &Value) -> Result<Response, ExecError> {
        self.calls.push(Call {
            method: method_id.to_owned(),
            input: input.clone(),
            request_key: None,
        });
        match self.queued.pop_front() {
            Some(result) => result,
            None => Ok(Response::succeeded_empty()),
        }
    }

    fn invoke_keyed(
        &mut self,
        method_id: &str,
        input: &Value,
        request_key: &str,
    ) -> Result<Response, ExecError> {
        self.calls.push(Call {
            method: method_id.to_owned(),
            input: input.clone(),
            request_key: Some(request_key.to_owned()),
        });
        match self.queued.pop_front() {
            Some(result) => result,
            None => Ok(Response::succeeded_empty()),
        }
    }

    fn set_target(&mut self, target: Option<String>) {
        self.target = target;
    }

    fn is_live(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::json;

    #[test]
    fn fake_executor_records_calls_and_replays_results() {
        let mut exec = FakeExecutor::new()
            .push(Ok(Response::succeeded_empty()))
            .push(Err(ExecError::TargetNotFound {
                searched: "PATH".to_owned(),
            }));

        let first = exec
            .invoke("session.create", &json!({"channel": "cli"}))
            .unwrap();
        assert!(first.is_success());
        assert!(matches!(
            exec.invoke("run.submit", &json!({"prompt": "hi"})),
            Err(ExecError::TargetNotFound { .. })
        ));
        // Queue exhausted: default success, still recorded.
        assert!(exec.invoke("run.events", &json!({})).unwrap().is_success());
        assert_eq!(
            exec.methods(),
            ["session.create", "run.submit", "run.events"]
        );
        assert_eq!(exec.calls[1].input, json!({"prompt": "hi"}));
        assert!(exec.calls[1].request_key.is_none());
    }

    #[test]
    fn fake_executor_records_request_keys_and_target() {
        let mut exec = FakeExecutor::new();
        exec.set_target(Some("remote-profile".to_owned()));
        assert_eq!(exec.target.as_deref(), Some("remote-profile"));
        assert!(!exec.is_live());

        exec.invoke_keyed("run.submit", &json!({"sessionId": "s"}), "req-1")
            .unwrap();
        assert_eq!(exec.methods(), ["run.submit"]);
        assert_eq!(exec.calls[0].request_key.as_deref(), Some("req-1"));
    }

    #[test]
    fn response_envelope_round_trips() {
        let ok: Response =
            serde_json::from_str(r#"{"version":"agentcpm-pd.operation/1","requestId":"r1","outcome":"succeeded","result":{"sessionId":"s"}}"#)
                .unwrap();
        assert_eq!(ok.outcome, Outcome::Succeeded);
        assert!(ok.is_success());
        assert_eq!(ok.result.as_ref().unwrap()["sessionId"], "s");

        let failed: Response = serde_json::from_str(
            r#"{"version":"agentcpm-pd.operation/1","requestId":"r2","outcome":"failed",
                "error":{"code":"CAPABILITY_UNAVAILABLE","message":"not served","retry":{"mode":"none"},"effect":"not_applied"}}"#,
        )
        .unwrap();
        assert_eq!(failed.outcome, Outcome::Failed);
        assert!(!failed.is_success());
        assert_eq!(
            failed.error.as_ref().unwrap().code,
            "CAPABILITY_UNAVAILABLE"
        );
        assert_eq!(failed.error.as_ref().unwrap().message, "not served");
    }

    #[test]
    fn resolution_env_hook_wins_and_never_falls_back() {
        let resolved = resolve_target_bin(Some("custom-bin".into()), None).unwrap();
        assert_eq!(resolved, PathBuf::from("custom-bin"));
    }

    #[test]
    fn resolution_without_path_or_env_is_a_clear_error() {
        let err = resolve_target_bin(None, Some("".into())).unwrap_err();
        assert!(matches!(err, ExecError::TargetNotFound { .. }));
        assert!(err.to_string().contains(TARGET_BIN_ENV));
    }

    #[test]
    fn resolution_finds_first_executable_on_path() {
        let dir = TempDir::new("resolve-path");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let bin = second.join(TARGET_BIN_NAME);
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&bin);

        let path_var = format!("{}:{}", first.display(), second.display());
        let resolved = resolve_target_bin(None, Some(path_var.clone().into())).unwrap();
        assert_eq!(resolved, bin);

        // Non-executable files are skipped.
        std::fs::write(first.join(TARGET_BIN_NAME), b"not executable\n").unwrap();
        let resolved = resolve_target_bin(None, Some(path_var.into())).unwrap();
        assert_eq!(resolved, bin);
    }

    #[test]
    fn invoke_spawns_target_with_api_invoke_argv_and_stdin() {
        let dir = TempDir::new("invoke");
        let args_file = dir.path().join("argv.txt");
        let stdin_file = dir.path().join("stdin.txt");
        // Script: capture argv and stdin, then emit one envelope.
        let script = dir.path().join("fake-agentcpm-pd.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {args}\ncat > {stdin}\nprintf '%s' '{envelope}'\n",
                args = shell_quote(&args_file),
                stdin = shell_quote(&stdin_file),
                envelope = r#"{"version":"agentcpm-pd.operation/1","requestId":"r","outcome":"accepted"}"#,
            ),
        )
        .unwrap();
        make_executable(&script);

        let mut exec = AgentcpmPdInvoke::with_bin(&script);
        let resp = exec
            .invoke("session.create", &json!({"project": "p"}))
            .expect("script invocation succeeds");
        assert_eq!(resp.outcome, Outcome::Accepted);

        let argv = std::fs::read_to_string(&args_file).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(
            argv,
            ["api", "invoke", "session.create", "--input-json", "-"]
        );
        let stdin = std::fs::read_to_string(&stdin_file).unwrap();
        assert_eq!(stdin.trim_end(), r#"{"project":"p"}"#);
    }

    #[test]
    fn keyed_invoke_passes_request_key_and_target() {
        let dir = TempDir::new("invoke-keyed");
        let args_file = dir.path().join("argv.txt");
        let script = dir.path().join("fake-keyed.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {args}\nprintf '%s' '{{\"version\":\"agentcpm-pd.operation/1\",\"requestId\":\"r\",\"outcome\":\"accepted\"}}'",
                args = shell_quote(&args_file),
            ),
        )
        .unwrap();
        make_executable(&script);

        let mut exec = AgentcpmPdInvoke::with_bin(&script);
        Executor::set_target(&mut exec, Some("profile-x".to_owned()));
        let resp = exec
            .invoke_keyed("run.submit", &json!({"sessionId": "s"}), "req-42")
            .expect("keyed invocation succeeds");
        assert_eq!(resp.outcome, Outcome::Accepted);

        let argv = std::fs::read_to_string(&args_file).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(
            argv,
            [
                "--target",
                "profile-x",
                "api",
                "invoke",
                "run.submit",
                "--request-key",
                "req-42",
                "--input-json",
                "-"
            ]
        );
    }

    #[test]
    fn invoke_reports_missing_binary_clearly() {
        let dir = TempDir::new("missing");
        let mut exec = AgentcpmPdInvoke::with_bin(dir.path().join("does-not-exist"));
        let err = exec.invoke("session.create", &json!({})).unwrap_err();
        assert!(matches!(err, ExecError::Spawn { .. }));
        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn invoke_surfaces_child_usage_error_without_envelope() {
        let dir = TempDir::new("usage");
        let script = dir.path().join("usage-error.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'agentcpm-pd: api invoke requires a methodId' >&2\nexit 2\n",
        )
        .unwrap();
        make_executable(&script);

        let mut exec = AgentcpmPdInvoke::with_bin(&script);
        let err = exec.invoke("bogus", &json!({})).unwrap_err();
        match err {
            ExecError::Exit { code, stderr } => {
                assert_eq!(code, Some(2));
                assert!(stderr.contains("api invoke requires a methodId"));
            }
            other => panic!("expected Exit, got {other:?}"),
        }
    }

    #[test]
    fn invoke_rejects_non_envelope_stdout() {
        let dir = TempDir::new("garbage");
        let script = dir.path().join("garbage.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'hello not json'\n").unwrap();
        make_executable(&script);

        let mut exec = AgentcpmPdInvoke::with_bin(&script);
        let err = exec.invoke("x", &json!({})).unwrap_err();
        assert!(matches!(err, ExecError::InvalidStdout { .. }));
    }

    #[test]
    fn invoke_completing_quickly_is_not_reported_as_a_timeout() {
        let dir = TempDir::new("invoke-fast");
        let script = dir.path().join("fast.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' '{\"version\":\"agentcpm-pd.operation/1\",\"requestId\":\"r\",\"outcome\":\"accepted\"}'\n",
        )
        .unwrap();
        make_executable(&script);

        let mut exec =
            AgentcpmPdInvoke::with_bin(&script).with_invoke_timeout(Duration::from_millis(1500));
        let resp = exec.invoke("session.create", &json!({})).unwrap();
        assert_eq!(resp.outcome, Outcome::Accepted);
    }

    #[test]
    fn invoke_times_out_and_kills_the_whole_process_group() {
        let dir = TempDir::new("invoke-timeout");
        let pid_file = dir.path().join("grandchild.pid");
        // Spawns a grandchild and hangs: the watchdog must kill the whole
        // group (child + grandchild — a group kill of just the child would
        // orphan the grandchild), and `wait` must still reap the child.
        let script = dir.path().join("hang.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > {pid}\nwait\n",
                pid = shell_quote(&pid_file),
            ),
        )
        .unwrap();
        make_executable(&script);

        let mut exec =
            AgentcpmPdInvoke::with_bin(&script).with_invoke_timeout(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let err = exec.invoke("session.create", &json!({})).unwrap_err();
        assert!(
            matches!(err, ExecError::Timeout { .. }),
            "expected Timeout, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout must fire at the deadline, not after the child's own 30s"
        );

        // The grandchild died with the group kill (it would otherwise hold
        // `sleep 30` for the full half minute). Reaping can take a moment,
        // so poll briefly.
        let grandchild: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..40 {
            let alive = Command::new("/bin/kill")
                .arg("-0")
                .arg(grandchild.to_string())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("grandchild {grandchild} survived the group kill");
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display())
    }
}
