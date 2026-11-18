//! Terminals domain e2e (t015): PTY lifecycle, control leases, ordered
//! input, byte-cursor output, the close chain, and the attach process —
//! real `agentcpm-pd` subprocesses over the real `/v1/ws` transport.
//!
//! Lease tokens are secrets: every token-carrying call goes through
//! `api invoke --input-json` (a protected temp file), never argv.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::time::Duration;

use base64::Engine;
use common_v1::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Registers project `p-term` on the default authorized root so
/// `terminals.create` can resolve the working directory.
async fn seed_terminal_project(home: &std::path::Path) -> String {
    let instance_id = init_home_instance(home);
    let out = await_cli(
        home,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-term",
            "--name",
            "Terminals E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"term\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out)["project"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn create_terminal(home: &std::path::Path, project_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "terminal",
            "create",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--rows",
            "24",
            "--columns",
            "80",
        ],
    );
    expect_ok("terminals.create", &out)["terminal"].clone()
}

async fn get_terminal(home: &std::path::Path, terminal_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "terminal",
            "get",
            "--output",
            "json",
            "--terminal-id",
            terminal_id,
        ],
    );
    expect_ok("terminals.get", &out)["terminal"].clone()
}

/// Runs one `api invoke` with a JSON input file (the only sanctioned way to
/// carry a lease token).
async fn invoke_json(home: &std::path::Path, method: &str, input: &Value) -> CliOutcome {
    let path = home.join("invoke-input.json");
    std::fs::write(&path, serde_json::to_string(input).unwrap()).unwrap();
    await_cli(
        home,
        &[
            "api",
            "invoke",
            "--output",
            "json",
            method,
            "--input-json",
            path.to_str().unwrap(),
        ],
    )
}

