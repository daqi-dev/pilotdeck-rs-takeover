//! SIGINT/SIGTERM guardian for `run`/`resume` (`sdk-run` mapping:
//! "仅旧兼容信号（SIGINT/SIGTERM）时显式提交 cancel 并等待相应结果").
//!
//! The shim process itself cannot catch SIGINT/SIGTERM without a signal
//! handler, and the crate pins itself to `serde`/`serde_json` with no
//! `unsafe` (crate red lines). The observable old contract is preserved
//! as follows:
//!
//! 1. The shim dies from the signal, so the invoking shell reports the old
//!    `exit 130` for SIGINT (128 + 2) — the legacy exit code is preserved
//!    without the shim fabricating it.
//! 2. The explicit `run.cancel` and the wait for the cancellation terminal
//!    state are performed by this guardian: a child process spawned through
//!    `/bin/sh -c 'trap "" INT TERM; exec "$@"'` — the SIG_IGN disposition
//!    set by `trap ""` survives `exec`, so the guardian is immune to the
//!    terminal's process-group SIGINT/SIGTERM.
//! 3. The guardian blocks reading its stdin pipe; the shim holds the write
//!    end, so the pipe closing means the shim exited. It then reads the
//!    write-ahead run record: a `settled` record makes it exit quietly; an
//!    unsettled one triggers `run.cancel` (recovering the run id from the
//!    record or `requests.lookup` by the persisted request key) followed by
//!    a bounded poll of `run.get` for the real terminal state.
//!
//! This is a registered accepted difference: the cancel-and-await happens
//! after the shim's death (the user's prompt may return first), because a
//! signal-catching implementation is impossible under the no-unsafe /
//! no-new-dependencies red lines.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::SHIM_NAME;
use crate::executor::{Executor, Response};
use crate::state::{self, RunRecord};

/// Hidden first-argv marker selecting guardian mode.
pub const MODE_ARG: &str = "__compat-guardian";

/// Builds the guardian command: `/bin/sh -c 'trap "" INT TERM; exec "$@"'`
/// wrapping our own binary in guardian mode (the `sh` argv trick passes the
/// paths without any shell quoting).
pub fn command_for(exe: &Path, record_path: &Path, target: Option<&str>) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("trap \"\" INT TERM; exec \"$@\"")
        .arg("sh");
    cmd.arg(exe).arg(MODE_ARG).arg("--record").arg(record_path);
    if let Some(target) = target {
        cmd.arg("--target").arg(target);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

/// Spawns the guardian (production executors only; fake executors never
/// spawn processes). The returned child owns the stdin pipe: dropping it
/// (or the shim dying) wakes the guardian.
pub fn spawn_if_live(is_live: bool, record_path: &Path, target: Option<&str>) -> Option<Child> {
    if !is_live {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    command_for(&exe, record_path, target).spawn().ok()
}

/// Dispatch entry for `pilotdeck __compat-guardian ...` (private mode).
pub fn main(rest: &[String]) -> i32 {
    let mut record_path: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--record" => {
                i += 1;
                match rest.get(i) {
                    Some(path) => record_path = Some(PathBuf::from(path)),
                    None => {
                        eprintln!("{SHIM_NAME}: guardian: --record requires a value");
                        return 1;
                    }
                }
            }
            "--target" => {
                i += 1;
                match rest.get(i) {
                    Some(value) => target = Some(value.clone()),
                    None => {
                        eprintln!("{SHIM_NAME}: guardian: --target requires a value");
                        return 1;
                    }
                }
            }
            other => {
                eprintln!("{SHIM_NAME}: guardian: unknown argument `{other}`");
                return 1;
            }
        }
        i += 1;
    }
    let Some(record_path) = record_path else {
        eprintln!("{SHIM_NAME}: guardian: --record is required");
        return 1;
    };
    let mut executor = crate::executor::AgentcpmPdInvoke::new();
    executor.set_target(target);
    run_guardian(&mut executor, &record_path)
}

/// Guardian logic over any executor (tests inject a fake). Returns the
/// guardian's own exit code (always 0: it is a best-effort helper and the
/// parent process is already gone).
pub fn run_guardian(executor: &mut dyn Executor, record_path: &Path) -> i32 {
    run_guardian_with(executor, record_path, 120, Duration::from_millis(500))
}

