//! End-to-end smoke for the S5 W0 `RemoteTarget` apparatus (the J8
//! same-machine shape, minimal slice): a local dual-home topology where
//! home A's CLI reaches home B's gateway through a target profile.
//!
//! 本地双 home 冒烟：instance.init B → `server serve` 显式承载 B → A 侧
//! `targets.write` 指向 B（wsUrl 取自 B 的 `run/ready.json`）→ A 侧 api
//! 操作经 `--target` 打到 B。断言 B 的落盘（`run/ready.json`、
//! `v1/state.sqlite`）与响应（B 自己的本地视图能看到远程创建的数据）、
//! A 侧不落任何 gateway 数据。跨主机轮按计划走 W4 的 `#[ignore]` + 独立
//! shell 驱动，不在本文件。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use common_v1::{
    TestHome, await_cli, expect_ok, init_home, remote_pair, seed_remote_project_session,
    serve_remote,
};
use serde_json::json;

/// The explicit journey (not via `remote_pair`): every step is a real CLI
/// invocation against the product, so the apparatus is characterized before
/// the one-step fixture wraps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_dual_home_target_journey_reaches_b_and_never_lands_on_a() {
    let home_a = TestHome::new("rt-journey-a");
    let home_b = TestHome::new("rt-journey-b");

    // instance.init B — home B becomes the server data root.
    let init = init_home(home_b.path());
    let instance_id_b = init["identity"]["instanceId"].as_str().unwrap().to_owned();

    // The explicit server side: `server serve --data-root <home_b>`.
    let serve = serve_remote(home_b.path());
    assert_eq!(serve.instance_id(), instance_id_b);
    let ws_url = serve.ws_url();
    assert!(
        ws_url.starts_with("ws://127.0.0.1:"),
        "default serve bind is loopback: {ws_url}"
    );

    // A 侧 targets.write 指向 B: home A's credential store references home
    // B's local token, and the profile pins B's endpoint + identity.
    let token = std::fs::read_to_string(home_b.path().join("local.token"))
        .unwrap()
        .trim()
        .to_owned();
    std::fs::write(
        home_a.path().join("credentials.json"),
        serde_json::to_vec(&json!({"tokens": {"rt-journey": token}})).unwrap(),
    )
    .unwrap();
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
                "rt-journey",
                "--name",
                "Home B over ws",
                "--kind",
                "https",
                "--endpoint",
                &ws_url,
                "--expected-instance-id",
                &instance_id_b,
                "--credential-ref",
                "rt-journey",
            ],
        ),
    );
    assert_eq!(wrote["target"]["kind"], "https");
    assert_eq!(wrote["target"]["endpoint"], ws_url);
    let revision = wrote["target"]["revision"].as_str().unwrap().to_owned();

    // J8 shape: select the profile, then verify the identity over the wire.
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
                "rt-journey",
                "--expected-revision",
                &revision,
            ],
        ),
    );
    let opened = expect_ok(
        "connection.open",
        &await_cli(
            home_a.path(),
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                "rt-journey",
            ],
        ),
    );
    assert_eq!(opened["identity"]["instanceId"], instance_id_b);
    assert_eq!(opened["identity"]["product"], "agentcpm-pd");
    assert!(
        opened["connectionId"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "server-assigned connectionId present"
    );
    assert!(
        !opened.to_string().contains(&token),
        "credential material must never appear in connection.open output"
    );

    // A 侧 api 操作打到 B: project + session + run, all through the target.
    let (project_id, session_id) =
        seed_remote_project_session(home_a.path(), "rt-journey", &instance_id_b, "rt").await;
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
                "rt-journey",
                "--session-id",
                &session_id,
                "--request-key",
                "rt-journey-run-1",
                "--text",
                "hello remote",
            ],
        ),
    );
    let run_id = run["runId"].as_str().unwrap().to_owned();

    // B 的响应: home B's own local view (same data root, same live serve —
    // a local CLI reuses the running gateway, it never starts a second one)
    // sees everything the remote commands created.
    let pget = expect_ok(
        "project.get",
        &await_cli(
            home_b.path(),
            &[
                "projects",
                "get",
                "--output",
                "json",
                "--project-id",
                &project_id,
            ],
        ),
    );
    assert_eq!(pget["project"]["projectId"], project_id);
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
                "rt-journey",
                "--run-id",
                &run_id,
                "--timeout-ms",
                "60000",
            ],
        ),
    );
    assert_eq!(waited["run"]["state"], "succeeded");
    let rget = expect_ok(
        "run.get",
        &await_cli(
            home_b.path(),
            &["run", "get", "--output", "json", "--run-id", &run_id],
        ),
    );
    assert_eq!(rget["run"]["state"], "succeeded");

    // B 的落盘: the serve gateway artifacts live only in home B.
    assert!(home_b.path().join("run").join("ready.json").is_file());
    assert!(
        home_b.path().join("v1").join("state.sqlite").is_file(),
        "home B carries the gateway state store"
    );

    // A 不落: home A kept only client-side bookkeeping (profile store +
    // credential reference + idempotency ledger); no gateway data, no
    // auto-started service.
    assert!(!home_a.path().join("local.json").exists());
    assert!(!home_a.path().join("local.token").exists());
    assert!(!home_a.path().join("v1").exists());
    assert!(!home_a.path().join("run").exists());
    assert_eq!(
        home_a.gateway_pid(),
        None,
        "no gateway was ever auto-started in home A"
    );
    assert!(home_a.path().join("targets.json").is_file());
    assert!(home_a.path().join("credentials.json").is_file());
}

/// The one-step fixture: `remote_pair` links the two homes through the same
/// product paths the journey above drives by hand, so W1+ journey suites can
/// construct the topology in a single call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_pair_one_step_fixture_links_homes_for_remote_commands() {
    let pair = remote_pair("rt-pair");

    // The profile is in home A and pins home B's live endpoint + identity.
    let got = expect_ok(
        "target.get",
        &await_cli(
            pair.home_a.path(),
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
    assert_eq!(got["target"]["kind"], "https");
    assert_eq!(got["target"]["endpoint"], pair.ws_url());
    assert_eq!(got["target"]["credentialRef"], pair.credential_ref);

    // One remote command through the fixture-built profile reaches home B.
    let opened = expect_ok(
        "connection.open",
        &await_cli(
            pair.home_a.path(),
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                &pair.target_id,
            ],
        ),
    );
    assert_eq!(opened["identity"]["instanceId"], pair.instance_id_b());
    assert_eq!(opened["identity"]["product"], "agentcpm-pd");

    // Home B is initialized and served; home A stays data-clean.
    assert!(pair.home_b.path().join("local.json").is_file());
    assert!(pair.home_b.path().join("run").join("ready.json").is_file());
    assert!(!pair.home_a.path().join("local.json").exists());
    assert_eq!(
        pair.home_a.gateway_pid(),
        None,
        "no gateway was ever auto-started in home A"
    );
}
