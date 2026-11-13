use super::super::http::ExternalHttpBinding;
use super::*;
use crate::skills::{
    ModuleEnvironment, ModuleHttpRequest, ModuleHttpResponse, ModuleHttpTransport,
    ModuleTransportError,
};
use pilotdeck_protocol::{parse_json, stringify_json};
use std::sync::{Mutex, atomic::AtomicUsize};
use tokio::sync::Barrier;

struct NoEnv;
impl ModuleEnvironment for NoEnv {
    fn var(&self, _: &str) -> Option<String> {
        None
    }
}
#[derive(Clone, Copy)]
enum Reply {
    Single,
    Batch,
    ShortBatch,
    WrongBatchId,
    ModuleFailure,
    Unknown,
    Cancel,
    Timeout,
    Disconnect,
    Wait,
}
struct Wire {
    mode: Reply,
    methods: Vec<String>,
    posts: AtomicUsize,
    requests: Mutex<Vec<JsValue>>,
}
impl ModuleHttpTransport for Wire {
    fn send<'a>(&'a self, request: ModuleHttpRequest) -> crate::skills::ModuleHttpFuture<'a> {
        Box::pin(async move {
            if request.method == "GET" {
                return Ok(reply(JsValue::Object(object([
                    ("protocolVersion", "2.0".into()),
                    ("implementationId", "test.tools".into()),
                    ("contract", "pilotdeck.tools/v1".into()),
                    ("transport", "module-http-v2".into()),
                    (
                        "methods",
                        JsValue::Array(self.methods.iter().map(|v| v.as_str().into()).collect()),
                    ),
                ]))));
            }
            self.posts.fetch_add(1, Ordering::SeqCst);
            let request = parse_json(std::str::from_utf8(&request.body).unwrap()).unwrap();
            self.requests.lock().unwrap().push(request.clone());
            match self.mode {
                Reply::Timeout => return Err(ModuleTransportError::Timeout),
                Reply::Disconnect => return Err(ModuleTransportError::Unavailable),
                Reply::Wait => return std::future::pending().await,
                _ => {}
            }
            let mut response = object([
                ("kind", "response".into()),
                (
                    "messageId",
                    JsValue::String(
                        format!(
                            "response-{}",
                            text(request.get("messageId").unwrap().as_string().unwrap())
                        )
                        .into(),
                    ),
                ),
                ("inReplyTo", request.get("messageId").unwrap().clone()),
                ("requestId", request.get("requestId").unwrap().clone()),
                ("ok", JsValue::Bool(true)),
            ]);
            let payload = request.get("payload").unwrap();
            match self.mode {
                Reply::Batch | Reply::ShortBatch | Reply::WrongBatchId => {
                    let JsValue::Array(calls) = payload.get("calls").unwrap() else {
                        panic!("expected execute_batch")
                    };
                    let mut results: Vec<_> = calls
                        .iter()
                        .map(|call| {
                            JsValue::Object(object([
                                ("type", "success".into()),
                                ("toolCallId", call.get("toolCallId").unwrap().clone()),
                                ("data", call.get("arguments").unwrap().clone()),
                            ]))
                        })
                        .collect();
                    if matches!(self.mode, Reply::ShortBatch) {
                        results.pop();
                    }
                    if matches!(self.mode, Reply::WrongBatchId) {
                        results[0] = JsValue::Object(object([
                            ("type", "success".into()),
                            ("toolCallId", "other".into()),
                        ]));
                    }
                    response.insert(
                        "payload".into(),
                        JsValue::Object(object([("results", JsValue::Array(results))])),
                    );
                }
                Reply::ModuleFailure | Reply::Unknown | Reply::Cancel => {
                    let code = if matches!(self.mode, Reply::Cancel) {
                        "ABORTED"
                    } else {
                        "remote_failure"
                    };
                    response.insert("ok".into(), JsValue::Bool(false));
                    response.insert("code".into(), code.into());
                    response.insert(
                        "outcome".into(),
                        if matches!(self.mode, Reply::Unknown) {
                            "result_unknown".into()
                        } else {
                            "failed".into()
                        },
                    );
                    response.insert(
                        "error".into(),
                        JsValue::Object(object([
                            ("code", code.into()),
                            ("message", "remote message".into()),
                            ("retryability", "unsafe".into()),
                            (
                                "details",
                                JsValue::Object(object([("reason", "kept".into())])),
                            ),
                        ])),
                    );
                }
                _ => {
                    response.insert(
                        "payload".into(),
                        JsValue::Object(object([
                            ("type", "success".into()),
                            ("toolCallId", payload.get("toolCallId").unwrap().clone()),
                            ("data", payload.get("arguments").unwrap().clone()),
                        ])),
                    );
                }
            }
            Ok(reply(JsValue::Object(response)))
        })
    }
}
fn reply(value: JsValue) -> ModuleHttpResponse {
    ModuleHttpResponse {
        status: 200,
        body: stringify_json(&value).unwrap().unwrap().into_bytes(),
    }
}
fn descriptor(name: &str, safe: bool) -> JsObject {
    object([
        ("name", name.into()),
        ("description", "remote descriptor".into()),
        (
            "inputSchema",
            JsValue::Object(object([("type", "object".into())])),
        ),
        ("readOnly", JsValue::Bool(true)),
        ("concurrencySafe", JsValue::Bool(safe)),
        ("requiresUserInteraction", JsValue::Bool(true)),
    ])
}
fn fixture(mode: Reply, methods: &[&str]) -> (ExternalToolExecution, Arc<Wire>) {
    let wire = Arc::new(Wire {
        mode,
        methods: methods.iter().map(|s| s.to_string()).collect(),
        posts: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let binding = ExternalHttpBinding {
        implementation_id: "test.tools".into(),
        contract: "pilotdeck.tools/v1".into(),
        transport: "module-http-v2".into(),
        endpoint: "http://localhost:1/root/".into(),
        manifest_path: "/manifest".into(),
        call_path: "/call".into(),
        methods: wire.methods.iter().cloned().map(Into::into).collect(),
        timeout_ms: 1000.0,
        credential_env: None,
    };
    let identity = ExecutionIdentity {
        run_id: "run".into(),
        operation_id: "operation".into(),
        request_id: "request".into(),
        session_id: "session".into(),
        turn_id: "turn".into(),
    };
    (
        ExternalToolExecution::new(
            Arc::new(HttpModuleClient::new(
                binding,
                wire.clone(),
                Arc::new(NoEnv),
            )),
            vec![descriptor("remote", false), descriptor("safe", true)],
            identity,
            Cancellation::default(),
            Arc::new(AtomicU64::new(0)),
        ),
        wire,
    )
}
fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: JsValue::Object(object([("original", JsValue::Bool(true))])),
    }
}
fn context() -> ToolInvocationContext {
    use crate::host::modules::tool_context::ToolFileState;
    ToolInvocationContext {
        runtime: Arc::new(ToolRuntimeContext {
            fields: object([
                ("sessionId", "session".into()),
                ("turnId", "turn".into()),
                ("cwd", "/isolated".into()),
                ("permissionMode", "default".into()),
                ("runMode", "agent".into()),
                ("maxResultBytes", JsValue::Number(4096.0)),
                ("outputTruncated", JsValue::Bool(false)),
                ("privateHandle", "must not cross wire".into()),
            ]),
            files: ToolFileState {
                read_file_state: Arc::new(Mutex::new(JsObject::new())),
                write_snapshots: Arc::new(Mutex::new(JsObject::new())),
                allowed_read_files: Vec::new(),
            },
            abort_signal: None,
            model: None,
            elicitation: None,
            user_dialog: None,
            audit_recorder: None,
            file_history: None,
            file_update_notifier: None,
            now: None,
            progress: None,
            subagent: None,
            plan_todo: None,
            goal: None,
            plan_directory: None,
        }),
        execution: Arc::new(ExecutionContext {
            fields: object([
                ("runId", "actual-run".into()),
                ("operationId", "actual-operation".into()),
                ("turnId", "turn".into()),
                ("idempotencyKey", "idem".into()),
                ("privateHandle", "must not cross wire".into()),
            ]),
            abort_signal: None,
        }),
    }
}