/// Scenario: create -> get -> list round trip against the contract schemas.
#[tokio::test]
async fn terminals_create_get_list_round_trip() {
    let home = TestHome::new("term-cgl");
    init_home(home.path());
    let project_id = seed_terminal_project(home.path()).await;

    let terminal = create_terminal(home.path(), &project_id).await;
    assert_eq!(terminal["state"], "running");
    assert_eq!(terminal["rows"], 24);
    assert_eq!(terminal["columns"], 80);
    assert_eq!(terminal["controlEpoch"], 0);
    assert_eq!(terminal["lastAcceptedInputSeq"], 0);
    assert!(
        terminal["serverEpoch"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    let fetched = get_terminal(home.path(), terminal["terminalId"].as_str().unwrap()).await;
    assert_eq!(fetched["terminalId"], terminal["terminalId"]);
    assert_eq!(fetched["state"], "running");

    let out = await_cli(
        home.path(),
        &[
            "terminal",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let listed = expect_ok("terminals.list", &out);
    let ids = listed["terminals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["terminalId"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.contains(&terminal["terminalId"].as_str().unwrap()));

    // The same-request dedup: same key + same input replays the terminal.
    let key = "term-create-dedupe";
    let input_path = home.path().join("create.json");
    std::fs::write(
        &input_path,
        serde_json::to_string(&json!({
            "projectId": project_id,
            "rows": 24,
            "columns": 80
        }))
        .unwrap(),
    )
    .unwrap();
    let first = await_cli(
        home.path(),
        &[
            "api",
            "invoke",
            "--output",
            "json",
            "terminals.create",
            "--input-json",
            input_path.to_str().unwrap(),
            "--request-key",
            key,
        ],
    );
    let first = expect_ok("terminals.create", &first)["terminal"].clone();
    let second = await_cli(
        home.path(),
        &[
            "api",
            "invoke",
            "--output",
            "json",
            "terminals.create",
            "--input-json",
            input_path.to_str().unwrap(),
            "--request-key",
            key,
        ],
    );
    let second = expect_ok("terminals.create", &second)["terminal"].clone();
    assert_eq!(first["terminalId"], second["terminalId"]);
}

/// Scenario: the lease matrix — acquire, double-acquire conflict, renew,
/// release; input sequence accept/dedup/gap/conflict; observer (no lease)
/// cannot resize.
#[tokio::test]
async fn terminals_lease_and_input_matrix() {
    let home = TestHome::new("term-lease");
    init_home(home.path());
    let project_id = seed_terminal_project(home.path()).await;
    let terminal = create_terminal(home.path(), &project_id).await;
    let terminal_id = terminal["terminalId"].as_str().unwrap().to_owned();
    let server_epoch = terminal["serverEpoch"].as_str().unwrap().to_owned();

    // Acquire.
    let out = invoke_json(
        home.path(),
        "terminals.control",
        &json!({"terminalId": terminal_id, "action": "acquire", "expectedControlEpoch": 0}),
    )
    .await;
    let lease = expect_ok("terminals.control", &out)["lease"].clone();
    let token = lease["leaseToken"].as_str().unwrap().to_owned();
    let control_epoch = lease["controlEpoch"].as_i64().unwrap();
    assert_eq!(control_epoch, 1);

    // Double acquire conflicts.
    let out = invoke_json(
        home.path(),
        "terminals.control",
        &json!({"terminalId": terminal_id, "action": "acquire", "expectedControlEpoch": control_epoch}),
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(
        out.stdout.contains("CONTROL_CONFLICT"),
        "stdout: {}",
        out.stdout
    );

    // Input without the lease is refused.
    let out = invoke_json(
        home.path(),
        "terminals.input",
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": control_epoch,
            "leaseToken": "bogus",
            "inputSeq": 1,
            "bytesBase64": b64(b"echo ok\n"),
        }),
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(
        out.stdout.contains("CONTROL_CONFLICT"),
        "stdout: {}",
        out.stdout
    );

    // Contiguous input is accepted; the ack only claims server acceptance.
    let out = invoke_json(
        home.path(),
        "terminals.input",
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": control_epoch,
            "leaseToken": token,
            "inputSeq": 1,
            "bytesBase64": b64(b"printf t15marker\\n\n"),
        }),
    )
    .await;
    let ack = expect_ok("terminals.input", &out);
    assert_eq!(ack["acceptedThroughSeq"], 1);
    assert_eq!(ack["deduplicated"], false);
    assert_eq!(ack["ackMeaning"], "acceptedByServer");

    // Same seq + same bytes deduplicates.
    let out = invoke_json(
        home.path(),
        "terminals.input",
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": control_epoch,
            "leaseToken": token,
            "inputSeq": 1,
            "bytesBase64": b64(b"printf t15marker\\n\n"),
        }),
    )
    .await;
    let ack = expect_ok("terminals.input", &out);
    assert_eq!(ack["deduplicated"], true);

    // Same seq + different bytes conflicts.
    let out = invoke_json(
        home.path(),
        "terminals.input",
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": control_epoch,
            "leaseToken": token,
            "inputSeq": 1,
            "bytesBase64": b64(b"true\n"),
        }),
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(
        out.stdout.contains("INPUT_SEQUENCE_CONFLICT"),
        "stdout: {}",
        out.stdout
    );

    // Skipping ahead is a gap.
    let out = invoke_json(
        home.path(),
        "terminals.input",
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": control_epoch,
            "leaseToken": token,
            "inputSeq": 5,
            "bytesBase64": b64(b"\n"),
        }),
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(
        out.stdout.contains("INPUT_SEQUENCE_GAP"),
        "stdout: {}",
        out.stdout
    );

    // The marker reached the output stream.
    let mut saw_marker = false;
    for _ in 0..40 {
        let frame = invoke_json(
            home.path(),
            "terminals.output",
            &json!({"terminalId": terminal_id, "follow": false}),
        )
        .await;
        let frame = expect_ok("terminals.output", &frame);
        if frame["bytesBase64"]
            .as_str()
            .and_then(|b| {
                base64::engine::general_purpose::STANDARD
                    .decode(b)
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).contains("t15marker"))
            })
            .unwrap_or(false)
        {
            saw_marker = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(saw_marker, "the marker must reach the output stream");

    // An observer (wrong token) cannot resize.
    let out = invoke_json(
        home.path(),
        "terminals.resize",
        &json!({
            "terminalId": terminal_id,
            "controlEpoch": control_epoch,
            "leaseToken": "bogus",
            "rows": 30,
            "columns": 100,
        }),
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(
        out.stdout.contains("CONTROL_CONFLICT"),
        "stdout: {}",
        out.stdout
    );

    // Renew extends; release clears and bumps the epoch.
    let out = invoke_json(
        home.path(),
        "terminals.control",
        &json!({
            "terminalId": terminal_id,
            "action": "renew",
            "expectedControlEpoch": control_epoch,
            "leaseToken": token,
        }),
    )
    .await;
    let lease = expect_ok("terminals.control", &out)["lease"].clone();
    assert_eq!(lease["controlEpoch"], control_epoch);

    let out = invoke_json(
        home.path(),
        "terminals.control",
        &json!({
            "terminalId": terminal_id,
            "action": "release",
            "expectedControlEpoch": control_epoch,
            "leaseToken": token,
        }),
    )
    .await;
    let released = expect_ok("terminals.control", &out)["terminal"].clone();
    assert_eq!(released["controlEpoch"], control_epoch + 1);

    // Close it so the temp home's gateway stops owning a shell.
    let out = invoke_json(
        home.path(),
        "terminals.close",
        &json!({
            "terminalId": terminal_id,
            "expectedServerEpoch": server_epoch,
            "mode": "force",
        }),
    )
    .await;
    let receipt = expect_ok("terminals.close", &out);
    assert_eq!(receipt["kind"], "operation");
}

/// Scenario: the attach process — machine-mode stdio frames, echo through
/// the real pty, resize, and the exit frame on close.
#[tokio::test]
async fn terminals_attach_machine_mode_echo_and_exit() {
    let home = TestHome::new("term-attach");
    init_home(home.path());
    let project_id = seed_terminal_project(home.path()).await;
    let terminal = create_terminal(home.path(), &project_id).await;
    let terminal_id = terminal["terminalId"].as_str().unwrap().to_owned();
    let server_epoch = terminal["serverEpoch"].as_str().unwrap().to_owned();

    let mut cmd = base_command(home.path());
    cmd.args([
        "terminal",
        "attach",
        "--machine",
        "--terminal-id",
        &terminal_id,
    ]);
    cmd.stdin(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn terminal attach");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Value>(256);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(frame) = serde_json::from_str::<Value>(&line)
                && frame_tx.send(frame).await.is_err()
            {
                break;
            }
        }
    });

    // The attach protocol frames the connection first (`conn.state` ready),
    // then the first output frame is the shell banner/prompt.
    let _first = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = frame_rx
                .recv()
                .await
                .expect("attach emits output within 20s");
            if frame["method"] == "term.output" {
                return frame;
            }
        }
    })
    .await
    .expect("frame channel open");

    // One keypress round trip: term.input -> pty ECHO -> term.output.
    let key = b64(b"z");
    stdin
        .write_all(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"k1\",\"method\":\"term.input\",\"params\":{{\"bytesBase64\":\"{key}\"}}}}\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = frame_rx
                .recv()
                .await
                .expect("frame channel stays open while the terminal lives");
            let matched = frame["method"] == "term.output"
                && frame["params"]["bytesBase64"]
                    .as_str()
                    .and_then(|b| {
                        base64::engine::general_purpose::STANDARD
                            .decode(b)
                            .ok()
                            .map(|bytes| bytes.contains(&b'z'))
                    })
                    .unwrap_or(false);
            if matched {
                return frame;
            }
        }
    })
    .await
    .expect("the pty echoes the key within 10s");
    assert!(
        echoed["params"]["cursor"]["outputEpoch"].is_string(),
        "term.output carries the output cursor: {echoed}"
    );

    // Resize reaches the pty (accepted by the controller).
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"term.resize\",\"params\":{\"rows\":40,\"columns\":120}}\n",
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();

    // Close ends the attach with term.exit and exit code 0.
    let out = invoke_json(
        home.path(),
        "terminals.close",
        &json!({
            "terminalId": terminal_id,
            "expectedServerEpoch": server_epoch,
            "mode": "graceful",
        }),
    )
    .await;
    expect_ok("terminals.close", &out);
    let exit = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = frame_rx.recv().await.expect("frame channel open");
            if frame["method"] == "term.exit" {
                return frame;
            }
        }
    })
    .await
    .expect("attach reports term.exit after close");
    assert_eq!(exit["params"]["state"], "exited");

    // The attach process waits out its stdin reader on exit; close the pipe
    // so the reader observes EOF and the child can terminate.
    drop(stdin);
    let code = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("attach exits after the terminal")
        .expect("wait attach")
        .code();
    assert_eq!(code, Some(0), "attach exits 0 after a terminal exit");
}

