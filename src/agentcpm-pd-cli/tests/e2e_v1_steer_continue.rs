//! t009 e2e: mid-run steering and the explicit crash-recovery continue
//! (`cli.case.c008`, `cli.case.c009`, `cli.case.c010`, `cli.case.c013`,
//! `cli.case.c014`, `cli.case.wait-detach` 的本任务切片,
//! `cli.case.operation.steer.submit`, `cli.case.operation.steer.get`,
//! `cli.case.operation.steer.withdraw`, `cli.case.operation.run.continue`,
//! `cli.case.contract.capability.a014`).
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real `/v1/ws`
//! transport against an isolated `AGENTCPM_PD_HOME`, using the shared
//! `common_v1` harness. Scenario -> acceptance mapping:
//!
//! - `steer_submit_roundtrip` (operation.steer.submit/get + c008/c009/c010):
//!   the deterministic `[pd-driver:await_steer]` run suspends until a steer
//!   arrives, consumes it, and succeeds; `steer.submit` is durable and
//!   idempotent (same steerId + input replays the current status, different
//!   input conflicts), `steer.get` distinguishes pending from consumed, and a
//!   terminal run refuses new steers with `RUN_TERMINAL`.
//! - `steer_withdraw_roundtrip` (operation.steer.withdraw): a pending steer
//!   is withdrawn under its revision CAS (the run keeps waiting — the
//!   withdrawn input is never delivered), a second steer completes the run,
//!   and withdrawing the consumed steer fails with `ALREADY_CONSUMED`.
//! - `run_continue_after_gateway_crash` (operation.run.continue +
//!   cli.case.c014 + contract.capability.a014): SIGKILL the gateway mid-run,
//!   verify the crash-recovery `interrupted` marking with the queue paused
//!   (D9: no auto-replay), `queue.resume`, then `run.continue` creates a new
//!   run with `continuedFrom` set while the source stays `interrupted`;
//!   same-key replay and stale-revision / non-interrupted refusals.
//!
//! 本地未跑（2026-11-18 构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use common_v1::*;

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code.
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert!(
        out.code != 0,
        "`{method}` must exit non-zero on a contract failure (got {}); stderr: {}",
        out.code,
        out.stderr
    );
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "`{method}` must print exactly one envelope line; stdout:\n{}",
        out.stdout
    );
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` outcome: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// `steer submit` with typed options (the input array binds to `--input`).
fn steer_submit(
    home: &Path,
    run_id: &str,
    steer_id: &str,
    text: &str,
    key: Option<&str>,
) -> CliOutcome {
    let input_json = format!("[{{\"kind\":\"text\",\"text\":\"{text}\"}}]");
    let mut args = vec![
        "steer",
        "submit",
        "--output",
        "json",
        "--run-id",
        run_id,
        "--steer-id",
        steer_id,
        "--input",
        input_json.as_str(),
    ];
    if let Some(k) = key {
        args.push("--request-key");
        args.push(k);
    }
    await_cli(home, &args)
}

fn steer_get(home: &Path, run_id: &str, steer_id: &str) -> CliOutcome {
    await_cli(
        home,
        &[
            "steer",
            "get",
            "--output",
            "json",
            "--run-id",
            run_id,
            "--steer-id",
            steer_id,
        ],
    )
}

fn steer_withdraw(home: &Path, run_id: &str, steer_id: &str, revision: &str) -> CliOutcome {
    await_cli(
        home,
        &[
            "steer",
            "withdraw",
            "--output",
            "json",
            "--run-id",
            run_id,
            "--steer-id",
            steer_id,
            "--expected-revision",
            revision,
        ],
    )
}

/// `run get` result.run JSON (one bounded CLI round trip per poll).
fn run_get(home: &Path, run_id: &str) -> Value {
    let out = await_cli(
        home,
        &["run", "get", "--output", "json", "--run-id", run_id],
    );
    expect_ok("run.get", &out)["run"].clone()
}

/// Polls `run get` until the run reaches `want` (bounded).
async fn wait_run_state(home: &Path, run_id: &str, want: &str) -> Value {
    for _ in 0..100 {
        let run = run_get(home, run_id);
        if run["state"].as_str() == Some(want) {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("run {run_id} never reached `{want}`");
}

/// c008/c009/c010 + operation.steer.submit/get: durable steer acceptance,
/// deterministic consumption by the awaiting run, idempotent resubmit, and
/// the RUN_TERMINAL / NOT_FOUND / IDEMPOTENCY_CONFLICT refusals.
#[tokio::test]
async fn steer_submit_roundtrip() {
    let home = TestHome::new("t009-st");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t009st").await;

    // The run suspends on the steer seam until input arrives.
    let submit = submit_run(root, &session_id, "[pd-driver:await_steer]", "t009-st-1").await;
    let run_id = submit["runId"].as_str().unwrap().to_owned();

    // Durable acceptance (c008): the client exits right after; the steer row
    // lives in the gateway store. The submit response itself is the stable
    // pending observation (the awaiting driver consumes within milliseconds,
    // so a later read is not guaranteed to still see pending — that face is
    // asserted on the sleeping session below).
    let out = steer_submit(root, &run_id, "st-1", "redirect now", Some("t009-st-2"));
    let status = expect_ok("steer.submit", &out);
    assert_eq!(status["steerId"], "st-1");
    assert_eq!(status["phase"], "pending");
    assert_eq!(status["state"], "queued");

    // The run consumes the steer and completes; consumption is observable.
    let waited = expect_ok("run.wait", &wait_run(root, &run_id));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");

    let consumed = expect_ok("steer.get", &steer_get(root, &run_id, "st-1"));
    assert_eq!(consumed["phase"], "consumed", "{consumed}");
    assert_eq!(consumed["state"], "succeeded", "{consumed}");

    // c009 idempotency: the same key replays the CURRENT (now consumed)
    // status instead of the frozen accept-time one (受理不等于已消费).
    let replay_after = expect_ok(
        "steer.submit",
        &steer_submit(root, &run_id, "st-1", "redirect now", Some("t009-st-2")),
    );
    assert_eq!(replay_after["phase"], "consumed");
    assert_eq!(replay_after["state"], "succeeded");
    let fresh_replay = expect_ok(
        "steer.submit",
        &steer_submit(root, &run_id, "st-1", "redirect now", Some("t009-st-3")),
    );
    assert_eq!(fresh_replay["phase"], "consumed");

    // Same steerId with different input conflicts (「同ID不同输入冲突」).
    let code = expect_fail(
        "steer.submit",
        &steer_submit(root, &run_id, "st-1", "different", Some("t009-st-4")),
    );
    assert_eq!(code, "IDEMPOTENCY_CONFLICT");

    // Terminal run: new steers are refused (不误送继任/已终态 run).
    let code = expect_fail(
        "steer.submit",
        &steer_submit(root, &run_id, "st-9", "late", Some("t009-st-5")),
    );
    assert_eq!(code, "RUN_TERMINAL");

    // Missing steer / wrong ownership: NOT_FOUND.
    let code = expect_fail("steer.get", &steer_get(root, &run_id, "nope"));
    assert_eq!(code, "NOT_FOUND");

    // A sleeping run never consumes: the pending phase is stably observable
    // (c010's pending-vs-consumed face; separate session because a busy
    // session queues instead of starting the run).
    let (_, sleepy) = seed_project_session(root, "t009st2").await;
    let sleep_run = submit_run(root, &sleepy, "[pd-driver:sleep=60000]", "t009-st-6").await;
    let sleep_id = sleep_run["runId"].as_str().unwrap().to_owned();
    let pending = expect_ok(
        "steer.submit",
        &steer_submit(root, &sleep_id, "st-p", "held", Some("t009-st-7")),
    );
    assert_eq!(pending["phase"], "pending");
    assert_eq!(pending["state"], "queued");
    let got = expect_ok("steer.get", &steer_get(root, &sleep_id, "st-p"));
    assert_eq!(got["phase"], "pending");
    assert_eq!(got["state"], "queued");
    assert_eq!(
        got["revision"].as_str().unwrap(),
        pending["revision"].as_str().unwrap()
    );
}

/// operation.steer.withdraw: revision-CAS withdrawal of a pending steer (the
/// run keeps running and is NOT delivered the withdrawn input), a consumed
/// steer fails withdrawal with ALREADY_CONSUMED; stale revisions conflict.
#[tokio::test]
async fn steer_withdraw_roundtrip() {
    let home = TestHome::new("t009-wd");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t009wd").await;

    // A sleeping run never consumes the steer, so the withdrawal target
    // stays deterministically pending.
    let submit = submit_run(root, &session_id, "[pd-driver:sleep=60000]", "t009-wd-1").await;
    let run_id = submit["runId"].as_str().unwrap().to_owned();

    let status = expect_ok(
        "steer.submit",
        &steer_submit(root, &run_id, "st-1", "first thought", Some("t009-wd-2")),
    );
    let rev = status["revision"].as_str().unwrap().to_owned();

    // Stale revision first: REVISION_CONFLICT, nothing withdrawn.
    let code = expect_fail(
        "steer.withdraw",
        &steer_withdraw(root, &run_id, "st-1", "rev-bogus"),
    );
    assert_eq!(code, "REVISION_CONFLICT");

    // Withdraw under the observed revision.
    let withdrawn = expect_ok(
        "steer.withdraw",
        &steer_withdraw(root, &run_id, "st-1", &rev),
    );
    assert_eq!(withdrawn["phase"], "withdrawn");
    assert_eq!(withdrawn["state"], "cancelled");

    let got = expect_ok("steer.get", &steer_get(root, &run_id, "st-1"));
    assert_eq!(got["phase"], "withdrawn");
    assert_eq!(got["state"], "cancelled");

    // The withdrawn input is never delivered: the run keeps running.
    let run = wait_run_state(root, &run_id, "running").await;
    assert_eq!(run["state"], "running");

    // A consumed steer (separate session so the awaiting run starts
    // immediately) cannot be withdrawn (「已消费不可撤」).
    let (_, consume_session) = seed_project_session(root, "t009wd2").await;
    let consume_run = submit_run(
        root,
        &consume_session,
        "[pd-driver:await_steer]",
        "t009-wd-3",
    )
    .await;
    let consume_id = consume_run["runId"].as_str().unwrap().to_owned();
    expect_ok(
        "steer.submit",
        &steer_submit(
            root,
            &consume_id,
            "st-2",
            "second thought",
            Some("t009-wd-4"),
        ),
    );
    let waited = expect_ok("run.wait", &wait_run(root, &consume_id));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");

    let consumed = expect_ok("steer.get", &steer_get(root, &consume_id, "st-2"));
    assert_eq!(consumed["phase"], "consumed");
    assert_eq!(consumed["state"], "succeeded");
    let consumed_rev = consumed["revision"].as_str().unwrap().to_owned();

    // Consumed steers cannot be withdrawn (「已消费不可撤」).
    let code = expect_fail(
        "steer.withdraw",
        &steer_withdraw(root, &consume_id, "st-2", &consumed_rev),
    );
    assert_eq!(code, "ALREADY_CONSUMED");

    // Missing steer: NOT_FOUND.
    let code = expect_fail(
        "steer.withdraw",
        &steer_withdraw(root, &run_id, "nope", &consumed_rev),
    );
    assert_eq!(code, "NOT_FOUND");
}

/// operation.run.continue + cli.case.c014 + contract.capability.a014:
/// SIGKILL the gateway mid-run -> crash recovery marks the run `interrupted`
/// and pauses the queue (D9: no auto-replay) -> `queue.resume` ->
/// `run.continue` creates a NEW run recording `continuedFrom` while the
/// source stays `interrupted`; idempotent replay, and the
/// SOURCE_NOT_INTERRUPTED / REVISION_CONFLICT refusals.
#[tokio::test]
async fn run_continue_after_gateway_crash() {
    let home = TestHome::new("t009-rc");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t009rc").await;

    // c008: durable acceptance — the CLI exits, the run keeps running in the
    // gateway (wait-detach's background-continues face).
    let submit = submit_run(root, &session_id, "[pd-driver:sleep=60000]", "t009-rc-1").await;
    let source_id = submit["runId"].as_str().unwrap().to_owned();

    wait_run_state(root, &source_id, "running").await;

    // c014: kill the isolated gateway mid-run.
    let pid = home.gateway_pid().expect("gateway pid");
    signal_pid(pid, rustix::process::Signal::KILL);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The next CLI invocation auto-starts a fresh gateway; crash recovery
    // marks the run `interrupted` and pauses the session queue.
    let source = wait_run_state(root, &source_id, "interrupted").await;
    let source_rev = source["revision"].as_str().unwrap().to_owned();
    assert_eq!(source["terminal"], true);

    // D9: nothing auto-replays — the queue stays paused until resumed.
    let queue = expect_ok(
        "queue.get",
        &await_cli(
            root,
            &[
                "queue",
                "get",
                "--output",
                "json",
                "--session-id",
                &session_id,
            ],
        ),
    );
    assert_eq!(queue["queue"]["paused"], true, "{queue}");

    resume_paused_queue(root, &session_id).await;

    // Explicit continue with a fresh input (no replay of the old one).
    let cont = await_cli(
        root,
        &[
            "run",
            "continue",
            "--output",
            "json",
            "--source-run-id",
            &source_id,
            "--expected-revision",
            &source_rev,
            "--request-key",
            "t009-rc-continue",
            "--text",
            "after the crash",
        ],
    );
    let receipt = expect_ok("run.continue", &cont);
    assert_eq!(receipt["kind"], "run");
    assert_eq!(receipt["durable"], true);
    assert_eq!(receipt["state"], "queued");
    let new_id = receipt["runId"].as_str().unwrap().to_owned();
    assert_ne!(new_id, source_id, "new runId must differ from sourceRunId");

    // The new run completes and records continuedFrom; the source keeps its
    // interrupted state (旧run不复活).
    let waited = expect_ok("run.wait", &wait_run(root, &new_id));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");
    assert_eq!(
        waited["run"]["continuedFrom"].as_str().unwrap(),
        source_id,
        "{waited}"
    );
    let source_after = run_get(root, &source_id);
    assert_eq!(source_after["state"], "interrupted");
    assert_eq!(source_after["terminal"], true);

    // c009: same key + same input replays the original receipt
    // (deduplicated), requests.lookup returns the original resource id.
    let replay_out = await_cli(
        root,
        &[
            "run",
            "continue",
            "--output",
            "json",
            "--source-run-id",
            &source_id,
            "--expected-revision",
            &source_rev,
            "--request-key",
            "t009-rc-continue",
            "--text",
            "after the crash",
        ],
    );
    let replay = expect_ok("run.continue", &replay_out);
    assert_eq!(replay["deduplicated"], true, "{replay}");
    assert_eq!(replay["runId"].as_str().unwrap(), new_id);

    let lookup = expect_ok(
        "requests.lookup",
        &await_cli(
            root,
            &[
                "requests",
                "lookup",
                "--output",
                "json",
                "--method-id",
                "run.continue",
                "--idempotency-key",
                "t009-rc-continue",
            ],
        ),
    );
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(lookup["resource"]["runId"].as_str().unwrap(), new_id);

    // Refusals: the succeeded continuation run is not interrupted…
    let code = expect_fail(
        "run.continue",
        &await_cli(
            root,
            &[
                "run",
                "continue",
                "--output",
                "json",
                "--source-run-id",
                &new_id,
                "--expected-revision",
                waited["run"]["revision"].as_str().unwrap(),
                "--request-key",
                "t009-rc-r1",
                "--text",
                "again",
            ],
        ),
    );
    assert_eq!(code, "SOURCE_NOT_INTERRUPTED");

    // …and a stale observation of the interrupted source conflicts.
    let code = expect_fail(
        "run.continue",
        &await_cli(
            root,
            &[
                "run",
                "continue",
                "--output",
                "json",
                "--source-run-id",
                &source_id,
                "--expected-revision",
                "rev-bogus",
                "--request-key",
                "t009-rc-r2",
                "--text",
                "again",
            ],
        ),
    );
    assert_eq!(code, "REVISION_CONFLICT");
}