#[tokio::test]
async fn static_descriptor_and_single_payload_match_fixed_capability_contract() {
    let (port, wire) = fixture(Reply::Single, &["execute"]);
    let tools = port.list().await.unwrap();
    let descriptor = crate::host::modules::capability::serialize_tool_descriptor(tools[0].as_ref());
    assert_eq!(descriptor.get("kind"), Some(&"custom".into()));
    for field in ["readOnly", "requiresUserInteraction"] {
        assert_eq!(descriptor.get(field), Some(&JsValue::Bool(true)));
    }
    assert_eq!(
        descriptor.get("concurrencySafe"),
        Some(&JsValue::Bool(false))
    );
    let result = port
        .execute_all(vec![call("one", "remote")], context())
        .await
        .unwrap();
    assert_eq!(result[0].get("toolCallId"), Some(&"one".into()));
    assert_eq!(result[0].get("type"), Some(&"success".into()));
    assert_eq!(result[0].get("data"), Some(&call("one", "remote").input));
    let requests = wire.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.get("module"), Some(&"capability".into()));
    assert_eq!(request.get("runId"), Some(&"actual-run".into()));
    assert_eq!(request.get("operationId"), Some(&"actual-operation".into()));
    assert_eq!(request.get("idempotencyKey"), Some(&"idem".into()));
    let payload = request.get("payload").unwrap();
    assert!(payload.get("operation").is_none());
    assert_eq!(payload.get("arguments"), Some(&call("one", "remote").input));
    assert!(
        payload
            .get("context")
            .unwrap()
            .get("privateHandle")
            .is_none()
    );
    assert!(
        payload
            .get("execution")
            .unwrap()
            .get("privateHandle")
            .is_none()
    );
    assert!(request.get("abortSignal").is_none());
}

