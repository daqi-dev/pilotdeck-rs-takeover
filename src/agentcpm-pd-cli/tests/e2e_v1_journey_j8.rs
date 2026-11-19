//! S5 业务旅程 J8（远程单用户工作——核心远程旅程，同机模拟版）端到端：
//! `_tmp/20261119-s5/journeys-execution-plan.md` §3 J8 的命令序列，经真实
//! `agentcpm-pd` 二进制、双 home 同机「模拟远程」拓扑（W0 `RemoteTarget`
//! 挂具：home B `server serve` 显式承载，home A 经 target 档案走真实 ws://
//! 一跳）、`common_v1` 的 envelope schema 校验与隔离 `AGENTCPM_PD_HOME`。
//!
//! 旅程判据（计划文档原文）：客户端 stdout + 服务端 ready.json/state.sqlite
//! 双侧留存；重连后 connectionId 不同且身份链完整（A2）。
//!
//! - `j8_remote_single_user_journey`（同机模拟）：select → `connection open`
//!   （身份=服务端 instanceId，凭据不外泄）→ 远程 project/session/run
//!   （`[pd-driver:sleep=60000]`，比计划原文的 30000 多留一倍余量：EOF 分支
//!   自带 5s 排空，慢机上的全部断言都要落在 run 存活窗口内）→ 客户端死亡
//!   双分支（`api connect` 的 EOF 与 SIGTERM，退出码均 0——引擎语义「排空
//!   绝不取消 run」，D4）→ 新进程 `run list --states` 列出在跑（D4/D13/§12）
//!   → `run get/events` → `--target` 远程 `policy get/update`（B 侧本地视图
//!   复核落库）→ `connection close` → 负例：不可达 target 的命令直接
//!   `CONNECTION_FAILED`、绝不回落本机（D8；B 侧 requests 台账查无该键）
//!   → `run cancel` + `run wait` 显式收尾（取消前 run 一直在跑，D4 终证）。
//!   落盘断言：B 留存 ready.json/v1/state.sqlite；A 仅 targets.json/
//!   credentials.json，无 local.json/local.token/v1//run/，从不自启网关。
//! - `journey_j8_cross_host_client_round`（`#[ignore]`）：跨主机轮的客户端
//!   半程，由 W4/W5 的独立 shell 驱动（`journeys-remote-gate.sh` 形态）在
//!   cb8↔mbp2 双机在位时以 `J8_REMOTE_WS_URL`/`J8_REMOTE_INSTANCE_ID`/
//!   `J8_REMOTE_TOKEN_FILE` 环境变量驱动；服务端半程的 ready.json/
//!   state.sqlite 留存与 SHA256 归档是驱动侧的职责。不进常规 nextest。

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::{
    CliOutcome, ConnectSession, TestHome, await_cli, base_command, expect_envelope, expect_ok,
    init_home_instance, remote_pair, seed_remote_project_session, signal_pid,
};

/// One remote argv invocation: the command words plus `--output json` and the
/// `--target <profile>` hop (remote failures never fall back to the local
/// home, D8). The global options parse wherever they appear, so trailing
/// placement matches the t005/seed_remote precedents.
fn rt(home: &Path, target: &str, words: &[&str]) -> CliOutcome {
    let mut args: Vec<&str> = words.to_vec();
    args.extend(["--output", "json", "--target", target]);
    await_cli(home, &args)
}

/// The contract error code of one failed envelope (with a non-zero exit).
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

/// The non-terminal run states: after every client death the server-side run
/// must still be in flight (D4).
const LIVE_STATES: [&str; 4] = ["queued", "running", "waiting", "cancelling"];

fn live_state(run: &Value, what: &str) {
    let state = run["run"]["state"].as_str().unwrap_or_default();
    assert!(
        LIVE_STATES.contains(&state),
        "{what}: the run must still be in flight, saw `{state}`: {run}"
    );
}

