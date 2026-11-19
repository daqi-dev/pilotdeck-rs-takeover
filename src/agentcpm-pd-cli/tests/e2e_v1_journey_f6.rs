//! S5 journey F6: the server's instanceId changes underneath a live remote
//! connection (`_tmp/20261119-s5/journeys-execution-plan.md` §3 F6).
//!
//! t10 已有单操作面(本机身份漂移);本文件做旅程化复核,拓扑取 J8 同机
//! 「模拟远程」形状:home B 经 `server serve` 显式承载(端口由 B 侧已选
//! target 档案钉死),home A 通过 target 档案(expectedInstanceId 钉死)
//! 经真实 ws:// 一跳打到 B。旅程前缀先远程建 project/session/run 证通路。
//!
//! 注入(t10 先例的服务端版):停 serve → B 的 local.json 换 instanceId、
//! 清掉绑定旧身份的 state.sqlite(local.token 原样保留,握手仍可通过
//! 鉴权)→ 同端口重启 serve。判据:重连握手后 `conn.state failed`
//! (reason=instance_changed)且引擎退出 1(拒绝继续、不静默切换目标);
//! 新 CLI 进程对同档案的写入在客户端即被拒(INSTANCE_MISMATCH);替换实例
//! 的 requests 台账查无任何业务调用(服务端零业务到达);A 侧档案钉死的
//! 身份不变、从未回落本机。注入只作用于本测试自有的 serve 子进程与临时
//! home,绝不触碰 `~/.pilotdeck` 或按名字匹配的系统进程。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::time::Duration;

use serde_json::{Value, json};