#[tokio::test]
async fn advertised_batch_is_one_dispatch_and_validates_result_length_and_identity() {
    let (port, wire) = fixture(Reply::Batch, &["execute_batch"]);
    let result = port
        .execute_all(vec![call("one", "remote"), call("two", "safe")], context())
        .await
        .unwrap();
    assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
    assert!(
        result
            .iter()
            .all(|result| result.get("type") == Some(&"success".into()))
    );
    assert_eq!(
        result
            .iter()
            .map(|result| result.get("toolCallId").unwrap().clone())
            .collect::<Vec<_>>(),
        vec!["one".into(), "two".into()]
    );
    assert_eq!(
        wire.requests.lock().unwrap()[0]
            .get("payload")
            .unwrap()
            .get("operation"),
        Some(&"execute_batch".into())
    );
    for (mode, expected) in [
        (
            Reply::ShortBatch,
            "Capability batch response must contain one result for every tool call.",
        ),
        (
            Reply::WrongBatchId,
            "Capability batch result 0 does not match tool call one.",
        ),
    ] {
        let (port, wire) = fixture(mode, &["execute_batch"]);
        let error = port
            .execute_all(vec![call("one", "remote")], context())
            .await
            .unwrap_err();
        assert_eq!(error.message, expected);
        assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn static_catalog_rejects_list_tools_without_dispatch() {
    let (port, wire) = fixture(Reply::Single, &["execute", "list_tools"]);
    let error = port.refresh().await.unwrap_err();
    assert_eq!(
        error.message,
        "Host advertised list_tools without a sidecar tool descriptor decoder."
    );
    assert_eq!(wire.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn timeout_disconnect_and_remote_failure_preserve_public_shapes_without_replay() {
    for (mode, expected, module) in [
        (Reply::Timeout, "tool_timeout", "MODULE_TIMEOUT"),
        (
            Reply::Disconnect,
            "tool_execution_failed",
            "MODULE_TRANSPORT_UNAVAILABLE",
        ),
    ] {
        let (port, wire) = fixture(mode, &["execute"]);
        let results = port
            .execute_all(vec![call("one", "remote")], context())
            .await
            .unwrap();
        let error = results[0].get("error").unwrap();
        assert_eq!(error.get("code"), Some(&expected.into()));
        assert_eq!(
            error.get("details"),
            Some(&JsValue::Object(object([
                ("moduleCode", module.into()),
                ("outcome", "result_unknown".into()),
                ("retryability", "unsafe".into())
            ])))
        );
        assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
    }
    let (port, wire) = fixture(Reply::ModuleFailure, &["execute"]);
    let results = port
        .execute_all(vec![call("one", "remote")], context())
        .await
        .unwrap();
    let error = results[0].get("error").unwrap();
    assert_eq!(error.get("code"), Some(&"tool_execution_failed".into()));
    assert_eq!(error.get("message"), Some(&"remote message".into()));
    assert_eq!(
        error
            .get("details")
            .unwrap()
            .get("moduleError")
            .unwrap()
            .get("details"),
        Some(&JsValue::Object(object([("reason", "kept".into())])))
    );
    assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_unknown_and_cancellation_reject_without_success_or_replay() {
    let (port, wire) = fixture(Reply::Unknown, &["execute"]);
    let error = port
        .execute_all(vec![call("one", "remote")], context())
        .await
        .unwrap_err();
    assert_eq!(
        error.module_error.unwrap().code,
        Some("RESULT_UNKNOWN".into())
    );
    assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
    let (port, wire) = fixture(Reply::Cancel, &["execute"]);
    assert_eq!(
        port.execute_all(vec![call("one", "remote")], context())
            .await
            .unwrap_err()
            .message,
        "Tool execution cancelled."
    );
    assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
    let (port, wire) = fixture(Reply::Wait, &["execute"]);
    let signal = port.signal.clone();
    let task = tokio::spawn(port.execute_all(vec![call("one", "remote")], context()));
    while wire.posts.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    signal.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err().message, "Tool execution cancelled.");
    assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
}

struct Owner {
    name: &'static str,
    tools: Vec<Arc<dyn ToolDefinition>>,
    log: Arc<Mutex<Vec<String>>>,
    barrier: Arc<Barrier>,
    incomplete: bool,
}
impl ToolCatalog for Owner {
    fn list(&self) -> HostFuture<Vec<Arc<dyn ToolDefinition>>> {
        let tools = self.tools.clone();
        Box::pin(async move { Ok(tools) })
    }
}
impl ToolExecution for Owner {
    fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        _: ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let (name, log, barrier, incomplete) = (
            self.name,
            self.log.clone(),
            self.barrier.clone(),
            self.incomplete,
        );
        Box::pin(async move {
            log.lock().unwrap().push(format!(
                "{name}:{}",
                calls
                    .iter()
                    .map(|call| text(&call.name))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            if calls
                .iter()
                .any(|call| call.name.equals_str("es") || call.name.equals_str("ns"))
            {
                barrier.wait().await;
            }
            if incomplete {
                return Ok(Vec::new());
            }
            Ok(calls
                .into_iter()
                .map(|call| {
                    object([
                        ("type", "success".into()),
                        ("toolCallId", JsValue::String(call.id)),
                    ])
                })
                .collect())
        })
    }
}
fn owner(
    name: &'static str,
    names: &[(&str, bool)],
    log: Arc<Mutex<Vec<String>>>,
    barrier: Arc<Barrier>,
    incomplete: bool,
) -> Arc<dyn ToolExecution> {
    Arc::new(Owner {
        name,
        tools: names
            .iter()
            .map(|(name, safe)| {
                Arc::new(StaticTool::new(descriptor(name, *safe))) as Arc<dyn ToolDefinition>
            })
            .collect(),
        log,
        barrier,
        incomplete,
    })
}
#[tokio::test]
async fn composed_owners_run_safe_groups_concurrently_then_global_unsafe_order_and_restore_slots() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(2));
    let external = owner(
        "external",
        &[("es", true), ("eu", false)],
        log.clone(),
        barrier.clone(),
        false,
    );
    let native = owner(
        "native",
        &[("ns", true), ("nu", false)],
        log.clone(),
        barrier,
        false,
    );
    let port = compose_tool_ports(external, native).await.unwrap();
    assert_eq!(port.list().await.unwrap().len(), 4);
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        port.execute_all(
            vec![
                call("0", "eu"),
                call("1", "ns"),
                call("2", "nu"),
                call("3", "es"),
                call("4", "eu"),
                call("5", "missing"),
            ],
            context(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.get("toolCallId").unwrap().clone())
            .collect::<Vec<_>>(),
        (0..6)
            .map(|i| JsValue::String(i.to_string().into()))
            .collect::<Vec<_>>()
    );
    let log = log.lock().unwrap();
    assert!(log[..2].contains(&"external:es".to_string()));
    assert!(log[..2].contains(&"native:ns".to_string()));
    assert_eq!(
        log[2..],
        ["external:eu", "native:nu", "external:eu", "native:missing"]
    );
}
#[tokio::test]
async fn composed_duplicate_and_incomplete_results_fail_closed() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(2));
    let duplicate = compose_tool_ports(
        owner(
            "external",
            &[("same", false)],
            log.clone(),
            barrier.clone(),
            false,
        ),
        owner(
            "native",
            &[("same", false)],
            log.clone(),
            barrier.clone(),
            false,
        ),
    )
    .await;
    assert_eq!(
        duplicate.err().unwrap().message,
        "External tool binding conflicts with host tool 'same'."
    );
    assert!(log.lock().unwrap().is_empty());
    let port = compose_tool_ports(
        owner(
            "external",
            &[("remote", false)],
            log.clone(),
            barrier.clone(),
            true,
        ),
        owner("native", &[], log, barrier, false),
    )
    .await
    .unwrap();
    assert_eq!(
        port.execute_all(vec![call("0", "remote")], context())
            .await
            .unwrap_err()
            .message,
        "Composed tool port returned an incomplete result set."
    );
}

struct Decision(JsValue, Arc<Mutex<Vec<JsValue>>>);
impl crate::host::modules::permission::PermissionDecision for Decision {
    fn decide(
        &self,
        _: Arc<dyn ToolDefinition>,
        input: JsValue,
        _: crate::host::modules::capability::RuntimeContext,
        _: JsString,
    ) -> HostFuture<JsValue> {
        self.1.lock().unwrap().push(input);
        let decision = self.0.clone();
        Box::pin(async move { Ok(decision) })
    }
}
#[tokio::test]
async fn permission_denial_and_cancel_send_nothing_and_updated_input_preserves_batch() {
    for (kind, expected) in [
        ("deny", "permission_denied"),
        ("cancel", "permission_cancelled"),
        ("ask", "permission_required"),
    ] {
        let (raw, wire) = fixture(Reply::Single, &["execute"]);
        let decisions = Arc::new(Mutex::new(Vec::new()));
        let port = permission_aware_tool_port(
            Arc::new(raw),
            Arc::new(Decision(
                JsValue::Object(object([
                    ("type", kind.into()),
                    ("message", "blocked".into()),
                ])),
                decisions.clone(),
            )),
            false,
        );
        let result = port
            .execute_all(vec![call("one", "remote")], context())
            .await
            .unwrap();
        assert_eq!(
            result[0].get("error").unwrap().get("code"),
            Some(&expected.into())
        );
        assert_eq!(wire.posts.load(Ordering::SeqCst), 0);
        assert_eq!(decisions.lock().unwrap().len(), 1);
    }
    let (raw, wire) = fixture(Reply::Batch, &["execute_batch"]);
    let input = object([("approved", JsValue::Bool(true))]);
    let port = permission_aware_tool_port(
        Arc::new(raw),
        Arc::new(Decision(
            JsValue::Object(object([
                ("type", "allow".into()),
                ("updatedInput", JsValue::Object(input.clone())),
            ])),
            Arc::new(Mutex::new(Vec::new())),
        )),
        true,
    );
    let result = port
        .execute_all(vec![call("one", "remote"), call("two", "safe")], context())
        .await
        .unwrap();
    assert_eq!(wire.posts.load(Ordering::SeqCst), 1);
    assert!(
        result
            .iter()
            .all(|result| result.get("data") == Some(&JsValue::Object(input.clone())))
    );
}

#[tokio::test]
async fn per_turn_identity_and_abort_are_fresh_while_request_sequence_is_session_owned() {
    let (first, wire) = fixture(Reply::Single, &["execute"]);
    first
        .execute_all(vec![call("one", "remote")], context())
        .await
        .unwrap();
    first.signal.cancel();
    let mut identity = first.identity.clone();
    identity.turn_id = "next-turn".into();
    let next = ExternalToolExecution::new(
        first.client.clone(),
        vec![descriptor("remote", false)],
        identity,
        Cancellation::default(),
        first.sequence.clone(),
    );
    next.execute_all(vec![call("two", "remote")], context())
        .await
        .unwrap();
    let requests = wire.requests.lock().unwrap();
    assert_eq!(requests[0].get("requestId"), Some(&"tool-1".into()));
    assert_eq!(requests[1].get("requestId"), Some(&"tool-2".into()));
    assert_eq!(wire.posts.load(Ordering::SeqCst), 2);
}

#[test]
fn static_catalog_keeps_kind_and_requires_literal_true_for_flags() {
    let tool = StaticTool::new(object([
        ("name", "remote".into()),
        ("kind", "mcp".into()),
        ("readOnly", "true".into()),
        ("concurrencySafe", JsValue::Bool(true)),
    ]));
    let descriptor = crate::host::modules::capability::serialize_tool_descriptor(&tool);
    assert_eq!(descriptor.get("kind"), Some(&"mcp".into()));
    assert_eq!(descriptor.get("readOnly"), Some(&JsValue::Bool(false)));
    assert_eq!(
        descriptor.get("concurrencySafe"),
        Some(&JsValue::Bool(true))
    );
    assert_eq!(
        descriptor.get("requiresUserInteraction"),
        Some(&JsValue::Bool(false))
    );
}

#[derive(Default)]
struct HeldWork {
    started: AtomicUsize,
    effects: AtomicUsize,
    disposed: AtomicUsize,
    unsafe_started: AtomicUsize,
    release: tokio::sync::Notify,
    finished: tokio::sync::Notify,
}
struct WorkGuard(Arc<HeldWork>);
impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0.disposed.fetch_add(1, Ordering::SeqCst);
    }
}
impl HeldWork {
    async fn perform(self: &Arc<Self>) {
        let _guard = WorkGuard(self.clone());
        self.started.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
        self.effects.fetch_add(1, Ordering::SeqCst);
        self.finished.notify_one();
    }
    fn assert_held(&self) {
        assert_eq!(self.started.load(Ordering::SeqCst), 1);
        assert_eq!(self.effects.load(Ordering::SeqCst), 0);
        assert_eq!(
            self.disposed.load(Ordering::SeqCst),
            0,
            "a rejected sibling must not drop the accepted operation"
        );
        assert_eq!(self.unsafe_started.load(Ordering::SeqCst), 0);
    }
    async fn release_and_assert_settled(&self) {
        self.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), self.finished.notified())
            .await
            .unwrap();
        assert_eq!(self.effects.load(Ordering::SeqCst), 1);
        assert_eq!(self.disposed.load(Ordering::SeqCst), 1);
        assert_eq!(self.unsafe_started.load(Ordering::SeqCst), 0);
    }
}
struct HeldOwner {
    work: Arc<HeldWork>,
    fail: bool,
}
impl ToolCatalog for HeldOwner {
    fn list(&self) -> HostFuture<Vec<Arc<dyn ToolDefinition>>> {
        let names = if self.fail {
            vec![("fail", true)]
        } else {
            vec![("hold", true), ("unsafe", false)]
        };
        let tools = names
            .into_iter()
            .map(|(name, safe)| {
                Arc::new(StaticTool::new(descriptor(name, safe))) as Arc<dyn ToolDefinition>
            })
            .collect();
        Box::pin(async move { Ok(tools) })
    }
}
impl ToolExecution for HeldOwner {
    fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        _: ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let (work, fail) = (self.work.clone(), self.fail);
        Box::pin(async move {
            if fail {
                return Err(ModuleError::new("safe failure").into());
            }
            if calls.iter().any(|call| call.name.equals_str("unsafe")) {
                work.unsafe_started.fetch_add(1, Ordering::SeqCst);
            }
            work.perform().await;
            Ok(calls
                .into_iter()
                .map(|call| {
                    object([
                        ("type", "success".into()),
                        ("toolCallId", JsValue::String(call.id)),
                    ])
                })
                .collect())
        })
    }
}
#[tokio::test]
async fn composed_first_rejection_returns_promptly_and_started_native_owner_still_settles() {
    let work = Arc::new(HeldWork::default());
    let port = compose_tool_ports(
        Arc::new(HeldOwner {
            work: work.clone(),
            fail: true,
        }),
        Arc::new(HeldOwner {
            work: work.clone(),
            fail: false,
        }),
    )
    .await
    .unwrap();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        port.execute_all(
            vec![
                call("fail", "fail"),
                call("hold", "hold"),
                call("unsafe", "unsafe"),
            ],
            context(),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.message, "safe failure");
    work.assert_held();
    work.release_and_assert_settled().await;
}

