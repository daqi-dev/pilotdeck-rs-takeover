//! S5 journey F1: gateway kill -9 mid-journey
//! (`_tmp/20261119-s5/journeys-execution-plan.md` §3 F1).
//!
//! t9 已覆盖单操作面（冻结→SIGKILL→RESULT_UNKNOWN/disconnected/重连/恢复）；
//! 本文件做旅程级组合：J1 前缀推进到「审批待办 + 队列有 queued + 订阅活跃」
//! 三态同时在场（同会话三个 run：`[pd-driver:approval]` 头 run 挂起待批、
//! sleep 与普通 turn 排队），然后 `TestHome::gateway_pid()` → `SIGKILL`。
//!
//! 判据（§3 F1）：
//! - 在途写请求 `RESULT_UNKNOWN`（effect=unknown、幂等键回显）；
//! - 订阅 `disconnected`（resumeCursor=最后送达游标），重启后从该游标续订
//!   不重不漏、`run.interrupted` 可达；
//! - 再发一条命令触发 auto-start：新 gateway 进程（新 pid）承载同一 home，
//!   `api connect` 引擎自行重连且绑定同一 instanceId；
//! - 恢复终态：审批 run→`interrupted`+`GATEWAY_RESTART`（恢复映射的另一分支
//!   effect-unknown→`result_unknown` 无法从持久受理的旅程确定性构造，由 t9
//!   的写级 RESULT_UNKNOWN 面覆盖，此处登记口径）；待处理交互→`expired`，
//!   旧审批不可再答（REVISION_CONFLICT、零副作用）；会话队列
//!   pauseReason=`gateway_restart`、排队 run 原地保留；
//! - `queue resume` 后续跑至队列空（排队 run 依次跑完、items 清空）；
//! - `requests lookup` 回执逐字段：未落地的在途写 notObserved/replaySameKey，
//!   已落地受理 receiptFound/doNotReplay；run 数不因注入增长（D9：不自动重放）。
//!
//! 注入只作用于测试自有子进程（home 的 gateway pid）与测试自己的连接；HOME
//! 一律隔离临时目录（TestHome），绝不触碰 `~/.pilotdeck` 或按名字匹配系统进程。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::{
    CliOutcome, ConnectSession, QUIET_WINDOW, TestHome, await_cli, contract_code, expect_ok,
    init_home, init_home_instance, rpc_request, seed_project_session, signal_pid, submit_run,
    wait_run,
};

/// The idempotency key of the write that is in flight when the gateway dies.
const INFLIGHT_KEY: &str = "f1-inflight-1";