/// [`run_guardian`] with injectable poll bounds (tests use fast values).
fn run_guardian_with(
    executor: &mut dyn Executor,
    record_path: &Path,
    max_polls: u32,
    poll_interval: Duration,
) -> i32 {
    // Block until the shim closes our stdin (normal exit or death).
    {
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
    }
    let record = match RunRecord::load_by_path(record_path) {
        Ok(Some(record)) => record,
        Ok(None) => return 0,
        Err(message) => {
            eprintln!("{SHIM_NAME}: guardian: cannot read the run record: {message}");
            return 0;
        }
    };
    if record.state == state::RUN_SETTLED || record.state == state::RUN_CANCELLED {
        return 0;
    }
    eprintln!(
        "{SHIM_NAME}: shim interrupted; cancelling run (request key {})",
        record.request_key
    );

    // Recover the run id: from the record, or via requests.lookup by the
    // persisted key (a still-submitting record may have landed server-side).
    let run_id = match record.run_id.clone() {
        Some(run_id) => run_id,
        None => match lookup_run_id(executor, &record.request_key) {
            Some(run_id) => run_id,
            None => {
                eprintln!("{SHIM_NAME}: guardian: no run id to cancel (submission not accepted)");
                return 0;
            }
        },
    };

    let cancel_key = state::generate_request_key();
    match executor.invoke_keyed("run.cancel", &json!({"runId": run_id}), &cancel_key) {
        Ok(resp) => report_cancel_receipt(&resp),
        Err(e) => {
            eprintln!("{SHIM_NAME}: guardian: run.cancel failed: {e}");
        }
    }

    // Wait (bounded) for the cancellation terminal state.
    let mut updated = record;
    updated.state = state::RUN_CANCELLED.to_owned();
    updated.run_id = Some(run_id.clone());
    for _ in 0..max_polls {
        match executor.invoke("run.get", &json!({"runId": run_id})) {
            Ok(resp) if resp.is_success() => {
                if let Some(run) = resp.result.as_ref().and_then(|r| r.get("run")) {
                    let terminal = run.get("terminal").and_then(Value::as_bool);
                    let state_str = run.get("state").and_then(Value::as_str);
                    if terminal == Some(true)
                        && let Some(state_str) = state_str
                    {
                        updated.terminal_state = Some(state_str.to_owned());
                        eprintln!("{SHIM_NAME}: guardian: run reached terminal state {state_str}");
                        break;
                    }
                }
            }
            Ok(_) | Err(_) => {}
        }
        std::thread::sleep(poll_interval);
    }
    // Best-effort persistence next to the original record.
    if let Ok(updated) = serde_json::to_value(&updated) {
        let _ = state::atomic_write_json(record_path, &updated);
    }
    0
}

/// Reports the cancel receipt (acceptance is not a terminal state).
fn report_cancel_receipt(resp: &Response) {
    if resp.is_success() {
        if let Some(receipt) = resp.result.as_ref() {
            let disposition = receipt.pointer("/receipt/disposition");
            let state = receipt.pointer("/receipt/state");
            eprintln!(
                "{SHIM_NAME}: guardian: run.cancel accepted (disposition: {}, state: {})",
                disposition.and_then(Value::as_str).unwrap_or("unknown"),
                state.and_then(Value::as_str).unwrap_or("unknown")
            );
        }
    } else if let Some(error) = &resp.error {
        eprintln!(
            "{SHIM_NAME}: guardian: run.cancel rejected: {}: {}",
            error.code, error.message
        );
    }
}

