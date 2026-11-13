use super::super::http::ExternalHttpBinding;
use super::*;
use crate::skills::{
    ModuleEnvironment, ModuleHttpFuture, ModuleHttpRequest, ModuleHttpResponse,
    ModuleHttpTransport, ModuleTransportError,
};
use pilotdeck_protocol::{parse_json, stringify_json};
use std::sync::Mutex;
use std::time::Duration;

struct EmptyEnvironment;
impl ModuleEnvironment for EmptyEnvironment {
    fn var(&self, _: &str) -> Option<String> {
        None
    }
}

enum Reply {
    Payload(JsValue),
    Failure(JsValue),
    Disconnect,
    Pending,
    Held(Arc<PostGate>, Box<Reply>),
}

struct ModuleServer {
    methods: Vec<JsString>,
    replies: Mutex<VecDeque<Reply>>,
    calls: Mutex<Vec<JsValue>>,
}
impl ModuleHttpTransport for ModuleServer {
    fn send<'a>(&'a self, request: ModuleHttpRequest) -> ModuleHttpFuture<'a> {
        Box::pin(async move {
            if request.method == "GET" {
                return Ok(json_response(object([
                    ("protocolVersion", "2.0".into()),
                    ("implementationId", "example.model".into()),
                    ("contract", "pilotdeck.model/v1".into()),
                    ("transport", "module-http-v2".into()),
                    (
                        "methods",
                        JsValue::Array(
                            self.methods
                                .iter()
                                .map(|method| JsValue::String(method.clone()))
                                .collect(),
                        ),
                    ),
                ])));
            }
            let call = parse_json(std::str::from_utf8(&request.body).unwrap()).unwrap();
            self.calls.lock().unwrap().push(call.clone());
            let mut reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected model HTTP dispatch");
            while let Reply::Held(gate, next) = reply {
                gate.entered.notify_one();
                gate.release.acquire().await.unwrap().forget();
                reply = *next;
            }
            let payload = match reply {
                Reply::Disconnect => return Err(ModuleTransportError::Unavailable),
                Reply::Pending => return std::future::pending().await,
                Reply::Payload(payload) => {
                    object([("ok", JsValue::Bool(true)), ("payload", payload)])
                }
                Reply::Failure(response) => response,
                Reply::Held(_, _) => unreachable!("held reply is resolved above"),
            };
            let mut response = payload.as_object().unwrap().clone();
            response.insert("kind".into(), "response".into());
            response.insert("messageId".into(), "remote-response".into());
            response.insert("inReplyTo".into(), call.get("messageId").unwrap().clone());
            response.insert("requestId".into(), call.get("requestId").unwrap().clone());
            Ok(json_response(JsValue::Object(response)))
        })
    }
}

fn json_response(value: JsValue) -> ModuleHttpResponse {
    ModuleHttpResponse {
        status: 200,
        body: stringify_json(&value).unwrap().unwrap().into_bytes(),
    }
}

fn fixture(
    methods: &[&str],
    replies: Vec<Reply>,
    abort: Cancellation,
) -> (ExternalModelExecution, Arc<ModuleServer>) {
    let methods: Vec<_> = methods
        .iter()
        .map(|method| JsString::from(*method))
        .collect();
    let server = Arc::new(ModuleServer {
        methods: methods.clone(),
        replies: Mutex::new(replies.into()),
        calls: Mutex::new(Vec::new()),
    });
    let client = Arc::new(HttpModuleClient::new(
        ExternalHttpBinding {
            implementation_id: "example.model".into(),
            contract: "pilotdeck.model/v1".into(),
            transport: "module-http-v2".into(),
            endpoint: "http://localhost:9999".into(),
            manifest_path: "/manifest".into(),
            call_path: "/call".into(),
            methods,
            timeout_ms: 60_000.0,
            credential_env: None,
        },
        server.clone(),
        Arc::new(EmptyEnvironment),
    ));
    (ExternalModelExecution::new(client, abort), server)
}

