//! `runNodeExecutable` (`tool/execution-world/SubprocessPort.ts:50-155`): the
//! direct-executable path a sandboxed execution world dispatches its prepared
//! argv on. Unlike [`super::runner`] it never resolves a shell, settles with
//! the error `Subprocess execution was aborted.` once the signal fired
//! (already-aborted signals never start the process), and escalates a stop to
//! SIGKILL after 1 s.
//!
//! [`SandboxedShellRunner`] is `createNodeSandboxedShellPort`: `/bin/sh -c
//! <command>` confined by the execution world's [`Sandbox`], with the
//! command's cwd as the workspace root.
use super::runner::{
    AbortSignal, CommandOptions, CommandResult, CommandRunner, Utf8Stream, fire_and_forget,
    inherited_env, timer_delay,
};
use crate::{
    process_group::GroupGuard,
    runtime::tools::{ToolError, ToolFuture, sandbox::Sandbox},
};
use pilotdeck_protocol::JsString;
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command, time::sleep_until};

/// `new Error("Subprocess execution was aborted.")`.
pub const ABORTED_MESSAGE: &str = "Subprocess execution was aborted.";
const KILL_AFTER: Duration = Duration::from_millis(1_000);

/// One `executeFile` request.
pub struct ExecutableRequest {
    pub executable: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// `None` inherits the gateway process environment.
    pub env: Option<Vec<(JsString, JsString)>>,
    pub timeout_ms: f64,
    pub signal: Option<AbortSignal>,
    pub on_stdout: Option<super::OutputCallback>,
    pub on_stderr: Option<super::OutputCallback>,
}