/// Recovers the run id of a possibly-landed submission by its request key.
fn lookup_run_id(executor: &mut dyn Executor, request_key: &str) -> Option<String> {
    let resp = executor
        .invoke(
            "requests.lookup",
            &json!({"methodId": "run.submit", "idempotencyKey": request_key}),
        )
        .ok()?;
    let result = resp.result?;
    if result.get("requestState").and_then(Value::as_str) == Some("accepted") {
        result
            .pointer("/resource/runId")
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ErrorPayload, FakeExecutor, Outcome};
    use crate::state::{RUN_SETTLED, RunRecord};
    use crate::testutil::TempDir;
    use serde_json::json;

    fn ok(result: Value) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Succeeded,
            result: Some(result),
            error: None,
        })
    }

    fn failed(code: &str) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(ErrorPayload {
                code: code.to_owned(),
                message: "not served".to_owned(),
                details: None,
            }),
        })
    }

    #[test]
    fn command_wraps_the_binary_with_ignored_signals() {
        let cmd = command_for(
            Path::new("/usr/local/bin/pilotdeck"),
            Path::new("/h/compat/runs/req-1.json"),
            Some("profile-x"),
        );
        assert_eq!(cmd.get_program().to_string_lossy(), "/bin/sh");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-c".to_owned(),
                "trap \"\" INT TERM; exec \"$@\"".to_owned(),
                "sh".to_owned(),
                "/usr/local/bin/pilotdeck".to_owned(),
                "__compat-guardian".to_owned(),
                "--record".to_owned(),
                "/h/compat/runs/req-1.json".to_owned(),
                "--target".to_owned(),
                "profile-x".to_owned(),
            ]
        );
    }

    #[test]
    fn spawn_is_skipped_for_fake_executors() {
        // is_live() false for FakeExecutor; spawn_if_live returns None
        // without touching current_exe or /bin/sh.
        assert!(spawn_if_live(false, Path::new("/nope"), None).is_none());
    }

    #[test]
    fn settled_records_exit_quietly_without_business_calls() {
        let home = TempDir::new("guardian-settled");
        let mut record = RunRecord::new(
            "req-1".to_owned(),
            "shim-s-1".to_owned(),
            "shim-p-1".to_owned(),
            "hi".to_owned(),
        );
        record.state = RUN_SETTLED.to_owned();
        record.run_id = Some("run-1".to_owned());
        record.save(home.path()).unwrap();

        let mut exec = FakeExecutor::new();
        let code = run_guardian(&mut exec, &record.path(home.path()));
        assert_eq!(code, 0);
        assert!(exec.calls.is_empty(), "settled: nothing to cancel");
    }

    #[test]
    fn unsettled_record_cancels_and_awaits_the_terminal_state() {
        let home = TempDir::new("guardian-cancel");
        let mut record = RunRecord::new(
            "req-2".to_owned(),
            "shim-s-1".to_owned(),
            "shim-p-1".to_owned(),
            "hi".to_owned(),
        );
        record.run_id = Some("run-2".to_owned());
        record.save(home.path()).unwrap();

        let mut exec = FakeExecutor::new()
            .push(ok(
                json!({"receipt": {"disposition": "requested", "state": "cancelling"}}),
            ))
            .push(ok(json!({"run": {"terminal": true, "state": "cancelled"}})));
        let code = run_guardian(&mut exec, &record.path(home.path()));
        assert_eq!(code, 0);
        assert_eq!(exec.methods(), ["run.cancel", "run.get"]);
        assert!(
            exec.calls[0].request_key.is_some(),
            "run.cancel is keyed for idempotent retry"
        );
        // The record is updated to cancelled with the terminal state.
        let updated = RunRecord::load_by_path(&record.path(home.path()))
            .unwrap()
            .expect("record present");
        assert_eq!(updated.state, state::RUN_CANCELLED);
        assert_eq!(updated.terminal_state.as_deref(), Some("cancelled"));
    }

    #[test]
    fn missing_run_id_is_recovered_via_requests_lookup() {
        let home = TempDir::new("guardian-lookup");
        let record = RunRecord::new(
            "req-3".to_owned(),
            "shim-s-1".to_owned(),
            "shim-p-1".to_owned(),
            "hi".to_owned(),
        );
        // No run id: still submitting when the shim died.
        record.save(home.path()).unwrap();

        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "requestState": "accepted",
                "resource": {"kind": "run", "runId": "run-3"},
            })))
            .push(ok(
                json!({"receipt": {"disposition": "requested", "state": "cancelling"}}),
            ))
            .push(ok(json!({"run": {"terminal": true, "state": "cancelled"}})));
        let code = run_guardian(&mut exec, &record.path(home.path()));
        assert_eq!(code, 0);
        assert_eq!(exec.methods(), ["requests.lookup", "run.cancel", "run.get"]);
        assert_eq!(
            exec.calls[0].input["idempotencyKey"], "req-3",
            "lookup uses the persisted request key"
        );
    }

    #[test]
    fn not_accepted_submission_cannot_be_cancelled() {
        let home = TempDir::new("guardian-notaccepted");
        let record = RunRecord::new(
            "req-4".to_owned(),
            "shim-s-1".to_owned(),
            "shim-p-1".to_owned(),
            "hi".to_owned(),
        );
        record.save(home.path()).unwrap();

        let mut exec = FakeExecutor::new().push(ok(
            json!({"requestState": "notAccepted", "resource": Value::Null}),
        ));
        let code = run_guardian(&mut exec, &record.path(home.path()));
        assert_eq!(code, 0);
        assert_eq!(exec.methods(), ["requests.lookup"]);
    }

    #[test]
    fn cancel_failure_is_reported_and_terminal_poll_is_bounded() {
        let home = TempDir::new("guardian-fail");
        let mut record = RunRecord::new(
            "req-5".to_owned(),
            "shim-s-1".to_owned(),
            "shim-p-1".to_owned(),
            "hi".to_owned(),
        );
        record.run_id = Some("run-5".to_owned());
        record.save(home.path()).unwrap();

        // run.cancel rejected, then run.get never terminal: the poll loop
        // must stop at the injected bound instead of spinning.
        let mut exec = FakeExecutor::new()
            .push(failed("NOT_FOUND"))
            .push(ok(json!({"unrelated": true})));
        let code = run_guardian_with(
            &mut exec,
            &record.path(home.path()),
            2,
            Duration::from_millis(1),
        );
        assert_eq!(code, 0);
        assert_eq!(
            exec.methods(),
            ["run.cancel", "run.get", "run.get"],
            "bounded to max_polls run.get calls"
        );
    }
}