fn request() -> JsValue {
    parse_json(r#"{"provider":"remote","model":"m","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"tools":[{"name":"lookup","inputSchema":{"type":"object"}}],"generation":{"maxOutputTokens":77},"reasoning":{"effort":"high"},"vendorExtension":{"retained":true}}"#).unwrap()
}
fn context() -> JsObject {
    parse_json(r#"{"sessionId":"session-1","turnId":"turn-1","runId":"run-1","operationId":"operation-1","idempotencyKey":"once","workspaceId":"workspace-1","storageConfigVersion":"v1","operationDeadline":"2030-01-01T00:00:00Z","metadata":{"caller":"child"},"modelOverride":{"provider":"remote","model":"m"},"abortSignal":"host-only","invocationLogSink":"host-only"}"#).unwrap().as_object().unwrap().clone()
}
fn prepared_reply() -> Reply {
    Reply::Payload(object([(
        "prepared",
        object([
            ("request", request()),
            ("provider", "selected".into()),
            ("model", "selected-model".into()),
            ("maxContextTokens", JsValue::Number(4096.0)),
            ("maxOutputTokens", JsValue::Number(80.0)),
            (
                "opaque",
                "remote opaque must not become host identity".into(),
            ),
        ]),
    )]))
}
fn events(events: &str, done: Option<bool>) -> Reply {
    let mut payload = JsObject::new();
    payload.insert("events".into(), parse_json(events).unwrap());
    if let Some(done) = done {
        payload.insert("done".into(), JsValue::Bool(done));
    }
    Reply::Payload(JsValue::Object(payload))
}
async fn prepare(port: &ExternalModelExecution) -> PreparedInvocation {
    port.prepare(PrepareInput {
        request: request(),
        context: context(),
    })
    .await
    .unwrap()
}
async fn stream(
    port: &ExternalModelExecution,
    prepared: PreparedInvocation,
) -> Box<dyn ModelStream> {
    port.stream(StreamInput {
        prepared,
        context: context(),
        abort: None,
    })
    .await
    .unwrap()
}
fn operation(call: &JsValue) -> &JsValue {
    call.get("payload").unwrap().get("operation").unwrap()
}

#[tokio::test]
async fn prepare_and_stream_preserve_complete_request_identity_and_events() {
    let canonical_events = r#"[{"type":"reasoning_delta","text":"think","vendor":{"signature":"opaque"}},{"type":"usage","usage":{"inputTokens":17,"outputTokens":4,"cacheReadTokens":9},"raw":{"retained":true}},{"type":"message_end","finishReason":"stop"}]"#;
    let (port, server) = fixture(
        &["prepare", "stream"],
        vec![prepared_reply(), events(canonical_events, None)],
        Cancellation::new(),
    );
    let prepared = prepare(&port).await;
    assert_eq!(prepared.provider, "selected".into());
    assert_eq!(prepared.max_context_tokens, Some(JsValue::Number(4096.0)));
    let mut iterator = stream(&port, prepared.clone()).await;
    assert_eq!(
        server.calls.lock().unwrap().len(),
        1,
        "stream body must stay lazy"
    );
    let mut observed = Vec::new();
    while let Some(event) = iterator.next().await.unwrap() {
        observed.push(event);
    }
    assert_eq!(
        JsValue::Array(observed),
        parse_json(canonical_events).unwrap()
    );
    let calls = server.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for call in calls.iter() {
        assert_eq!(call.get("runId"), Some(&"run-1".into()));
        assert_eq!(call.get("operationId"), Some(&"operation-1".into()));
        assert_eq!(call.get("idempotencyKey"), Some(&"once".into()));
        assert_eq!(call.get("module"), Some(&"model".into()));
        let payload = call.get("payload").unwrap();
        assert_eq!(payload.get("request"), Some(&request()));
        assert_eq!(
            payload.get("context"),
            Some(&serializable_context(&context()))
        );
        assert!(payload.get("opaque").is_none());
    }
    assert_eq!(
        calls[0].get("payload").unwrap().get("preparationId"),
        calls[1].get("payload").unwrap().get("preparationId")
    );
    assert_ne!(
        prepared.opaque,
        Some("remote opaque must not become host identity".into())
    );
}

#[tokio::test]
async fn undeclared_prepare_is_local_and_unstarted_close_has_no_http_side_effect() {
    let (port, server) = fixture(&["stream"], vec![], Cancellation::new());
    let prepared = prepare(&port).await;
    assert_eq!(prepared.request, request());
    assert_eq!(prepared.provider, "remote".into());
    let mut iterator = stream(&port, prepared).await;
    iterator.close().await.unwrap();
    assert_eq!(iterator.next().await.unwrap(), None);
    assert!(server.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn pull_events_are_lazy_and_early_close_cleans_up_even_at_terminal_batch_yield() {
    let (port, server) = fixture(
        &["prepare", "stream_next", "close_stream"],
        vec![
            prepared_reply(),
            events(
                r#"[{"type":"text_delta","text":"first"},{"type":"message_end","finishReason":"stop"}]"#,
                Some(true),
            ),
            Reply::Disconnect,
        ],
        Cancellation::new(),
    );
    let mut iterator = stream(&port, prepare(&port).await).await;
    assert!(iterator.next().await.unwrap().is_some());
    assert_eq!(server.calls.lock().unwrap().len(), 2);
    iterator.close().await.unwrap();
    assert_eq!(iterator.next().await.unwrap(), None);
    let calls = server.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(operation(&calls[2]), &"close_stream".into());
    assert_eq!(
        calls[2].get("payload").unwrap().as_object().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn pull_completion_does_not_close_and_does_not_prefetch_batches() {
    let (port, server) = fixture(
        &["prepare", "stream_next", "close_stream"],
        vec![
            prepared_reply(),
            events(r#"[{"type":"text_delta","text":"prefix"}]"#, Some(false)),
            events(
                r#"[{"type":"message_end","finishReason":"stop"}]"#,
                Some(true),
            ),
        ],
        Cancellation::new(),
    );
    let mut iterator = stream(&port, prepare(&port).await).await;
    assert!(iterator.next().await.unwrap().is_some());
    assert_eq!(server.calls.lock().unwrap().len(), 2);
    assert!(iterator.next().await.unwrap().is_some());
    assert_eq!(iterator.next().await.unwrap(), None);
    iterator.close().await.unwrap();
    assert_eq!(server.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn malformed_pull_and_cleanup_failure_preserve_the_primary_error_without_replay() {
    let (port, server) = fixture(
        &["prepare", "stream_next", "close_stream"],
        vec![prepared_reply(), events("[]", None), Reply::Disconnect],
        Cancellation::new(),
    );
    let mut iterator = stream(&port, prepare(&port).await).await;
    let error = iterator.next().await.unwrap_err();
    assert_eq!(error.code, Some("INVALID_MODEL_RESPONSE".into()));
    assert_eq!(iterator.next().await.unwrap(), None);
    assert_eq!(server.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn turn_cancellation_interrupts_prepare_and_stream_without_replay() {
    for during_prepare in [true, false] {
        let abort = Cancellation::new();
        let mut replies = Vec::new();
        if !during_prepare {
            replies.push(prepared_reply());
        }
        replies.push(Reply::Pending);
        let (port, server) = fixture(&["prepare", "stream"], replies, abort.clone());
        let prepared = if during_prepare {
            None
        } else {
            Some(prepare(&port).await)
        };
        let task = tokio::spawn(async move {
            if let Some(prepared) = prepared {
                stream(&port, prepared).await.next().await.map(|_| ())
            } else {
                port.prepare(PrepareInput {
                    request: request(),
                    context: context(),
                })
                .await
                .map(|_| ())
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if server.calls.lock().unwrap().len() == if during_prepare { 1 } else { 2 } {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        abort.abort();
        let error = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, Some("MODULE_ABORTED".into()));
        assert!(error.canonical.is_none());
        assert!(error.retryable.is_none());
        assert_eq!(
            server.calls.lock().unwrap().len(),
            if during_prepare { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn disconnect_after_dispatch_is_not_replayed_or_misclassified_as_provider_error() {
    let (port, server) = fixture(&["prepare"], vec![Reply::Disconnect], Cancellation::new());
    let error = port
        .prepare(PrepareInput {
            request: request(),
            context: context(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, Some("MODULE_TRANSPORT_UNAVAILABLE".into()));
    assert!(error.canonical.is_none());
    assert!(error.retryable.is_none());
    assert_eq!(server.calls.lock().unwrap().len(), 1);
}

#[test]
fn canonical_error_preserves_raw_provider_details_and_matches_callback_projection() {
    let response = parse_json(r#"{"ok":false,"code":"REMOTE_WRAPPER","error":{"message":"wrapper","retryable":false,"retryAfterMs":99,"canonical":{"code":"rate_limit_error","message":"provider limit","retryable":true,"retryAfterMs":25,"raw":{"opaque":1},"streamInterruption":{"phase":"reasoning"}}}}"#).unwrap();
    let error = assert_response(&response, "fallback").unwrap_err();
    assert_eq!(error.message, "provider limit".into());
    assert_eq!(
        error.canonical.as_ref(),
        response
            .get("error")
            .unwrap()
            .get("canonical")
            .unwrap()
            .as_object()
    );
    assert!(error.code.is_none());
    assert!(error.retryable.is_none());
    assert!(error.retry_after_ms.is_none());
    let ordinary = parse_json(r#"{"ok":false,"code":"REMOTE_FAILURE","error":{"message":"failed","retryable":true,"retryAfterMs":31,"retryability":"unsafe","details":{"opaque":7}},"outcome":"result_unknown"}"#).unwrap();
    let error = assert_response(&ordinary, "fallback").unwrap_err();
    assert_eq!(error.code, Some("REMOTE_FAILURE".into()));
    assert_eq!(error.retryable, Some(true));
    assert_eq!(error.retry_after_ms, Some(31.0));
}

#[tokio::test]
async fn optional_remote_materialization_is_owned_and_production_selects_independent_local_fallback()
 {
    let (port, server) = fixture(
        &["prepare", "materialize_prepared_request"],
        vec![
            prepared_reply(),
            Reply::Payload(object([("request", request())])),
        ],
        Cancellation::new(),
    );
    let prepared = prepare(&port).await;
    let materialized = port
        .materialize(prepared.clone(), request().as_object().unwrap().clone())
        .await
        .unwrap();
    assert_eq!(materialized, request());
    {
        let calls = server.calls.lock().unwrap();
        assert_eq!(operation(&calls[1]), &"materialize_prepared_request".into());
        assert_eq!(
            calls[0].get("payload").unwrap().get("preparationId"),
            calls[1].get("payload").unwrap().get("preparationId")
        );
    }
    let foreign = ExternalModelExecution::new(port.client.clone(), Cancellation::new());
    assert_eq!(
        foreign
            .materialize(prepared, request().as_object().unwrap().clone())
            .await
            .unwrap_err()
            .code,
        Some("INVALID_MODEL_RESPONSE".into())
    );
    let bundle = ports(port.client.clone(), Cancellation::new());
    assert!(bundle.metadata.is_none());
    assert!(bundle.materializer.is_some());
    assert_eq!(server.calls.lock().unwrap().len(), 2);
}

#[test]
fn malformed_preparation_and_token_limits_keep_fixed_model_contract() {
    for projection in ["null", "{}", r#"{"provider":1,"model":"m","request":{}}"#] {
        let response =
            parse_json(&format!(r#"{{"payload":{{"prepared":{projection}}}}}"#)).unwrap();
        assert_eq!(
            read_prepared(&response).unwrap_err().code,
            Some("INVALID_MODEL_PREPARATION".into())
        );
    }
    let response = parse_json(r#"{"payload":{"prepared":{"provider":"p","model":"m","request":{},"maxContextTokens":0,"maxOutputTokens":1.5}}}"#).unwrap();
    let prepared = read_prepared(&response).unwrap();
    assert!(prepared.max_context_tokens.is_none());
    assert!(prepared.max_output_tokens.is_none());
}

#[tokio::test]
async fn validated_remote_failure_retains_retry_fields_without_a_second_dispatch() {
    let failure = parse_json(r#"{"ok":false,"code":"REMOTE_BUSY","outcome":"failed","error":{"code":"REMOTE_BUSY","message":"busy","retryability":"retry_after_status","retryable":true,"retryAfterMs":123,"details":{"retainedAtHttpBoundary":true}}}"#).unwrap();
    let (port, server) = fixture(
        &["prepare"],
        vec![Reply::Failure(failure)],
        Cancellation::new(),
    );
    let error = port
        .prepare(PrepareInput {
            request: request(),
            context: context(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, Some("REMOTE_BUSY".into()));
    assert_eq!(error.message, "busy".into());
    assert_eq!(error.retryable, Some(true));
    assert_eq!(error.retry_after_ms, Some(123.0));
    assert_eq!(server.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_caller_retry_retains_preparation_but_uses_a_new_request_id() {
    let (port, server) = fixture(
        &["prepare", "stream"],
        vec![prepared_reply(), events("[]", None), events("[]", None)],
        Cancellation::new(),
    );
    let prepared = prepare(&port).await;
    for _ in 0..2 {
        assert_eq!(
            stream(&port, prepared.clone()).await.next().await.unwrap(),
            None
        );
    }
    let calls = server.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].get("payload"), calls[2].get("payload"));
    assert_ne!(calls[1].get("requestId"), calls[2].get("requestId"));
}

#[tokio::test]
async fn production_materializer_preserves_prepared_controls_and_replaces_only_context_and_route() {
    let (port, server) = fixture(
        &["prepare", "stream", "materialize_prepared_request"],
        vec![],
        Cancellation::new(),
    );
    let bundle = ports(port.client.clone(), Cancellation::new());
    let materializer = bundle.materializer.expect("external prepared fallback");
    let base = parse_json(r#"{"provider":"request-provider","model":"request-model","messages":[{"role":"user","content":[{"type":"text","text":"old"}]}],"systemPrompt":[{"type":"text","text":"prepared prompt"}],"tools":[{"name":"prepared-tool","inputSchema":{"type":"object"}}],"maxOutputTokens":128,"generation":{"temperature":0.25,"maxOutputTokens":128},"reasoning":{"effort":"high"},"providerOptions":{"special":{"enabled":true}},"cacheBreakpoints":[{"index":1}],"cachePlan":{"id":"old-cache"},"vendorExtension":{"opaque":"retain"}}"#).unwrap();
    let candidate = parse_json(r#"{"provider":"candidate-provider","model":"candidate-model","messages":[{"role":"user","content":[{"type":"text","text":"new compact window"}]}],"systemPrompt":"must not replace prepared prompt","tools":[],"maxOutputTokens":1,"generation":{"temperature":1},"reasoning":{"effort":"low"},"providerOptions":null,"cacheBreakpoints":[{"index":2}],"cachePlan":{"id":"new-cache"},"vendorExtension":null}"#).unwrap();
    let mut prepared = PreparedInvocation::new(
        base.clone(),
        "prepared-provider".into(),
        "prepared-model".into(),
    );
    prepared.opaque = Some(parse_json(r#"{"requestPatch":{"maxOutputTokens":1},"provider":"malicious-route","model":"malicious-model","externalModelOwner":"unrelated"}"#).unwrap());
    let result = materializer
        .materialize(prepared, candidate.as_object().unwrap().clone())
        .await
        .unwrap();
    assert_eq!(result.get("provider"), Some(&"prepared-provider".into()));
    assert_eq!(result.get("model"), Some(&"prepared-model".into()));
    for field in [
        "systemPrompt",
        "tools",
        "maxOutputTokens",
        "generation",
        "reasoning",
        "providerOptions",
        "vendorExtension",
    ] {
        assert_eq!(
            result.get(field),
            base.get(field),
            "prepared control {field} must survive"
        );
    }
    for field in ["messages", "cacheBreakpoints", "cachePlan"] {
        assert_eq!(
            result.get(field),
            candidate.get(field),
            "context field {field} must be replaced"
        );
    }
    assert!(
        server.calls.lock().unwrap().is_empty(),
        "the production materializer must not call the module, even when remote materialize is advertised"
    );
}

#[tokio::test]
async fn production_materializer_clears_absent_cache_fields_and_never_decodes_external_opaque() {
    let (port, server) = fixture(
        &["materialize_prepared_request"],
        vec![],
        Cancellation::new(),
    );
    let materializer = ports(port.client.clone(), Cancellation::new())
        .materializer
        .unwrap();
    let base = parse_json(r#"{"provider":"p","model":"m","messages":[],"cacheBreakpoints":[{"index":9}],"cachePlan":{"id":"stale"},"maxOutputTokens":64}"#).unwrap();
    let candidate = parse_json(r#"{"messages":[{"role":"user","content":[]}]}"#).unwrap();
    for opaque in [
        None,
        Some("external preparation is not a decision".into()),
        Some(parse_json(r#"{"requestPatch":null}"#).unwrap()),
    ] {
        let mut prepared = PreparedInvocation::new(base.clone(), "p".into(), "m".into());
        prepared.opaque = opaque;
        let result = materializer
            .materialize(prepared, candidate.as_object().unwrap().clone())
            .await
            .unwrap();
        assert_eq!(result.get("messages"), candidate.get("messages"));
        assert_eq!(result.get("cacheBreakpoints"), Some(&JsValue::Undefined));
        assert_eq!(result.get("cachePlan"), Some(&JsValue::Undefined));
        assert_eq!(result.get("maxOutputTokens"), Some(&JsValue::Number(64.0)));
        let serialized = stringify_json(&result).unwrap().unwrap();
        assert!(!serialized.contains("cacheBreakpoints"));
        assert!(!serialized.contains("cachePlan"));
    }
    assert!(server.calls.lock().unwrap().is_empty());
}

struct PostGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
impl PostGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    }
}

struct DurableTranscript {
    root: std::path::PathBuf,
    path: std::path::PathBuf,
}
impl Drop for DurableTranscript {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("EXTERNAL_MODEL_DURABLE_SCRATCH {}", self.root.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
impl DurableTranscript {
    fn entries(&self, kind: &str) -> Vec<JsValue> {
        std::fs::read_to_string(&self.path)
            .unwrap()
            .lines()
            .map(|line| parse_json(line).unwrap())
            .filter(|entry| entry.get("type") == Some(&JsValue::String(kind.into())))
            .collect()
    }
}

fn durable_port(port: ExternalModelExecution) -> (Arc<dyn ModelExecution>, DurableTranscript) {
    use crate::host::modules::model_port::{DurableRecorder, durable_execution};
    use crate::storage::{JsonlTranscriptStore, TranscriptKey, recorder::Recorder};
    let root = std::env::temp_dir().join(format!(
        "pdrs-external-model-durable-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("transcript.jsonl");
    let recorder = Arc::new(Recorder::with_sources(
        Arc::new(JsonlTranscriptStore::new()),
        TranscriptKey {
            project_root: root.clone(),
            pilot_home: root.join("pilot"),
            session_id: "session-1".into(),
            transcript_path: path.clone(),
        },
        Arc::new(|| Ok("2026-11-14T00:00:00.000Z".into())),
        Arc::new(|| Ok(uuid::Uuid::new_v4().to_string())),
    ));
    recorder
        .start_turn(&"session-1".into(), &"turn-1".into())
        .unwrap();
    let execution = durable_execution(
        Arc::new(port),
        Some(DurableRecorder {
            recorder,
            recording_session_id: None,
        }),
    );
    (execution, DurableTranscript { root, path })
}

async fn durable_stream(execution: &Arc<dyn ModelExecution>) -> Box<dyn ModelStream> {
    let prepared = execution
        .prepare(PrepareInput {
            request: request(),
            context: context(),
        })
        .await
        .unwrap();
    execution
        .stream(StreamInput {
            prepared,
            context: context(),
            abort: None,
        })
        .await
        .unwrap()
}

// A valid consumer may poll and drop a borrowing next future, then pull again.
// The production durable wrapper also does this to its inner stream each poll.
async fn poll_next_once(
    iterator: &mut dyn ModelStream,
) -> std::task::Poll<Result<Option<JsValue>, ModuleError>> {
    std::future::poll_fn(|cx| {
        let mut next = iterator.next();
        std::task::Poll::Ready(next.as_mut().poll(cx))
    })
    .await
}

async fn drive_to_post(iterator: &mut dyn ModelStream, server: &ModuleServer, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(poll_next_once(iterator).await.is_pending());
            if server.calls.lock().unwrap().len() == count {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable stream never dispatched its owned pending exchange");
}

#[tokio::test]
async fn durable_http_pending_pull_resumes_one_dispatch_and_records_request_and_event_once() {
    let gate = PostGate::new();
    let (port, server) = fixture(
        &["stream"],
        vec![Reply::Held(
            gate.clone(),
            Box::new(events(r#"[{"type":"text_delta","text":"released"}]"#, None)),
        )],
        Cancellation::new(),
    );
    let (execution, transcript) = durable_port(port);
    let mut iterator = durable_stream(&execution).await;
    drive_to_post(iterator.as_mut(), &server, 1).await;
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(poll_next_once(iterator.as_mut()).await.is_pending());
        assert_eq!(server.calls.lock().unwrap().len(), 1);
    }
    assert_eq!(transcript.entries("model_request").len(), 1);
    assert!(transcript.entries("model_stream_event").is_empty());
    gate.release.add_permits(1);
    let event = tokio::time::timeout(Duration::from_secs(5), iterator.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        parse_json(r#"{"type":"text_delta","text":"released"}"#).unwrap()
    );
    assert_eq!(iterator.next().await.unwrap(), None);
    iterator.close().await.unwrap();
    assert_eq!(server.calls.lock().unwrap().len(), 1);
    let requests = transcript.entries("model_request");
    let recorded = transcript.entries("model_stream_event");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].get("request"), Some(&request()));
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].get("event"), Some(&event));
}

#[tokio::test]
async fn durable_unstarted_close_keeps_http_and_model_records_empty() {
    let (port, server) = fixture(
        &["stream_next", "close_stream"],
        vec![],
        Cancellation::new(),
    );
    let (execution, transcript) = durable_port(port);
    let mut iterator = durable_stream(&execution).await;
    iterator.close().await.unwrap();
    iterator.close().await.unwrap();
    assert_eq!(iterator.next().await.unwrap(), None);
    assert!(server.calls.lock().unwrap().is_empty());
    assert!(transcript.entries("model_request").is_empty());
    assert!(transcript.entries("model_stream_event").is_empty());
}

#[tokio::test]
async fn durable_pull_error_survives_pending_cleanup_and_close_is_dispatched_once() {
    let gate = PostGate::new();
    let (port, server) = fixture(
        &["stream_next", "close_stream"],
        vec![
            events("[]", None),
            Reply::Held(gate.clone(), Box::new(Reply::Disconnect)),
        ],
        Cancellation::new(),
    );
    let (execution, transcript) = durable_port(port);
    let mut iterator = durable_stream(&execution).await;
    drive_to_post(iterator.as_mut(), &server, 2).await;
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(poll_next_once(iterator.as_mut()).await.is_pending());
        assert_eq!(server.calls.lock().unwrap().len(), 2);
    }
    gate.release.add_permits(1);
    let error = tokio::time::timeout(Duration::from_secs(5), iterator.next())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.code,
        Some("INVALID_MODEL_RESPONSE".into()),
        "cleanup transport failure must not replace the primary protocol error"
    );
    assert_eq!(iterator.next().await.unwrap(), None);
    iterator.close().await.unwrap();
    let operations: Vec<_> = server
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| operation(call).clone())
        .collect();
    assert_eq!(
        operations,
        vec!["stream_next".into(), "close_stream".into()]
    );
    assert_eq!(transcript.entries("model_request").len(), 1);
    assert!(transcript.entries("model_stream_event").is_empty());
}

#[tokio::test]
async fn durable_pull_early_close_resumes_pending_cleanup_without_duplicate_close_post() {
    let gate = PostGate::new();
    let (port, server) = fixture(
        &["stream_next", "close_stream"],
        vec![
            events(r#"[{"type":"text_delta","text":"prefix"}]"#, Some(false)),
            Reply::Held(
                gate.clone(),
                Box::new(Reply::Payload(object([("closed", JsValue::Bool(true))]))),
            ),
        ],
        Cancellation::new(),
    );
    let (execution, transcript) = durable_port(port);
    let mut iterator = durable_stream(&execution).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), iterator.next())
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
    for _ in 0..3 {
        let polled = std::future::poll_fn(|cx| {
            let mut close = iterator.close();
            std::task::Poll::Ready(close.as_mut().poll(cx))
        })
        .await;
        assert!(polled.is_pending());
        assert_eq!(server.calls.lock().unwrap().len(), 2);
    }
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), iterator.close())
        .await
        .unwrap()
        .unwrap();
    iterator.close().await.unwrap();
    assert_eq!(iterator.next().await.unwrap(), None);
    let operations: Vec<_> = server
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| operation(call).clone())
        .collect();
    assert_eq!(
        operations,
        vec!["stream_next".into(), "close_stream".into()]
    );
    assert_eq!(transcript.entries("model_request").len(), 1);
    assert_eq!(transcript.entries("model_stream_event").len(), 1);
}

#[tokio::test]
async fn durable_close_during_pending_pull_discards_fetch_and_closes_known_identity_once() {
    let gate = PostGate::new();
    let (port, server) = fixture(
        &["stream_next", "close_stream"],
        vec![
            Reply::Held(
                gate.clone(),
                Box::new(events(
                    r#"[{"type":"text_delta","text":"must not publish"}]"#,
                    Some(false),
                )),
            ),
            Reply::Payload(object([("closed", JsValue::Bool(true))])),
        ],
        Cancellation::new(),
    );
    let (execution, transcript) = durable_port(port);
    let mut iterator = durable_stream(&execution).await;
    drive_to_post(iterator.as_mut(), &server, 1).await;
    tokio::time::timeout(Duration::from_secs(5), iterator.close())
        .await
        .unwrap()
        .unwrap();
    gate.release.add_permits(1);
    iterator.close().await.unwrap();
    assert_eq!(iterator.next().await.unwrap(), None);
    {
        let calls = server.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(operation(&calls[0]), &"stream_next".into());
        assert_eq!(operation(&calls[1]), &"close_stream".into());
        assert_eq!(
            calls[0].get("payload").unwrap().get("preparationId"),
            calls[1].get("payload").unwrap().get("preparationId")
        );
    }
    assert_eq!(transcript.entries("model_request").len(), 1);
    assert!(transcript.entries("model_stream_event").is_empty());
}
