use super::*;
use crate::skills::{
    ModuleEnvironment, ModuleHttpFuture, ModuleHttpRequest, ModuleHttpResponse,
    ModuleHttpTransport, ModuleTransportError,
};
use pilotdeck_protocol::{parse_json, stringify_json};
use std::{collections::VecDeque, sync::Mutex};

const METHODS: [&str; 5] = [
    "prepare_for_model",
    "apply_tool_results",
    "recover_from_model_error",
    "capture_turn",
    "try_auto_compact",
];

enum Reply {
    Result(JsValue),
    NoResult,
    Failure(JsValue),
    Unavailable,
    Pending,
}

struct MockTransport {
    methods: Vec<JsString>,
    requests: Mutex<Vec<ModuleHttpRequest>>,
    replies: Mutex<VecDeque<Reply>>,
    posted: tokio::sync::Notify,
}

impl ModuleHttpTransport for MockTransport {
    fn send<'a>(&'a self, request: ModuleHttpRequest) -> ModuleHttpFuture<'a> {
        let is_get = request.method == "GET";
        let call =
            (!is_get).then(|| parse_json(std::str::from_utf8(&request.body).unwrap()).unwrap());
        self.requests.lock().unwrap().push(request);
        let reply = if is_get {
            None
        } else {
            self.replies.lock().unwrap().pop_front()
        };
        Box::pin(async move {
            let response = if is_get {
                JsValue::Object(object([
                    ("protocolVersion", "2.0".into()),
                    ("implementationId", "example.context".into()),
                    ("contract", "pilotdeck.context/v1".into()),
                    ("transport", "module-http-v2".into()),
                    (
                        "methods",
                        JsValue::Array(self.methods.iter().cloned().map(JsValue::String).collect()),
                    ),
                ]))
            } else {
                self.posted.notify_one();
                let call = call.unwrap();
                let mut response = object([
                    ("kind", "response".into()),
                    ("messageId", "response-1".into()),
                    ("inReplyTo", call.get("messageId").unwrap().clone()),
                    ("requestId", call.get("requestId").unwrap().clone()),
                    ("ok", JsValue::Bool(true)),
                ]);
                match reply.expect("unexpected external context POST") {
                    Reply::Result(result) => {
                        response.insert(
                            "payload".into(),
                            JsValue::Object(object([("result", result)])),
                        );
                    }
                    Reply::NoResult => {
                        response.insert("payload".into(), JsValue::Object(JsObject::new()));
                    }
                    Reply::Failure(error) => {
                        response.insert("ok".into(), JsValue::Bool(false));
                        response.insert("code".into(), error.get("code").unwrap().clone());
                        response.insert("outcome".into(), "failed".into());
                        response.insert("error".into(), error);
                    }
                    Reply::Unavailable => return Err(ModuleTransportError::Unavailable),
                    Reply::Pending => return std::future::pending().await,
                }
                JsValue::Object(response)
            };
            Ok(ModuleHttpResponse {
                status: 200,
                body: stringify_json(&response).unwrap().unwrap().into_bytes(),
            })
        })
    }
}

struct EmptyEnvironment;
impl ModuleEnvironment for EmptyEnvironment {
    fn var(&self, _: &str) -> Option<String> {
        None
    }
}

fn setup(methods: &[&str], replies: Vec<Reply>) -> (Arc<HttpModuleClient>, Arc<MockTransport>) {
    let binding = super::super::http::ExternalHttpBinding {
        implementation_id: "example.context".into(),
        contract: "pilotdeck.context/v1".into(),
        transport: "module-http-v2".into(),
        endpoint: "http://context.invalid/".into(),
        manifest_path: "/manifest".into(),
        call_path: "/call".into(),
        methods: methods.iter().map(|s| (*s).into()).collect(),
        timeout_ms: 10_000.0,
        credential_env: None,
    };
    let transport = Arc::new(MockTransport {
        methods: binding.methods.clone(),
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(replies.into()),
        posted: tokio::sync::Notify::new(),
    });
    (
        Arc::new(HttpModuleClient::new(
            binding,
            transport.clone(),
            Arc::new(EmptyEnvironment),
        )),
        transport,
    )
}

fn input(fields: JsObject) -> ContextInput {
    ContextInput {
        fields,
        abort_signal: None,
        budget_evaluator: None,
    }
}