use common_v1::{
    CliOutcome, ConnectSession, TestHome, await_cli, contract_code, expect_ok, init_home,
    seed_remote_project_session, serve_remote,
};

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code (same shape as the targets suite).
fn failure_code(method: &str, out: &CliOutcome) -> String {
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
    assert_eq!(lines.len(), 1, "`{method}` must print one envelope line");
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` outcome: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// F6: with a live same-machine "remote" connection (J8 shape), swapping the
/// serving instance's identity must fail the reconnect loudly —
/// `conn.state failed` with `reason=instance_changed`, exit 1 — instead of
/// silently rebinding to the replacement instance, and no business call may
/// reach the replacement's request ledger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f6_server_identity_change_fails_the_reconnect_and_never_rebinds() {
    // A pinned port so the replacement instance serves the same endpoint the
    // client's profile points at (a fresh OS-assigned port would only ever be
    // unreachable, never identity-checked).
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port probe");
    let port = probe.local_addr().expect("probe addr").port();
    drop(probe);
    let endpoint = format!("ws://127.0.0.1:{port}");

    // Home B: the server data root, initialized and served on the pinned port
    // (the bind comes from B's own selected `https` target profile — the
    // lifecycle serve binding gap closure).
    let home_b = TestHome::new("f6-b");
    let init = init_home(home_b.path());
    let instance_x = init["identity"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();
    let bind = expect_ok(
        "target.write",
        &await_cli(
            home_b.path(),
            &[
                "target",
                "write",
                "--output",
                "json",
                "--target-id",
                "f6-bind",
                "--name",
                "F6 pinned serve bind",
                "--kind",
                "https",
                "--endpoint",
                &endpoint,
                "--expected-instance-id",
                &instance_x,
            ],
        ),
    );
    expect_ok(
        "target.select",
        &await_cli(
            home_b.path(),
            &[
                "target",
                "select",
                "--output",
                "json",
                "--target-id",
                "f6-bind",
                "--expected-revision",
                bind["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );
    let mut serve = serve_remote(home_b.path());
    let serve_ws = serve.ws_url();
    assert_eq!(
        serve_ws,
        format!("{endpoint}/v1/ws"),
        "the profile steered the bind"
    );

    // Home A: the client home. Its credential store references home B's
    // local token and the profile pins B's endpoint AND identity.
    let home_a = TestHome::new("f6-a");
    let token = std::fs::read_to_string(home_b.path().join("local.token"))
        .expect("read home B local.token")
        .trim()
        .to_owned();
    std::fs::write(
        home_a.path().join("credentials.json"),
        serde_json::to_vec(&json!({"tokens": {"f6-cred": token}})).unwrap(),
    )
    .expect("write home A credentials.json");
    let wrote = expect_ok(
        "target.write",
        &await_cli(
            home_a.path(),
            &[
                "target",
                "write",
                "--output",
                "json",
                "--target-id",
                "f6-rt",
                "--name",
                "F6 remote target",
                "--kind",
                "https",
                "--endpoint",
                &endpoint,
                "--expected-instance-id",
                &instance_x,
                "--credential-ref",
                "f6-cred",
            ],
        ),
    );
    expect_ok(
        "target.select",
        &await_cli(
            home_a.path(),
            &[
                "target",
                "select",
                "--output",
                "json",
                "--target-id",
                "f6-rt",
                "--expected-revision",
                wrote["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );

    // Journey prefix (J8 shape): project + session + run, all through the
    // target — the topology is proven end-to-end before the injection.
    let (_project_id, session_id) =
        seed_remote_project_session(home_a.path(), "f6-rt", &instance_x, "f6").await;
    let run = expect_ok(
        "run.submit",
        &await_cli(
            home_a.path(),
            &[
                "run",
                "submit",
                "--output",
                "json",
                "--target",
                "f6-rt",
                "--session-id",
                &session_id,
                "--request-key",
                "f6-j8-prefix",
                "--text",
                "hello f6",
            ],
        ),
    );
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let waited = expect_ok(
        "run.wait",
        &await_cli(
            home_a.path(),
            &[
                "run",
                "wait",
                "--output",
                "json",
                "--target",
                "f6-rt",
                "--run-id",
                &run_id,
                "--timeout-ms",
                "60000",
            ],
        ),
    );
    assert_eq!(waited["run"]["state"], "succeeded", "{waited}");

    // The live remote session (`api connect` resolves home A's selected
    // profile).
    let mut conn = ConnectSession::start(home_a.path()).await;
    conn.initialize().await;
    let ok = conn.call("run.list", json!({}), None).await;
    assert!(
        ok.get("result").is_some(),
        "link usable before the swap: {ok}"
    );

    // Injection, step 1: stop the serving gateway (only this test's own
    // child is ever signalled).
    serve.stop();
    conn.wait_for(
        |v| {
            v.get("method").and_then(Value::as_str) == Some("conn.state")
                && v["params"]["state"] == "reconnecting"
        },
        // 满载预算（判据不变）：等引擎检测断连后的 reconnecting 通告，20s→60s。
        Duration::from_secs(60),
        "conn.state reconnecting after the serve stopped",
    )
    .await;

    // A write attempted during the reconnecting window is refused client-side
    // (D9: no automatic replay) — it is never queued for the replacement.
    let refused = conn
        .call(
            "run.submit",
            json!({
                "sessionId": "s-f6-never-existed",
                "input": [{"kind": "text", "text": "f6 inflight probe"}]
            }),
            Some("f6-inflight"),
        )
        .await;
    assert_eq!(
        contract_code(&refused),
        "CONNECTION_FAILED",
        "writes while reconnecting must be refused, not queued: {refused}"
    );
    assert_eq!(
        refused["error"]["data"]["effect"], "not_applied",
        "{refused}"
    );

    // Injection, step 2: home B swaps its identity in place (t10 precedent).
    // The store is cleared because a gateway booted with the new identity
    // refuses the old DB; local.token is kept so the handshake still
    // authenticates and the identity check is what fires.
    let instance_y = format!("{instance_x}f6");
    let local_path = home_b.path().join("local.json");
    let mut local: Value =
        serde_json::from_str(&std::fs::read_to_string(&local_path).expect("read local.json"))
            .expect("parse local.json");
    local["instanceId"] = json!(instance_y);
    std::fs::write(&local_path, serde_json::to_vec_pretty(&local).unwrap())
        .expect("rewrite local.json");
    for suffix in ["state.sqlite", "state.sqlite-wal", "state.sqlite-shm"] {
        let _ = std::fs::remove_file(home_b.path().join("v1").join(suffix));
    }

    // Injection, step 3: the replacement instance comes up on the SAME
    // pinned endpoint (the still-selected bind profile steers it).
    let serve2 = serve_remote(home_b.path());
    assert_eq!(serve2.instance_id(), instance_y);
    assert_eq!(serve2.ws_url(), serve_ws, "same pinned endpoint");

    // The reconnect handshake reaches the replacement and the engine must
    // fail loudly instead of rebinding to the new identity.
    let failed = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("conn.state")
                    && v["params"]["state"] == "failed"
            },
            // 满载预算（判据不变）：替换网关上线后引擎带退避重连并触发
            // 身份失配失败帧，45s→300s（重启类窗口）。
            Duration::from_secs(300),
            "conn.state failed after the identity change",
        )
        .await;
    assert_eq!(failed["params"]["reason"], "instance_changed", "{failed}");
    assert_eq!(
        failed["params"]["instanceId"].as_str(),
        Some(instance_y.as_str()),
        "the failed frame must name the replacement identity: {failed}"
    );
    conn.wait_exit(1, "an identity change must exit 1 (refuse to continue)")
        .await;

    // A brand-new CLI process against the same profile: the pinned identity
    // mismatches the replacement, so the write is refused in the client
    // before it can reach the server — no silent switch to the new target.
    let refused_write = await_cli(
        home_a.path(),
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--target",
            "f6-rt",
            "--session-id",
            "s-f6-never-existed",
            "--request-key",
            "f6-post-failure",
            "--text",
            "must never land",
        ],
    );
    assert_eq!(
        failure_code("run.submit", &refused_write),
        "INSTANCE_MISMATCH",
        "a fresh process must refuse the swapped target"
    );

    // Server-side oracle: neither attempted write reached the replacement
    // instance's request ledger (a write that landed with an unknown session
    // would have recorded a definitive rejection for its key). The probes go
    // through home B's own CLI, explicitly pinned to its local data root.
    for key in ["f6-inflight", "f6-post-failure"] {
        let lookup = expect_ok(
            "requests.lookup",
            &await_cli(
                home_b.path(),
                &[
                    "requests",
                    "lookup",
                    "--output",
                    "json",
                    "--target",
                    "local",
                    "--method-id",
                    "run.submit",
                    "--idempotency-key",
                    key,
                ],
            ),
        );
        assert_eq!(
            lookup["requestState"], "result_unknown",
            "`{key}` must never have been observed by the replacement instance: {lookup}"
        );
        assert_eq!(lookup["reason"], "notObserved", "{key}: {lookup}");
    }
    let runs = expect_ok(
        "run.list",
        &await_cli(
            home_b.path(),
            &["run", "list", "--output", "json", "--target", "local"],
        ),
    );
    let items = runs["items"]
        .as_array()
        .or_else(|| runs["runs"].as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        items.is_empty(),
        "no business call may reach the replacement instance: {runs}"
    );

    // 不静默切换目标: home A's profile still pins the ORIGINAL identity, and
    // home A never fell back to starting a local gateway of its own.
    let got = expect_ok(
        "target.get",
        &await_cli(
            home_a.path(),
            &["target", "get", "--output", "json", "--target-id", "f6-rt"],
        ),
    );
    assert_eq!(
        got["target"]["expectedInstanceId"].as_str(),
        Some(instance_x.as_str()),
        "the profile must keep pinning the original identity: {got}"
    );
    assert!(!home_a.path().join("local.json").exists());
    assert_eq!(
        home_a.gateway_pid(),
        None,
        "no gateway was ever auto-started in home A"
    );
}
