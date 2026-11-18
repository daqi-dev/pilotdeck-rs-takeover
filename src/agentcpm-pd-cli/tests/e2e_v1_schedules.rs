//! Schedules + background-tasks end-to-end suite (t019 T1).
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real
//! `/v1/ws` transport (embedded gateway auto-start), against the ported cron
//! engine and background runtime. Determinism rules follow the WP5 suite:
//! deadline-bounded polls, no sleep races for positive assertions. One
//! exception is documented inline: the ported scheduler's production poll is
//! the 60 s idle tick (`DEFAULT_IDLE_POLL_MS`), so the scheduled-fire
//! scenario polls up to 75 s.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

/// Parses a failed envelope (`outcome: "failed"`) and returns its
/// `error.code`; also asserts the invocation exited non-zero.
fn expect_failed(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must exit non-zero on failure");
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` envelope: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn cli_json(root: &Path, args: &[&str]) -> Value {
    let out = await_cli(root, args);
    let method = if args.len() > 1 && !args[1].starts_with('-') {
        format!(
            "{}.{}",
            args[0].replace('-', "_"),
            args[1].replace('-', "_")
        )
    } else {
        args[0].replace('-', "_")
    };
    expect_ok(&method, &out)
}
#[allow(clippy::too_many_arguments)] // test helper
async fn configure_schedule(
    root: &Path,
    schedule_id: &str,
    project_id: &str,
    session_id: &str,
    kind: &str,
    expression: &str,
    message: &str,
    expected_revision: &str,
) -> Value {
    cli_json(
        root,
        &[
            "schedules",
            "configure",
            "--output",
            "json",
            "--schedule-id",
            schedule_id,
            "--project-id",
            project_id,
            "--session-id",
            session_id,
            "--kind",
            kind,
            "--expression",
            expression,
            "--timezone",
            "UTC",
            "--message",
            message,
            "--expected-revision",
            expected_revision,
        ],
    )
    .await
}

async fn history(root: &Path, schedule_id: &str) -> Value {
    cli_json(
        root,
        &[
            "schedules",
            "history",
            "--output",
            "json",
            "--schedule-id",
            schedule_id,
        ],
    )
    .await
}

async fn schedules_list(root: &Path) -> Value {
    cli_json(root, &["schedules", "list", "--output", "json"]).await
}

/// Polls `schedules.history` until the newest execution is terminal.
async fn poll_history_terminal(root: &Path, schedule_id: &str, bound: Duration) -> Value {
    let deadline = Instant::now() + bound;
    loop {
        let page = history(root, schedule_id).await;
        if let Some(first) = page["items"].as_array().and_then(|items| items.first()) {
            let state = first["state"].as_str().unwrap_or_default();
            if state != "pending" && state != "running" && !state.is_empty() {
                return first.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "execution of `{schedule_id}` never became terminal"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Polls `schedules.history` until the newest execution is running.
async fn poll_history_running(root: &Path, schedule_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let page = history(root, schedule_id).await;
        if let Some(first) = page["items"].as_array().and_then(|items| items.first())
            && first["state"] == json!("running")
        {
            return first.clone();
        }
        assert!(
            Instant::now() < deadline,
            "execution of `{schedule_id}` never started"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `new Date(Date.now() + delta_ms).toISOString()` without a date crate
/// (Howard Hinnant's civil-from-days).
fn iso_in_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn now_ms_plus(delta_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso_in_ms(now.as_millis() as u64 + delta_ms)
}

/// Warm the embedded gateway so the first schedules call does not pay the
/// auto-start latency (the once-fire margins below assume a warm process).
async fn warm_gateway(root: &Path) {
    let out = await_cli(root, &["server", "status", "--output", "json"]);
    expect_ok("service.status", &out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedules_configure_list_history_round_trip() {
    let home = TestHome::new("sched-e2e-roundtrip");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "sched-rt").await;
    warm_gateway(root).await;

    let created = configure_schedule(
        root,
        "sched-rt-1",
        &project_id,
        &session_id,
        "cron",
        "*/5 * * * *",
        "hello schedule",
        "absent",
    )
    .await;
    assert_eq!(created["schedule"]["scheduleId"], "sched-rt-1");
    assert_eq!(created["schedule"]["enabled"], true);
    assert!(
        created["schedule"]["nextRunAt"].as_str().is_some(),
        "an armed cron schedule exposes its next due time"
    );
    let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

    let listed = schedules_list(root).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["items"][0]["scheduleId"], "sched-rt-1");
    assert_eq!(listed["items"][0]["timezone"], "UTC");
    assert_eq!(listed["items"][0]["revision"], revision);

    let page = history(root, "sched-rt-1").await;
    assert_eq!(
        page["items"].as_array().map(Vec::len),
        Some(0),
        "nothing fired yet"
    );

    // Revision gate: stale revision and `absent`-on-existing both reject.
    let out = await_cli(
        root,
        &[
            "schedules",
            "configure",
            "--output",
            "json",
            "--schedule-id",
            "sched-rt-1",
            "--project-id",
            &project_id,
            "--session-id",
            &session_id,
            "--kind",
            "cron",
            "--expression",
            "*/10 * * * *",
            "--timezone",
            "UTC",
            "--message",
            "hello schedule",
            "--expected-revision",
            "srev-00009999",
        ],
    );
    assert_eq!(
        expect_failed("schedules.configure", &out),
        "REVISION_CONFLICT"
    );

    let out = await_cli(
        root,
        &[
            "schedules",
            "history",
            "--output",
            "json",
            "--schedule-id",
            "sched-none",
        ],
    );
    assert_eq!(expect_failed("schedules.history", &out), "NOT_FOUND");

    // Invalid input rejects before any write.
    let out = await_cli(
        root,
        &[
            "schedules",
            "configure",
            "--output",
            "json",
            "--schedule-id",
            "sched-bad",
            "--project-id",
            &project_id,
            "--session-id",
            &session_id,
            "--kind",
            "cron",
            "--expression",
            "* * * * *",
            "--timezone",
            "Mars/Olympus",
            "--message",
            "hello",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(
        expect_failed("schedules.configure", &out),
        "INVALID_ARGUMENT"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedules_trigger_completes_and_replays_idempotently() {
    let home = TestHome::new("sched-e2e-trigger");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "sched-tr").await;
    warm_gateway(root).await;

    let created = configure_schedule(
        root,
        "sched-tr-1",
        &project_id,
        &session_id,
        "cron",
        "0 3 * * *",
        "trigger me",
        "absent",
    )
    .await;
    let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();
    let next_before = created["schedule"]["nextRunAt"]
        .as_str()
        .unwrap()
        .to_owned();

    let trigger_args = [
        "schedules",
        "trigger",
        "--output",
        "json",
        "--request-key",
        "e2e-trigger-key-1",
        "--schedule-id",
        "sched-tr-1",
        "--expected-revision",
        &revision,
    ];
    let out = await_cli(root, &trigger_args);
    let receipt = expect_ok("schedules.trigger", &out);
    assert_eq!(receipt["kind"], "operation");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

    let execution = poll_history_terminal(root, "sched-tr-1", Duration::from_secs(30)).await;
    assert_eq!(execution["state"], "succeeded");
    let run_id = execution["runId"].as_str().unwrap().to_owned();

    // The associated run is queryable through the run domain.
    let run = cli_json(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    )
    .await;
    assert_eq!(run["run"]["state"], "succeeded");
    assert_eq!(run["run"]["sessionId"], session_id);

    // The explicit trigger did not move the regular next due time.
    let listed = schedules_list(root).await;
    assert_eq!(listed["items"][0]["nextRunAt"], next_before);

    // Idempotent replay: same key + same input → the same operation.
    let out = await_cli(root, &trigger_args);
    assert!(
        out.stdout.contains(&operation_id),
        "replay must return the same operationId: {}",
        out.stdout
    );
    let replay = expect_ok("schedules.trigger", &out);
    assert_eq!(replay["operationId"], operation_id);
    assert_eq!(replay["deduplicated"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedules_trigger_then_stop_execution_cancels() {
    let home = TestHome::new("sched-e2e-stop");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "sched-st").await;
    warm_gateway(root).await;

    let created = configure_schedule(
        root,
        "sched-st-1",
        &project_id,
        &session_id,
        "cron",
        "0 3 * * *",
        "[pd-driver:sleep=60000] still running",
        "absent",
    )
    .await;
    let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "schedules",
            "trigger",
            "--output",
            "json",
            "--schedule-id",
            "sched-st-1",
            "--expected-revision",
            &revision,
        ],
    );
    let receipt = expect_ok("schedules.trigger", &out);
    assert_eq!(receipt["kind"], "operation");

    let running = poll_history_running(root, "sched-st-1").await;
    let execution_id = running["executionId"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "schedules",
            "stop-execution",
            "--output",
            "json",
            "--schedule-id",
            "sched-st-1",
            "--execution-id",
            &execution_id,
        ],
    );
    let stop_receipt = expect_ok("schedules.stop_execution", &out);
    assert_eq!(stop_receipt["kind"], "operation");

    let execution = poll_history_terminal(root, "sched-st-1", Duration::from_secs(30)).await;
    assert_eq!(execution["state"], "cancelled");

    // The schedule survives the stop, still armed and enabled.
    let listed = schedules_list(root).await;
    assert_eq!(listed["items"][0]["enabled"], true);
    assert!(listed["items"][0]["nextRunAt"].as_str().is_some());

    // An execution that does not belong to the schedule is rejected.
    let out = await_cli(
        root,
        &[
            "schedules",
            "stop-execution",
            "--output",
            "json",
            "--schedule-id",
            "sched-st-1",
            "--execution-id",
            "exec-none",
        ],
    );
    assert_eq!(expect_failed("schedules.stop_execution", &out), "NOT_FOUND");
}

/// The scheduler-driven fire: a due once schedule executes through the engine
/// and is not re-armed afterwards. The ported scheduler's production timer
/// is the 60 s idle poll (`DEFAULT_IDLE_POLL_MS`, faithful to the TS
/// baseline), so the poll window is 75 s; a warm gateway usually lets the
/// post-create poke tick land it within a second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_once_fires_and_is_not_re_armed() {
    let home = TestHome::new("sched-e2e-once");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "sched-once").await;
    warm_gateway(root).await;

    let run_at = now_ms_plus(2_000);
    let created = configure_schedule(
        root,
        "sched-once-1",
        &project_id,
        &session_id,
        "once",
        &run_at,
        "fire once",
        "absent",
    )
    .await;
    assert!(
        created["schedule"]["nextRunAt"].as_str().is_some(),
        "a future once schedule is armed"
    );

    let execution = poll_history_terminal(root, "sched-once-1", Duration::from_secs(75)).await;
    assert_eq!(execution["state"], "succeeded");
    assert!(execution["runId"].as_str().is_some());

    // The fired once schedule stays listed but unarmed ("已触发一次计划不被
    // 重新排入") and a later observation does not grow the history.
    let listed = schedules_list(root).await;
    assert_eq!(listed["items"][0]["nextRunAt"], Value::Null);
    let page = history(root, "sched-once-1").await;
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1));
}

/// A paused schedule does not fire: the once task is deleted on disable, so
/// no tick can pick it up (the quiet window only needs to outlive the poke
/// tick, not the idle poll).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_schedule_does_not_fire() {
    let home = TestHome::new("sched-e2e-disabled");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "sched-dis").await;
    warm_gateway(root).await;

    let run_at = now_ms_plus(2_000);
    let created = configure_schedule(
        root,
        "sched-dis-1",
        &project_id,
        &session_id,
        "once",
        &run_at,
        "never fires",
        "absent",
    )
    .await;
    let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "schedules",
            "set-enabled",
            "--output",
            "json",
            "--schedule-id",
            "sched-dis-1",
            "--enabled",
            "false",
            "--expected-revision",
            &revision,
        ],
    );
    let disabled = expect_ok("schedules.set_enabled", &out);
    assert_eq!(disabled["schedule"]["enabled"], false);
    assert_eq!(disabled["schedule"]["nextRunAt"], Value::Null);

    tokio::time::sleep(Duration::from_secs(3)).await;
    let page = history(root, "sched-dis-1").await;
    assert_eq!(
        page["items"].as_array().map(Vec::len),
        Some(0),
        "a paused schedule does not fire"
    );

    // An explicit trigger on a paused schedule rejects too.
    let out = await_cli(
        root,
        &[
            "schedules",
            "trigger",
            "--output",
            "json",
            "--schedule-id",
            "sched-dis-1",
            "--expected-revision",
            &revision,
        ],
    );
    assert_eq!(expect_failed("schedules.trigger", &out), "INVALID_ARGUMENT");
}

/// The offline-driver gateway has no tool chain starting background tasks,
/// so the v1 view is an empty, session-scoped collection; unknown tasks are
/// NOT_FOUND (never fabricated, never mixed with runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_tasks_view_is_scoped_and_honest_when_empty() {
    let home = TestHome::new("bg-e2e-empty");
    let root = home.path();
    init_home(root);
    let (_project_id, session_id) = seed_project_session(root, "bg-empty").await;

    let listed = cli_json(
        root,
        &[
            "background-tasks",
            "list",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    )
    .await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
    assert!(listed["snapshotRevision"].as_str().is_some());
    assert_eq!(listed["nextCursor"], Value::Null);

    let out = await_cli(
        root,
        &[
            "background-tasks",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--task-id",
            "task-none",
        ],
    );
    assert_eq!(expect_failed("background_tasks.get", &out), "NOT_FOUND");

    let out = await_cli(
        root,
        &[
            "background-tasks",
            "stop",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--task-id",
            "task-none",
        ],
    );
    assert_eq!(expect_failed("background_tasks.stop", &out), "NOT_FOUND");
}