fn posts(transport: &MockTransport) -> Vec<JsValue> {
    transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| parse_json(std::str::from_utf8(&request.body).unwrap()).unwrap())
        .collect()
}

#[test]
fn requires_prepare_and_rejects_unadvertised_methods_without_http() {
    let (client, transport) = setup(&["capture_turn"], Vec::new());
    let error = match context_runtime(client, "session".into(), None) {
        Ok(_) => panic!("missing prepare_for_model must reject construction"),
        Err(error) => error,
    };
    assert_eq!(
        error.message,
        "Host context module must support prepare_for_model."
    );
    assert!(transport.requests.lock().unwrap().is_empty());

    let (client, _) = setup(&["prepare_for_model", "capture_turn"], Vec::new());
    let runtime = context_runtime(client, "session".into(), None).unwrap();
    assert!(runtime.supports(Operation::PrepareForModel));
    assert!(runtime.supports(Operation::CaptureTurn));
    assert!(!runtime.supports(Operation::ApplyToolResults));
    assert!(!runtime.supports(Operation::RecoverFromModelError));
    assert!(!runtime.supports(Operation::TryAutoCompact));
}

#[tokio::test]
async fn preserves_session_envelope_input_history_and_materialization() {
    let history = parse_json(r#"[{"role":"user","content":[{"type":"text","text":"hello"}],"metadata":{"opaque":[null,false,0]}}]"#).unwrap();
    let result = JsValue::Object(object([
        ("messages", history.clone()),
        ("materialization", parse_json(r#"{"promptGeneration":4,"runtimeContexts":[{"name":"host","text":"external context"}]}"#).unwrap()),
        ("usage", JsValue::Null),
    ]));
    let (client, transport) = setup(&["prepare_for_model"], vec![Reply::Result(result.clone())]);
    let session = JsString::from_utf16(vec![b's' as u16, 0xd800]);
    let runtime = context_runtime(client, session.clone(), None).unwrap();
    let mut fields = object([
        ("sessionId", "input-session".into()),
        ("turnId", "turn-2".into()),
        ("messages", history),
        ("runtimeContextSurface", "user_message".into()),
        ("abortSignal", "do not serialize".into()),
        ("budgetEvaluator", "do not serialize".into()),
        ("budgetStage", "routed".into()),
        ("opaque", JsValue::Null),
    ]);
    fields.insert("undefined".into(), JsValue::Undefined);
    assert_eq!(
        runtime.prepare_for_model(input(fields)).await.unwrap(),
        result
    );
    let sent = posts(&transport);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].get("runId"), Some(&JsValue::String(session)));
    assert_eq!(
        sent[0].get("operationId"),
        Some(&JsValue::String(JsString::from_utf16(vec![
            99, 111, 110, 116, 101, 120, 116, 45, 115, 0xd800
        ])))
    );
    assert_eq!(sent[0].get("module"), Some(&JsValue::from("context")));
    let request_id = sent[0].get("requestId").unwrap().as_string().unwrap();
    assert!(String::from_utf16_lossy(request_id.units()).starts_with("context-prepare_for_model-"));
    let payload = sent[0].get("payload").unwrap();
    assert_eq!(
        payload.get("operation"),
        Some(&JsValue::from("prepare_for_model"))
    );
    let fields = payload.get("input").unwrap();
    for omitted in ["abortSignal", "budgetEvaluator", "budgetStage", "undefined"] {
        assert!(fields.get(omitted).is_none());
    }
    assert_eq!(
        fields.get("sessionId"),
        Some(&JsValue::from("input-session"))
    );
    assert_eq!(fields.get("turnId"), Some(&JsValue::from("turn-2")));
    assert_eq!(fields.get("messages"), result.get("messages"));
    assert_eq!(fields.get("opaque"), Some(&JsValue::Null));
    assert!(sent[0].get("idempotencyKey").is_none());
}

#[tokio::test]
async fn dispatches_all_declared_methods_and_preserves_falsy_results() {
    let (client, transport) = setup(
        &METHODS,
        vec![
            Reply::Result(JsValue::Null),
            Reply::Result(JsValue::Bool(false)),
            Reply::Result(JsValue::Number(0.0)),
            Reply::Result("ignored capture value".into()),
            Reply::Result(JsValue::Array(Vec::new())),
        ],
    );
    let runtime = context_runtime(client, "s".into(), None).unwrap();
    assert_eq!(
        runtime
            .prepare_for_model(input(JsObject::new()))
            .await
            .unwrap(),
        JsValue::Null
    );
    assert_eq!(
        runtime
            .apply_tool_results(input(JsObject::new()))
            .await
            .unwrap(),
        JsValue::Bool(false)
    );
    assert_eq!(
        runtime
            .recover_from_model_error(input(JsObject::new()))
            .await
            .unwrap(),
        JsValue::Number(0.0)
    );
    assert_eq!(
        runtime.capture_turn(input(JsObject::new())).await.unwrap(),
        JsValue::Undefined
    );
    assert_eq!(
        runtime
            .try_auto_compact(input(JsObject::new()))
            .await
            .unwrap(),
        JsValue::Array(Vec::new())
    );
    let sent = posts(&transport);
    assert_eq!(sent.len(), METHODS.len());
    for (call, method) in sent.iter().zip(METHODS) {
        assert_eq!(
            call.get("payload").and_then(|p| p.get("operation")),
            Some(&JsValue::from(method))
        );
    }
    assert_eq!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "GET")
            .count(),
        1
    );
}