fn lossy(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

/// `runNodeExecutable(request)`.
pub async fn run_executable(request: ExecutableRequest) -> Result<CommandResult, ToolError> {
    // `if (request.signal?.aborted) return Promise.reject(...)`: the process
    // is never started for an already-aborted signal.
    if request.signal.as_ref().is_some_and(AbortSignal::aborted) {
        return Err(ToolError::failed(ABORTED_MESSAGE));
    }
    let started = Instant::now();
    let mut process = Command::new(&request.executable);
    process
        .args(&request.args)
        .current_dir(&request.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = &request.env {
        process.env_clear();
        for (key, value) in env {
            process.env(lossy(key), lossy(value));
        }
    }
    // `detached: true` on POSIX: the child leads its own session and group.
    pilotdeck_child_signals::install_child_detach_session(process.as_std_mut());
    pilotdeck_child_signals::install_child_signal_reset(process.as_std_mut());
    let mut child = process.spawn().map_err(|error| {
        let code = crate::node_io::lookup(&error)
            .map_or_else(|| error.to_string(), |(code, _)| code.to_owned());
        ToolError::failed(format!("spawn {} {code}", request.executable).as_str())
    })?;
    let pid = child.id();
    let group = pid.and_then(|pid| rustix::process::Pid::from_raw(pid as i32));
    let mut guard = GroupGuard(group);
    // `child.stdin?.end(request.stdin ?? "")`.
    drop(child.stdin.take());
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_text = String::new();
    let mut err_text = String::new();
    // `setEncoding("utf8")`: a StringDecoder, which keeps a leading BOM.
    let mut out_decoder = Utf8Stream::keeping_bom();
    let mut err_decoder = Utf8Stream::keeping_bom();
    let mut out_buffer = vec![0u8; 64 * 1024];
    let mut err_buffer = vec![0u8; 64 * 1024];
    let timeout_at = tokio::time::Instant::now() + timer_delay(request.timeout_ms);
    let mut timeout_armed = true;
    let mut abort_armed = request.signal.is_some();
    let mut aborted = false;
    let mut timed_out = false;
    let mut kill_at: Option<tokio::time::Instant> = None;
    let mut exited = false;
    let mut leader_status: Option<Option<i32>> = None;
    let stop = |guard: &GroupGuard, kill_at: &mut Option<tokio::time::Instant>| {
        guard.signal(rustix::process::Signal::TERM);
        kill_at.get_or_insert(tokio::time::Instant::now() + KILL_AFTER);
    };
    while stdout.is_some() || stderr.is_some() || !exited {
        let streams_closed = stdout.is_none() && stderr.is_none();
        tokio::select! {
            read = async { stdout.as_mut().unwrap().read(&mut out_buffer).await }, if stdout.is_some() => {
                match read {
                    Ok(length) if length > 0 => {
                        let text = out_decoder.decode(&out_buffer[..length]);
                        if !text.is_empty() {
                            out_text.push_str(&text);
                            if let Some(callback) = &request.on_stdout {
                                fire_and_forget(callback, &text);
                            }
                        }
                    }
                    _ => stdout = None,
                }
            }
            read = async { stderr.as_mut().unwrap().read(&mut err_buffer).await }, if stderr.is_some() => {
                match read {
                    Ok(length) if length > 0 => {
                        let text = err_decoder.decode(&err_buffer[..length]);
                        if !text.is_empty() {
                            err_text.push_str(&text);
                            if let Some(callback) = &request.on_stderr {
                                fire_and_forget(callback, &text);
                            }
                        }
                    }
                    _ => stderr = None,
                }
            }
            status = async {
                match group {
                    Some(leader) => crate::process_group::wait_exit_unreaped(leader).await.ok().flatten().map(|exit| exit.code),
                    None => None,
                }
            }, if streams_closed && !exited => {
                exited = true;
                leader_status = status;
            }
            _ = sleep_until(timeout_at), if timeout_armed => {
                timeout_armed = false;
                timed_out = true;
                stop(&guard, &mut kill_at);
            }
            _ = async { request.signal.as_ref().unwrap().wait().await }, if abort_armed => {
                abort_armed = false;
                aborted = true;
                stop(&guard, &mut kill_at);
            }
            _ = sleep_until(kill_at.unwrap_or(timeout_at)), if kill_at.is_some() => {
                kill_at = None;
                guard.signal(rustix::process::Signal::KILL);
            }
        }
    }
    out_text.push_str(&out_decoder.flush());
    err_text.push_str(&err_decoder.flush());
    let exit_code = match (kill_at, leader_status) {
        // The 1 s SIGKILL timer outlives the settled run (`unref`ed): it fires
        // on schedule for group members that ignored SIGTERM. The leader stays
        // unreaped until then so the group ID cannot be reused.
        (Some(deadline), Some(code)) => {
            tokio::spawn(async move {
                sleep_until(deadline).await;
                guard.signal(rustix::process::Signal::KILL);
                guard.disarm();
                let _ = child.wait().await;
            });
            code
        }
        (pending, _) => {
            if pending.is_some() {
                guard.signal(rustix::process::Signal::KILL);
            }
            let status = child.wait().await;
            guard.disarm();
            status.ok().and_then(|status| status.code())
        }
    };
    if aborted {
        return Err(ToolError::failed(ABORTED_MESSAGE));
    }
    Ok(CommandResult {
        exit_code,
        stdout: out_text,
        stderr: err_text,
        timed_out,
        duration_ms: started.elapsed().as_millis() as f64,
        pid,
    })
}

/// `createNodeSandboxedShellPort` over `runNodeExecutable`.
#[derive(Clone, Copy, Debug)]
pub struct SandboxedShellRunner {
    pub sandbox: Sandbox,
}

impl CommandRunner for SandboxedShellRunner {
    fn run(&self, command: JsString, options: CommandOptions) -> ToolFuture<CommandResult> {
        let sandbox = self.sandbox;
        Box::pin(async move {
            // `prepare` checks the signal first (`throwIfAborted`).
            if options.signal.as_ref().is_some_and(AbortSignal::aborted) {
                return Err(ToolError::aborted());
            }
            let cwd = super::super::os_path(&options.cwd);
            // `resolvePolicy({ workspaceRoot: request.cwd })`.
            let prepared = sandbox
                .prepare("/bin/sh", vec!["-c".into(), lossy(&command)], &cwd, None)
                .map_err(|message| ToolError::failed(message.as_str()))?;
            let env = options.env.or_else(|| Some(inherited_env()));
            run_executable(ExecutableRequest {
                executable: prepared.executable,
                args: prepared.args,
                cwd,
                env,
                timeout_ms: options.timeout_ms,
                signal: options.signal,
                on_stdout: options.on_stdout,
                on_stderr: options.on_stderr,
            })
            .await
        })
    }
}
