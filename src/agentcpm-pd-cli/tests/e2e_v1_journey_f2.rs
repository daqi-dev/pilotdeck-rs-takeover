//! S5 journey F2: CLI 崩溃重启（宿主模拟）
//! (`_tmp/20261119-s5/journeys-execution-plan.md` §3 F2)。
//!
//! a4 已覆盖单操作面（CLI 死于写入途中 → `requests.lookup` → 同键重放）；
//! 本文件做旅程化收口：`api connect`（ConnectSession）持有未决 `run.submit`
//! 时 SIGKILL CLI 子进程，测试器重启新 `api connect`（新 connectionId），
//! 先 `requests.lookup {methodId, idempotencyKey}` 确认（A4），再同幂等键
//! 重放——回放原回执且 `deduplicated=true`，run 数恰 1；业务 run 不随 CLI
//! 死亡（属 gateway），恢复后照常跑完。
//!
//! 确定性口径：a4 容忍「写落/未落」两分支；F2 用独立观察者会话轮询
//! `requests.lookup` 直到受理落盘（requestState 离开 result_unknown）才注入，
//! 把崩溃钉在「写已落地、回执未观察」分支上。被杀会话 stdout 里可能已缓冲
//! 的原回执只在事后取证读取（绝不喂给恢复流程；帧随进程死亡丢失是合法竞
//! 态，取证缺席时跳过该补充比对）。「重放 == 原回执」的逐字段合同由 F5
//! （持有可见原始回执）钉死；F2 以跨通道（api connect 会话 vs 全新 argv
//! 进程）回执逐字段一致 + `deduplicated=true` + run 恰 1 承接崩溃恢复面。
//!
//! 平台：本文件含本地（单 home）与远程（J8 同机「模拟远程」双 home 拓扑，
//! `remote_pair`）两个变体；跨主机轮按计划走 W4 的 `#[ignore]` + 独立 shell
//! 驱动，不在本文件。
//!
//! 注入只作用于测试自有的 CLI 子进程（ConnectSession 的 child pid）；HOME
//! 一律隔离临时目录，绝不触碰 `~/.pilotdeck` 或按名字匹配系统进程。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::{
    ConnectSession, TestHome, await_cli, expect_ok, init_home, remote_pair, rpc_request,
    seed_project_session, seed_remote_project_session, signal_pid, submit_run, wait_run,
};

/// The receipt with the `deduplicated` bookkeeping flag stripped, so the
/// original and a replay can be compared field-for-field.
fn receipt_without_dedup(receipt: &Value) -> Value {
    let mut stripped = receipt.clone();
    if let Some(obj) = stripped.as_object_mut() {
        obj.remove("deduplicated");
    }
    stripped
}

/// The session's run inventory through `run list` (sorted: submission order is
/// not part of the contract).
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

