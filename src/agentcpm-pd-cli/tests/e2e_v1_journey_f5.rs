//! S5 journey F5: the idempotent replay matrix around `run.submit`
//! (`_tmp/20261119-s5/journeys-execution-plan.md` §3 F5).
//!
//! a4/t1 已有单操作面;本文件做旅程化收口:同一 `--request-key` 的重放矩阵——
//! (1) 同键同输入:新 CLI 进程与 `api connect` 会话两条通道都回放原回执,
//!     `deduplicated=true` 且除该标志外逐字段一致,run 终态后重放同样成立;
//! (2) 同键不同输入:argv 与 `api connect` 两条通道都在副作用前报
//!     `IDEMPOTENCY_CONFLICT`(effect=not_applied),原始受理记录不被覆盖;
//! (3) `requests.lookup` 逐字段核对应答;全程 run 数恰 1。
//!
//! 注入方式 = 同键重放本身,只作用于测试自有会话,不触碰任何系统资源。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;

use serde_json::{Value, json};

use common_v1::{
    CliOutcome, ConnectSession, TestHome, await_cli, contract_code, expect_ok, init_home,
    seed_project_session, submit_run, wait_run,
};

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code (same shape as the queue-admin suite).
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

/// The receipt with the `deduplicated` bookkeeping flag stripped, so the
/// original and a replay can be compared field-for-field.
fn receipt_without_dedup(receipt: &Value) -> Value {
    let mut stripped = receipt.clone();
    if let Some(obj) = stripped.as_object_mut() {
        obj.remove("deduplicated");
    }
    stripped
}

/// Every run id in the session scope, through the paged-unary `run list`.
async fn session_run_ids(home: &Path, project_id: &str, session_id: &str) -> Vec<String> {
    let scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
    );
    let out = await_cli(
        home,
        &["run", "list", "--output", "json", "--scope", &scope],
    );
    let listing = expect_ok("run.list", &out);
    listing["items"]
        .as_array()
        .or_else(|| listing["runs"].as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| item["runId"].as_str().unwrap_or_default().to_owned())
        .collect()
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

/// F5 matrix cell (1): same key + same input replays the original receipt on
/// every transport — a brand-new argv process before and after the run reaches
/// its terminal state, and an `api connect` session — with `deduplicated=true`
/// and an otherwise field-identical receipt. The run count stays exactly one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f5_same_key_same_input_replays_the_original_receipt() {
    let home = TestHome::new("f5-replay");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "f5r").await;

    let key = "f5-replay-1";
    let text = "f5 replay matrix run";

    // Acceptance: the first submit is fresh, not deduplicated.
    let first = submit_run(root, &session_id, text, key).await;
    assert_eq!(first["kind"], "run", "{first}");
    assert_eq!(first["deduplicated"], false, "{first}");
    assert_eq!(first["durable"], true, "{first}");
    assert_eq!(first["state"], "queued", "{first}");
    assert!(
        first["acceptedRevision"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "{first}"
    );
    let run_id = first["runId"].as_str().unwrap().to_owned();
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await,
        vec![run_id.clone()],
        "exactly one run after the original acceptance"
    );

    // Replay 1: a brand-new CLI process (the F2 recovery shape), same key and
    // same input.
    let replay = submit_run(root, &session_id, text, key).await;
    assert_eq!(replay["deduplicated"], true, "same key must deduplicate");
    assert_eq!(replay["runId"].as_str(), Some(run_id.as_str()));
    assert_eq!(
        receipt_without_dedup(&replay),
        receipt_without_dedup(&first),
        "the replay must be the original receipt, field for field"
    );

    // Replay 2: after the run reached its terminal state, the stored
    // acceptance receipt is still what gets replayed.
    let waited = wait_run(root, &run_id);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded", "{waited}");
    let replay_after = submit_run(root, &session_id, text, key).await;
    assert_eq!(replay_after["deduplicated"], true, "{replay_after}");
    assert_eq!(
        receipt_without_dedup(&replay_after),
        receipt_without_dedup(&first),
        "post-terminal replay must still be the original receipt"
    );

    // Replay 3: through `api connect` (the other transport) — the engine binds
    // the same instance and forwards the same input, so the digest matches
    // and the gateway answers with the stored receipt.
    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;
    let via_session = conn
        .call(
            "run.submit",
            json!({
                "sessionId": session_id,
                "input": [{"kind": "text", "text": text}]
            }),
            Some(key),
        )
        .await;
    let session_receipt = via_session["result"]["result"].clone();
    assert_eq!(
        session_receipt["deduplicated"], true,
        "in-session replay: {via_session}"
    );
    assert_eq!(
        receipt_without_dedup(&session_receipt),
        receipt_without_dedup(&first),
        "in-session replay must be the original receipt"
    );
    conn.close_stdin().await;
    conn.wait_exit(0, "api connect exits cleanly").await;

    // `requests.lookup` resolves the persisted acceptance for the key.
    let lookup = lookup_request(root, key);
    assert!(
        matches!(
            lookup["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "unexpected requestState: {lookup}"
    );
    assert_eq!(
        lookup["resource"]["runId"].as_str(),
        Some(run_id.as_str()),
        "{lookup}"
    );
    assert_eq!(lookup["reason"], "receiptFound", "{lookup}");
    assert_eq!(lookup["retryDisposition"], "doNotReplay", "{lookup}");
    assert!(
        lookup["inputDigest"]
            .as_str()
            .is_some_and(|d| !d.is_empty()),
        "{lookup}"
    );

    // Run count is still exactly one after every replay.
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await,
        vec![run_id],
        "replays must never create a second run"
    );
}

