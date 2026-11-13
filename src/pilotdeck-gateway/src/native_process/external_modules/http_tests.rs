//! Controls for the shared HTTP client, using the cc7c4e6 HttpModuleClient
//! contract and the locally executed 2026-11-14 http-module-runtime baseline.
use super::*;
use crate::skills::{ModuleHttpFuture, ModuleHttpResponse};
use std::{collections::VecDeque, time::Duration};
use tokio::sync::Notify;

enum Action {
    Reply(ModuleHttpResponse),
    Error(ModuleTransportError),
    Gate(Arc<Notify>, ModuleHttpResponse),
    Pending,
}

struct MockTransport {
    actions: Mutex<VecDeque<Action>>,
    requests: Mutex<Vec<ModuleHttpRequest>>,
    dispatched: Notify,
}
impl MockTransport {
    fn new(actions: impl IntoIterator<Item = Action>) -> Arc<Self> {
        Arc::new(Self {
            actions: Mutex::new(actions.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            dispatched: Notify::new(),
        })
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    async fn wait_count(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let notified = self.dispatched.notified();
                if self.count() >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("expected HTTP dispatch");
    }
}
impl ModuleHttpTransport for MockTransport {
    fn send<'a>(&'a self, request: ModuleHttpRequest) -> ModuleHttpFuture<'a> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            let action = self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected HTTP replay");
            self.dispatched.notify_one();
            match action {
                Action::Reply(reply) => Ok(reply),
                Action::Error(error) => Err(error),
                Action::Gate(gate, reply) => {
                    gate.notified().await;
                    Ok(reply)
                }
                Action::Pending => std::future::pending().await,
            }
        })
    }
}

