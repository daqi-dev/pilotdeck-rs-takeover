//! S5 业务旅程 J10（运维与诊断）端到端：`_tmp/20261119-s5/
//! journeys-execution-plan.md` §3 J10 的命令序列（S4 t021 已合入，
//! installation/data 段不再受阻）。真实 `agentcpm-pd` 二进制、隔离
//! `AGENTCPM_PD_HOME`（installation 段另加隔离 `AGENTCPM_PD_INSTALL_ROOT`，
//! 绝不触碰本机真实 `~/.local`）、`common_v1` 的 envelope schema 校验。
//!
//! 旅程判据（计划文档原文）：stdout + 备份 artifact + `operations events`
//! 游标终态 cancelled。
//!
//! - `j10_operations_diagnostics_and_remote_longop_journey`：`server
//!   health`/`server diagnostics`（干净态 + 失败 run 产生
//!   SESSION_QUEUE_PAUSED 且提示词正文不外泄）→ `server resources`（含
//!   kind=browser CAPABILITY_UNAVAILABLE、非法 kind INVALID_ARGUMENT 负
//!   例）→ 长作业（`[pd-driver:maintenance=8000]`）→ `operations list/
//!   get`（running + progress）→ `operations events` 逐页链（`--page
//!   {"limit":1}` + `--after-cursor`，类型序 + 游标严格单调；「订阅」半
//!   程的诚实注记：follow 流门未开时 operations.events 仅合同化为分页读，
//!   与 J13 同口径）→ `operations cancel`（requested，repeated cancel 不
//!   记第二次意图）→ run 本体照常 succeeded（两条生命周期独立）→ 终态
//!   cancelled + 事件链含 cancel_requested 且止于 cancelled → `resources
//!   snapshot`（present/revision + known-versions 条件读折叠）→ `resources
//!   watch` 正例（t023 E 流数据面：一元 head 页返回首资源 snapshot 帧；
//!   follow 流观察另一客户端注册 project 产生的 changed 帧——500ms 公布
//!   条件轮询间隔内收敛，两客户端互见）→ 远程半程（remote_pair：
//!   `--target` 提交 maintenance run、`operations list/get/events/cancel`
//!   全部经远程、run wait succeeded；B 落 ready.json/state.sqlite，A 不
//!   落）。
//! - `j10_installation_and_data_backup_segment_journey`：`installation
//!   inspect`（source 态）→ 未配置渠道 `installation check` 负例
//!   （INVALID_ARGUMENT）→ 活渠道 check（updateAvailable+verified）→
//!   BUILD_CONFLICT CAS 负例（install_root 零残留）→ `installation apply`
//!   （发布 + 代际落盘断言）→ inspect（managed/9.9.9）→ `installation
//!   diagnose`（redacted 报告 artifact + checks 诚实）→ data 段：`data
//!   inspect`→`create-backup`（受理→完成）→`list-backup-entries`（分页）
//!   →`read-backup-artifact`（+DESTINATION_EXISTS 负例）。渠道桩只绑
//!   127.0.0.1 + 系统分配端口（009 §8.3 先例）；包体走产品自己的打包库。

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256, Sha512};
use tokio::io::{AsyncBufReadExt, BufReader};

use pilotdeck_gateway::public_v1::domains::installation::release::{
    self as release_lib, PackageFile,
};

use common_v1::*;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// One remote argv invocation with the `--target <profile>` hop (the J8
/// helper's shape).
fn rt(home: &Path, target: &str, words: &[&str]) -> CliOutcome {
    let mut args: Vec<&str> = words.to_vec();
    args.extend(["--output", "json", "--target", target]);
    await_cli(home, &args)
}