/// F5 matrix cell (2): the same key with different input is refused with
/// `IDEMPOTENCY_CONFLICT` before any side effect, on both transports; the
/// original acceptance record survives the conflicts untouched and still
/// replays; the run count stays exactly one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f5_same_key_different_input_conflicts_before_side_effects() {
    let home = TestHome::new("f5-conflict");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "f5c").await;

    let key = "f5-conflict-1";
    let original = submit_run(root, &session_id, "original input", key).await;
    let run_id = original["runId"].as_str().unwrap().to_owned();

    // argv: same key, different text -> IDEMPOTENCY_CONFLICT before side
    // effects.
    let conflicted = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--request-key",
            key,
            "--text",
            "a different input",
        ],
    );
    assert_eq!(
        expect_fail("run.submit", &conflicted),
        "IDEMPOTENCY_CONFLICT",
        "same key with different input must conflict"
    );

    // `api connect`: same key, another different text -> the same contract
    // code on the other transport, with no effect applied.
    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;
    let rejected = conn
        .call(
            "run.submit",
            json!({
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "yet another different input"}]
            }),
            Some(key),
        )
        .await;
    assert_eq!(
        contract_code(&rejected),
        "IDEMPOTENCY_CONFLICT",
        "in-session conflict: {rejected}"
    );
    assert_eq!(
        rejected["error"]["data"]["effect"], "not_applied",
        "{rejected}"
    );
    conn.close_stdin().await;
    conn.wait_exit(0, "api connect exits cleanly").await;

    // Side-effect check: run 恰 1 (the original), and the idempotency record
    // still carries the original acceptance — the conflicts overwrote
    // nothing.
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await,
        vec![run_id.clone()],
        "the conflicting attempts must not create a run"
    );
    let lookup = lookup_request(root, key);
    assert_eq!(lookup["requestState"], "accepted", "{lookup}");
    assert_eq!(
        lookup["resource"]["runId"].as_str(),
        Some(run_id.as_str()),
        "{lookup}"
    );
    assert!(lookup["rejectionCode"].is_null(), "{lookup}");

    // The original acceptance still replays after the conflicts.
    let still = submit_run(root, &session_id, "original input", key).await;
    assert_eq!(still["deduplicated"], true, "{still}");
    assert_eq!(still["runId"].as_str(), Some(run_id.as_str()), "{still}");
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await,
        vec![run_id],
        "run count must stay exactly one"
    );
}