struct Env(Option<String>);
impl ModuleEnvironment for Env {
    fn var(&self, key: &str) -> Option<String> {
        assert_eq!(key, "FICTIONAL_MODULE_CREDENTIAL");
        self.0.clone()
    }
}
fn json(value: &str) -> JsValue {
    parse_json(value).unwrap()
}
fn response(status: u16, value: &str) -> ModuleHttpResponse {
    ModuleHttpResponse {
        status,
        body: value.as_bytes().to_vec(),
    }
}
fn manifest() -> ModuleHttpResponse {
    response(
        200,
        r#"{"protocolVersion":"2.0","implementationId":"example.tools","contract":"pilotdeck.tools/v1","transport":"module-http-v2","methods":["execute"]}"#,
    )
}
fn success(sequence: u64) -> ModuleHttpResponse {
    response(
        200,
        &format!(
            r#"{{"kind":"response","messageId":"response-{sequence}","inReplyTo":"module-http-{sequence}","requestId":"request-1","ok":true,"payload":{{"results":[]}}}}"#
        ),
    )
}
fn binding() -> ExternalHttpBinding {
    ExternalHttpBinding {
        implementation_id: "example.tools".into(),
        contract: "pilotdeck.tools/v1".into(),
        transport: "module-http-v2".into(),
        endpoint: "http://module.invalid/base".into(),
        manifest_path: "manifest".into(),
        call_path: "/call".into(),
        methods: vec!["execute".into()],
        timeout_ms: 1_000.0,
        credential_env: None,
    }
}
fn client(transport: Arc<MockTransport>) -> Arc<HttpModuleClient> {
    Arc::new(HttpModuleClient::new(
        binding(),
        transport,
        Arc::new(Env(None)),
    ))
}
fn input() -> ModuleCallInput {
    ModuleCallInput {
        run_id: "run-1".into(),
        operation_id: "operation-1".into(),
        request_id: "request-1".into(),
        module: "capability".into(),
        payload: json(r#"{"calls":[{"id":"tool-1","name":"remote_lookup","input":{"key":"v"}}]}"#),
        idempotency_key: None,
        abort: None,
    }
}

fn utf16_binding() -> ExternalHttpBinding {
    ExternalHttpBinding::from_value(&json(
        r#"{"implementationId":"id\ud800","contract":"pilotdeck.tools/v1","transport":"module-http-v2","endpoint":"http://module.invalid/","methods":["op\ud800"]}"#,
    ))
    .unwrap()
}

#[tokio::test]
async fn manifest_preserves_unpaired_utf16_identity_and_method() {
    let transport = MockTransport::new([Action::Reply(response(
        200,
        r#"{"protocolVersion":"2.0","implementationId":"id\ud800","contract":"pilotdeck.tools/v1","transport":"module-http-v2","methods":["op\ud800"]}"#,
    ))]);
    let client = HttpModuleClient::new(utf16_binding(), transport, Arc::new(Env(None)));
    let manifest = client.manifest().await.unwrap();
    assert_eq!(
        manifest.get("implementationId"),
        Some(&json(r#""id\ud800""#))
    );
    assert_eq!(manifest.get("methods"), Some(&json(r#"["op\ud800"]"#)));
}

#[tokio::test]
async fn manifest_rejects_replacement_character_for_unpaired_utf16_identity() {
    let transport = MockTransport::new([Action::Reply(response(
        200,
        r#"{"protocolVersion":"2.0","implementationId":"id\ufffd","contract":"pilotdeck.tools/v1","transport":"module-http-v2","methods":["op\ud800"]}"#,
    ))]);
    let client = HttpModuleClient::new(utf16_binding(), transport, Arc::new(Env(None)));
    let error = client.manifest().await.unwrap_err();
    assert!(error.code.equals_str("MODULE_PROTOCOL_INCOMPATIBLE"));
    assert!(error.message.equals_str(
        "Module manifest identity, protocol, contract, transport, or methods are incompatible."
    ));
}

#[tokio::test]
async fn manifest_rejects_replacement_method_and_preserves_utf16_error_message() {
    let transport = MockTransport::new([Action::Reply(response(
        200,
        r#"{"protocolVersion":"2.0","implementationId":"id\ud800","contract":"pilotdeck.tools/v1","transport":"module-http-v2","methods":["op\ufffd"]}"#,
    ))]);
    let client = HttpModuleClient::new(utf16_binding(), transport, Arc::new(Env(None)));
    let error = client.manifest().await.unwrap_err();
    assert!(error.code.equals_str("MODULE_PROTOCOL_INCOMPATIBLE"));
    assert_eq!(
        JsValue::String(error.message),
        json(r#""Module manifest does not provide required method 'op\ud800'.""#)
    );
}

#[tokio::test]
async fn call_preserves_and_correlates_unpaired_utf16_request_id() {
    let transport = MockTransport::new([
        Action::Reply(manifest()),
        Action::Reply(response(
            200,
            r#"{"kind":"response","messageId":"answer","inReplyTo":"module-http-1","requestId":"q\ud800","ok":true,"payload":{}}"#,
        )),
        Action::Reply(response(
            200,
            r#"{"kind":"response","messageId":"answer-2","inReplyTo":"module-http-2","requestId":"q\ufffd","ok":true,"payload":{}}"#,
        )),
    ]);
    let client = client(transport.clone());
    let mut request = input();
    request.request_id = JsString::from_utf16(vec![u16::from(b'q'), 0xd800]);
    client.call(request.clone()).await.unwrap();
    let error = client.call(request).await.unwrap_err();
    assert!(error.code.equals_str("MODULE_PROTOCOL_INCOMPATIBLE"));
    assert!(
        error
            .message
            .equals_str("Module response correlation does not match the request.")
    );
    let requests = transport.requests.lock().unwrap();
    let sent = parse_json(std::str::from_utf8(&requests[1].body).unwrap()).unwrap();
    assert_eq!(sent.get("requestId"), Some(&json(r#""q\ud800""#)));
}

#[tokio::test]
async fn envelope_is_owned_by_client_and_manifest_is_cached() {
    let transport = MockTransport::new([
        Action::Reply(manifest()),
        Action::Reply(success(1)),
        Action::Reply(success(2)),
    ]);
    let client = client(transport.clone());
    let mut call = input();
    call.idempotency_key = Some("once-1".into());
    client.call(call).await.unwrap();
    client.call(input()).await.unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].url, "http://module.invalid/base/manifest");
    assert_eq!(requests[0].authorization, None);
    assert_eq!(requests[1].url, "http://module.invalid/call");
    assert_eq!(requests[1].content_type, Some("application/json"));
    assert_eq!(
        parse_json(std::str::from_utf8(&requests[1].body).unwrap()).unwrap(),
        json(
            r#"{"kind":"request","method":"module_call","messageId":"module-http-1","runId":"run-1","operationId":"operation-1","requestId":"request-1","module":"capability","payload":{"calls":[{"id":"tool-1","name":"remote_lookup","input":{"key":"v"}}]},"idempotencyKey":"once-1"}"#
        )
    );
}

#[tokio::test]
async fn failed_manifest_is_retried_without_consuming_message_sequence() {
    let transport = MockTransport::new([
        Action::Reply(response(
            200,
            r#"{"protocolVersion":"2.0","implementationId":"wrong","contract":"pilotdeck.tools/v1","transport":"module-http-v2","methods":["execute"]}"#,
        )),
        Action::Reply(manifest()),
        Action::Reply(success(1)),
    ]);
    let client = client(transport.clone());
    let error = client.call(input()).await.unwrap_err();
    assert!(error.code.equals_str("MODULE_PROTOCOL_INCOMPATIBLE"));
    assert_eq!(transport.count(), 1);
    client.call(input()).await.unwrap();
    assert_eq!(transport.count(), 3);
}

async fn wait_for_manifest_join(client: &HttpModuleClient) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if client
                .manifest
                .lock()
                .unwrap()
                .flight
                .as_ref()
                .is_some_and(|flight| flight.receiver_count() > 0)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("manifest waiter joined owner");
}

#[tokio::test]
async fn concurrent_manifest_failure_is_shared_then_next_round_retries() {
    let gate = Arc::new(Notify::new());
    let transport = MockTransport::new([
        Action::Gate(gate.clone(), response(200, "[]")),
        Action::Reply(manifest()),
    ]);
    let client = client(transport.clone());
    let owner_client = client.clone();
    let owner = tokio::spawn(async move { owner_client.manifest().await });
    transport.wait_count(1).await;
    let waiter_client = client.clone();
    let waiter = tokio::spawn(async move { waiter_client.manifest().await });
    wait_for_manifest_join(&client).await;
    gate.notify_one();
    let first = owner.await.unwrap().unwrap_err();
    let second = waiter.await.unwrap().unwrap_err();
    assert_eq!(first, second);
    assert_eq!(transport.count(), 1);
    client.manifest().await.unwrap();
    assert_eq!(transport.count(), 2);
}

#[tokio::test]
async fn dropped_manifest_owner_releases_joined_waiter() {
    let transport = MockTransport::new([Action::Pending, Action::Reply(manifest())]);
    let client = client(transport.clone());
    let owner_client = client.clone();
    let owner = tokio::spawn(async move { owner_client.manifest().await });
    transport.wait_count(1).await;
    let waiter_client = client.clone();
    let waiter = tokio::spawn(async move { waiter_client.manifest().await });
    wait_for_manifest_join(&client).await;
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(transport.count(), 2);
}

#[tokio::test]
async fn structured_non_success_http_preserves_full_provider_failure() {
    let raw = r#"{"kind":"response","messageId":"error-1","inReplyTo":"module-http-1","requestId":"request-1","ok":false,"outcome":"result_unknown","code":"REMOTE_WRITE_UNKNOWN","error":{"code":"REMOTE_WRITE_UNKNOWN","message":"unpaired \ud800","retryability":"unsafe","details":{"sideEffectId":"remote-1"},"providerExtra":[1,2]},"extra":{"preserved":true}}"#;
    let transport =
        MockTransport::new([Action::Reply(manifest()), Action::Reply(response(503, raw))]);
    let result = client(transport).call(input()).await.unwrap();
    assert_eq!(result, json(raw));
    let failure = HttpModuleError::from_response(&result);
    assert_eq!(*failure.structure, result);
    assert!(failure.code.equals_str("REMOTE_WRITE_UNKNOWN"));
    assert!(failure.outcome.unwrap().equals_str("result_unknown"));
    assert!(failure.retryability.unwrap().equals_str("unsafe"));
    assert_eq!(
        failure.details.as_deref(),
        Some(&json(r#"{"sideEffectId":"remote-1"}"#))
    );
    assert_eq!(failure.message.units().last(), Some(&0xd800));
}

#[tokio::test]
async fn contradictory_and_uncorrelated_responses_fail_before_slot_projection() {
    let cases = [
        (
            r#"{"kind":"response","messageId":"r","inReplyTo":"module-http-1","ok":true,"code":"BAD"}"#,
            "Successful module response contains failure fields.",
        ),
        (
            r#"{"kind":"response","messageId":"r","inReplyTo":"module-http-1","ok":false,"code":"BAD","error":{"code":"OTHER","message":"bad","retryability":"unsafe"}}"#,
            "Failed module response error fields are inconsistent.",
        ),
        (
            r#"{"kind":"response","messageId":"r","inReplyTo":"wrong","ok":true}"#,
            "Module response correlation does not match the request.",
        ),
        (
            r#"{"kind":"response","messageId":"r","inReplyTo":"module-http-1","requestId":"wrong","ok":true}"#,
            "Module response correlation does not match the request.",
        ),
        (
            r#"{"kind":"response","messageId":"r","inReplyTo":"module-http-1","ok":true,"outcome":"unknown"}"#,
            "Response outcome is invalid.",
        ),
        (
            r#"{"kind":"response","inReplyTo":"module-http-1","ok":true}"#,
            "Message messageId is required.",
        ),
    ];
    for (raw, expected) in cases {
        let transport =
            MockTransport::new([Action::Reply(manifest()), Action::Reply(response(200, raw))]);
        let error = client(transport).call(input()).await.unwrap_err();
        assert!(error.code.equals_str("MODULE_PROTOCOL_INCOMPATIBLE"));
        assert!(error.message.equals_str(expected), "{error:?}");
    }
}

#[tokio::test]
async fn non_json_and_http_status_keep_protocol_and_transport_errors_distinct() {
    for (status, raw, code) in [
        (200, "not json", "MODULE_PROTOCOL_INCOMPATIBLE"),
        (502, "not json", "MODULE_TRANSPORT_UNAVAILABLE"),
        (
            503,
            r#"{"kind":"response","messageId":"r","inReplyTo":"module-http-1","ok":true}"#,
            "MODULE_TRANSPORT_UNAVAILABLE",
        ),
        (
            422,
            r#"{"kind":"response","messageId":"r","inReplyTo":"module-http-1","ok":false}"#,
            "MODULE_PROTOCOL_INCOMPATIBLE",
        ),
    ] {
        let transport = MockTransport::new([
            Action::Reply(manifest()),
            Action::Reply(response(status, raw)),
        ]);
        let error = client(transport).call(input()).await.unwrap_err();
        assert!(error.code.equals_str(code), "{error:?}");
        assert_eq!(
            error.outcome.is_some(),
            code == "MODULE_TRANSPORT_UNAVAILABLE"
        );
    }
}

#[tokio::test]
async fn cancellation_before_dispatch_still_loads_shared_manifest() {
    let transport = MockTransport::new([Action::Reply(manifest())]);
    let client = client(transport.clone());
    let abort = Cancellation::new();
    abort.abort();
    let mut call = input();
    call.abort = Some(abort);
    let error = client.call(call).await.unwrap_err();
    assert!(error.code.equals_str("MODULE_ABORTED"));
    assert!(
        error
            .message
            .equals_str("Module request was cancelled before dispatch.")
    );
    assert!(error.outcome.unwrap().equals_str("result_unknown"));
    assert_eq!(transport.count(), 1);
}

#[tokio::test]
async fn cancellation_after_dispatch_drops_wait_without_replay() {
    let transport = MockTransport::new([Action::Reply(manifest()), Action::Pending]);
    let client = client(transport.clone());
    let abort = Cancellation::new();
    let mut call = input();
    call.abort = Some(abort.clone());
    let request = tokio::spawn(async move { client.call(call).await });
    transport.wait_count(2).await;
    abort.abort();
    let error = request.await.unwrap().unwrap_err();
    assert!(error.code.equals_str("MODULE_ABORTED"));
    assert!(error.outcome.unwrap().equals_str("result_unknown"));
    assert_eq!(transport.count(), 2);
}

#[tokio::test]
async fn timeout_and_disconnect_are_unknown_and_never_replayed() {
    for (action, expected) in [
        (Action::Pending, "MODULE_TIMEOUT"),
        (
            Action::Error(ModuleTransportError::Unavailable),
            "MODULE_TRANSPORT_UNAVAILABLE",
        ),
    ] {
        let transport = MockTransport::new([Action::Reply(manifest()), action]);
        let mut config = binding();
        config.timeout_ms = 5.0;
        let client = HttpModuleClient::new(config, transport.clone(), Arc::new(Env(None)));
        let error = client.call(input()).await.unwrap_err();
        assert!(error.code.equals_str(expected), "{error:?}");
        assert!(error.outcome.unwrap().equals_str("result_unknown"));
        assert_eq!(transport.count(), 2);
    }
}

#[tokio::test]
async fn credential_is_checked_after_manifest_and_trimmed_only_for_post() {
    let mut config = binding();
    config.credential_env = Some("FICTIONAL_MODULE_CREDENTIAL".into());
    let transport = MockTransport::new([Action::Reply(manifest())]);
    let client = HttpModuleClient::new(
        config.clone(),
        transport.clone(),
        Arc::new(Env(Some(" \u{feff}\n".into()))),
    );
    let error = client.call(input()).await.unwrap_err();
    assert!(
        error
            .message
            .equals_str("Knowledge module account credential is not configured.")
    );
    assert_eq!(transport.count(), 1);
    let transport = MockTransport::new([Action::Reply(manifest()), Action::Reply(success(1))]);
    let client = HttpModuleClient::new(
        config,
        transport.clone(),
        Arc::new(Env(Some("\u{feff} fictional-module-token \n".into()))),
    );
    client.call(input()).await.unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].authorization, None);
    assert_eq!(
        requests[1].authorization.as_deref(),
        Some("Bearer fictional-module-token")
    );
}

#[tokio::test]
async fn manifest_requires_every_declared_method_and_strips_extra_fields() {
    let mut config = binding();
    config.methods.push("missing".into());
    let transport = MockTransport::new([Action::Reply(manifest())]);
    let missing_client = HttpModuleClient::new(config, transport.clone(), Arc::new(Env(None)));
    let error = missing_client.manifest().await.unwrap_err();
    assert!(
        error
            .message
            .equals_str("Module manifest does not provide required method 'missing'.")
    );
    assert_eq!(transport.count(), 1);
    let transport = MockTransport::new([Action::Reply(response(
        200,
        r#"{"protocolVersion":"2.0","implementationId":"example.tools","contract":"pilotdeck.tools/v1","transport":"module-http-v2","methods":["execute","extra"],"privateField":"discard"}"#,
    ))]);
    let value = client(transport).manifest().await.unwrap();
    assert!(value.get("privateField").is_none());
    assert_eq!(value.get("methods"), Some(&json(r#"["execute","extra"]"#)));
}