struct HeldWire {
    base: Arc<Wire>,
    work: Arc<HeldWork>,
}
impl ModuleHttpTransport for HeldWire {
    fn send<'a>(&'a self, request: ModuleHttpRequest) -> crate::skills::ModuleHttpFuture<'a> {
        Box::pin(async move {
            if request.method == "GET" {
                return self.base.send(request).await;
            }
            self.base.posts.fetch_add(1, Ordering::SeqCst);
            let request = parse_json(std::str::from_utf8(&request.body).unwrap()).unwrap();
            let payload = request.get("payload").unwrap();
            if payload.get("name") == Some(&"hold".into()) {
                self.work.perform().await;
            }
            if payload.get("name") == Some(&"unsafe".into()) {
                self.work.unsafe_started.fetch_add(1, Ordering::SeqCst);
            }
            let mut response = object([
                ("kind", "response".into()),
                (
                    "messageId",
                    JsValue::String(
                        format!(
                            "response-{}",
                            text(request.get("messageId").unwrap().as_string().unwrap())
                        )
                        .into(),
                    ),
                ),
                ("inReplyTo", request.get("messageId").unwrap().clone()),
                ("requestId", request.get("requestId").unwrap().clone()),
                ("ok", JsValue::Bool(true)),
            ]);
            if payload.get("name") == Some(&"fail".into()) {
                response.insert("ok".into(), JsValue::Bool(false));
                response.insert("code".into(), "RESULT_UNKNOWN".into());
                response.insert("outcome".into(), "result_unknown".into());
                response.insert(
                    "error".into(),
                    JsValue::Object(object([
                        ("code", "RESULT_UNKNOWN".into()),
                        ("message", "safe remote unknown".into()),
                        ("retryability", "unsafe".into()),
                    ])),
                );
            } else {
                response.insert(
                    "payload".into(),
                    JsValue::Object(object([
                        ("type", "success".into()),
                        ("toolCallId", payload.get("toolCallId").unwrap().clone()),
                    ])),
                );
            }
            Ok(reply(JsValue::Object(response)))
        })
    }
}
#[tokio::test]
async fn raw_first_unknown_does_not_drop_other_dispatched_http_call_or_start_unsafe() {
    let (template, wire) = fixture(Reply::Single, &["execute"]);
    let work = Arc::new(HeldWork::default());
    let client = Arc::new(HttpModuleClient::new(
        template.client.binding().clone(),
        Arc::new(HeldWire {
            base: wire.clone(),
            work: work.clone(),
        }),
        Arc::new(NoEnv),
    ));
    let port = ExternalToolExecution::new(
        client,
        vec![
            descriptor("fail", true),
            descriptor("hold", true),
            descriptor("unsafe", false),
        ],
        template.identity.clone(),
        Cancellation::default(),
        template.sequence.clone(),
    );
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        port.execute_all(
            vec![
                call("fail", "fail"),
                call("hold", "hold"),
                call("unsafe", "unsafe"),
            ],
            context(),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.message, "safe remote unknown");
    assert_eq!(
        error.module_error.unwrap().code,
        Some("RESULT_UNKNOWN".into())
    );
    assert_eq!(wire.posts.load(Ordering::SeqCst), 2);
    work.assert_held();
    work.release_and_assert_settled().await;
    assert_eq!(wire.posts.load(Ordering::SeqCst), 2);
}

