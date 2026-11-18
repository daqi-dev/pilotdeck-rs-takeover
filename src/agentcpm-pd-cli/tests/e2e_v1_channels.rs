//! Channels + integrations + notification-preferences end-to-end suite
//! (t019 T2).
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real
//! `/v1/ws` transport (embedded gateway auto-start). The channel network
//! exchanges go against loopback protocol stubs (the contract's e2e
//! seam for channel protocol behavior; no external provider is
//! contacted), and the login flow runs the begin/poll challenge state
//! machine end to end through the same stub.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

/// Parses a failed envelope and returns its `error.code`; also asserts a
/// non-zero exit.
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
    // Build the operation id from the first two non-flag CLI words
    // (e.g. "channels list ..." -> "channels.list").
    let method = if args.len() > 1 && !args[1].starts_with('-') {
        format!("{}.{}", args[0], args[1])
    } else {
        args[0].to_string()
    };
    expect_ok(&method, &out)
}

const GLOBAL_SCOPE: &str = "{\"kind\":\"global\"}";

/// A minimal HTTP/1.1 protocol stub on loopback: every connection
/// consumes the next scripted `(status, json_body)` answer; once the
/// script is exhausted it answers `200 {"ok":true}`. Blocking std I/O on
/// its own threads (the CLI crate's tokio build has no `net` feature).
fn spawn_stub(responses: Vec<(u16, String)>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut responses = responses;
    responses.reverse();
    let script = Arc::new(std::sync::Mutex::new(responses));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut socket) = stream else { break };
            let script = script.clone();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buffer = [0u8; 8192];
                let mut seen = Vec::new();
                let mut content_length = 0usize;
                loop {
                    let n = match socket.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    seen.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&seen);
                    if let Some(header_end) = text.find("\r\n\r\n") {
                        let headers = &text[..header_end];
                        if let Some(value) = headers.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        }) {
                            content_length = value;
                        }
                        if seen.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                    if seen.len() > 1 << 20 {
                        return;
                    }
                }
                let (status, body) = script
                    .lock()
                    .unwrap()
                    .pop()
                    .unwrap_or((200, "{\"ok\":true}".to_owned()));
                let answer = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(answer.as_bytes());
            });
        }
    });
    format!("http://{addr}")
}

fn schema_id_of(adapter: &str) -> String {
    format!("agentcpm-pd.channel.{adapter}.config/1")
}

async fn channels_validate(
    root: &Path,
    adapter: &str,
    schema_id: &str,
    configuration: &str,
) -> Value {
    cli_json(
        root,
        &[
            "channels",
            "validate",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--adapter-type",
            adapter,
            "--schema-id",
            schema_id,
            "--configuration",
            configuration,
        ],
    )
    .await
}

async fn channels_configure(
    root: &Path,
    channel_id: &str,
    adapter: &str,
    configuration: &str,
    expected_revision: &str,
) -> Value {
    cli_json(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            channel_id,
            "--adapter-type",
            adapter,
            "--schema-id",
            &schema_id_of(adapter),
            "--configuration",
            configuration,
            "--expected-revision",
            expected_revision,
        ],
    )
    .await
}

async fn channels_list(root: &Path, include_unconfigured: bool) -> Value {
    cli_json(
        root,
        &[
            "channels",
            "list",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--include-unconfigured",
            if include_unconfigured {
                "true"
            } else {
                "false"
            },
        ],
    )
    .await
}

async fn channels_probe(root: &Path, channel_id: &str, expected_revision: &str) -> Value {
    cli_json(
        root,
        &[
            "channels",
            "probe",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            channel_id,
            "--expected-revision",
            expected_revision,
        ],
    )
    .await
}

async fn channels_transition(
    root: &Path,
    channel_id: &str,
    action: &str,
    expected_generation: &str,
) -> Value {
    cli_json(
        root,
        &[
            "channels",
            "transition",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            channel_id,
            "--action",
            action,
            "--expected-generation",
            expected_generation,
        ],
    )
    .await
}