/// Parses a failed argv invocation: exactly one envelope line,
/// `outcome: "failed"`, non-zero exit. Returns the `error` object.
fn expect_error(method: &str, out: &CliOutcome) -> Value {
    assert_ne!(
        out.code, 0,
        "`{method}` must exit non-zero; stdout:\n{}\nstderr:\n{}",
        out.stdout, out.stderr
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
    assert_eq!(value["version"], OP_VERSION, "{value}");
    assert_eq!(value["outcome"], "failed", "{value}");
    value["error"].clone()
}

/// The contract error code of one failed envelope (non-zero exit asserted).
/// The t021 shape: the LAST non-empty stdout line is the envelope (tolerant
/// of any diagnostics printed ahead of it on the installation paths).
fn expect_failed_code(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must exit non-zero on failure");
    let line = out
        .stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let value: Value = serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("`{method}` stdout is not JSON ({error}): {line}"));
    assert_eq!(value["outcome"], "failed", "`{method}` envelope: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Waits until the offline driver's maintenance operation surfaces as
/// `running` in the unified listing (locally or through a target profile).
async fn await_running_maintenance(home: &Path, target: Option<&str>) -> String {
    // 满载预算（判据不变）：等维护操作进入 running，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let mut args = vec![
            "operations",
            "list",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"instance"}"#,
        ];
        if let Some(t) = target {
            args.push("--target");
            args.push(t);
        }
        let page = expect_ok("operations.list", &await_cli(home, &args));
        for item in page["items"].as_array().cloned().unwrap_or_default() {
            if item["operationType"] == json!("instance.maintenance")
                && item["state"] == json!("running")
                && let Some(id) = item["operationId"].as_str()
            {
                return id.to_owned();
            }
        }
        assert!(
            Instant::now() < deadline,
            "no running maintenance operation appeared in operations.list"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// One `operations events` page (the pre-cancel observation of the still
/// running operation: bounded, never a follow loop).
fn operations_events_page(
    home: &Path,
    target: Option<&str>,
    operation_id: &str,
    after: Option<&str>,
) -> Value {
    let mut args = vec![
        "operations",
        "events",
        "--output",
        "json",
        "--operation-id",
        operation_id,
        "--page",
        "{\"limit\":10}",
    ];
    if let Some(t) = target {
        args.push("--target");
        args.push(t);
    }
    if let Some(cursor) = after {
        args.push("--after-cursor");
        args.push(cursor);
    }
    expect_ok("operations.events", &await_cli(home, &args))
}

/// Reads the operation's whole event journal page by page (`--page
/// {"limit":1}` forces pagination) and asserts the journey's stream-cursor
/// criterion: the event types arrive in lifecycle order and the cursor
/// sequence is strictly increasing across pages (the J13 helper's shape,
/// target-aware). Only called on an operation already in (or reaching) a
/// terminal state, so the chain converges.
fn paged_operation_events(home: &Path, target: Option<&str>, operation_id: &str) -> Vec<String> {
    let mut types = Vec::new();
    let mut last_seq: Option<u64> = None;
    let mut after: Option<String> = None;
    for _ in 0..200 {
        let mut args = vec![
            "operations",
            "events",
            "--output",
            "json",
            "--operation-id",
            operation_id,
            "--page",
            "{\"limit\":1}",
        ];
        if let Some(t) = target {
            args.push("--target");
            args.push(t);
        }
        if let Some(cursor) = &after {
            args.push("--after-cursor");
            args.push(cursor);
        }
        let page = expect_ok("operations.events", &await_cli(home, &args));
        let frame = &page["frame"];
        for event in frame["events"].as_array().cloned().unwrap_or_default() {
            types.push(event["type"].as_str().unwrap().to_owned());
            let cursor = event["cursor"].as_str().unwrap().to_owned();
            let seq = cursor
                .trim_start_matches("cur-")
                .parse::<u64>()
                .unwrap_or_else(|e| panic!("unparseable cursor `{cursor}`: {e}"));
            if let Some(last) = last_seq {
                assert!(
                    seq > last,
                    "cursor sequence must be strictly increasing: cur-{last} -> {cursor}"
                );
            }
            last_seq = Some(seq);
        }
        if frame["terminal"] == true {
            assert_eq!(
                frame["caughtUp"], true,
                "a terminal frame must be caught up"
            );
            return types;
        }
        after = frame["nextCursor"].as_str().map(str::to_owned);
        assert!(
            after.is_some(),
            "a non-terminal page must carry the next cursor: {frame}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("operations.events pagination did not converge for {operation_id}");
}

// ---------------------------------------------------------------------------
// J10 journey 1: operations, diagnostics, resources (+ the remote half)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn j10_operations_diagnostics_and_remote_longop_journey() {
    let home = TestHome::new("j10-ops");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "j10").await;

    // ---- server health + the clean diagnostics view. ----
    let health = expect_ok(
        "system.health",
        &await_cli(home.path(), &["server", "health", "--output", "json"]),
    );
    assert_eq!(health["alive"], true, "{health}");
    assert_eq!(health["ready"], true, "{health}");
    let clean = expect_ok(
        "system.diagnostics",
        &await_cli(home.path(), &["server", "diagnostics", "--output", "json"]),
    );
    assert_eq!(clean["pageInfo"]["complete"], true, "{clean}");

    // A failing run pauses the session queue: the diagnostic appears and
    // never leaks the prompt body.
    let failed = submit_run(home.path(), &session_id, "[pd-driver:fail]", "j10-fail-1").await;
    let failed_id = failed["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(home.path(), &failed_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["conditionMet"], true, "{waited}");
    let diags = expect_ok(
        "system.diagnostics",
        &await_cli(home.path(), &["server", "diagnostics", "--output", "json"]),
    );
    let paused = diags["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["code"] == json!("SESSION_QUEUE_PAUSED"))
        .cloned()
        .unwrap_or_else(|| panic!("SESSION_QUEUE_PAUSED missing: {}", diags["items"]));
    assert_eq!(paused["resourceId"], session_id.as_str());
    assert!(
        !paused["message"]
            .as_str()
            .unwrap_or_default()
            .contains("[pd-driver"),
        "the prompt body must never leak: {paused}"
    );
    resume_paused_queue(home.path(), &session_id).await;

    // ---- server resources + the honest negative kinds. ----
    let resources = expect_ok(
        "system.resources",
        &await_cli(home.path(), &["server", "resources", "--output", "json"]),
    );
    let items = resources["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|item| item["kind"] == json!("session")));
    assert_eq!(resources["pageInfo"]["complete"], true);
    let browser = expect_error(
        "system.resources",
        &await_cli(
            home.path(),
            &[
                "server",
                "resources",
                "--output",
                "json",
                "--kind",
                "browser",
            ],
        ),
    );
    assert_eq!(browser["code"], "CAPABILITY_UNAVAILABLE", "{browser}");
    let printer = expect_error(
        "system.resources",
        &await_cli(
            home.path(),
            &[
                "server",
                "resources",
                "--output",
                "json",
                "--kind",
                "printer",
            ],
        ),
    );
    assert_eq!(printer["code"], "INVALID_ARGUMENT", "{printer}");

    // ---- The long operation over the unified operations surface. ----
    let submitted = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:maintenance=8000]",
        "j10-maint-1",
    )
    .await;
    let maintenance_run = submitted["runId"].as_str().unwrap().to_owned();
    let operation_id = await_running_maintenance(home.path(), None).await;

    // operations.get: journey criterion "running + progress". The driver
    // appends the first `elapsedMs` progress record the moment the
    // operation turns running and then every 100ms, but a `get` that races
    // ahead of that first append must retry rather than fail — so poll
    // until the running operation reports progress (bounded like the
    // other wait loops in this file).
    // 满载预算（判据不变）：等 running 操作报出首条 progress，30s→60s。
    let progress_deadline = Instant::now() + Duration::from_secs(60);
    let _got = loop {
        let got = expect_ok(
            "operations.get",
            &await_cli(
                home.path(),
                &[
                    "operations",
                    "get",
                    "--output",
                    "json",
                    "--operation-id",
                    &operation_id,
                ],
            ),
        );
        assert_eq!(got["operation"]["state"], "running", "{got}");
        if got["operation"]["progress"]
            .as_array()
            .is_some_and(|progress| !progress.is_empty())
        {
            break got;
        }
        assert!(
            Instant::now() < progress_deadline,
            "the maintenance operation reports progress: {got}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // operations.events: the running operation's journal is readable now
    // (one bounded page; the terminal chain follows after the cancel below).
    let page = operations_events_page(home.path(), None, &operation_id, None);
    let frame = &page["frame"];
    let running_types: Vec<String> = frame["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        running_types.first().map(String::as_str),
        Some("operation.queued"),
        "{page}"
    );
    assert!(
        running_types.iter().any(|kind| kind == "operation.started"),
        "{page}"
    );
    assert_eq!(frame["terminal"], false, "still running: {page}");

    // operations.cancel: requested (one intent), repeatable without a second
    // intent, and independent of the run's own lifecycle.
    let cancel = expect_ok(
        "operations.cancel",
        &await_cli(
            home.path(),
            &[
                "operations",
                "cancel",
                "--output",
                "json",
                "--operation-id",
                &operation_id,
            ],
        ),
    );
    assert_eq!(cancel["receipt"]["disposition"], "requested", "{cancel}");
    assert_eq!(cancel["receipt"]["state"], "cancelling", "{cancel}");
    let cancel_revision = cancel["receipt"]["revision"].as_str().unwrap().to_owned();
    let again = expect_ok(
        "operations.cancel",
        &await_cli(
            home.path(),
            &[
                "operations",
                "cancel",
                "--output",
                "json",
                "--operation-id",
                &operation_id,
            ],
        ),
    );
    match again["receipt"]["disposition"].as_str().unwrap() {
        "requested" => {
            assert_eq!(
                again["receipt"]["revision"], cancel_revision,
                "a repeated cancel never records a second intent: {again}"
            );
        }
        "alreadyTerminal" => {}
        other => panic!("unexpected cancel disposition `{other}`: {again}"),
    }

    // The run itself completes: cancelling the operation does not cancel the
    // run (independent lifecycles), and the operation reaches cancelled.
    let run_done = wait_run(home.path(), &maintenance_run);
    let run_done = expect_ok("run.wait", &run_done);
    assert_eq!(run_done["run"]["state"], "succeeded", "{run_done}");
    let terminal = expect_ok(
        "operations.get",
        &await_cli(
            home.path(),
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                &operation_id,
            ],
        ),
    );
    assert_eq!(terminal["operation"]["state"], "cancelled", "{terminal}");
    let events = paged_operation_events(home.path(), None, &operation_id);
    assert!(
        events
            .iter()
            .any(|kind| kind == "operation.cancel_requested"),
        "{events:?}"
    );
    assert_eq!(
        events.last().map(String::as_str),
        Some("operation.cancelled")
    );

    // ---- resources snapshot: present/revision + the conditional fold. ----
    let watched =
        format!(r#"[{{"kind":"projects"}},{{"kind":"session","resourceId":"{session_id}"}}]"#);
    let snap = expect_ok(
        "resources.snapshot",
        &await_cli(
            home.path(),
            &[
                "resources",
                "snapshot",
                "--output",
                "json",
                "--resources",
                &watched,
            ],
        ),
    );
    let session_entry = snap["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["resource"]["kind"] == json!("session"))
        .cloned()
        .unwrap();
    assert_eq!(session_entry["state"], "present");
    assert_eq!(session_entry["snapshotSchemaId"], "resources.session.v1");
    let session_revision = session_entry["revision"].as_str().unwrap().to_owned();
    let known = json!([{
        "resource": {"kind": "session", "resourceId": session_id},
        "revision": session_revision,
    }])
    .to_string();
    let folded = expect_ok(
        "resources.snapshot",
        &await_cli(
            home.path(),
            &[
                "resources",
                "snapshot",
                "--output",
                "json",
                "--resources",
                &watched,
                "--known-versions",
                &known,
            ],
        ),
    );
    assert_eq!(folded["unchanged"].as_array().unwrap().len(), 1);
    assert_eq!(folded["snapshots"].as_array().unwrap().len(), 1);

    // ---- resources watch: the stream data plane (t023 E). The unary page
    // answers the HEAD of the frame sequence (the first resource's current
    // version — the batch current-versions read stays `resources.snapshot`'s
    // job); the follow flow then observes a second client registering a
    // project through the published 500 ms conditional-poll interval. ----
    let watch_input =
        format!(r#"[{{"kind":"projects"}},{{"kind":"session","resourceId":"{session_id}"}}]"#);
    let unary = expect_ok(
        "resources.watch",
        &await_cli(
            home.path(),
            &[
                "resources",
                "watch",
                "--output",
                "json",
                "--resources",
                &watch_input,
            ],
        ),
    );
    assert_eq!(unary["type"], "snapshot", "{unary}");
    assert_eq!(unary["resource"]["kind"], "projects");
    assert_eq!(unary["snapshot"]["state"], "present");
    assert_eq!(
        unary["snapshot"]["snapshotSchemaId"],
        "resources.projects.v1"
    );
    assert_eq!(unary["cursor"]["sequence"], 0);
    assert!(
        unary["cursor"]["subscriptionEpoch"]
            .as_str()
            .is_some_and(|epoch| !epoch.is_empty()),
        "the page carries a fresh per-subscription epoch: {unary}"
    );

    // The follow stream: a live `resources watch --follow` process whose
    // NDJSON event envelopes are read while it runs (the j1 terminal-attach
    // shape). The gateway is already up from the earlier invocations, so
    // both clients share one instance.
    let mut watch_cmd = base_command(home.path());
    watch_cmd.args([
        "resources",
        "watch",
        "--output",
        "ndjson",
        "--follow",
        "--resources",
        r#"[{"kind":"projects"}]"#,
    ]);
    let mut watch_child = watch_cmd.spawn().expect("spawn resources watch --follow");
    let watch_stdout = watch_child.stdout.take().expect("piped watch stdout");
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Value>(256);
    tokio::spawn(async move {
        let mut lines = BufReader::new(watch_stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(frame) = serde_json::from_str::<Value>(&line)
                && frame_tx.send(frame).await.is_err()
            {
                break;
            }
        }
    });
    // 满载预算（判据不变）：等 watch 的初始 snapshot 帧，15s→60s。
    let initial = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let frame = frame_rx
                .recv()
                .await
                .expect("the watch keeps streaming while it lives");
            if frame["type"] == "resources.watch"
                && frame["data"]["type"] == "snapshot"
                && frame["data"]["resource"]["kind"] == "projects"
            {
                return frame;
            }
        }
    })
    .await
    .expect("the watch delivers its initial snapshot frame");
    let initial_revision = initial["data"]["revision"].as_str().unwrap().to_owned();

    // A second client registers a project: the watch reports the revision
    // move (acceptance #4's two-clients criterion, within the published
    // conditional-poll interval).
    let instance_id = init_home_instance(home.path());
    let registered = await_cli(
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-j10-watch",
            "--name",
            "J10 watch second client",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"root-j10w\",\"relativePath\":\"ws\"}}"
            ),
        ],
    );
    expect_ok("project.register", &registered);
    let changed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let frame = frame_rx
                .recv()
                .await
                .expect("the watch keeps streaming while it lives");
            if frame["type"] == "resources.watch" && frame["data"]["type"] == "changed" {
                return frame;
            }
        }
    })
    .await
    .expect("the watch reports the second client's registration");
    assert_eq!(changed["data"]["resource"]["kind"], "projects", "{changed}");
    assert_ne!(
        changed["data"]["revision"].as_str().unwrap_or_default(),
        initial_revision,
        "the changed frame carries the moved revision: {changed}"
    );

    // A watch never ends on its own (it has no terminal state); the test
    // stops the consumer here.
    assert!(
        watch_child.try_wait().expect("poll watch child").is_none(),
        "the watch subscription stays live until its consumer stops it"
    );
    watch_child.kill().await.expect("stop the watch process");

    // ---- The remote half (远程 ◐: the long operation's events over the
    // real ws hop): everything through the target profile. ----
    let pair = remote_pair("j10");
    let home_a = pair.home_a.path();
    let target = pair.target_id.clone();
    let instance_b = pair.instance_id_b();
    let (_remote_project, remote_session) =
        seed_remote_project_session(home_a, &target, &instance_b, "j10r").await;
    let submitted = expect_ok(
        "run.submit",
        &rt(
            home_a,
            &target,
            &[
                "run",
                "submit",
                "--session-id",
                &remote_session,
                "--request-key",
                "j10-remote-maint-1",
                "--text",
                "j10 remote maintenance [pd-driver:maintenance=6000]",
            ],
        ),
    );
    let remote_run = submitted["runId"].as_str().unwrap().to_owned();
    let remote_op = await_running_maintenance(home_a, Some(&target)).await;
    let page = operations_events_page(home_a, Some(&target), &remote_op, None);
    let frame = &page["frame"];
    assert_eq!(
        frame["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .first()
            .and_then(|event| event["type"].as_str().map(str::to_owned))
            .as_deref(),
        Some("operation.queued"),
        "{page}"
    );
    assert_eq!(frame["terminal"], false, "still running remotely: {page}");
    let cancel = expect_ok(
        "operations.cancel",
        &rt(
            home_a,
            &target,
            &["operations", "cancel", "--operation-id", &remote_op],
        ),
    );
    assert_eq!(cancel["receipt"]["disposition"], "requested", "{cancel}");
    // 满载预算（判据不变）：等远程维护操作取消落终态，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    let terminal = loop {
        let got = expect_ok(
            "operations.get",
            &rt(
                home_a,
                &target,
                &["operations", "get", "--operation-id", &remote_op],
            ),
        );
        let state = got["operation"]["state"].as_str().unwrap_or_default();
        if matches!(
            state,
            "cancelled" | "failed" | "interrupted" | "result_unknown"
        ) {
            break got;
        }
        assert!(
            Instant::now() < deadline,
            "the remote maintenance operation never became terminal"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(terminal["operation"]["state"], "cancelled", "{terminal}");
    let events = paged_operation_events(home_a, Some(&target), &remote_op);
    assert_eq!(
        events.last().map(String::as_str),
        Some("operation.cancelled")
    );
    let waited = await_cli(
        home_a,
        &[
            "run",
            "wait",
            "--output",
            "json",
            "--target",
            &target,
            "--run-id",
            &remote_run,
            "--timeout-ms",
            "60000",
        ],
    );
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded", "{waited}");

    // 双侧留存: the server side carries everything, the client side nothing.
    assert!(pair.home_b.path().join("run").join("ready.json").is_file());
    assert!(pair.home_b.path().join("v1").join("state.sqlite").is_file());
    assert!(!home_a.join("local.json").exists());
    assert!(!home_a.join("v1").exists());
    assert_eq!(pair.home_a.gateway_pid(), None);
}

// ---------------------------------------------------------------------------
// J10 journey 2: the installation + data backup segment (t021/t020 merged)
// ---------------------------------------------------------------------------

/// One isolated environment: data root + install root (both under a temp
/// dir). Drop kills the serving gateway and removes the directory (the t021
/// Env shape).
struct Env {
    base: PathBuf,
    home: PathBuf,
    install_root: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "j10-install-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let home = base.join("home");
        let install_root = base.join("install-root");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::create_dir_all(&install_root).expect("mkdir install root");
        Self {
            base,
            home,
            install_root,
        }
    }

    fn read_ready_pid(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(self.home.join("run").join("ready.json")).ok()?;
        let value: Value = serde_json::from_str(&raw).ok()?;
        value.get("pid").and_then(Value::as_u64).map(|p| p as u32)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(pid) = self.read_ready_pid()
            && let Some(raw) = rustix::process::Pid::from_raw(pid as i32)
        {
            let _ = rustix::process::kill_process(raw, rustix::process::Signal::KILL);
        }
        std::thread::sleep(Duration::from_millis(60));
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Runs one CLI invocation with BOTH the isolated home and install root (the
/// running gateway inherits the env from the first invocation that starts
/// it, so every call in the scenario goes through here).
fn cli(home: &Path, install_root: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = StdCommand::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home)
        .env("AGENTCPM_PD_ENABLE_OFFLINE_DIRECTIVES", "1")
        .env("AGENTCPM_PD_INSTALL_ROOT", install_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    let deadline = Instant::now() + CLI_TIMEOUT;
    let mut child = cmd.spawn().expect("spawn agentcpm-pd");
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout = std::thread::spawn(move || read_all(stdout_pipe));
    let stderr = std::thread::spawn(move || read_all(stderr_pipe));
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait agentcpm-pd") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("cli invocation timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    CliOutcome {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout.join().expect("read stdout")).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.join().expect("read stderr")).into_owned(),
    }
}

fn read_all(mut pipe: impl std::io::Read) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = std::io::Read::read_to_end(&mut pipe, &mut buffer);
    buffer
}