struct HeldPermission(Arc<HeldWork>);
impl crate::host::modules::permission::PermissionDecision for HeldPermission {
    fn decide(
        &self,
        _: Arc<dyn ToolDefinition>,
        _: JsValue,
        _: crate::host::modules::capability::RuntimeContext,
        id: JsString,
    ) -> HostFuture<JsValue> {
        let work = self.0.clone();
        Box::pin(async move {
            if id.equals_str("fail") {
                return Err(ModuleError::new("permission service failed").into());
            }
            if id.equals_str("unsafe") {
                work.unsafe_started.fetch_add(1, Ordering::SeqCst);
            }
            work.perform().await;
            Ok(JsValue::Object(object([("type", "allow".into())])))
        })
    }
}
#[tokio::test]
async fn permission_first_rejection_keeps_started_authorization_alive_without_execution() {
    let (raw, wire) = fixture(Reply::Single, &["execute"]);
    let work = Arc::new(HeldWork::default());
    let port =
        permission_aware_tool_port(Arc::new(raw), Arc::new(HeldPermission(work.clone())), false);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        port.execute_all(
            vec![
                call("fail", "safe"),
                call("hold", "safe"),
                call("unsafe", "remote"),
            ],
            context(),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.message, "permission service failed");
    work.assert_held();
    assert_eq!(wire.posts.load(Ordering::SeqCst), 0);
    work.release_and_assert_settled().await;
    assert_eq!(
        wire.posts.load(Ordering::SeqCst),
        0,
        "a late approval must not resume a rejected batch"
    );
}