/// 满载预算（判据不变）：单帧等待从共享 `FRAME_TIMEOUT`（20s）放宽到 60s，
/// 饱和机器上首帧/终帧/续传帧仍会到达，只是更慢（f3 的同名先例）。
const LOADED_FRAME_TIMEOUT: Duration = Duration::from_secs(60);

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit — returns the whole `error` object so call sites pin code and effect.
fn expect_failed(method: &str, out: &CliOutcome) -> Value {
    assert_ne!(
        out.code, 0,
        "`{method}` was expected to fail, got: {}",
        out.stdout
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
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON ({e}): {}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` outcome: {value}");
    value["error"].clone()
}

fn queue_cmd(home: &Path, sub: &str, args: &[&str]) -> CliOutcome {
    let mut full = vec!["queue", sub, "--output", "json"];
    full.extend_from_slice(args);
    await_cli(home, &full)
}

async fn queue_of(home: &Path, session_id: &str) -> Value {
    let out = queue_cmd(home, "get", &["--session-id", session_id]);
    expect_ok("queue.get", &out)["queue"].clone()
}

fn queue_item_ids(queue: &Value) -> Vec<String> {
    queue["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["runId"].as_str().unwrap().to_owned())
        .collect()
}

async fn run_of(home: &Path, run_id: &str) -> Value {
    let out = await_cli(
        home,
        &["run", "get", "--output", "json", "--run-id", run_id],
    );
    expect_ok("run.get", &out)["run"].clone()
}

async fn run_state_of(home: &Path, run_id: &str) -> String {
    run_of(home, run_id).await["state"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The session's run inventory through `run list` (sorted: submission order is
/// not part of the contract) — the zero-side-effect accounting base.
async fn session_run_ids(home: &Path, project_id: &str, session_id: &str) -> Vec<String> {
    let scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
    );
    let out = await_cli(
        home,
        &["run", "list", "--output", "json", "--scope", &scope],
    );
    let listing = expect_ok("run.list", &out);
    let mut ids: Vec<String> = listing["items"]
        .as_array()
        .or_else(|| listing["runs"].as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| item["runId"].as_str().unwrap_or_default().to_owned())
        .collect();
    ids.sort();
    ids
}

/// The numeric sequence inside a `rev-%08d` revision (monotonicity basis).
fn revision_seq(revision: &str) -> u64 {
    revision
        .trim_start_matches("rev-")
        .parse()
        .unwrap_or_else(|e| panic!("unparseable queue revision `{revision}`: {e}"))
}

fn lookup_request(home: &Path, key: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "run.submit",
            "--idempotency-key",
            key,
        ],
    );
    expect_ok("requests.lookup", &out)
}

/// The pending approval interaction opened by `run_id` (poll: the worker
/// creates it shortly after the run starts).
async fn pending_approval(home: &Path, run_id: &str) -> (String, String) {
    for _ in 0..100 {
        let out = await_cli(
            home,
            &[
                "interaction",
                "list",
                "--output",
                "json",
                "--state",
                "pending",
            ],
        );
        let listed = expect_ok("interaction.list", &out);
        if let Some(item) = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["runId"].as_str() == Some(run_id))
        {
            return (
                item["interactionId"].as_str().unwrap().to_owned(),
                item["revision"].as_str().unwrap().to_owned(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("run `{run_id}` never opened its approval interaction");
}

/// F1: with the journey parked at「审批待办 + 队列有 queued + 订阅活跃」,
/// SIGKILLing the gateway must answer the in-flight write RESULT_UNKNOWN, end
/// the subscription `disconnected`, and — after a fresh command auto-starts a
/// new gateway process — recover on disk: the waiting run `interrupted`
/// (GATEWAY_RESTART), its pending interaction `expired` (and unanswerable),
/// the session queue paused with `gateway_restart`; `queue resume` then drains
/// the queued runs to completion, and `requests lookup` accounts for both the
/// never-observed write and the durable acceptance, field by field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f1_gateway_sigkill_mid_journey_recovery() {
    let home = TestHome::new("f1-kill9");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "f1k").await;

    // ------------------------------------------------------------------
    // Journey prefix (the J1 slice F1 rides on): the head run waits on an
    // approval, two more runs queue behind it, a live subscription watches
    // the head run's event stream.
    // ------------------------------------------------------------------
    let approval = submit_run(root, &session_id, "[pd-driver:approval]", "f1-approval-1").await;
    let approval_id = approval["runId"].as_str().unwrap().to_owned();
    let sleep_submit = submit_run(root, &session_id, "[pd-driver:sleep=3000]", "f1-sleep-1").await;
    let sleep_id = sleep_submit["runId"].as_str().unwrap().to_owned();
    let tail_submit = submit_run(root, &session_id, "f1 tail turn", "f1-tail-1").await;
    let tail_id = tail_submit["runId"].as_str().unwrap().to_owned();

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;
    let open = conn
        .sub_open("run.events", json!({"runId": approval_id}), 64)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // The subscription carries the approval request: the pending state and
    // the live subscription exist simultaneously.
    let requested = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.event")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
                    && v["params"]["event"]["type"].as_str() == Some("interaction.requested")
            },
            LOADED_FRAME_TIMEOUT,
            "interaction.requested before the kill",
        )
        .await;
    let mut last_cursor = requested["params"]["event"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    // Settle: the newest delivered cursor is what the disconnect terminal
    // reports. Everything drained here is journaled by the reader, so the
    // end-of-scenario accounting still sees it.
    while let Ok(Some(frame)) = tokio::time::timeout(QUIET_WINDOW, conn.rx.recv()).await {
        let Ok(value) = serde_json::from_str::<Value>(&frame) else {
            continue;
        };
        if value.get("method").and_then(Value::as_str) == Some("sub.event")
            && value["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
        {
            last_cursor = value["params"]["event"]["cursor"]
                .as_str()
                .unwrap()
                .to_owned();
        }
    }

    let (interaction_id, interaction_revision) = pending_approval(root, &approval_id).await;
    assert_eq!(
        run_state_of(root, &approval_id).await,
        "waiting",
        "the head run must be parked on the approval"
    );

    let queue = queue_of(root, &session_id).await;
    assert_eq!(queue["paused"], false, "{queue}");
    assert_eq!(queue["activeRunId"], json!(approval_id), "{queue}");
    assert_eq!(
        queue_item_ids(&queue),
        vec![approval_id.clone(), sleep_id.clone(), tail_id.clone()],
        "the journey's three runs must be on the session queue: {queue}"
    );
    let revision_pre_kill = queue["revision"].as_str().unwrap().to_owned();

    // ------------------------------------------------------------------
    // Injection (t9's deterministic shape): freeze the gateway so the write
    // below is in flight, then SIGKILL. Only this test's own gateway child is
    // ever signalled.
    // ------------------------------------------------------------------
    let pid_before = home.gateway_pid().expect("gateway pid");
    signal_pid(pid_before, rustix::process::Signal::STOP);

    let write_id = conn.fresh_id("w");
    conn.send(&rpc_request(
        &write_id,
        "run.submit",
        json!({
            "input": {
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "in flight when the gateway dies"}]
            },
            "idempotencyKey": INFLIGHT_KEY
        }),
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    signal_pid(pid_before, rustix::process::Signal::KILL);

    // The in-flight write resolves RESULT_UNKNOWN with the key echoed.
    let write_resp = conn
        .wait_response(&write_id, "RESULT_UNKNOWN for the in-flight write")
        .await;
    assert_eq!(contract_code(&write_resp), "RESULT_UNKNOWN", "{write_resp}");
    assert_eq!(
        write_resp["error"]["data"]["effect"], "unknown",
        "{write_resp}"
    );
    assert_eq!(
        write_resp["error"]["data"]["details"]["idempotencyKey"], INFLIGHT_KEY,
        "{write_resp}"
    );

    // The subscription ends `disconnected` with the last delivered cursor.
    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            LOADED_FRAME_TIMEOUT,
            "disconnected terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "disconnected", "{end}");
    assert_eq!(
        end["params"]["resumeCursor"].as_str(),
        Some(last_cursor.as_str()),
        "{end}"
    );

    // ------------------------------------------------------------------
    // Recovery: a fresh command (queue get) triggers the auto-start — a new
    // gateway process now serves the same home. The api connect engine
    // reconnects on its own and must bind the same instanceId.
    // ------------------------------------------------------------------
    let queue = queue_of(root, &session_id).await;
    let pid_after = home.gateway_pid().expect("revived gateway pid");
    assert_ne!(
        pid_after, pid_before,
        "a new gateway process must serve the home after the kill"
    );
    let reconnected = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("conn.state")
                    && v["params"]["state"] == "connected"
            },
            // 满载预算（判据不变）：等重启后网关重连成功的单帧，30s→300s。
            Duration::from_secs(300),
            "conn.state connected after the restart",
        )
        .await;
    assert_eq!(
        reconnected["params"]["instanceId"],
        init_home_instance(root),
        "reconnect must bind the same instance: {reconnected}"
    );

    // Crash recovery on disk: waiting run -> interrupted, queue paused with
    // gateway_restart, queued runs stay queued, the interrupted active run
    // leaves the queue.
    assert_eq!(queue["paused"], true, "{queue}");
    assert_eq!(queue["pauseReason"], "gateway_restart", "{queue}");
    assert_eq!(queue["activeRunId"], Value::Null, "{queue}");
    assert_eq!(
        queue_item_ids(&queue),
        vec![sleep_id.clone(), tail_id.clone()],
        "queued runs must survive the restart in place: {queue}"
    );
    let revision_recovered = queue["revision"].as_str().unwrap().to_owned();
    assert!(
        revision_seq(&revision_recovered) > revision_seq(&revision_pre_kill),
        "recovery must bump the queue revision: {revision_pre_kill} -> {revision_recovered}"
    );

    let approval_run = run_of(root, &approval_id).await;
    assert_eq!(approval_run["state"], "interrupted", "{approval_run}");
    assert_eq!(approval_run["terminal"], true, "{approval_run}");
    assert_eq!(
        approval_run["outcomeCode"], "GATEWAY_RESTART",
        "{approval_run}"
    );
    assert_eq!(run_state_of(root, &sleep_id).await, "queued");
    assert_eq!(run_state_of(root, &tail_id).await, "queued");

    // Pending interactions from crashed runs expire (007 §7.4): the approval
    // is expired on disk, no longer listed as pending, and the pre-kill
    // revision can no longer answer it.
    let iget = expect_ok(
        "interaction.get",
        &await_cli(
            root,
            &[
                "interaction",
                "get",
                "--output",
                "json",
                "--interaction-id",
                &interaction_id,
            ],
        ),
    );
    assert_eq!(iget["interaction"]["state"], "expired", "{iget}");
    assert_eq!(iget["interaction"]["resolution"], "expired", "{iget}");
    let expired_list = expect_ok(
        "interaction.list",
        &await_cli(
            root,
            &[
                "interaction",
                "list",
                "--output",
                "json",
                "--state",
                "expired",
            ],
        ),
    );
    let expired_ids: Vec<&str> = expired_list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["interactionId"].as_str().unwrap())
        .collect();
    assert!(
        expired_ids.contains(&interaction_id.as_str()),
        "the expired approval must be listed as expired: {expired_list}"
    );
    let pending_list = expect_ok(
        "interaction.list",
        &await_cli(
            root,
            &[
                "interaction",
                "list",
                "--output",
                "json",
                "--state",
                "pending",
            ],
        ),
    );
    assert!(
        pending_list["items"].as_array().unwrap().is_empty(),
        "no pending interaction may survive the restart: {pending_list}"
    );
    let refused = await_cli(
        root,
        &[
            "approval",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &interaction_revision,
            "--decision",
            "allow",
        ],
    );
    let refused_error = expect_failed("approval.respond", &refused);
    assert_eq!(
        refused_error["code"], "REVISION_CONFLICT",
        "the expired approval must refuse the stale answer: {refused_error}"
    );
    assert_eq!(refused_error["effect"], "not_applied", "{refused_error}");

    // requests.lookup, field by field. The in-flight write never landed (the
    // frozen gateway never read it), so its receipt is the notObserved shape.
    let inflight = lookup_request(root, INFLIGHT_KEY);
    assert_eq!(inflight["requestState"], "result_unknown", "{inflight}");
    assert_eq!(inflight["reason"], "notObserved", "{inflight}");
    assert_eq!(inflight["resource"], Value::Null, "{inflight}");
    assert_eq!(inflight["inputDigest"], Value::Null, "{inflight}");
    assert_eq!(inflight["retainedUntil"], Value::Null, "{inflight}");
    assert_eq!(inflight["retryDisposition"], "replaySameKey", "{inflight}");
    assert_eq!(inflight["rejectionCode"], Value::Null, "{inflight}");
    assert!(
        inflight["observedAt"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "{inflight}"
    );

    // The durable acceptance, on the other hand, replays its receipt.
    let approval_lookup = lookup_request(root, "f1-approval-1");
    assert!(
        matches!(
            approval_lookup["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "unexpected requestState: {approval_lookup}"
    );
    assert_eq!(
        approval_lookup["resource"]["runId"].as_str(),
        Some(approval_id.as_str()),
        "{approval_lookup}"
    );
    assert_eq!(
        approval_lookup["reason"], "receiptFound",
        "{approval_lookup}"
    );
    assert_eq!(
        approval_lookup["retryDisposition"], "doNotReplay",
        "{approval_lookup}"
    );
    assert!(
        approval_lookup["inputDigest"]
            .as_str()
            .is_some_and(|d| !d.is_empty()),
        "{approval_lookup}"
    );

    // No run was created by the in-flight write, and nothing was replayed on
    // the engine's reconnect (D9): the journey's run inventory is unchanged.
    let mut expected_ids = vec![approval_id.clone(), sleep_id.clone(), tail_id.clone()];
    expected_ids.sort();
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await,
        expected_ids,
        "the kill must not add (or replay) any run"
    );

    // Resubscribe from the disconnect cursor: no duplicates, nothing missing,
    // and the interrupted terminal event is reachable.
    let open2 = conn
        .sub_open(
            "run.events",
            json!({"runId": approval_id, "afterCursor": last_cursor}),
            64,
        )
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut tail_events: Vec<(String, String)> = Vec::new(); // (cursor, event type)
    let end2 = loop {
        let frame = conn
            .wait_for(
                |v| {
                    v["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str())
                        && matches!(
                            v.get("method").and_then(Value::as_str),
                            Some("sub.event") | Some("sub.end")
                        )
                },
                LOADED_FRAME_TIMEOUT,
                "resubscription tail after the restart",
            )
            .await;
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") => tail_events.push((
                frame["params"]["event"]["cursor"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                frame["params"]["event"]["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )),
            _ => break frame,
        }
    };
    assert_eq!(end2["params"]["terminal"], "completed", "{end2}");
    assert!(
        tail_events.iter().all(|(c, _)| c != &last_cursor),
        "the resumed stream must not repeat the last delivered cursor: {tail_events:?}"
    );
    assert!(
        tail_events.iter().any(|(_, ty)| ty == "run.interrupted"),
        "the interrupted terminal event must be reachable from the resume cursor: {tail_events:?}"
    );

    // queue.resume acknowledges gateway_restart and the queue drains to
    // empty: the queued runs run through, one after the other.
    let out = queue_cmd(
        root,
        "resume",
        &[
            "--session-id",
            &session_id,
            "--expected-revision",
            &revision_recovered,
            "--acknowledged-pause-reason",
            "gateway_restart",
        ],
    );
    let resumed = expect_ok("queue.resume", &out);
    assert_eq!(resumed["queue"]["paused"], false, "{resumed}");
    let revision_resumed = resumed["queue"]["revision"].as_str().unwrap().to_owned();
    assert!(
        revision_seq(&revision_resumed) > revision_seq(&revision_recovered),
        "resume must advance the queue revision: {revision_recovered} -> {revision_resumed}"
    );

    let slept = expect_ok("run.wait", &wait_run(root, &sleep_id));
    assert_eq!(slept["conditionMet"], true, "{slept}");
    assert_eq!(slept["run"]["state"], "succeeded", "{slept}");
    let tailed = expect_ok("run.wait", &wait_run(root, &tail_id));
    assert_eq!(tailed["conditionMet"], true, "{tailed}");
    assert_eq!(tailed["run"]["state"], "succeeded", "{tailed}");

    let drained = queue_of(root, &session_id).await;
    assert_eq!(drained["paused"], false, "{drained}");
    assert_eq!(drained["activeRunId"], Value::Null, "{drained}");
    assert!(
        queue_item_ids(&drained).is_empty(),
        "the queue must drain to empty: {drained}"
    );

    // Terminal-state consistency for every run of the journey.
    let approval_run = run_of(root, &approval_id).await;
    assert_eq!(approval_run["state"], "interrupted", "{approval_run}");
    assert_eq!(
        approval_run["outcomeCode"], "GATEWAY_RESTART",
        "{approval_run}"
    );
    assert_eq!(run_state_of(root, &sleep_id).await, "succeeded");
    assert_eq!(run_state_of(root, &tail_id).await, "succeeded");

    // The recovered queue machine lives on disk, not in the dead process.
    assert!(
        root.join("v1").join("state.sqlite").is_file(),
        "the gateway state store must exist on disk"
    );

    conn.drain_channel_to_journal();
    conn.assert_subscription_accounting();
    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after F1").await;
}