/// Polls `operations get` until the operation is terminal and returns its
/// completion `result`.
async fn wait_operation(root: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = await_cli(
            root,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                operation_id,
            ],
        );
        let got = expect_ok("operations.get", &out);
        let operation = &got["operation"];
        if operation["terminal"] == json!(true) {
            return operation["result"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never became terminal"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channels_validate_reports_schema_and_credential_diagnostics() {
    let home = TestHome::new("channels-e2e-validate");
    let root = home.path();
    init_home(root);

    let valid = channels_validate(
        root,
        "feishu",
        &schema_id_of("feishu"),
        "{\"enabled\":true,\"appId\":\"cli-a\",\"connectionMode\":\"stream\"}",
    )
    .await;
    assert_eq!(valid["valid"], true);
    assert_eq!(valid["diagnostics"].as_array().map(Vec::len), Some(0));

    // Unknown field, missing `enabled`, bad enum, dangling credentialRef.
    let invalid = channels_validate(
        root,
        "feishu",
        &schema_id_of("feishu"),
        "{\"appId\":7,\"shell\":\"/bin/sh\",\"domainName\":\"lark-or-mars\",\"credentialRef\":\"cred-none\"}",
    )
    .await;
    assert_eq!(invalid["valid"], false);
    let codes: Vec<String> = invalid["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(codes.contains(&"MISSING_FIELD".to_owned()), "{codes:?}");
    assert!(codes.contains(&"UNKNOWN_FIELD".to_owned()), "{codes:?}");
    assert!(codes.contains(&"TYPE_MISMATCH".to_owned()), "{codes:?}");
    assert!(codes.contains(&"INVALID_VALUE".to_owned()), "{codes:?}");
    assert!(
        codes.contains(&"CREDENTIAL_NOT_FOUND".to_owned()),
        "{codes:?}"
    );

    // A schemaId that does not match the adapter is a diagnostic, not a
    // transport error.
    let mismatch = channels_validate(
        root,
        "telegram",
        "agentcpm-pd.channel.telegram.config/9",
        "{\"enabled\":true}",
    )
    .await;
    assert_eq!(mismatch["valid"], false);
    assert_eq!(mismatch["diagnostics"][0]["code"], "SCHEMA_MISMATCH");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channels_configure_list_round_trip_and_revision_gate() {
    let home = TestHome::new("channels-e2e-configure");
    let root = home.path();
    init_home(root);

    let created = channels_configure(
        root,
        "ch-feishu",
        "feishu",
        "{\"enabled\":true,\"appId\":\"cli-a\",\"defaultSessionLabel\":\"main\"}",
        "absent",
    )
    .await;
    assert_eq!(created["applied"], true);
    let revision = created["revision"].as_str().unwrap().to_owned();
    assert!(revision.starts_with("crev-"));
    assert_eq!(
        created["generation"],
        Value::Null,
        "saving alone does not assemble a running generation"
    );

    // The revision gate.
    let out = await_cli(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-feishu",
            "--adapter-type",
            "feishu",
            "--schema-id",
            &schema_id_of("feishu"),
            "--configuration",
            "{\"enabled\":false}",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(
        expect_failed("channels.configure", &out),
        "REVISION_CONFLICT"
    );
    let out = await_cli(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-feishu",
            "--adapter-type",
            "feishu",
            "--schema-id",
            &schema_id_of("feishu"),
            "--configuration",
            "{\"enabled\":false}",
            "--expected-revision",
            "crev-99999999",
        ],
    );
    assert_eq!(
        expect_failed("channels.configure", &out),
        "REVISION_CONFLICT"
    );

    // An invalid candidate keeps the stored row untouched.
    let out = await_cli(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-feishu",
            "--adapter-type",
            "feishu",
            "--schema-id",
            &schema_id_of("feishu"),
            "--configuration",
            "{\"enabled\":true,\"whoops\":1}",
            "--expected-revision",
            &revision,
        ],
    );
    assert_eq!(
        expect_failed("channels.configure", &out),
        "INVALID_ARGUMENT"
    );

    // The list view: one configured row + 18 unconfigured types.
    let listed = channels_list(root, true).await;
    assert_eq!(
        listed["supportedAdapterTypes"].as_array().map(Vec::len),
        Some(19)
    );
    let channels = listed["channels"].as_array().unwrap().clone();
    assert_eq!(channels.len(), 19);
    let row = channels
        .iter()
        .find(|c| c["channelId"] == json!("ch-feishu"))
        .cloned()
        .unwrap();
    assert_eq!(row["adapterType"], "feishu");
    assert_eq!(row["schemaId"], schema_id_of("feishu"));
    assert_eq!(row["revision"], revision);
    assert_eq!(row["enabled"], true);
    assert_eq!(row["runtime"]["state"], "disabled");
    assert_eq!(row["runtime"]["available"], false);
    assert_eq!(row["runtime"]["reasonCode"], "NOT_ASSEMBLED");
    assert_eq!(row["configuration"]["appId"], "cli-a");
    let unconfigured = channels
        .iter()
        .find(|c| c["channelId"] == json!("telegram"))
        .cloned()
        .unwrap();
    assert_eq!(unconfigured["runtime"]["state"], "unconfigured");
    let only = channels_list(root, false).await;
    assert_eq!(only["channels"].as_array().map(Vec::len), Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channels_probe_reports_classified_connectivity() {
    let home = TestHome::new("channels-e2e-probe");
    let root = home.path();
    init_home(root);

    // A closed port classifies as a connection failure.
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    };
    let created = channels_configure(
        root,
        "ch-email",
        "email",
        &format!("{{\"enabled\":true,\"probeEndpoint\":\"{closed_port}\"}}"),
        "absent",
    )
    .await;
    let revision = created["revision"].as_str().unwrap().to_owned();

    let receipt = channels_probe(root, "ch-email", &revision).await;
    assert_eq!(receipt["kind"], "operation");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_operation(root, &operation_id).await;
    assert_eq!(completion["channelId"], "ch-email");
    assert_eq!(completion["adapterType"], "email");
    assert_eq!(completion["reachable"], false);
    assert_eq!(completion["authenticated"], false);
    assert_eq!(completion["diagnostics"][0]["code"], "CONN_FAILURE");

    // A stub answering 401: reachable, authentication failed.
    let stub = spawn_stub(vec![(401, "{\"error\":\"invalid_token\"}".to_owned())]);
    let created = channels_configure(
        root,
        "ch-email",
        "email",
        &format!("{{\"enabled\":true,\"probeEndpoint\":\"{stub}\"}}"),
        &revision,
    )
    .await;
    let revision = created["revision"].as_str().unwrap().to_owned();
    let receipt = channels_probe(root, "ch-email", &revision).await;
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["reachable"], true);
    assert_eq!(completion["authenticated"], false);
    assert_eq!(
        completion["diagnostics"][0]["code"],
        "AUTHENTICATION_FAILED"
    );

    // The revision gate.
    let out = await_cli(
        root,
        &[
            "channels",
            "probe",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-email",
            "--expected-revision",
            "crev-00000001",
        ],
    );
    assert_eq!(expect_failed("channels.probe", &out), "REVISION_CONFLICT");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channels_transition_assembles_generations_and_disables() {
    let home = TestHome::new("channels-e2e-transition");
    let root = home.path();
    init_home(root);

    // No probeEndpoint: the assembly surface is the configuration itself,
    // so enable reports ready (the probe-seamed enable is covered below
    // with its failure classification).
    channels_configure(
        root,
        "ch-telegram",
        "telegram",
        "{\"enabled\":true}",
        "absent",
    )
    .await;

    // First enable from the absent generation.
    let receipt = channels_transition(root, "ch-telegram", "enable", "absent").await;
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["channelId"], "ch-telegram");
    assert_eq!(completion["state"], "ready");
    let generation = completion["generation"].as_str().unwrap().to_owned();
    assert!(generation.starts_with("gen-"));
    assert_eq!(completion["drainedGeneration"], Value::Null);

    // The list reports the assembled channel online.
    let listed = channels_list(root, false).await;
    assert_eq!(listed["channels"][0]["runtime"]["state"], "ready");
    assert_eq!(listed["channels"][0]["runtime"]["available"], true);
    assert_eq!(listed["channels"][0]["runtime"]["reasonCode"], Value::Null);
    assert_eq!(listed["channels"][0]["runtime"]["generation"], generation);

    // A stale generation rejects.
    let out = await_cli(
        root,
        &[
            "channels",
            "transition",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-telegram",
            "--action",
            "reload",
            "--expected-generation",
            "absent",
        ],
    );
    assert_eq!(
        expect_failed("channels.transition", &out),
        "GENERATION_CONFLICT"
    );

    // Reload drains the old generation; disable stops reporting online.
    let receipt = channels_transition(root, "ch-telegram", "reload", &generation).await;
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    let next = completion["generation"].as_str().unwrap().to_owned();
    assert_ne!(next, generation);
    assert_eq!(completion["drainedGeneration"], generation);
    let receipt = channels_transition(root, "ch-telegram", "disable", &next).await;
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["state"], "disabled");
    // Disable advances the generation too (the drained one is `next`).
    let after_disable = completion["generation"].as_str().unwrap().to_owned();
    let listed = channels_list(root, false).await;
    assert_eq!(listed["channels"][0]["runtime"]["available"], false);
    assert_eq!(
        listed["channels"][0]["runtime"]["reasonCode"],
        "CHANNEL_DISABLED"
    );
    let revision = listed["channels"][0]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // Enabling against a failing endpoint never reports ready: the stub
    // answers a signature rejection. Re-enable starts from the current
    // (post-disable) generation, not from `absent`.
    let failing = spawn_stub(vec![(403, "{\"code\":\"BAD_SIGNATURE\"}".to_owned())]);
    channels_configure(
        root,
        "ch-telegram",
        "telegram",
        &format!("{{\"enabled\":true,\"probeEndpoint\":\"{failing}\"}}"),
        &revision,
    )
    .await;
    let receipt = channels_transition(root, "ch-telegram", "enable", &after_disable).await;
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["state"], "failed");
    let listed = channels_list(root, false).await;
    assert_eq!(listed["channels"][0]["runtime"]["state"], "failed");
    assert_eq!(
        listed["channels"][0]["runtime"]["reasonCode"],
        "SIGNATURE_INVALID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channels_login_challenge_state_machine() {
    let home = TestHome::new("channels-e2e-login");
    let root = home.path();
    init_home(root);

    // Script: begin (presentation fields) → poll pending → poll succeeded.
    let stub = spawn_stub(vec![
        (
            200,
            "{\"userActionUrl\":\"https://example.test/device\",\"userCode\":\"WD-42\"}".to_owned(),
        ),
        (200, "{\"state\":\"pending\"}".to_owned()),
        (
            200,
            "{\"state\":\"succeeded\",\"credential\":\"exchanged-secret\"}".to_owned(),
        ),
    ]);
    channels_configure(
        root,
        "ch-weixin",
        "weixin",
        &format!("{{\"enabled\":true,\"probeEndpoint\":\"{stub}\"}}"),
        "absent",
    )
    .await;

    // An adapter without a login mode rejects the challenge (the channel
    // itself must exist, or the answer is NOT_FOUND).
    channels_configure(
        root,
        "ch-telegram",
        "telegram",
        "{\"enabled\":true}",
        "absent",
    )
    .await;
    let out = await_cli(
        root,
        &[
            "channels",
            "begin-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-telegram",
        ],
    );
    assert_eq!(
        expect_failed("channels.begin_login", &out),
        "INVALID_ARGUMENT"
    );

    let begun = cli_json(
        root,
        &[
            "channels",
            "begin-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
        ],
    )
    .await;
    assert_eq!(begun["challengeType"], "qr");
    assert_eq!(begun["userActionUrl"], "https://example.test/device");
    assert_eq!(begun["userCode"], "WD-42");
    assert!(begun["expiresAt"].as_str().is_some());
    assert_eq!(begun["pollAfterMs"], 1000);
    let challenge_id = begun["challengeId"].as_str().unwrap().to_owned();

    let pending = cli_json(
        root,
        &[
            "channels",
            "poll-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
            "--challenge-id",
            challenge_id.as_str(),
        ],
    )
    .await;
    assert_eq!(pending["state"], "pending");
    assert_eq!(pending["credentialRef"], Value::Null);

    let succeeded = cli_json(
        root,
        &[
            "channels",
            "poll-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
            "--challenge-id",
            challenge_id.as_str(),
        ],
    )
    .await;
    assert_eq!(succeeded["state"], "succeeded");
    let credential_ref = succeeded["credentialRef"].as_str().unwrap().to_owned();
    assert!(!credential_ref.is_empty());
    assert_eq!(succeeded["errorCode"], Value::Null);

    // Repeated polls replay the terminal state without re-registering.

    let again = cli_json(
        root,
        &[
            "channels",
            "poll-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
            "--challenge-id",
            challenge_id.as_str(),
        ],
    )
    .await;
    assert_eq!(again["state"], "succeeded");
    assert_eq!(again["credentialRef"], credential_ref);
    // The credential store lists it without the secret.
    let creds = cli_json(
        root,
        &[
            "credentials",
            "list",
            "--output",
            "json",
            "--kind",
            "channel.weixin",
        ],
    )
    .await;
    let stored = creds["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["credentialId"] == json!(credential_ref))
        .cloned()
        .unwrap();
    assert_eq!(stored["kind"], "channel.weixin");
    assert!(stored.get("secret").is_none());

    // Cancel on a terminal challenge changes nothing.
    let cancelled = cli_json(
        root,
        &[
            "channels",
            "cancel-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
            "--challenge-id",
            &challenge_id,
        ],
    )
    .await;
    assert_eq!(cancelled["cancelled"], false);
    assert_eq!(cancelled["state"], "succeeded");

    // A second challenge (fresh stub script): cancel while pending, then
    // poll must not revive it.
    let stub = spawn_stub(Vec::new());
    channels_configure(
        root,
        "ch-weixin",
        "weixin",
        &format!("{{\"enabled\":true,\"probeEndpoint\":\"{stub}\"}}"),
        &listed_revision(root).await,
    )
    .await;
    let begun = cli_json(
        root,
        &[
            "channels",
            "begin-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
        ],
    )
    .await;
    let second = begun["challengeId"].as_str().unwrap().to_owned();
    let cancelled = cli_json(
        root,
        &[
            "channels",
            "cancel-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
            "--challenge-id",
            &second,
        ],
    )
    .await;
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(cancelled["state"], "cancelled");
    let polled = cli_json(
        root,
        &[
            "channels",
            "poll-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
            "--challenge-id",
            second.as_str(),
        ],
    )
    .await;
    assert_eq!(polled["state"], "cancelled");
    assert_eq!(polled["credentialRef"], Value::Null);

    // An unknown challenge is NOT_FOUND.
    let out = await_cli(
        root,
        &[
            "channels",
            "poll-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-weixin",
            "--challenge-id",
            "chall-none",
        ],
    );
    assert_eq!(expect_failed("channels.poll_login", &out), "NOT_FOUND");
}

/// The current revision of the login scenario's weixin channel (helper for
/// its re-configure step).
async fn listed_revision(root: &Path) -> String {
    let listed = channels_list(root, false).await;
    listed["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["channelId"] == json!("ch-weixin"))
        .unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifications_preferences_round_trip_and_survive_restart() {
    let home = TestHome::new("channels-e2e-notifications");
    let root = home.path();
    init_home(root);

    let initial = cli_json(
        root,
        &[
            "notifications",
            "get-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
        ],
    )
    .await;
    assert_eq!(initial["revision"], "nrev-00000000");
    assert_eq!(initial["preferences"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        initial["deliveryChannels"].as_array().map(Vec::len),
        Some(0)
    );

    let preferences = "[{\"eventType\":\"run.completed\",\"deliveryChannelId\":\"ch-telegram\",\"enabled\":true},{\"eventType\":\"run.failed\",\"deliveryChannelId\":\"ch-telegram\",\"enabled\":false}]";
    let written = cli_json(
        root,
        &[
            "notifications",
            "set-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--preferences",
            preferences,
            "--expected-revision",
            "absent",
        ],
    )
    .await;
    assert_eq!(written["applied"], true);
    assert_eq!(written["generation"], Value::Null);
    let revision = written["revision"].as_str().unwrap().to_owned();
    assert!(revision.starts_with("nrev-"));

    // The revision gate.
    let out = await_cli(
        root,
        &[
            "notifications",
            "set-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--preferences",
            "[]",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(
        expect_failed("notifications.set_preferences", &out),
        "REVISION_CONFLICT"
    );

    // Kill the auto-started gateway and let the next CLI call re-start it:
    // the preferences must survive the restart.
    if let Some(pid) = home.gateway_pid() {
        signal_pid(pid, rustix::process::Signal::KILL);
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    let persisted = cli_json(
        root,
        &[
            "notifications",
            "get-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
        ],
    )
    .await;
    assert_eq!(persisted["revision"], revision);
    assert_eq!(
        persisted["preferences"],
        serde_json::from_str::<Value>(preferences).unwrap()
    );

    // A channel that becomes ready appears as a delivery adapter (no
    // probeEndpoint: the assembly surface is the configuration itself).
    channels_configure(
        root,
        "ch-telegram",
        "telegram",
        "{\"enabled\":true}",
        "absent",
    )
    .await;
    let receipt = channels_transition(root, "ch-telegram", "enable", "absent").await;
    wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    let read = cli_json(
        root,
        &[
            "notifications",
            "get-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
        ],
    )
    .await;
    let delivery = read["deliveryChannels"].as_array().unwrap().clone();
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0]["channelId"], "ch-telegram");
    assert_eq!(delivery[0]["available"], true);
    assert_eq!(delivery[0]["reasonCode"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integrations_detect_reports_installation_and_projects() {
    let home = TestHome::new("channels-e2e-integrations");
    let root = home.path();
    init_home(root);

    // No projects registered: the projects list is empty; the runtime block
    // reports whatever the environment actually has on PATH.
    let detected = cli_json(root, &["integrations", "detect", "--output", "json"]).await;
    assert_eq!(detected["projects"].as_array().map(Vec::len), Some(0));
    let installed = detected["installed"].as_bool().unwrap();
    assert_eq!(detected["runtime"]["available"], installed);
    assert_eq!(detected["runtime"]["generation"], Value::Null);
    if installed {
        assert_eq!(detected["runtime"]["state"], "ready");
        assert_eq!(detected["runtime"]["reasonCode"], Value::Null);
        assert!(detected["version"].as_str().is_some());
    } else {
        assert_eq!(detected["runtime"]["state"], "not_installed");
        assert_eq!(
            detected["runtime"]["reasonCode"],
            "TASKMASTER_NOT_INSTALLED"
        );
        assert_eq!(detected["version"], Value::Null);
    }

    // An unregistered project rejects.
    let out = await_cli(
        root,
        &[
            "integrations",
            "detect",
            "--output",
            "json",
            "--project-id",
            "proj-none",
        ],
    );
    assert_eq!(
        expect_failed("integrations.detect", &out),
        "INVALID_ARGUMENT"
    );

    // Register a project with a real authorized root and a .taskmaster
    // folder: the per-project probe sees it. The workspace lives under the
    // default authorized root (`<home>/workspaces`), seeded through t012's
    // `directories create` ServerPath form.
    let instance_id = init_home_instance(root);
    let out = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--path",
            "{\"authorizedRootId\":\"default\",\"relativePath\":{\"encoding\":\"utf8\",\"value\":\"detect-ws/proj-a\"}}",
        ],
    );
    expect_ok("directories.create", &out);
    let workspace_root = home.path().join("workspaces").join("detect-ws");
    std::fs::create_dir_all(
        workspace_root
            .join("proj-a")
            .join(".taskmaster")
            .join("tasks"),
    )
    .unwrap();
    std::fs::write(
        workspace_root
            .join("proj-a")
            .join(".taskmaster")
            .join("tasks")
            .join("tasks.json"),
        "{\"tasks\": []}",
    )
    .unwrap();
    let out = await_cli(
        root,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "proj-a",
            "--name",
            "Detect",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"detect-ws/proj-a\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);

    let detected = cli_json(root, &["integrations", "detect", "--output", "json"]).await;
    let projects = detected["projects"].as_array().unwrap().clone();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["projectId"], "proj-a");
    assert_eq!(projects[0]["initialized"], true);
    assert_eq!(projects[0]["reasonCode"], Value::Null);

    // Remove the tasks file: the reasonCode names the missing piece.
    std::fs::remove_file(
        workspace_root
            .join("proj-a")
            .join(".taskmaster")
            .join("tasks")
            .join("tasks.json"),
    )
    .unwrap();
    let detected = cli_json(
        root,
        &[
            "integrations",
            "detect",
            "--output",
            "json",
            "--project-id",
            "proj-a",
        ],
    )
    .await;
    let projects = detected["projects"].as_array().unwrap().clone();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["initialized"], false);
    assert_eq!(projects[0]["reasonCode"], "TASKMASTER_TASKS_MISSING");
}
