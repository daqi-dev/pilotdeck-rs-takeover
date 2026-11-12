use super::*;
use crate::metadata::object;
use std::sync::Arc;

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("pilotdeck-sop-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn value(source: &str) -> JsValue {
    pilotdeck_protocol::parse_json(source).unwrap()
}
fn step() -> JsValue {
    value(
        r#"{"skillId":"s","skillName":"Workflow","version":"v1","nodeId":"n1","node":{"type":"response"},"instruction":"Answer","expectedUserInfo":[],"knownSlots":{},"allowedNextStepIds":["n2"],"requiredToolNames":[],"allowedActions":[],"isTerminal":false,"declaresHandoff":false,"contextMode":"inherit"}"#,
    )
}
fn state(node: &str, status: &str) -> JsValue {
    object([
        ("selected_skill_id", "s".into()),
        ("active_skill_id", "s".into()),
        ("active_step_id", node.into()),
        ("status", status.into()),
        ("slots_json", object([])),
    ])
}

#[tokio::test]
async fn submission_delivery_restart_and_revision_share_one_durable_state() {
    let root = Scratch::new();
    let store = store::Store::new(root.path().to_string_lossy().into_owned(), "s/a".into());
    let bundle = value(r#"{"sops":[{"id":"s","version":"v1","content":{"nodes":[]}}]}"#);
    let created = store
        .load_or_create(bundle.clone(), "s".into())
        .await
        .unwrap();
    assert_eq!(created.get("revision"), Some(&JsValue::Number(1.0)));
    let prepared = store
        .replace(bundle.clone(), state("n1", "active"), Some(1.0))
        .await
        .unwrap();
    assert_eq!(prepared.get("revision"), Some(&JsValue::Number(2.0)));
    store
        .record_successful_tools(vec!["knowledge_query".into()])
        .await
        .unwrap();
    let result =
        value(r#"{"status":"completed","replyFragment":"done","slotUpdates":{},"events":[]}"#);
    let stale = store
        .commit(
            bundle.clone(),
            state("n2", "completed"),
            2.0,
            "turn".into(),
            result.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(stale.code, "SOP_REVISION_CONFLICT");
    let committed = store
        .commit(bundle, state("n2", "completed"), 3.0, "turn".into(), result)
        .await
        .unwrap();
    assert!(is(
        committed.get("replyDelivery").unwrap(),
        "phase",
        "pending"
    ));
    let reopened = store::Store::new(root.path().to_string_lossy().into_owned(), "s/a".into());
    reopened
        .mark_reply_durable("turn".into(), "recovery".into())
        .await
        .unwrap();
    let state = reopened.read().await.unwrap().unwrap();
    assert!(is(state.get("replyDelivery").unwrap(), "phase", "durable"));
    assert!(is(
        state.get("replyDelivery").unwrap(),
        "turnId",
        "recovery"
    ));
    reopened.clear_reply("wrong".into()).await.unwrap();
    assert!(
        reopened
            .read()
            .await
            .unwrap()
            .unwrap()
            .get("replyDelivery")
            .is_some()
    );
    reopened.clear_reply("recovery".into()).await.unwrap();
    assert!(
        reopened
            .read()
            .await
            .unwrap()
            .unwrap()
            .get("replyDelivery")
            .is_none()
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("sessions"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn competing_preparation_cannot_overwrite_committed_revision() {
    let root = Scratch::new();
    let store = store::Store::new(root.path().to_string_lossy().into_owned(), "s".into());
    let bundle = value(r#"{"sops":[{"id":"s"}]}"#);
    store
        .load_or_create(bundle.clone(), "s".into())
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        store.replace(bundle.clone(), state("a", "active"), Some(1.0)),
        store.replace(bundle, state("b", "active"), Some(1.0))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(
        a.err().or_else(|| b.err()).unwrap().code,
        "SOP_REVISION_CONFLICT"
    );
}

#[test]
fn model_results_and_transition_validation_keep_owner_contracts() {
    let mut step = step();
    assert!(client::valid_step(&step));
    set(&mut step, "contextMode", JsValue::Null);
    assert!(!client::valid_step(&step));
    set(&mut step, "contextMode", "inherit".into());
    let mut proposal = proposal::parse(&value(r#"{"status":"completed","replyFragment":" ok ","slotUpdates":{},"nextStepId":"invented","ignored":true}"#)).unwrap();
    assert_eq!(proposal.get("replyFragment"), Some(&"ok".into()));
    assert!(proposal.get("ignored").is_none());
    let error = proposal::validate(&mut proposal, &step, &state("n1", "active"), "", &mut false)
        .unwrap_err();
    assert_eq!(error.code, "invalid_tool_input");
    assert!(error.message.starts_with("SOP_INVALID_TRANSITION"));
    let mut p =
        value(r#"{"status":"awaiting_user","replyFragment":"Need input","slotUpdates":{}}"#);
    assert!(
        proposal::validate(&mut p, &step, &state("n1", "active"), "", &mut false)
            .unwrap_err()
            .message
            .starts_with("STEP_MUST_ADVANCE")
    );
}

#[tokio::test]
async fn sop_http_manifest_is_cached_and_requests_keep_exact_identity() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let server = tokio::spawn(async move {
        for index in 0..3 {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let header_end = loop {
                let count = connection.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + length {
                let count = connection.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let body = if length == 0 {
                JsValue::Null
            } else {
                pilotdeck_protocol::parse_json(&String::from_utf8_lossy(&bytes[header_end..]))
                    .unwrap()
            };
            sink.lock()
                .unwrap()
                .push((header.lines().next().unwrap().to_owned(), body.clone()));
            let response = if index == 0 {
                value(
                    r#"{"status":"ok","protocolVersion":"2.0","moduleId":"sop.runtime","contract":"sop.lifecycle/v2","operations":["prepare","submit"]}"#,
                )
            } else {
                object([
                    ("protocolVersion", "2.0".into()),
                    ("requestId", body.get("requestId").unwrap().clone()),
                    ("ok", JsValue::Bool(true)),
                    ("outcome", "completed".into()),
                    (
                        "payload",
                        if index == 1 {
                            object([("state", state("n1", "active")), ("step", step())])
                        } else {
                            object([
                                ("state", state("n2", "completed")),
                                (
                                    "result",
                                    value(
                                        r#"{"status":"completed","replyFragment":"ok","slotUpdates":{},"events":[],"taskSummary":null}"#,
                                    ),
                                ),
                            ])
                        },
                    ),
                ])
            };
            let body = json(&response);
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    let client = client::Client::new(object([
        ("endpoint", endpoint.as_str().into()),
        ("timeoutMs", JsValue::Number(2000.0)),
    ]))
    .unwrap();
    let context = object([
        ("runId", "run".into()),
        ("operationId", "op".into()),
        ("requestId", "request".into()),
        ("sessionId", "session".into()),
        ("turnId", "turn".into()),
        ("expectedRevision", JsValue::Number(7.0)),
    ])
    .as_object()
    .unwrap()
    .clone();
    let signal = crate::runtime::network::Cancellation::new();
    client
        .prepare(object([]), object([]), context.clone(), &signal)
        .await
        .unwrap();
    client.submit(object([]), context, &signal).await.unwrap();
    server.await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].0, "GET /healthz HTTP/1.1");
    assert_eq!(seen[1].0, "POST /v1/sop/prepare HTTP/1.1");
    assert_eq!(seen[2].0, "POST /v1/sop/submit HTTP/1.1");
    assert_eq!(
        seen[1].1.get("expectedRevision"),
        Some(&JsValue::Number(7.0))
    );
    assert_eq!(seen[1].1.get("sessionId"), Some(&"session".into()));
}

#[tokio::test]
async fn cancellation_aborts_the_owner_exchange_without_a_submit_retry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let signal = crate::runtime::network::Cancellation::new();
    let cancel = signal.clone();
    let server = tokio::spawn(async move {
        let (_connection, _) = listener.accept().await.unwrap();
        cancel.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });
    let client = client::Client::new(object([("endpoint", endpoint.as_str().into())])).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client.prepare(object([]), object([]), JsObject::new(), &signal),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(result.code, "SOP_RUNTIME_CANCELLED");
    assert_eq!(result.retryability.as_deref(), Some("unsafe"));
    server.await.unwrap();
}

#[tokio::test]
async fn owner_wait_identity_survives_same_node_replacement_and_changes_on_transition() {
    let root = Scratch::new();
    let store = store::Store::new(root.path().to_string_lossy().into_owned(), "session".into());
    let bundle = value(r#"{"sops":[{"id":"s"}]}"#);
    store
        .load_or_create(bundle.clone(), "s".into())
        .await
        .unwrap();
    let first = store
        .replace(bundle.clone(), state("n1", "handoff"), Some(1.0))
        .await
        .unwrap();
    let second = store
        .replace(bundle.clone(), state("n1", "handoff"), Some(2.0))
        .await
        .unwrap();
    assert_eq!(first.get("wait"), second.get("wait"));
    let mut external = state("n2", "waiting_external_task");
    set(
        &mut external,
        "awaiting_input_json",
        object([("wait_id", "owner-wait".into())]),
    );
    let third = store.replace(bundle, external, Some(3.0)).await.unwrap();
    assert!(is(third.get("wait").unwrap(), "id", "owner-wait"));
    assert!(is(third.get("wait").unwrap(), "kind", "external_task"));
}

#[test]
fn model_prompt_uses_current_pin_then_latest_authenticated_receipt() {
    let prepared = object([("step", step())]);
    let mut persisted = value(
        r#"{
      "sessionId":"session","revision":3,
      "bundle":{"sops":[{"id":"s","version":"v1","content":{"nodes":[{"node_id":"n1","assignee_user_id":"pinned-owner"}]}}]},
      "state":{"active_skill_id":"s","active_step_id":"n1","slots_json":{"assigneeUserId":"untrusted-text"}},
      "wait":{"id":"wait","kind":"handoff","skillId":"s","stepId":"n1","createdAt":"now"},
      "resumeRequests":{
        "older":{"accepted":true,"sessionId":"session","requestId":"older","revision":1,"message":"ok","authorization":{"approval":{"assigneeUserId":"old-owner"}}},
        "newer":{"accepted":true,"sessionId":"session","requestId":"newer","revision":2,"message":"ok","authorization":{"approval":{"assigneeUserId":"receipt-owner"}}},
        "invalid":{"authorization":{"approval":{"assigneeUserId":"invalid-owner"}}}
      }
    }"#,
    );
    let pinned = prompt::instruction(&prepared, Some(&persisted));
    assert!(pinned.contains("state: pinned-owner."));
    assert!(!pinned.contains("receipt-owner"));
    remove(&mut persisted, "wait");
    let resumed = prompt::instruction(&prepared, Some(&persisted));
    assert!(resumed.contains("state: receipt-owner."));
    assert!(!resumed.contains("invalid-owner"));
    assert!(!resumed.contains("untrusted-text"));
    remove(&mut persisted, "resumeRequests");
    assert!(!prompt::instruction(&prepared, Some(&persisted)).contains("approval assignee"));
}

#[tokio::test]
async fn successful_tools_normalize_receipts_and_keep_first_seen_order() {
    let root = Scratch::new();
    let store = store::Store::new(root.path().to_string_lossy().into_owned(), "session".into());
    let bundle = value(r#"{"sops":[{"id":"s"}]}"#);
    store
        .load_or_create(bundle.clone(), "s".into())
        .await
        .unwrap();
    let mut current = state("n1", "active");
    set(
        &mut current,
        "successful_tool_names",
        value(r#"["one",null,"","one",3,"two"]"#),
    );
    store.replace(bundle, current, Some(1.0)).await.unwrap();
    store
        .record_successful_tools(vec!["two".into(), "".into(), "three".into()])
        .await
        .unwrap();
    let persisted = store.read().await.unwrap().unwrap();
    assert_eq!(
        persisted.get("state").unwrap().get("successful_tool_names"),
        Some(&value(r#"["one","two","three"]"#))
    );
    assert_eq!(persisted.get("revision"), Some(&JsValue::Number(3.0)));
}