/// J8 same-machine shape: one user works from home A against home B's served
/// gateway; every client death leaves the server-side run untouched, the
/// reconnected identity chain is complete, and an unreachable profile fails
/// loudly without ever touching (or starting) a local gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn j8_remote_single_user_journey() {
    let pair = remote_pair("j8");
    let home_a = pair.home_a.path();
    let home_b = pair.home_b.path();
    let target = pair.target_id.clone();

    // Identity chain (A2): the serve's ready.json, home B's local.json and the
    // profile pin all name the same instance.
    let instance_b = pair.instance_id_b();
    assert_eq!(init_home_instance(home_b), instance_b);

    // The fixture wrote the profile; J8 selects it explicitly.
    let got = expect_ok(
        "target.get",
        &await_cli(
            home_a,
            &["target", "get", "--output", "json", "--target-id", &target],
        ),
    );
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
                &target,
                "--expected-revision",
                got["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );

    // connection.open: the identity over the wire is home B's.
    let opened = expect_ok(
        "connection.open",
        &await_cli(
            home_a,
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                &target,
            ],
        ),
    );
    assert_eq!(opened["identity"]["instanceId"], instance_b);
    assert_eq!(opened["identity"]["product"], "agentcpm-pd");
    let connection_1 = opened["connectionId"]
        .as_str()
        .expect("server-assigned connectionId")
        .to_owned();
    let token = std::fs::read_to_string(home_b.join("local.token"))
        .expect("read home B local.token")
        .trim()
        .to_owned();
    assert!(
        !opened.to_string().contains(&token),
        "credential material must never appear in connection.open output"
    );

    // The remote working set: project + session + one long run on the server.
    let (_project_id, session_id) =
        seed_remote_project_session(home_a, &target, &instance_b, "j8").await;
    let run = expect_ok(
        "run.submit",
        &rt(
            home_a,
            &target,
            &[
                "run",
                "submit",
                "--session-id",
                &session_id,
                "--request-key",
                "j8-run-1",
                "--text",
                "j8 remote single-user work [pd-driver:sleep=60000]",
            ],
        ),
    );
    let run_id = run["runId"].as_str().unwrap().to_owned();

    // ---- Client death, branch 1: EOF (stdin close drains, never cancels). ----
    let mut conn = ConnectSession::start(home_a).await;
    conn.initialize().await;
    let sub = conn
        .sub_open("run.events", json!({"runId": run_id}), 64)
        .await;
    assert!(
        sub["result"]["subscriptionId"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the live subscription was open before the death: {sub}"
    );
    conn.close_stdin().await;
    conn.wait_exit(0, "EOF drains and exits 0; runs are never cancelled")
        .await;

    // D4: the server-side run survives the client death.
    let after_eof = expect_ok(
        "run.get",
        &rt(home_a, &target, &["run", "get", "--run-id", &run_id]),
    );
    live_state(&after_eof, "after the EOF death");

    // ---- Client death, branch 2: SIGTERM (graceful drain, exit 0). ----
    let mut conn = ConnectSession::start(home_a).await;
    conn.initialize().await;
    let pid = conn.child.id().expect("api connect child pid");
    signal_pid(pid, rustix::process::Signal::TERM);
    conn.wait_exit(0, "SIGTERM drains and exits 0; runs are never cancelled")
        .await;

    // D4 again, then the new-process observation (D4/D13/§12): a fresh CLI
    // process lists the in-flight run through the state filter.
    let after_term = expect_ok(
        "run.get",
        &rt(home_a, &target, &["run", "get", "--run-id", &run_id]),
    );
    live_state(&after_term, "after the SIGTERM death");
    let listed = expect_ok(
        "run.list",
        &rt(
            home_a,
            &target,
            &[
                "run",
                "list",
                "--scope",
                r#"{"kind":"instance"}"#,
                "--states",
                r#"["queued","running","waiting","cancelling"]"#,
            ],
        ),
    );
    let items = listed["items"].as_array().expect("run.list items");
    let in_flight = items
        .iter()
        .find(|item| item["runId"].as_str() == Some(run_id.as_str()))
        .unwrap_or_else(|| panic!("the in-flight run must be listed: {listed}"))
        .clone();
    assert!(
        LIVE_STATES.contains(&in_flight["state"].as_str().unwrap_or_default()),
        "the listed run is still in flight: {in_flight}"
    );

    // run.events: the event history of the live run is readable from a new
    // process (one caught-up page).
    let events = expect_ok(
        "run.events",
        &rt(home_a, &target, &["run", "events", "--run-id", &run_id]),
    );
    let frame = &events["frame"];
    assert!(
        frame["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty()),
        "the live run already has events: {events}"
    );
    assert_eq!(frame["caughtUp"], true, "{events}");

    // ---- A2: reconnect through a new connection, identity chain intact. ----
    let reopened = expect_ok(
        "connection.open",
        &await_cli(
            home_a,
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                &target,
            ],
        ),
    );
    assert_eq!(reopened["identity"]["instanceId"], instance_b);
    let connection_2 = reopened["connectionId"]
        .as_str()
        .expect("server-assigned connectionId")
        .to_owned();
    assert_ne!(
        connection_2, connection_1,
        "a reconnect must get a fresh server-assigned connectionId"
    );

    // ---- Remote policy: update through the target, verify the landing from
    // home B's own local view (the same live serve serves it). ----
    let scope = r#"{"kind":"instance"}"#;
    let policy = expect_ok(
        "policy.get",
        &rt(home_a, &target, &["policy", "get", "--scope", scope]),
    );
    assert_eq!(policy["policy"]["mode"], "ask");
    let revision = policy["policy"]["revision"].as_str().unwrap().to_owned();
    let rules =
        r#"[{"ruleId":"j8-allow-ls","effect":"allow","tool":"bash","resourcePattern":"ls *"}]"#;
    let updated = expect_ok(
        "policy.update",
        &rt(
            home_a,
            &target,
            &[
                "policy",
                "update",
                "--scope",
                scope,
                "--expected-revision",
                &revision,
                "--mode",
                "allowConfigured",
                "--rules",
                rules,
            ],
        ),
    );
    let new_revision = updated["policy"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(new_revision, revision);
    let b_view = expect_ok(
        "policy.get",
        &await_cli(
            home_b,
            &["policy", "get", "--output", "json", "--scope", scope],
        ),
    );
    assert_eq!(
        b_view["policy"]["revision"], new_revision,
        "the remote policy update landed in home B's store: {b_view}"
    );
    assert_eq!(b_view["policy"]["mode"], "allowConfigured");

    // connection.close: the argv process that opened connection 2 has exited,
    // so the server honestly reports the id as already closed (t005 semantic).
    let closed = expect_ok(
        "connection.close",
        &await_cli(
            home_a,
            &[
                "connection",
                "close",
                "--output",
                "json",
                "--connection-id",
                &connection_2,
            ],
        ),
    );
    assert_eq!(closed["closed"], false, "{closed}");

    // ---- Negative (D8): an unreachable profile fails loudly, never falls
    // back to (or starts) a local gateway, and the write never reaches B. ----
    expect_ok(
        "target.write",
        &await_cli(
            home_a,
            &[
                "target",
                "write",
                "--output",
                "json",
                "--target-id",
                "j8-dead",
                "--name",
                "J8 unreachable remote",
                "--kind",
                "https",
                "--endpoint",
                "ws://127.0.0.1:1/v1/ws",
                "--expected-instance-id",
                "inst-j8-nowhere",
                "--credential-ref",
                &pair.credential_ref,
            ],
        ),
    );
    let refused = await_cli(
        home_a,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--target",
            "j8-dead",
            "--session-id",
            &session_id,
            "--request-key",
            "j8-dead-1",
            "--text",
            "must never land",
        ],
    );
    assert_eq!(
        failure_code("run.submit", &refused),
        "CONNECTION_FAILED",
        "an unreachable target must fail loudly, never fall back"
    );
    assert_eq!(
        pair.home_a.gateway_pid(),
        None,
        "D8: no local gateway may ever start in home A"
    );
    assert!(!home_a.join("v1").exists());
    let lookup = expect_ok(
        "requests.lookup",
        &await_cli(
            home_b,
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
                "j8-dead-1",
            ],
        ),
    );
    assert_eq!(lookup["requestState"], "result_unknown", "{lookup}");
    assert_eq!(lookup["reason"], "notObserved", "{lookup}");

    // The good target still works after the refusal (B unaffected), and the
    // run is still in flight right up to the explicit cancel (D4 end-to-end).
    let still_live = expect_ok(
        "run.get",
        &rt(home_a, &target, &["run", "get", "--run-id", &run_id]),
    );
    live_state(&still_live, "after the unreachable-target refusal");
    let cancel = expect_ok(
        "run.cancel",
        &rt(home_a, &target, &["run", "cancel", "--run-id", &run_id]),
    );
    assert_eq!(cancel["receipt"]["disposition"], "requested", "{cancel}");
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
            &run_id,
            "--timeout-ms",
            "60000",
        ],
    );
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["conditionMet"], true, "{waited}");
    assert_eq!(waited["run"]["state"], "cancelled", "{waited}");

    // ---- Disk: the server side keeps everything; the client side keeps
    // only profile bookkeeping and never lands gateway data. ----
    assert!(home_b.join("run").join("ready.json").is_file());
    assert!(
        home_b.join("v1").join("state.sqlite").is_file(),
        "home B carries the gateway state store"
    );
    assert!(!home_a.join("local.json").exists());
    assert!(!home_a.join("local.token").exists());
    assert!(!home_a.join("v1").exists());
    assert!(!home_a.join("run").exists());
    assert!(home_a.join("targets.json").is_file());
    assert!(home_a.join("credentials.json").is_file());
}