#[test]
fn compaction_projects_intent_and_preserves_request_preparation_and_calibration() {
    let source = parse_json(r#"{"messages":[],"budgetStage":"recovery","trigger":"manual","manualForce":false,"reservedOutputTokens":0,"maxContextTokens":null,"allowFallbackOnFailure":false,"budgetRequest":{"provider":"p","model":"m","messages":[],"metadata":{"x":[null,false]}},"budgetPreparation":{"provider":"p","model":"m","tools":[]},"budgetPreparationId":"prepared-7","budgetCalibration":{"provider":"p","model":"m","actualInputTokens":10},"abortSignal":{},"budgetEvaluator":{}}"#).unwrap();
    let source = source.as_object().unwrap();
    let serialized = serialize_input(Operation::TryAutoCompact, source.clone());
    for key in [
        "budgetRequest",
        "budgetPreparation",
        "budgetPreparationId",
        "budgetCalibration",
    ] {
        assert_eq!(serialized.get(key), source.get(key));
    }
    for key in ["budgetStage", "abortSignal", "budgetEvaluator"] {
        assert!(serialized.get(key).is_none());
    }
    let projection = serialized.get("budgetProjection").unwrap();
    assert_eq!(projection, &parse_json(r#"{"stage":"recovery","trigger":"manual","maxContextTokens":null,"reservedOutputTokens":0,"manualForce":false,"allowFallbackOnFailure":false}"#).unwrap());
    for key in ["request", "preparation", "calibration"] {
        assert!(projection.get(key).is_none());
    }
}

#[test]
fn compaction_default_stage_observes_null_undefined_and_strict_true() {
    for (source, expected) in [
        (r#"{}"#, r#"{"stage":"pre_route","trigger":"auto"}"#),
        (
            r#"{"maxContextTokens":null,"trigger":null}"#,
            r#"{"stage":"routed","trigger":"auto","maxContextTokens":null}"#,
        ),
        (
            r#"{"allowFallbackOnFailure":true}"#,
            r#"{"stage":"recovery","trigger":"auto","allowFallbackOnFailure":true}"#,
        ),
        (
            r#"{"allowFallbackOnFailure":1}"#,
            r#"{"stage":"pre_route","trigger":"auto","allowFallbackOnFailure":1}"#,
        ),
        (
            r#"{"budgetStage":"custom","trigger":false}"#,
            r#"{"stage":"custom","trigger":false}"#,
        ),
    ] {
        let fields = parse_json(source).unwrap().as_object().unwrap().clone();
        let result = serialize_input(Operation::TryAutoCompact, fields);
        assert_eq!(
            result.get("budgetProjection"),
            Some(&parse_json(expected).unwrap()),
            "{source}"
        );
    }
}

#[tokio::test]
async fn missing_result_and_unadvertised_operation_fail_without_fallback() {
    let (client, transport) = setup(&["prepare_for_model"], vec![Reply::NoResult]);
    let runtime = context_runtime(client, "s".into(), None).unwrap();
    let error = runtime
        .prepare_for_model(input(JsObject::new()))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "Context module prepare_for_model returned no result."
    );
    let error = runtime
        .apply_tool_results(input(JsObject::new()))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "Host context capability does not support apply_tool_results."
    );
    assert_eq!(posts(&transport).len(), 1);
}

#[tokio::test]
async fn context_failure_keeps_utf16_code_message_without_changing_public_projection() {
    let message = JsString::from_utf16(vec![b'e' as u16, 0xd800]);
    let code = JsString::from_utf16(vec![b'c' as u16, 0xdc00]);
    let error = JsValue::Object(object([
        ("code", JsValue::String(code.clone())),
        ("message", JsValue::String(message.clone())),
        ("retryability", "unsafe".into()),
        ("details", parse_json(r#"{"opaque":[1,null]}"#).unwrap()),
    ]));
    let (client, transport) = setup(&["prepare_for_model"], vec![Reply::Failure(error)]);
    let runtime = context_runtime(client, "s".into(), None).unwrap();
    let error = runtime
        .prepare_for_model(input(JsObject::new()))
        .await
        .unwrap_err();
    let failure = error.module_error.unwrap();
    assert_eq!(failure.message, message);
    assert_eq!(failure.code, Some(code));
    assert!(failure.canonical.is_none());
    assert!(failure.retryable.is_none());
    assert!(failure.retry_after_ms.is_none());
    assert_eq!(posts(&transport).len(), 1);
}

#[tokio::test]
async fn transport_failure_and_abort_are_not_replayed() {
    let (client, transport) = setup(
        &["prepare_for_model"],
        vec![Reply::Unavailable, Reply::Pending],
    );
    let signal = Cancellation::new();
    let runtime = context_runtime(client, "s".into(), Some(signal.clone())).unwrap();
    let error = runtime
        .prepare_for_model(input(JsObject::new()))
        .await
        .unwrap_err();
    assert_eq!(
        error.module_error.unwrap().code,
        Some("MODULE_TRANSPORT_UNAVAILABLE".into())
    );
    assert_eq!(posts(&transport).len(), 1);
    // Drain the notification from the completed first POST.
    transport.posted.notified().await;
    let task = tokio::spawn(async move { runtime.prepare_for_model(input(JsObject::new())).await });
    transport.posted.notified().await;
    signal.abort();
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(
        error.module_error.unwrap().code,
        Some("MODULE_ABORTED".into())
    );
    assert_eq!(posts(&transport).len(), 2);
}

#[tokio::test]
async fn input_signal_takes_precedence_and_never_crosses_wire() {
    let (client, transport) = setup(&["prepare_for_model"], Vec::new());
    let turn_signal = Cancellation::new();
    let input_signal = Cancellation::new();
    input_signal.abort();
    let runtime = context_runtime(client, "s".into(), Some(turn_signal)).unwrap();
    let mut request = input(JsObject::new());
    request.abort_signal = Some(Arc::new(input_signal));
    let error = runtime.prepare_for_model(request).await.unwrap_err();
    assert_eq!(
        error.module_error.unwrap().code,
        Some("MODULE_ABORTED".into())
    );
    // Manifest loading follows TS's shared flight, independent of caller abort.
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(posts(&transport).is_empty());
}

#[tokio::test]
async fn shared_generation_client_does_not_mix_concurrent_session_identity() {
    let (client, transport) = setup(
        &["prepare_for_model"],
        vec![Reply::Result(JsValue::Null), Reply::Result(JsValue::Null)],
    );
    let first = context_runtime(client.clone(), "session-a".into(), None).unwrap();
    let second = context_runtime(client, "session-b".into(), None).unwrap();
    let (first_result, second_result) = tokio::join!(
        first.prepare_for_model(input(object([("turnId", "turn-a".into())]))),
        second.prepare_for_model(input(object([("turnId", "turn-b".into())]))),
    );
    first_result.unwrap();
    second_result.unwrap();
    let sent = posts(&transport);
    assert_eq!(sent.len(), 2);
    for (session, turn) in [("session-a", "turn-a"), ("session-b", "turn-b")] {
        let call = sent
            .iter()
            .find(|call| call.get("runId") == Some(&JsValue::from(session)))
            .unwrap();
        assert_eq!(
            call.get("operationId"),
            Some(&JsValue::String(format!("context-{session}").into()))
        );
        assert_eq!(
            call.get("payload")
                .and_then(|p| p.get("input"))
                .and_then(|i| i.get("turnId")),
            Some(&JsValue::from(turn))
        );
    }
    assert_ne!(sent[0].get("requestId"), sent[1].get("requestId"));
}