/// Polls `requests.lookup {methodId: "run.submit", idempotencyKey}` through
/// the observer session until the write is durably observed (requestState
/// leaves `result_unknown`) — the deterministic landing oracle for the crash.
/// Returns the landed lookup receipt.
async fn await_landed_acceptance(probe: &mut ConnectSession, key: &str) -> Value {
    for _ in 0..150 {
        let lookup = probe
            .call(
                "requests.lookup",
                json!({"methodId": "run.submit", "idempotencyKey": key}),
                None,
            )
            .await;
        let result = lookup["result"]["result"].clone();
        if result["requestState"].as_str() != Some("result_unknown") {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "the crashed session's `run.submit` (key `{key}`) never landed; its stderr:\n{}",
        probe.stderr_log.lock().unwrap()
    );
}

/// F2, local variant: the CLI process dies holding an unresolved `run.submit`
/// whose write already landed. The recovery protocol — a NEW `api connect`
/// (new server-assigned connectionId), `requests.lookup` to confirm, then the
/// same-key replay — must reconstruct the original acceptance receipt
/// (`deduplicated=true`), never create a second run, and the orphaned business
/// run must still complete on the gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_cli_crash_recovered_by_new_session_lookup_and_same_key_replay() {
    let home = TestHome::new("f2-crash");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "f2c").await;

    let key = "f2-crash-1";
    let text = "f2 crash recovery run";

    // The session that will crash: it sends the write and never resolves it.
    let mut crashed = ConnectSession::start(root).await;
    let init_a = crashed.initialize().await;
    let connection_a = init_a["result"]["connectionId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !connection_a.is_empty(),
        "server-assigned connectionId present: {init_a}"
    );

    let submit_id = crashed.fresh_id("w");
    crashed
        .send(&rpc_request(
            &submit_id,
            "run.submit",
            json!({
                "input": {
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": text}]
                },
                "idempotencyKey": key
            }),
        ))
        .await;

    // Deterministic landing: an independent observer session confirms the
    // acceptance is durable while the crashing session still holds the
    // unresolved request.
    let mut probe = ConnectSession::start(root).await;
    probe.initialize().await;
    let landed = await_landed_acceptance(&mut probe, key).await;
    assert!(
        matches!(
            landed["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "the observer must see the acceptance, not a rejection: {landed}"
    );
    probe.close_stdin().await;
    probe.wait_exit(0, "observer session exits cleanly").await;

    // Injection: SIGKILL the CLI process (only this test's own child).
    let cli_pid = crashed.child.id().expect("cli pid");
    signal_pid(cli_pid, rustix::process::Signal::KILL);
    let _ = crashed.child.wait().await;

    // Post-mortem forensics ONLY (the recovery below never sees this): the
    // reader task may have pulled the acceptance receipt off the dead CLI's
    // pipe before the kill; if so, it is the original receipt and can be
    // compared against the replay. The frame dying with the pipe is a legal
    // race — the bonus comparison is simply skipped then.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut forensic_receipt: Option<Value> = None;
    for _ in 0..4 {
        match tokio::time::timeout(Duration::from_millis(500), crashed.rx.recv()).await {
            Ok(Some(line)) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && value.get("id").and_then(Value::as_str) == Some(submit_id.as_str())
                    && value.get("result").is_some()
                {
                    forensic_receipt = Some(value["result"]["result"].clone());
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Recovery step 0: the tester restarts `api connect` — a NEW connection
    // with its own server-assigned connectionId.
    let mut recovered = ConnectSession::start(root).await;
    let init_b = recovered.initialize().await;
    let connection_b = init_b["result"]["connectionId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !connection_b.is_empty(),
        "server-assigned connectionId present: {init_b}"
    );
    assert_ne!(
        connection_b, connection_a,
        "the restarted session must be a new connection"
    );

    // Recovery step 1 (A4): look the write up before doing anything else.
    let lookup = recovered
        .call(
            "requests.lookup",
            json!({"methodId": "run.submit", "idempotencyKey": key}),
            None,
        )
        .await;
    let lookup_result = lookup["result"]["result"].clone();
    assert!(
        matches!(
            lookup_result["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "the landed write must resolve through lookup: {lookup_result}"
    );
    assert_eq!(lookup_result["reason"], "receiptFound", "{lookup_result}");
    assert_eq!(
        lookup_result["retryDisposition"], "doNotReplay",
        "{lookup_result}"
    );
    let run_id = lookup_result["resource"]["runId"]
        .as_str()
        .expect("resource runId")
        .to_owned();

    // Recovery step 2 (A4): re-issue with the SAME key and the SAME input —
    // the stored acceptance receipt replays, `deduplicated=true`.
    let replay = recovered
        .call(
            "run.submit",
            json!({
                "sessionId": session_id,
                "input": [{"kind": "text", "text": text}]
            }),
            Some(key),
        )
        .await;
    let replay_receipt = replay["result"]["result"].clone();
    assert_eq!(
        replay_receipt["deduplicated"], true,
        "the same-key replay must deduplicate: {replay}"
    );
    assert_eq!(
        replay_receipt["runId"].as_str(),
        Some(run_id.as_str()),
        "{replay_receipt}"
    );
    assert_eq!(replay_receipt["kind"], "run", "{replay_receipt}");
    assert_eq!(replay_receipt["durable"], true, "{replay_receipt}");
    // The replayed receipt is the ACCEPTANCE-time receipt, not the run's
    // current state.
    assert_eq!(replay_receipt["state"], "queued", "{replay_receipt}");
    assert!(
        replay_receipt["acceptedRevision"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "{replay_receipt}"
    );

    // The stored receipt is stable across transports: a brand-new argv
    // process replaying the same key gets the identical receipt.
    let argv_replay = submit_run(root, &session_id, text, key).await;
    assert_eq!(argv_replay["deduplicated"], true, "{argv_replay}");
    assert_eq!(
        receipt_without_dedup(&argv_replay),
        receipt_without_dedup(&replay_receipt),
        "the replayed receipt must be the stored acceptance, field for field"
    );

    // Post-mortem cross-check (bonus, race-tolerant): if the dead session's
    // buffered original response survived, it must equal the replay.
    if let Some(original) = forensic_receipt {
        assert_eq!(
            receipt_without_dedup(&original),
            receipt_without_dedup(&replay_receipt),
            "the replay must be the receipt the crashed session never delivered"
        );
    }

    // Run count is exactly one — the crash and every replay created nothing.
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await,
        vec![run_id.clone()],
        "exactly one run across the crash, lookup, and replays"
    );

    // The orphaned business run belongs to the gateway, not the dead CLI: it
    // still runs to completion.
    let waited = expect_ok("run.wait", &wait_run(root, &run_id));
    assert_eq!(waited["conditionMet"], true, "{waited}");
    assert_eq!(waited["run"]["state"], "succeeded", "{waited}");

    recovered.close_stdin().await;
    recovered
        .wait_exit(0, "recovery session exits cleanly")
        .await;
}

/// F2, remote variant (the J8 same-machine "simulated remote" topology): the
/// same crash-recovery protocol over a real ws:// hop — home A's `api connect`
/// (selected target profile) holds the unresolved `run.submit` against home
/// B's explicit `server serve` child, the CLI dies, and a NEW session on home
/// A recovers via lookup + same-key replay. The server-side oracle (home B's
/// own local view) pins run count exactly one and the run's completion; home
/// A must never fall back to starting a local gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_cli_crash_recovery_over_remote_target() {
    let pair = remote_pair("f2r");
    let home_a = pair.home_a.path();
    let home_b = pair.home_b.path();
    let instance_b = pair.instance_id_b();

    // Select the profile on home A so plain `api connect` (and argv commands)
    // resolve the remote target — the F6 precedent.
    let got = expect_ok(
        "target.get",
        &await_cli(
            home_a,
            &[
                "target",
                "get",
                "--output",
                "json",
                "--target-id",
                &pair.target_id,
            ],
        ),
    );
    let revision = got["target"]["revision"].as_str().unwrap().to_owned();
    expect_ok(
        "target.select",
        &await_cli(
            home_a,
            &[
                "target",
                "select",
                "--output",
                "json",
                "--target-id",
                &pair.target_id,
                "--expected-revision",
                &revision,
            ],
        ),
    );

    let (project_id, session_id) =
        seed_remote_project_session(home_a, &pair.target_id, &instance_b, "f2r").await;

    let key = "f2r-crash-1";
    let text = "f2 remote crash recovery run";

    // The session that will crash: unresolved `run.submit` over the ws hop.
    let mut crashed = ConnectSession::start(home_a).await;
    let init_a = crashed.initialize().await;
    let connection_a = init_a["result"]["connectionId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !connection_a.is_empty(),
        "server-assigned connectionId present: {init_a}"
    );

    let submit_id = crashed.fresh_id("w");
    crashed
        .send(&rpc_request(
            &submit_id,
            "run.submit",
            json!({
                "input": {
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": text}]
                },
                "idempotencyKey": key
            }),
        ))
        .await;

    // Deterministic landing through an independent observer session on home A
    // (also resolving the remote target).
    let mut probe = ConnectSession::start(home_a).await;
    probe.initialize().await;
    let landed = await_landed_acceptance(&mut probe, key).await;
    assert!(
        matches!(
            landed["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "the observer must see the acceptance, not a rejection: {landed}"
    );
    probe.close_stdin().await;
    probe.wait_exit(0, "observer session exits cleanly").await;

    // Injection: SIGKILL the CLI process (only this test's own child). The
    // serving gateway on home B is a different process and must not notice
    // beyond the dropped connection.
    let cli_pid = crashed.child.id().expect("cli pid");
    signal_pid(cli_pid, rustix::process::Signal::KILL);
    let _ = crashed.child.wait().await;

    // Recovery: a new `api connect` on home A — new connectionId, same target.
    let mut recovered = ConnectSession::start(home_a).await;
    let init_b = recovered.initialize().await;
    let connection_b = init_b["result"]["connectionId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !connection_b.is_empty(),
        "server-assigned connectionId present: {init_b}"
    );
    assert_ne!(
        connection_b, connection_a,
        "the restarted session must be a new connection"
    );

    // A4 confirm, then the same-key replay — over the same ws hop.
    let lookup = recovered
        .call(
            "requests.lookup",
            json!({"methodId": "run.submit", "idempotencyKey": key}),
            None,
        )
        .await;
    let lookup_result = lookup["result"]["result"].clone();
    assert!(
        matches!(
            lookup_result["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "the landed write must resolve through lookup: {lookup_result}"
    );
    let run_id = lookup_result["resource"]["runId"]
        .as_str()
        .expect("resource runId")
        .to_owned();

    let replay = recovered
        .call(
            "run.submit",
            json!({
                "sessionId": session_id,
                "input": [{"kind": "text", "text": text}]
            }),
            Some(key),
        )
        .await;
    let replay_receipt = replay["result"]["result"].clone();
    assert_eq!(
        replay_receipt["deduplicated"], true,
        "the same-key replay must deduplicate: {replay}"
    );
    assert_eq!(
        replay_receipt["runId"].as_str(),
        Some(run_id.as_str()),
        "{replay_receipt}"
    );
    assert_eq!(replay_receipt["kind"], "run", "{replay_receipt}");
    assert_eq!(replay_receipt["durable"], true, "{replay_receipt}");
    assert_eq!(replay_receipt["state"], "queued", "{replay_receipt}");

    recovered.close_stdin().await;
    recovered
        .wait_exit(0, "recovery session exits cleanly")
        .await;

    // Server-side oracle (home B's own local view): run count exactly one,
    // and the orphaned run completed on the server despite the client crash.
    let scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
    );
    let listing = expect_ok(
        "run.list",
        &await_cli(
            home_b,
            &[
                "run", "list", "--output", "json", "--target", "local", "--scope", &scope,
            ],
        ),
    );
    let mut server_ids: Vec<String> = listing["items"]
        .as_array()
        .or_else(|| listing["runs"].as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| item["runId"].as_str().unwrap_or_default().to_owned())
        .collect();
    server_ids.sort();
    assert_eq!(
        server_ids,
        vec![run_id.clone()],
        "the server must hold exactly one run across the crash and replay: {listing}"
    );
    let waited = expect_ok(
        "run.wait",
        &await_cli(
            home_b,
            &[
                "run",
                "wait",
                "--output",
                "json",
                "--target",
                "local",
                "--run-id",
                &run_id,
                "--timeout-ms",
                "60000",
            ],
        ),
    );
    assert_eq!(waited["conditionMet"], true, "{waited}");
    assert_eq!(waited["run"]["state"], "succeeded", "{waited}");

    // Home A never fell back to a local gateway of its own (D8).
    assert_eq!(
        pair.home_a.gateway_pid(),
        None,
        "no gateway was ever auto-started in the client home"
    );
    assert!(!pair.home_a.path().join("local.json").exists());
}