/// The cross-host half of J8 (client side only): driven by the W4/W5 shell
/// driver on the client machine while the peer machine runs the serve. Set
/// `J8_REMOTE_WS_URL` (the peer's ws endpoint), `J8_REMOTE_INSTANCE_ID` (the
/// peer's instanceId) and `J8_REMOTE_TOKEN_FILE` (a local copy of the peer's
/// `local.token`); the driver collects the server-side ready.json/state.sqlite
/// evidence on the peer. Never part of the regular nextest run.
#[ignore = "cross-host W4 round: needs a second machine's serve; run through the journeys remote shell driver with J8_REMOTE_WS_URL / J8_REMOTE_INSTANCE_ID / J8_REMOTE_TOKEN_FILE set"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j8_cross_host_client_round() {
    let ws_url = std::env::var("J8_REMOTE_WS_URL")
        .expect("J8 cross-host round: J8_REMOTE_WS_URL must point at the peer's ws endpoint");
    let instance_id = std::env::var("J8_REMOTE_INSTANCE_ID")
        .expect("J8 cross-host round: J8_REMOTE_INSTANCE_ID must name the peer's instanceId");
    let token_file = std::env::var("J8_REMOTE_TOKEN_FILE")
        .expect("J8 cross-host round: J8_REMOTE_TOKEN_FILE must hold the peer's local.token");

    let home_a = TestHome::new("j8-xhost-a");
    let home = home_a.path();
    let token = std::fs::read_to_string(&token_file)
        .unwrap_or_else(|e| panic!("read {token_file}: {e}"))
        .trim()
        .to_owned();
    std::fs::write(
        home.join("credentials.json"),
        serde_json::to_vec(&json!({"tokens": {"j8-xhost-cred": token}})).unwrap(),
    )
    .unwrap();
    let wrote = expect_ok(
        "target.write",
        &await_cli(
            home,
            &[
                "target",
                "write",
                "--output",
                "json",
                "--target-id",
                "j8-xhost",
                "--name",
                "J8 cross-host peer",
                "--kind",
                "https",
                "--endpoint",
                &ws_url,
                "--expected-instance-id",
                &instance_id,
                "--credential-ref",
                "j8-xhost-cred",
            ],
        ),
    );
    expect_ok(
        "target.select",
        &await_cli(
            home,
            &[
                "target",
                "select",
                "--output",
                "json",
                "--target-id",
                "j8-xhost",
                "--expected-revision",
                wrote["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );

    // The identity over the real network hop is the peer's.
    let opened = expect_ok(
        "connection.open",
        &await_cli(
            home,
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                "j8-xhost",
            ],
        ),
    );
    assert_eq!(opened["identity"]["instanceId"], instance_id);
    assert!(
        !opened.to_string().contains(&token),
        "credential material must never appear in connection.open output"
    );

    let (_project_id, session_id) =
        seed_remote_project_session(home, "j8-xhost", &instance_id, "j8x").await;
    let run = expect_ok(
        "run.submit",
        &rt(
            home,
            "j8-xhost",
            &[
                "run",
                "submit",
                "--session-id",
                &session_id,
                "--request-key",
                "j8-xhost-1",
                "--text",
                "j8 cross-host [pd-driver:sleep=60000]",
            ],
        ),
    );
    let run_id = run["runId"].as_str().unwrap().to_owned();

    // A client process dies mid-observation (SIGTERM on a blocking run wait);
    // the command process installs no SIGTERM handler, so it is simply killed.
    let mut child = {
        let mut cmd = base_command(home);
        cmd.args([
            "run",
            "wait",
            "--output",
            "json",
            "--target",
            "j8-xhost",
            "--run-id",
            &run_id,
            "--timeout-ms",
            "60000",
        ]);
        cmd.stdin(Stdio::null());
        cmd.spawn().expect("spawn run wait")
    };
    tokio::time::sleep(Duration::from_millis(800)).await;
    signal_pid(
        child.id().expect("run wait child pid"),
        rustix::process::Signal::TERM,
    );
    let status = tokio::time::timeout(common_v1::CLI_TIMEOUT, child.wait())
        .await
        .expect("run wait exit")
        .expect("wait run wait");
    assert!(
        status.code().is_none_or(|code| code != 0),
        "the SIGTERMed client died: {status:?}"
    );

    // D4 over the real hop: the server-side run is untouched, and a fresh
    // process still lists it through the state filter.
    let after = expect_ok(
        "run.get",
        &rt(home, "j8-xhost", &["run", "get", "--run-id", &run_id]),
    );
    live_state(&after, "cross-host after the client death");
    let listed = expect_ok(
        "run.list",
        &rt(
            home,
            "j8-xhost",
            &[
                "run",
                "list",
                "--scope",
                r#"{"kind":"instance"}"#,
                "--states",
                r#"["queued","running","waiting","cancelling"]"#,
            ],
        ),
    );
    assert!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["runId"].as_str() == Some(run_id.as_str())),
        "the in-flight run must be listed cross-host: {listed}"
    );
    let policy = expect_ok(
        "policy.get",
        &rt(
            home,
            "j8-xhost",
            &["policy", "get", "--scope", r#"{"kind":"instance"}"#],
        ),
    );
    assert!(policy["policy"]["revision"].as_str().is_some());

    // Explicit cleanup: cancel, then the client home never fell back local.
    expect_ok(
        "run.cancel",
        &rt(home, "j8-xhost", &["run", "cancel", "--run-id", &run_id]),
    );
    assert_eq!(home_a.gateway_pid(), None);
    assert!(!home.join("local.json").exists());
    assert!(!home.join("v1").exists());
    assert!(home.join("targets.json").is_file());
}