async fn json_cli(home: &Path, install_root: &Path, args: &[&str], method: &str) -> Value {
    let out = cli(home, install_root, args);
    expect_ok(method, &out)
}

/// The controlled local channel (009 §8.3): `release.json` + assets at their
/// manifest names, loopback + system-assigned port only.
async fn spawn_channel(manifest_bytes: Vec<u8>, assets: Vec<(String, Vec<u8>)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let manifest_bytes = manifest_bytes.clone();
            let assets = assets.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut head = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    head.extend_from_slice(&chunk[..read]);
                    if head.windows(4).any(|window| window == b"\r\n\r\n") || head.len() > 64 * 1024
                    {
                        break;
                    }
                }
                let request_line = String::from_utf8_lossy(&head);
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_owned();
                let (body, content_type) = if path == "release.json" {
                    (manifest_bytes.clone(), "application/json")
                } else {
                    match assets.iter().find(|(name, _)| *name == path) {
                        Some((_, bytes)) => (bytes.clone(), "application/gzip"),
                        None => (Vec::new(), "application/octet-stream"),
                    }
                };
                let status = if body.is_empty() && path != "release.json" {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                if socket.write_all(&body).await.is_err() {
                    return;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha512_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
}

/// One complete, valid release package for the CURRENT platform, built
/// through the product's own packaging library; the packaged binary is a
/// script stub (this journey never restarts from the new generation — that
/// handoff is the t021 suite's dedicated scenario).
fn build_package(
    scratch: &Path,
    version: &str,
    tag: &str,
    source_sha: &str,
) -> (Value, Vec<u8>, String, String) {
    let dir = scratch.join(format!("pkg-{version}-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("manifest")).unwrap();
    let build_id = release_lib::build_id(version, source_sha, "ffffffffffff");
    let binary = dir.join("bin").join("agentcpm-pd");
    std::fs::write(&binary, b"#!/bin/sh\necho agentcpm-pd e2e j10\n").unwrap();
    let (platform, arch) = release_lib::current_platform_pair();
    let asset_name = format!("agentcpm-pd-{version}-{platform}-{arch}.tar.gz");
    // The in-package manifest carries the same asset row with placeholder
    // digests (the archive contains this manifest, so it cannot embed the
    // digest of itself); consumer validation only requires a well-formed row.
    let manifest_json = json!({
        "schemaVersion": 2,
        "product": "agentcpm-pd",
        "version": version,
        "buildId": build_id,
        "tag": tag,
        "date": "2026-11-19",
        "buildTime": "2026-11-19T00:00:00Z",
        "sourceSha": source_sha,
        "contractDigest": "ffffffffffff",
        "assets": [{
            "name": asset_name,
            "size": 1000,
            "sha256": "c".repeat(64),
            "sha512": "d".repeat(86) + "==",
            "platform": platform,
            "arch": arch,
            "kind": "archive",
        }],
    });
    std::fs::write(
        dir.join("manifest").join("release.json"),
        manifest_json.to_string(),
    )
    .unwrap();
    let sums = release_lib::render_sha256sums(&[
        ("bin/agentcpm-pd".to_owned(), binary.clone()),
        (
            "manifest/release.json".to_owned(),
            dir.join("manifest").join("release.json"),
        ),
    ])
    .unwrap();
    std::fs::write(dir.join("SHA256SUMS.txt"), &sums).unwrap();
    let archive_path = dir.join(&asset_name);
    release_lib::write_release_package(
        &archive_path,
        &[
            PackageFile {
                relative_path: "bin/agentcpm-pd".to_owned(),
                source: binary,
                mode: 0o755,
            },
            PackageFile {
                relative_path: "manifest/release.json".to_owned(),
                source: dir.join("manifest").join("release.json"),
                mode: 0o644,
            },
            PackageFile {
                relative_path: "SHA256SUMS.txt".to_owned(),
                source: dir.join("SHA256SUMS.txt"),
                mode: 0o644,
            },
        ],
    )
    .unwrap();
    let archive = std::fs::read(&archive_path).unwrap();

    // The channel manifest carries the real asset digests.
    let manifest_json = json!({
        "schemaVersion": 2,
        "product": "agentcpm-pd",
        "version": version,
        "buildId": build_id,
        "tag": tag,
        "date": "2026-11-19",
        "buildTime": "2026-11-19T00:00:00Z",
        "sourceSha": source_sha,
        "contractDigest": "ffffffffffff",
        "assets": [{
            "name": asset_name,
            "size": archive.len(),
            "sha256": sha256_hex(&archive),
            "sha512": sha512_base64(&archive),
            "platform": platform,
            "arch": arch,
            "kind": "archive",
        }],
    });
    (manifest_json, archive, asset_name, build_id)
}

fn write_channel_config(home: &Path, url: &str) {
    std::fs::create_dir_all(home.join("installation")).unwrap();
    std::fs::write(
        home.join("installation").join("channel.json"),
        json!({"channels": {"stable": url}}).to_string(),
    )
    .unwrap();
}

/// Polls `operations get` until the operation is terminal (`--no-start`
/// keeps the poll from racing a restart handoff with an auto-started
/// gateway; the t021 shape).
fn wait_operation_terminal(home: &Path, install_root: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = cli(
            home,
            install_root,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--no-start",
                "--operation-id",
                operation_id,
            ],
        );
        if out.code == 0
            && let Ok(value) = serde_json::from_str::<Value>(
                out.stdout
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(""),
            )
            && value["outcome"] == "succeeded"
        {
            let state = value["result"]["operation"]["state"].as_str().unwrap_or("");
            if matches!(
                state,
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return value["result"]["operation"].clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never became terminal; last output:\n{}\n{}",
            out.stdout,
            out.stderr
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn submit_and_wait(home: &Path, install_root: &Path, args: &[&str], method: &str) -> Value {
    let out = cli(home, install_root, args);
    let receipt = expect_ok(method, &out);
    let operation_id = receipt["operationId"]
        .as_str()
        .expect("operationId")
        .to_owned();
    wait_operation_terminal(home, install_root, &operation_id)
}

/// A minimal test-local legacy root for the data backup segment (the J9
/// journey drives the full shape; here only enough entries for the backup
/// loop to be observable).
fn seed_minimal_legacy_root(base: &Path) -> PathBuf {
    let root = base.join(format!("j10-legacy-{}", std::process::id()));
    let project = root.join("projects").join("j10-proj");
    std::fs::create_dir_all(project.join("chats")).unwrap();
    std::fs::create_dir_all(root.join("runs")).unwrap();
    std::fs::create_dir_all(root.join("memory")).unwrap();
    std::fs::write(
        root.join("pilotdeck.yaml"),
        "adapters:\n  feishu:\n    enabled: false\n",
    )
    .unwrap();
    std::fs::write(project.join(".cwd"), "/tmp/j10").unwrap();
    std::fs::write(
        project.join("chats").join("session.jsonl"),
        concat!(
            r#"{"type":"session_metadata","sessionId":"tui:project=/tmp/j10:default","turnId":"t0","sequence":1,"createdAt":"2026-11-01T00:00:00Z","title":"J10"}"#,
            "\n",
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("runs").join("registry.json"),
        r#"{"schemaVersion":1,"runs":{"55555555-5555-5555-5555-555555555555":{"sessionKey":"tui:project=/tmp/j10:default","state":"completed","revision":"r1","acceptedAt":"2026-11-01T00:00:00Z","updatedAt":"2026-11-01T00:00:05Z"}}}"#,
    )
    .unwrap();
    std::fs::write(root.join("memory").join("note.md"), "# memory\n").unwrap();
    root
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn j10_installation_and_data_backup_segment_journey() {
    let env = Env::new("j10");
    let home = &env.home;
    let install_root = &env.install_root;
    let init = cli(
        home,
        install_root,
        &["instance", "init", "--output", "json"],
    );
    expect_ok("instance.initialize", &init);

    // ---- installation inspect: the source identity. ----
    let result = json_cli(
        home,
        install_root,
        &["installation", "inspect", "--output", "json"],
        "installation.inspect",
    )
    .await;
    assert_eq!(result["productName"], "agentcpm-pd");
    assert_eq!(result["installationMode"], "source");
    assert_eq!(result["updateSupported"], false);
    assert_eq!(result["reasonCode"], "development");
    assert!(result["buildId"].as_str().is_some_and(|id| !id.is_empty()));

    // ---- check: the unconfigured channel is an honest usage error. ----
    let out = cli(
        home,
        install_root,
        &[
            "installation",
            "check",
            "--output",
            "json",
            "--request-key",
            "j10-check-0",
        ],
    );
    assert_eq!(
        expect_failed_code("installation.check", &out),
        "INVALID_ARGUMENT"
    );

    // ---- a live channel: check reports the verified update. ----
    let scratch = env.base.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let (manifest, archive, asset_name, build_id) = build_package(
        &scratch,
        "9.9.9",
        "v2026.11.19",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    );
    let base = spawn_channel(
        manifest.to_string().into_bytes(),
        vec![(asset_name.clone(), archive.clone())],
    )
    .await;
    write_channel_config(home, &base);
    let op = submit_and_wait(
        home,
        install_root,
        &[
            "installation",
            "check",
            "--output",
            "json",
            "--request-key",
            "j10-check-1",
        ],
        "installation.check",
    );
    assert_eq!(op["state"], "succeeded", "{op}");
    let result = &op["result"];
    assert_eq!(result["updateAvailable"], true);
    let releases = result["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0]["releaseId"], "v2026.11.19");
    assert_eq!(releases[0]["version"], "9.9.9");
    assert_eq!(releases[0]["verified"], true);

    // ---- apply: the BUILD_CONFLICT CAS refuses with zero residue. ----
    let op = submit_and_wait(
        home,
        install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "j10-apply-conflict",
            "--release-id",
            "v2026.11.19",
            "--expected-build-id",
            "0.0.0+000000000000.000000000000",
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "failed");
    assert_eq!(op["result"]["error"]["code"], "BUILD_CONFLICT");
    assert!(
        !install_root.join("generations").exists(),
        "a refused apply leaves no generation behind"
    );
    let result = json_cli(
        home,
        install_root,
        &["installation", "inspect", "--output", "json"],
        "installation.inspect",
    )
    .await;
    assert_eq!(result["installationMode"], "source");

    // ---- apply: the verified install publishes a generation on disk. ----
    let op = submit_and_wait(
        home,
        install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "j10-apply-1",
            "--release-id",
            "v2026.11.19",
            "--expected-build-id",
            &build_id,
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "succeeded", "{op}");
    assert_eq!(op["result"]["buildId"], build_id.as_str());
    assert_eq!(op["result"]["restartRequired"], true);
    assert!(
        install_root
            .join("generations")
            .join(&build_id)
            .join("bin")
            .join("agentcpm-pd")
            .is_file(),
        "the generation lands on disk"
    );
    assert!(
        install_root
            .join("current")
            .join("bin")
            .join("agentcpm-pd")
            .exists(),
        "the current pointer resolves to the new generation"
    );
    let result = json_cli(
        home,
        install_root,
        &["installation", "inspect", "--output", "json"],
        "installation.inspect",
    )
    .await;
    assert_eq!(result["installationMode"], "managed");
    assert_eq!(result["version"], "9.9.9");
    assert_eq!(result["buildId"], build_id.as_str());
    assert_eq!(result["updateSupported"], true);
    assert_eq!(result["reasonCode"], Value::Null);

    // ---- diagnose: the redacted report artifact with honest checks. ----
    let op = submit_and_wait(
        home,
        install_root,
        &[
            "installation",
            "diagnose",
            "--output",
            "json",
            "--request-key",
            "j10-diagnose-1",
            "--checks",
            "installation,service,runtime,storage,channels,mcp",
        ],
        "installation.diagnose",
    );
    assert_eq!(op["state"], "succeeded", "{op}");
    let result = &op["result"];
    assert_eq!(result["redacted"], true);
    let artifact_id = result["reportArtifactId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .expect("the diagnose report artifact");
    let checks = result["checks"].as_array().unwrap().clone();
    let by_name = |name: &str| -> Value {
        checks
            .iter()
            .find(|check| check["name"] == json!(name))
            .unwrap_or_else(|| panic!("missing check `{name}`: {checks:?}"))
            .clone()
    };
    assert_eq!(by_name("installation.manifest")["status"], "pass");
    assert_eq!(by_name("service.status")["status"], "pass");
    assert_eq!(by_name("storage.dataRoot")["status"], "pass");
    let artifact = json_cli(
        home,
        install_root,
        &[
            "artifacts",
            "get",
            "--output",
            "json",
            "--artifact-id",
            artifact_id,
        ],
        "artifacts.get",
    )
    .await;
    assert_eq!(artifact["artifact"]["mediaType"], "application/json");

    // ---- the data backup segment over the same home. ----
    let source = seed_minimal_legacy_root(&std::env::temp_dir());
    let inspect = json_cli(
        home,
        install_root,
        &[
            "data",
            "inspect",
            "--output",
            "json",
            "--data-root",
            source.to_string_lossy().as_ref(),
            "--depth",
            "full",
        ],
        "data.inspect",
    )
    .await;
    assert_eq!(
        inspect["formatVersion"],
        json!("agentcpm-pd.legacy-pilotdeck/1")
    );
    assert_eq!(inspect["summary"]["sessions"], json!(1));
    let backup_dir = env.base.join("backup");
    let receipt = json_cli(
        home,
        install_root,
        &[
            "data",
            "create-backup",
            "--output",
            "json",
            "--data-root",
            source.to_string_lossy().as_ref(),
            "--destination",
            backup_dir.to_string_lossy().as_ref(),
        ],
        "data.create_backup",
    )
    .await;
    let op = wait_operation_terminal(home, install_root, receipt["operationId"].as_str().unwrap());
    assert_eq!(op["state"], "succeeded", "{op}");
    let completion = &op["result"];
    assert_eq!(completion["includesSecrets"], json!(false));
    assert!(
        completion["entryCount"].as_u64().unwrap() >= 5,
        "{completion}"
    );
    let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();
    let page = json_cli(
        home,
        install_root,
        &[
            "data",
            "list-backup-entries",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--limit",
            "2",
        ],
        "data.list_backup_entries",
    )
    .await;
    assert_eq!(page["manifest"]["artifactId"], json!(artifact_id));
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let extract = env.base.join("extract");
    let extracted = json_cli(
        home,
        install_root,
        &[
            "data",
            "read-backup-artifact",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--entry-id",
            "projects/j10-proj/.cwd",
            "--destination",
            extract.to_string_lossy().as_ref(),
        ],
        "data.read_backup_artifact",
    )
    .await;
    assert!(extracted["bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        std::fs::read_to_string(&extract).unwrap().trim(),
        "/tmp/j10"
    );
    let dup = cli(
        home,
        install_root,
        &[
            "data",
            "read-backup-artifact",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--entry-id",
            "projects/j10-proj/.cwd",
            "--destination",
            extract.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(
        expect_failed_code("data.read_backup_artifact", &dup),
        "DESTINATION_EXISTS"
    );
    let _ = std::fs::remove_dir_all(&source);
}