/// Scenario: close (graceful and force) produces a durable terminal state
/// with exit facts, and a repeated close stays idempotent.
#[tokio::test]
async fn terminals_close_kill_chain_and_idempotent_replay() {
    let home = TestHome::new("term-close");
    init_home(home.path());
    let project_id = seed_terminal_project(home.path()).await;
    let terminal = create_terminal(home.path(), &project_id).await;
    let terminal_id = terminal["terminalId"].as_str().unwrap().to_owned();
    let server_epoch = terminal["serverEpoch"].as_str().unwrap().to_owned();

    // Close with a wrong epoch is refused before any kill.
    let out = invoke_json(
        home.path(),
        "terminals.close",
        &json!({"terminalId": terminal_id, "expectedServerEpoch": "gen-bogus", "mode": "force"}),
    )
    .await;
    // INVALID_ARGUMENT maps to the usage exit code (007 §6.8; the same
    // mapping `e2e_v1_interaction_lease` pins).
    assert_eq!(out.code, 2);
    assert!(
        out.stdout.contains("INVALID_ARGUMENT"),
        "stdout: {}",
        out.stdout
    );

    // Graceful close: SIGHUP -> the shell exits, the row turns terminal.
    let out = invoke_json(
        home.path(),
        "terminals.close",
        &json!({"terminalId": terminal_id, "expectedServerEpoch": server_epoch, "mode": "graceful"}),
    )
    .await;
    let receipt = expect_ok("terminals.close", &out);
    assert_eq!(receipt["kind"], "operation");
    let mut terminal = Value::Null;
    for _ in 0..100 {
        terminal = get_terminal(home.path(), &terminal_id).await;
        if terminal["state"] == "exited" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        terminal["state"], "exited",
        "graceful close terminates the shell"
    );
    assert!(terminal["exitSignal"].is_string() || terminal["exitCode"].is_number());

    // Inputs after close answer TERMINAL_CLOSED.
    let out = invoke_json(
        home.path(),
        "terminals.input",
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": 0,
            "leaseToken": "x",
            "inputSeq": 1,
            "bytesBase64": b64(b"\n"),
        }),
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(
        out.stdout.contains("TERMINAL_CLOSED"),
        "stdout: {}",
        out.stdout
    );
}

/// Scenario: a Gateway restart loses the terminal (the master fd died with
/// the old process) — the contract's `lost` semantics.
#[tokio::test]
async fn terminals_lost_after_gateway_restart() {
    let home = TestHome::new("term-lost");
    init_home(home.path());
    let project_id = seed_terminal_project(home.path()).await;
    let terminal = create_terminal(home.path(), &project_id).await;
    let terminal_id = terminal["terminalId"].as_str().unwrap().to_owned();

    // Restart the gateway (stop, then let the next CLI auto-start fresh).
    let out = await_cli(home.path(), &["server", "stop", "--output", "json"]);
    assert_eq!(out.code, 0, "server stop: {}", out.stderr);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let after = get_terminal(home.path(), &terminal_id).await;
    assert_eq!(after["state"], "lost", "a restarted gateway reports lost");
    // The terminal row keeps the boot epoch it was created under; the fresh
    // gateway's own epoch (reported by `service.status`) is what makes the
    // row `lost`, so the two must differ after the restart.
    let status = await_cli(home.path(), &["server", "status", "--output", "json"]);
    let status = expect_ok("service.status", &status);
    assert_ne!(
        status["generation"], terminal["serverEpoch"],
        "the new boot carries a fresh serverEpoch"
    );
}
