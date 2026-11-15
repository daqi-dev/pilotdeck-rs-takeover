#[path = "common/baseline.rs"]
mod baseline;
use pilotdeck_gateway::{
    host::{
        Cancellation, HostError, HostFuture, ModuleError,
        modules::{
            capability::{
                RuntimeContext, ToolCall, ToolDefinition, ToolExecution, ToolInvocationContext,
            },
            permission::PermissionDecision,
            tool_context::{ExecutionContext, HostToolCheckpoint, ToolRuntimeContext},
        },
    },
    runtime::tools::{
        ExecutableTool, SchedulerMode, ToolCancellation, ToolError, ToolFuture, ToolOutput,
        ToolRegistry, ToolRunContext, ToolRuntime, registry::UnavailableTool,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::time::Instant;

const ORACLE: &str = include_str!("fixtures/tool-runtime/oracle.json");
const EPOCH: f64 = 1_767_225_600_000.0;
static UNDEFINED: JsValue = JsValue::Undefined;
fn field<'a>(v: &'a JsValue, key: &str) -> &'a JsValue {
    v.get(key).unwrap_or(&UNDEFINED)
}
fn text(v: &JsValue) -> String {
    v.as_string().unwrap().to_utf8().unwrap()
}
fn array(v: &JsValue) -> &[JsValue] {
    if let JsValue::Array(v) = v {
        v
    } else {
        panic!("expected array: {v:?}")
    }
}
fn obj<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut o = JsObject::new();
    for (k, v) in entries {
        o.insert(k.into(), v);
    }
    o
}
fn value(s: impl Into<JsString>) -> JsValue {
    JsValue::String(s.into())
}
fn wire(v: &JsValue) -> String {
    stringify_json(v).unwrap().unwrap()
}
fn equal(a: &JsValue, b: &JsValue, label: &str) {
    assert_eq!(wire(a), wire(b), "{label}");
}

#[derive(Clone)]
struct Probe {
    started: Instant,
    events: Arc<Mutex<Vec<JsValue>>>,
    live: Arc<AtomicUsize>,
}
impl Probe {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            events: Arc::default(),
            live: Arc::default(),
        }
    }
    fn ms(&self) -> f64 {
        self.started.elapsed().as_millis() as f64
    }
    fn event(&self, event: &str, id: &JsValue) {
        self.events.lock().unwrap().push(JsValue::Array(vec![
            event.into(),
            id.clone(),
            JsValue::Number(self.ms()),
        ]));
    }
}
struct Live(Arc<AtomicUsize>);
impl Live {
    fn new(probe: &Probe) -> Self {
        probe.live.fetch_add(1, Ordering::SeqCst);
        Self(probe.live.clone())
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct FakeTool {
    fields: JsObject,
    spec: JsValue,
    probe: Probe,
}
impl ToolDefinition for FakeTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(self
            .spec
            .get("readOnly")
            .cloned()
            .unwrap_or(JsValue::Bool(true)))
    }
    fn is_concurrency_safe(&self, input: &JsValue) -> Result<JsValue, HostError> {
        if field(&self.spec, "predicateError") == &JsValue::Bool(true) {
            return Err(ModuleError::new("predicate failed").into());
        }
        Ok(input
            .get("safe")
            .cloned()
            .unwrap_or_else(|| field(&self.spec, "safe").clone()))
    }
    fn requires_user_interaction(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(self
            .spec
            .get("interaction")
            .cloned()
            .unwrap_or(JsValue::Bool(false)))
    }
}
impl ExecutableTool for FakeTool {
    fn validate_input(
        &self,
        input: JsValue,
        _: ToolRunContext,
    ) -> ToolFuture<Option<Vec<JsObject>>> {
        Box::pin(async move {
            if field(&input, "validationError") == &JsValue::Bool(true) {
                return Err(ToolError::new(
                    "path_not_allowed",
                    "Path validation failed.",
                ));
            }
            Ok(
                if let JsValue::Array(issues) = field(&input, "validationIssues") {
                    Some(
                        issues
                            .iter()
                            .map(|v| v.as_object().unwrap().clone())
                            .collect(),
                    )
                } else {
                    None
                },
            )
        })
    }
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let probe = self.probe.clone();
        Box::pin(async move {
            let _live = Live::new(&probe);
            let id = context.fields.get("currentToolCallId").unwrap().clone();
            probe.event("start", &id);
            if field(&input, "hang") == &JsValue::Bool(true) {
                return std::future::pending().await;
            }
            let ignore_abort = field(&input, "ignoreAbort") == &JsValue::Bool(true);
            let delay = field(&input, "delay").as_number().unwrap_or(0.0);
            let budget = if field(&input, "useBudget") == &JsValue::Bool(true) {
                context
                    .fields
                    .get("subagentTimeoutMs")
                    .and_then(JsValue::as_number)
            } else {
                None
            };
            let wait = delay.min(budget.unwrap_or(f64::INFINITY));
            let timer = async {
                if wait == 0.0 {
                    tokio::task::yield_now().await;
                } else {
                    tokio::time::sleep(Duration::from_secs_f64(wait / 1000.0)).await;
                }
            };
            let result = tokio::select! { biased;
                () = async { if ignore_abort { std::future::pending().await } else { context.cancelled().await } } => Err(ToolError::aborted()),
                () = timer => {
                    if budget.is_some_and(|v| v < delay) { Err(ToolError::new("tool_timeout", "Budget expired.")) }
                    else if field(&input, "fail").as_string().is_some() {
                        match text(field(&input, "fail")).as_str() {
                            "Error" => Err(ToolError::failed("ordinary failure")),
                            "nonError" => Err(ToolError::non_error()),
                            code => { let mut e = ToolError::new(code, "fake failure"); e.details = Some(input.get("failDetails").and_then(JsValue::as_object).cloned().unwrap_or_else(|| obj([("stderr", "ENOENT detail".into())]))); Err(e) }
                        }
                    } else { Ok(()) }
                }
            };
            if let Err(error) = result {
                probe.event("error", &id);
                return Err(error);
            }
            probe.event("end", &id);
            Ok(ToolOutput {
                content: match input.get("content") {
                    Some(content) => array(content).to_vec(),
                    None => vec![JsValue::Object(obj([
                        ("type", "text".into()),
                        ("text", input.get("value").cloned().unwrap_or(id)),
                    ]))],
                },
                data: input.get("data").cloned(),
                metadata: input.get("metadata").and_then(JsValue::as_object).cloned(),
                supplemental_messages: input.get("supplementalMessages").map(|v| array(v).to_vec()),
            })
        })
    }
}
struct Permission;
impl PermissionDecision for Permission {
    fn decide(
        &self,
        _: Arc<dyn ToolDefinition>,
        input: JsValue,
        _: RuntimeContext,
        _: JsString,
    ) -> HostFuture<JsValue> {
        Box::pin(async move {
            if input
                .get("decision")
                .and_then(JsValue::as_string)
                .is_some_and(|s| s.equals_str("throw"))
            {
                return Err(ModuleError::new("permission failed").into());
            }
            Ok(JsValue::Object(obj([
                (
                    "type",
                    input
                        .get("decision")
                        .cloned()
                        .unwrap_or_else(|| "allow".into()),
                ),
                (
                    "reason",
                    JsValue::Object(obj([
                        ("type", "runtime".into()),
                        (
                            "message",
                            input
                                .get("permissionMessage")
                                .cloned()
                                .unwrap_or_else(|| "fixture".into()),
                        ),
                    ])),
                ),
                ("message", "permission stopped".into()),
                (
                    "request",
                    JsValue::Object(obj([("options", JsValue::Array(vec!["allow".into()]))])),
                ),
                (
                    "updatedInput",
                    input
                        .get("updatedInput")
                        .cloned()
                        .unwrap_or(JsValue::Undefined),
                ),
            ])))
        })
    }
}
/// Tools must observe either signal type the host may store.
#[derive(Clone)]
enum Signal {
    Tool(ToolCancellation),
    Host(Cancellation),
}
impl Signal {
    fn cancel(&self) {
        match self {
            Signal::Tool(s) => s.cancel(),
            Signal::Host(s) => s.cancel(None),
        }
    }
    fn runtime(&self) -> RuntimeContext {
        match self {
            Signal::Tool(s) => Arc::new(s.clone()),
            Signal::Host(s) => Arc::new(s.clone()),
        }
    }
}
fn context(
    probe: &Probe,
    cancel: &ToolCancellation,
    patch: Option<&JsObject>,
) -> Arc<ToolRuntimeContext> {
    context_with(probe, &Signal::Tool(cancel.clone()), patch)
}
fn context_with(
    probe: &Probe,
    signal: &Signal,
    patch: Option<&JsObject>,
) -> Arc<ToolRuntimeContext> {
    let clock = probe.clone();
    let mut fields = obj([
        ("cwd", "/workspace".into()),
        ("sessionId", "s".into()),
        ("turnId", "t".into()),
        ("permissionMode", "default".into()),
        (
            "permissionContext",
            JsValue::Object(obj([
                ("mode", "default".into()),
                ("canPrompt", JsValue::Bool(true)),
                ("additionalWorkingDirectories", JsValue::Array(vec![])),
            ])),
        ),
    ]);
    if let Some(patch) = patch {
        for (key, v) in patch.entries() {
            fields.insert(key.clone(), v.clone());
        }
    }
    Arc::new(ToolRuntimeContext {
        fields,
        files: HostToolCheckpoint::default().tool_context_state(),
        abort_signal: Some(signal.runtime()),
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: Some(Arc::new(move || JsValue::Number(EPOCH + clock.ms()))),
        progress: None,
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    })
}
/// `schemaPrototypes: [{path, prototype}]` sets the prototype of the object at
/// `path` (null for Object.create(null), an array or object otherwise), the
/// same construction the TS generator applies.
fn with_prototypes(mut schema: JsValue, prototypes: &JsValue) -> JsValue {
    fn set(value: &mut JsValue, path: &[JsValue], prototype: &JsValue) {
        let JsValue::Object(object) = value else {
            panic!("prototype path must reach an object")
        };
        match path.split_first() {
            None => object.set_prototype(prototype.clone()),
            Some((key, rest)) => {
                let key = key.as_string().unwrap().clone();
                let mut child = object.get_key(&key).unwrap().clone();
                set(&mut child, rest, prototype);
                object.insert(key, child);
            }
        }
    }
    if let JsValue::Array(prototypes) = prototypes {
        for p in prototypes {
            set(&mut schema, array(field(p, "path")), field(p, "prototype"));
        }
    }
    schema
}
fn register(registry: &ToolRegistry, spec: &JsValue, probe: &Probe) {
    let mut fields = obj([
        ("name", field(spec, "name").clone()),
        ("description", field(spec, "name").clone()),
        ("aliases", field(spec, "aliases").clone()),
        ("kind", "builtin".into()),
        (
            "inputSchema",
            match spec.get("schemaJson") {
                Some(json) => with_prototypes(
                    parse_json(&text(json)).unwrap(),
                    field(spec, "schemaPrototypes"),
                ),
                None => spec
                    .get("schema")
                    .cloned()
                    .unwrap_or_else(|| JsValue::Object(obj([("type", "object".into())]))),
            },
        ),
    ]);
    if let Some(limit) = spec.get("maxResultBytes") {
        fields.insert("maxResultBytes".into(), limit.clone());
    }
    registry
        .register(Arc::new(FakeTool {
            fields,
            spec: spec.clone(),
            probe: probe.clone(),
        }))
        .unwrap();
}
fn calls(values: &JsValue) -> Vec<ToolCall> {
    array(values)
        .iter()
        .map(|v| ToolCall {
            id: field(v, "id").as_string().unwrap().clone(),
            name: field(v, "name").as_string().unwrap().clone(),
            // Raw JSON keeps own `__proto__` keys and non-sorted integer keys.
            input: match v.get("inputJson") {
                Some(json) => parse_json(&text(json)).unwrap(),
                None => field(v, "input").clone(),
            },
        })
        .collect()
}

/// `context` plus the fields of the raw-JSON `contextJson`, as the generator
/// spreads them.
fn scenario_context(spec: &JsValue) -> Option<JsObject> {
    let mut context = field(spec, "context").as_object().cloned();
    if let Some(json) = spec.get("contextJson") {
        let patch = parse_json(&text(json)).unwrap();
        let context = context.get_or_insert_with(JsObject::new);
        for (key, value) in patch.as_object().unwrap().entries() {
            context.insert(key.clone(), value.clone());
        }
    }
    context
}
struct Replay {
    result: Result<Vec<JsObject>, HostError>,
    events: Vec<JsValue>,
    settled_at: f64,
    live: usize,
}
/// Run one recorded scenario through the Rust runtime on a paused clock.
async fn replay(oracle: &JsValue, spec: &JsValue, signal: Signal) -> Replay {
    let probe = Probe::new();
    let cancel = signal;
    if field(spec, "preCancel") == &JsValue::Bool(true) {
        cancel.cancel();
    }
    let cancel_task = field(spec, "cancelAt").as_number().map(|at| {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(at / 1000.0)).await;
            cancel.cancel();
        })
    });
    let registry = ToolRegistry::default();
    for def in array(
        spec.get("definitions")
            .unwrap_or(field(oracle, "definitions")),
    ) {
        register(&registry, def, &probe);
    }
    if let JsValue::Array(hidden) = field(spec, "hidden") {
        for name in hidden {
            registry.hide(name.as_string().unwrap());
        }
    }
    if let JsValue::Array(unavailable) = field(spec, "unavailable") {
        for v in unavailable {
            registry
                .mark_unavailable(
                    UnavailableTool {
                        name: field(v, "toolName").as_string().unwrap().clone(),
                        code: field(v, "code").as_string().unwrap().clone(),
                        reason: field(v, "reason").as_string().unwrap().clone(),
                    },
                    &array(field(v, "aliases"))
                        .iter()
                        .map(|v| v.as_string().unwrap().clone())
                        .collect::<Vec<_>>(),
                )
                .unwrap();
        }
    }
    let mode = if field(spec, "mode")
        .as_string()
        .is_some_and(|s| s.equals_str("sequential"))
    {
        SchedulerMode::Sequential
    } else {
        SchedulerMode::Concurrent
    };
    let runtime = ToolRuntime::new(registry, Arc::new(Permission), mode);
    let mut execution = JsObject::new();
    if let Some(ms) = field(spec, "deadline").as_number() {
        execution.insert(
            "operationDeadline".into(),
            value(pilotdeck_transport::system_clock::format_epoch_iso(
                (EPOCH + ms) as i64,
            )),
        );
    }
    let result = runtime
        .execute_all(
            calls(field(spec, "calls")),
            ToolInvocationContext {
                runtime: context_with(&probe, &cancel, scenario_context(spec).as_ref()),
                execution: Arc::new(ExecutionContext {
                    fields: execution,
                    abort_signal: None,
                }),
            },
        )
        .await;
    let settled_at = probe.ms();
    if let Some(task) = cancel_task {
        task.await.unwrap();
    }
    // Promise.all rejects early and the started siblings finish detached;
    // let the (paused) clock run until they have settled.
    if field(spec, "drain") == &JsValue::Bool(true) {
        for _ in 0..60_000 {
            if probe.live.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    let events = probe.events.lock().unwrap().clone();
    Replay {
        result,
        events,
        settled_at,
        live: probe.live.load(Ordering::SeqCst),
    }
}

#[tokio::test(start_paused = true)]
async fn real_ts_runtime_schedules_results_errors_and_deadline_budgets() {
    let oracle = baseline::oracle(ORACLE);
    let scenarios = array(field(&oracle, "scenarios"));
    assert_eq!(scenarios.len(), 78);
    for (spec, signal) in scenarios.iter().flat_map(|spec| {
        [
            (spec, Signal::Tool(ToolCancellation::default())),
            (spec, Signal::Host(Cancellation::default())),
        ]
    }) {
        let label = format!(
            "{} ({})",
            text(field(spec, "name")),
            match signal {
                Signal::Tool(_) => "ToolCancellation",
                Signal::Host(_) => "host Cancellation",
            }
        );
        let Replay {
            result,
            events,
            settled_at,
            live,
        } = replay(&oracle, spec, signal).await;
        match result {
            Ok(_) if !field(spec, "error").is_undefined() => panic!(
                "{label}: TS rejected with {:?}, Rust resolved",
                text(field(spec, "error"))
            ),
            Ok(results) => equal(
                &JsValue::Array(results.into_iter().map(JsValue::Object).collect()),
                field(spec, "results"),
                &label,
            ),
            Err(e) if field(spec, "error").is_undefined() => {
                panic!("{label}: TS resolved, Rust rejected with {:?}", e.message)
            }
            Err(e) => assert_eq!(e.message, text(field(spec, "error")), "{label}"),
        }
        let expected = array(field(spec, "events")).to_vec();
        if field(spec, "drain") == &JsValue::Bool(true) {
            // The rejection time is TS's exactly; the started siblings run on
            // detached and finish when TS's do.
            let ts_settled = field(spec, "settledAt").as_number().unwrap();
            assert_eq!(settled_at, ts_settled, "{label}: rejection time");
        }
        equal(
            &JsValue::Array(events),
            &JsValue::Array(expected),
            &format!("{label} events"),
        );
        assert_eq!(live, 0, "{label}: leaked tool future");
    }
}

/// Rust's String() bound is js_value::MAX_ARRAY_DEPTH (2000 nested arrays),
/// fixed below V8's stack-dependent limit (measured from 2516; README). The
/// recorded scenarios pin 2000 and 3000; this pins the exact Rust boundary on
/// each converting path: 2000 nested arrays convert, 2001 raise the RangeError
/// message instead of overflowing the Rust stack.
#[tokio::test(start_paused = true)]
async fn string_conversion_depth_bound_is_shared_js_value_limit() {
    fn nested(arrays: usize, leaf: &str) -> String {
        format!("{}{leaf}{}", "[".repeat(arrays), "]".repeat(arrays))
    }
    for (arrays, converts) in [(2000, true), (2001, false)] {
        // enum/type/required members are converted one by one, so the member
        // itself carries the nesting; a nested `type` is walked as a whole.
        let inner = arrays;
        let cases = [
            (format!(r#"{{"enum":[{}]}}"#, nested(inner, "1")), "2", None),
            (
                format!(r#"{{"type":["string",{}]}}"#, nested(inner, r#""x""#)),
                "2",
                None,
            ),
            // The type walk allows one level more than String() (scheduler.rs).
            (
                format!(r#"{{"type":{}}}"#, nested(arrays + 1, r#""number""#)),
                "\"s\"",
                None,
            ),
            (
                format!(
                    r#"{{"type":"object","required":[{}]}}"#,
                    nested(inner, r#""k""#)
                ),
                "{}",
                None,
            ),
            (
                r#"{"type":"object","required":["content"]}"#.to_string(),
                "{}",
                Some(format!(
                    r#"{{"outputTruncated":true,"maxOutputTokens":{}}}"#,
                    nested(arrays, "1")
                )),
            ),
            (
                r#"{"type":"object"}"#.to_string(),
                &*format!(
                    r#"{{"fail":"command_failed","failDetails":{{"exitCode":{}}}}}"#,
                    nested(arrays, "1")
                ),
                None,
            ),
        ];
        for (schema, input, context_json) in cases {
            let probe = Probe::new();
            let registry = ToolRegistry::default();
            let def = JsValue::Object(obj([
                ("name", "write_file".into()),
                ("safe", JsValue::Bool(false)),
                ("schemaJson", schema.as_str().into()),
            ]));
            register(&registry, &def, &probe);
            let runtime =
                ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent);
            let mut spec = obj([]);
            if let Some(json) = &context_json {
                spec.insert("contextJson".into(), json.as_str().into());
            }
            let context = scenario_context(&JsValue::Object(spec));
            let call = ToolCall {
                id: "a".into(),
                name: "write_file".into(),
                input: parse_json(input).unwrap(),
            };
            let result = runtime
                .execute_batch(
                    vec![call],
                    ToolRunContext::new(context_with(
                        &probe,
                        &Signal::Tool(ToolCancellation::default()),
                        context.as_ref(),
                    )),
                )
                .await;
            let label = format!("{arrays} arrays: {schema} / {input} / {context_json:?}");
            if converts {
                assert!(
                    result.is_ok(),
                    "{label}: {:?}",
                    result.err().map(|e| e.message)
                );
            } else {
                assert_eq!(
                    result.err().map(|e| e.message).as_deref(),
                    Some("Maximum call stack size exceeded"),
                    "{label}"
                );
            }
        }
        // Preview: the RangeError is caught by execute and becomes a result.
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        register(
            &registry,
            &parse_json(r#"{"name":"read_file","safe":false}"#).unwrap(),
            &probe,
        );
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent);
        let input = format!(
            r#"{{"content":[{{"type":"file","path":{}}}]}}"#,
            nested(arrays, r#""p""#)
        );
        let call = ToolCall {
            id: "a".into(),
            name: "read_file".into(),
            input: parse_json(&input).unwrap(),
        };
        let context = obj([("maxResultBytes", JsValue::Number(1.0))]);
        let results = runtime
            .execute_batch(
                vec![call],
                ToolRunContext::new(context_with(
                    &probe,
                    &Signal::Tool(ToolCancellation::default()),
                    Some(&context),
                )),
            )
            .await
            .unwrap();
        let result = &results[0];
        if converts {
            assert!(
                result
                    .get("type")
                    .unwrap()
                    .as_string()
                    .unwrap()
                    .equals_str("success")
            );
        } else {
            let error = result.get("error").unwrap();
            assert_eq!(text(field(error, "code")), "tool_execution_failed");
            assert_eq!(
                text(field(error, "message")),
                "Maximum call stack size exceeded"
            );
        }
    }
}

async fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut future = future;
    std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}
fn hanging(probe: &Probe) -> (ToolRuntime, Vec<ToolCall>) {
    let registry = ToolRegistry::default();
    register(
        &registry,
        &parse_json(r#"{"name":"read_file","safe":true}"#).unwrap(),
        probe,
    );
    let calls = calls(&parse_json(r#"[{"id":"a","name":"read_file","input":{"hang":true}},{"id":"b","name":"read_file","input":{"hang":true}}]"#).unwrap());
    (
        ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent),
        calls,
    )
}
#[tokio::test(start_paused = true)]
async fn dropping_batch_reclaims_all_noncooperative_tool_futures_immediately() {
    let probe = Probe::new();
    let cancel = ToolCancellation::default();
    let (runtime, calls) = hanging(&probe);
    let mut batch =
        Box::pin(runtime.execute_batch(calls, ToolRunContext::new(context(&probe, &cancel, None))));
    assert!(poll_once(batch.as_mut()).await.is_pending());
    assert_eq!(probe.live.load(Ordering::SeqCst), 2);
    drop(batch);
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
}
#[tokio::test(start_paused = true)]
async fn abort_and_outer_operation_deadline_leave_no_detached_futures() {
    let probe = Probe::new();
    let cancel = ToolCancellation::default();
    let (runtime, calls) = hanging(&probe);
    let mut batch = Box::pin(runtime.execute_batch(
        calls.clone(),
        ToolRunContext::new(context(&probe, &cancel, None)),
    ));
    assert!(poll_once(batch.as_mut()).await.is_pending());
    // Abort is cooperative as in TS: started tools that ignore the signal are
    // not abandoned with a synthetic result, but stay owned by the batch.
    cancel.cancel();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(poll_once(batch.as_mut()).await.is_pending());
    assert_eq!(probe.live.load(Ordering::SeqCst), 2);
    drop(batch);
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    let signal = ToolCancellation::default();
    let timeout = tokio::time::timeout(
        Duration::from_millis(10),
        runtime.execute_batch(calls, ToolRunContext::new(context(&probe, &signal, None))),
    );
    assert!(timeout.await.is_err());
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    let before = probe.events.lock().unwrap().len();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(probe.events.lock().unwrap().len(), before);
}

/// Promise.all: the first concurrent error rejects at once and the started
/// siblings keep running detached, so a non-cooperative sibling can neither
/// delay the rejection nor be cut short by it.
#[tokio::test(start_paused = true)]
async fn concurrent_error_rejects_immediately_and_lets_started_siblings_run_on() {
    let probe = Probe::new();
    let cancel = ToolCancellation::default();
    let (runtime, _) = hanging(&probe);
    let calls = calls(&parse_json(r#"[{"id":"a","name":"read_file","input":{"decision":"throw"}},{"id":"b","name":"read_file","input":{"hang":true}},{"id":"c","name":"read_file","input":{"hang":true}}]"#).unwrap());
    let mut batch =
        Box::pin(runtime.execute_batch(calls, ToolRunContext::new(context(&probe, &cancel, None))));
    let Poll::Ready(Err(error)) = poll_once(batch.as_mut()).await else {
        panic!("first error must reject the batch on the same poll");
    };
    assert_eq!(error.message, "permission failed");
    assert_eq!(probe.ms(), 0.0);
    let started: Vec<_> = probe
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| text(&array(e)[1]))
        .collect();
    assert_eq!(started, ["b", "c"], "siblings had started");
    drop(batch);
    assert_eq!(
        probe.live.load(Ordering::SeqCst),
        2,
        "started siblings run on after the rejection"
    );
}

/// Tool destructors and visibility policies may query the same registry; the
/// registry must release its lock before either runs.
struct Reentrant {
    fields: JsObject,
    registry: ToolRegistry,
    drops: Arc<AtomicUsize>,
}
impl ToolDefinition for Reentrant {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}
impl ExecutableTool for Reentrant {
    fn execute(&self, _: JsValue, _: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async { Ok(ToolOutput::default()) })
    }
}
impl Drop for Reentrant {
    fn drop(&mut self) {
        let _ = self.registry.list_all();
        let _ = self.registry.has(&"x".into());
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn registry_releases_its_lock_before_tool_destructors_and_policies_run() {
    let drops = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::default();
    let tool = |name: &str| {
        Arc::new(Reentrant {
            fields: obj([("name", name.into()), ("aliases", JsValue::Array(vec![]))]),
            registry: registry.clone(),
            drops: drops.clone(),
        })
    };
    registry.register(tool("x")).unwrap();
    registry.replace(tool("x")).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let second = registry.register_or_replace(tool("y")).unwrap();
    registry.register_or_replace(tool("y")).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(!second.is_active());
    assert!(registry.register(tool("x")).is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert!(registry.unregister(&"x".into()).unwrap());
    assert_eq!(drops.load(Ordering::SeqCst), 4);
    let handle = registry.register(tool("z")).unwrap();
    handle.dispose();
    assert_eq!(drops.load(Ordering::SeqCst), 5);
    let inner = registry.clone();
    let scoped = registry
        .scoped(Arc::new(move |t: &dyn ExecutableTool| {
            !inner.is_hidden(t.fields().get("name").unwrap().as_string().unwrap())
                && inner.has(&"y".into())
        }))
        .unwrap();
    assert_eq!(scoped.list().unwrap().len(), 1);
    assert!(scoped.get(&"y".into()).is_some());
    registry.dispose();
    assert_eq!(drops.load(Ordering::SeqCst), 6);
}

#[cfg(not(feature = "kernel-managed-model"))]
mod continuation_seam_tests {
    use super::*;
    // 本门仅是行为负控；正式权限必须由 Kernel 同源最终许可实现。
    type FinalObservation = (usize, JsString, JsValue, JsValue);
    #[derive(Clone, Default)]
    struct FinalGateProbe {
        seen: Arc<Mutex<Vec<FinalObservation>>>,
        deny_danger: bool,
        deny_write: bool,
        polls: Arc<AtomicUsize>,
        detached: Arc<AtomicUsize>,
        fail_handoff: bool,
        fail_child: bool,
    }
    struct FinalPermitProbe(Arc<AtomicUsize>);
    impl pilotdeck_gateway::runtime::tools::scheduler::FinalToolPermit for FinalPermitProbe {
        fn revalidate(&self) -> Result<(), HostError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    impl pilotdeck_gateway::runtime::tools::scheduler::FinalToolAdmission for FinalGateProbe {
        fn child_producer(
            &self,
        ) -> Result<
            Arc<dyn pilotdeck_gateway::runtime::tools::scheduler::FinalToolAdmission>,
            HostError,
        > {
            if self.fail_child {
                return Err(ModuleError::new("child producer unavailable").into());
            }
            Ok(Arc::new(self.clone()))
        }
        fn spawn_detached(&self, future: HostFuture<JsObject>) -> Result<(), HostError> {
            // 仅用于测试原调度；正式 owner 必须先登记任务并保留真实租约。
            self.detached.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(future);
            if self.fail_handoff {
                return Err(ModuleError::new("tracked handoff unavailable").into());
            }
            Ok(())
        }
        fn execute(
            &self,
            request: pilotdeck_gateway::runtime::tools::scheduler::FinalToolRequest,
            continuation: pilotdeck_gateway::runtime::tools::scheduler::FinalToolContinuation,
        ) -> HostFuture<JsObject> {
            let deny = (self.deny_write && request.canonical_name.equals_str("write_file"))
                || self.deny_danger
                    && request
                        .final_input
                        .get("command")
                        .and_then(JsValue::as_string)
                        .is_some_and(|value| value.equals_str("rm file"));
            self.seen.lock().unwrap().push((
                request.ordinal,
                request.original_call.id.clone(),
                request.final_input,
                request.permission_decision,
            ));
            let polls = self.polls.clone();
            Box::pin(async move {
                if deny {
                    return Err(ModuleError::new("final input denied").into());
                }
                continuation(Arc::new(FinalPermitProbe(polls))).await
            })
        }
    }

    #[tokio::test]
    async fn final_gate_sees_permission_updated_input_before_any_tool_execution() {
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        register(
            &registry,
            &parse_json(r#"{"name":"bash","safe":false}"#).unwrap(),
            &probe,
        );
        let original = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent);
        let gate = Arc::new(FinalGateProbe {
            deny_danger: true,
            ..Default::default()
        });
        let runtime = original.clone().with_final_admission(gate.clone());
        let call = ToolCall {
            id: "same".into(),
            name: "bash".into(),
            input: parse_json(r#"{"command":"pwd","updatedInput":{"command":"rm file"}}"#).unwrap(),
        };
        let host = context(&probe, &ToolCancellation::default(), None);
        let error = runtime
            .execute_batch(vec![call], ToolRunContext::new(host))
            .await
            .unwrap_err();
        assert!(error.message.contains("final input denied"));
        assert!(
            probe.events.lock().unwrap().is_empty(),
            "tool body must not start"
        );
        let observed = gate.seen.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].0, 0);
        assert_eq!(observed[0].1, JsString::from("same"));
        assert_eq!(
            observed[0].2.get("command"),
            Some(&JsValue::from("rm file"))
        );
        assert_eq!(observed[0].3.get("type"), Some(&JsValue::from("allow")));
    }

    #[tokio::test]
    async fn managed_real_read_file_preserves_shared_checkpoint_and_error_result() {
        let root = std::env::temp_dir().join(format!(
            "pd-tool-final-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("sample.txt"), "hello\n").unwrap();
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        registry
            .register(Arc::new(
                pilotdeck_gateway::runtime::tools::builtin::read_file::ReadFileTool::new(),
            ))
            .unwrap();
        let original = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent);
        let gate = Arc::new(FinalGateProbe::default());
        let managed = original.for_managed_batch(gate.clone()).unwrap();
        let patch = obj([("cwd", JsValue::from(root.to_str().unwrap()))]);
        let host = context(&probe, &ToolCancellation::default(), Some(&patch));
        let run = |name: &'static str, path: &'static str| {
            let managed = managed.clone();
            let host = host.clone();
            async move {
                managed
                    .execute_all(
                        vec![ToolCall {
                            id: name.into(),
                            name: "read_file".into(),
                            input: JsValue::Object(obj([("file_path", path.into())])),
                        }],
                        ToolInvocationContext {
                            runtime: host,
                            execution: Arc::new(ExecutionContext {
                                fields: JsObject::new(),
                                abort_signal: None,
                            }),
                        },
                    )
                    .await
                    .unwrap()
                    .remove(0)
            }
        };
        let first = run("one", "sample.txt").await;
        assert_eq!(first.get("type"), Some(&JsValue::from("success")));
        let second = run("two", "sample.txt").await;
        assert_eq!(second.get("type"), Some(&JsValue::from("success")));
        assert_eq!(
            second.get("data").and_then(|data| data.get("unchanged")),
            Some(&JsValue::Bool(true))
        );
        let failure = run("three", "missing.txt").await;
        assert_eq!(failure.get("type"), Some(&JsValue::from("error")));
        assert_eq!(failure.get("toolCallId"), Some(&JsValue::from("three")));
        assert!(gate.polls.load(Ordering::SeqCst) >= 2);
        assert_eq!(
            gate.seen.lock().unwrap().len(),
            3,
            "a missing-file business failure must still pass through final admission"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn managed_batch_keeps_original_ordinals_for_duplicate_business_ids_and_safe_first() {
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        register(
            &registry,
            &parse_json(r#"{"name":"test","safe":false}"#).unwrap(),
            &probe,
        );
        let gate = Arc::new(FinalGateProbe::default());
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent)
            .with_final_admission(gate.clone());
        let calls = vec![
            ToolCall {
                id: "duplicate".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":false,"value":"first"}"#).unwrap(),
            },
            ToolCall {
                id: "duplicate".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true,"value":"second"}"#).unwrap(),
            },
        ];
        let host = context(&probe, &ToolCancellation::default(), None);
        let results = runtime
            .execute_batch(calls, ToolRunContext::new(host))
            .await
            .unwrap();
        let seen = gate.seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|entry| entry.0).collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert!(
            seen.iter()
                .all(|entry| entry.1 == JsString::from("duplicate"))
        );
        assert_eq!(
            results[0]
                .get("content")
                .and_then(|x| if let JsValue::Array(a) = x {
                    a.first()
                } else {
                    None
                })
                .and_then(|x| x.get("text")),
            Some(&JsValue::from("first"))
        );
        assert_eq!(
            results[1]
                .get("content")
                .and_then(|x| if let JsValue::Array(a) = x {
                    a.first()
                } else {
                    None
                })
                .and_then(|x| x.get("text")),
            Some(&JsValue::from("second"))
        );
    }

    struct HandlerContextProbe {
        host: Arc<ToolRuntimeContext>,
        calls: Arc<Mutex<Vec<JsValue>>>,
    }
    impl pilotdeck_gateway::host::modules::capability::ToolContext for HandlerContextProbe {
        fn prepare(&self, call: JsValue) -> HostFuture<ToolInvocationContext> {
            self.calls.lock().unwrap().push(call);
            let host = self.host.clone();
            Box::pin(async move {
                Ok(ToolInvocationContext {
                    runtime: host,
                    execution: Arc::new(ExecutionContext {
                        fields: JsObject::new(),
                        abort_signal: None,
                    }),
                })
            })
        }
    }
    #[derive(Default)]
    struct HandlerObserverProbe(Mutex<Vec<Vec<JsObject>>>);
    impl pilotdeck_gateway::host::modules::capability::CapabilityResultObserver
        for HandlerObserverProbe
    {
        fn on_capability_results(&self, results: Vec<JsObject>) -> HostFuture<()> {
            self.0.lock().unwrap().push(results);
            Box::pin(async { Ok(()) })
        }
    }
    #[derive(Default)]
    struct HandlerLifecycleProbe(Mutex<Vec<String>>);
    impl pilotdeck_gateway::runtime::tools::ToolLifecycle for HandlerLifecycleProbe {
        fn dispatch(
            &self,
            event: &'static str,
            _: &JsString,
            _: JsObject,
            _: &ToolRunContext,
        ) -> HostFuture<Option<pilotdeck_gateway::runtime::hooks::LifecycleDispatchResult>>
        {
            self.0.lock().unwrap().push(event.into());
            Box::pin(async { Ok(None) })
        }
    }
    #[tokio::test]
    async fn managed_read_file_uses_original_handler_context_and_observer() {
        let root = std::env::temp_dir().join(format!(
            "pd-handler-final-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("sample.txt"), "hello\n").unwrap();
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        registry
            .register(Arc::new(
                pilotdeck_gateway::runtime::tools::builtin::read_file::ReadFileTool::new(),
            ))
            .unwrap();
        let lifecycle = Arc::new(HandlerLifecycleProbe::default());
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent)
            .with_lifecycle(lifecycle.clone());
        let gate = Arc::new(FinalGateProbe::default());
        let execution = runtime.for_managed_batch(gate.clone()).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let host = context(
            &probe,
            &ToolCancellation::default(),
            Some(&obj([("cwd", JsValue::from(root.to_str().unwrap()))])),
        );
        let observer = Arc::new(HandlerObserverProbe::default());
        let handler = pilotdeck_gateway::host::modules::capability::Handler::new(
            execution,
            Arc::new(HandlerContextProbe {
                host,
                calls: seen.clone(),
            }),
            observer.clone(),
        );
        let call = |id: &str, path: &str| {
            JsValue::Object(obj([(
                "payload",
                JsValue::Object(obj([
                    ("operation", "execute".into()),
                    ("toolCallId", id.into()),
                    ("name", "read_file".into()),
                    (
                        "arguments",
                        JsValue::Object(obj([("file_path", path.into())])),
                    ),
                ])),
            )]))
        };
        let full_first = call("one", "sample.txt");
        let first = handler.call(full_first.clone()).await.unwrap();
        let second = handler.call(call("two", "sample.txt")).await.unwrap();
        let failure = handler.call(call("three", "missing.txt")).await.unwrap();
        assert_eq!(first.get("type"), Some(&JsValue::from("success")));
        assert_eq!(
            second.get("data").and_then(|v| v.get("unchanged")),
            Some(&JsValue::Bool(true))
        );
        assert_eq!(failure.get("type"), Some(&JsValue::from("error")));
        assert_eq!(failure.get("toolCallId"), Some(&JsValue::from("three")));
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert_eq!(seen.lock().unwrap()[0], full_first);
        let observed = observer.0.lock().unwrap();
        assert_eq!(observed.len(), 3);
        assert_eq!(observed[0], vec![first]);
        assert_eq!(observed[1], vec![second]);
        assert_eq!(observed[2], vec![failure]);
        assert_eq!(gate.seen.lock().unwrap().len(), 3);
        let events = lifecycle.0.lock().unwrap();
        assert_eq!(
            events.iter().filter(|e| e.as_str() == "PreToolUse").count(),
            3
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.as_str() == "PostToolUse")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.as_str() == "PostToolUseFailure")
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    struct MutatingPreHook(Arc<AtomicUsize>);
    impl pilotdeck_gateway::runtime::tools::ToolLifecycle for MutatingPreHook {
        fn dispatch(
            &self,
            event: &'static str,
            _: &JsString,
            _: JsObject,
            _: &ToolRunContext,
        ) -> HostFuture<Option<pilotdeck_gateway::runtime::hooks::LifecycleDispatchResult>>
        {
            let count = self.0.clone();
            Box::pin(async move {
                if event != "PreToolUse" {
                    return Ok(None);
                }
                count.fetch_add(1, Ordering::SeqCst);
                Ok(Some(
                    pilotdeck_gateway::runtime::hooks::LifecycleDispatchResult {
                        effects: vec![
                            pilotdeck_gateway::runtime::hooks::HookEffect::UpdatedToolInput {
                                input: obj([("command", "rm file".into())]),
                            },
                        ],
                        ..Default::default()
                    },
                ))
            })
        }
    }
    #[tokio::test]
    async fn final_gate_sees_pre_hook_updated_input_before_tool_execution() {
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        register(
            &registry,
            &parse_json(r#"{"name":"bash","safe":false}"#).unwrap(),
            &probe,
        );
        let gate = Arc::new(FinalGateProbe {
            deny_danger: true,
            ..Default::default()
        });
        let hook_count = Arc::new(AtomicUsize::new(0));
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent)
            .with_lifecycle(Arc::new(MutatingPreHook(hook_count.clone())))
            .with_final_admission(gate.clone());
        let call = ToolCall {
            id: "hooked".into(),
            name: "bash".into(),
            input: parse_json(r#"{"command":"pwd"}"#).unwrap(),
        };
        let host = context(&probe, &ToolCancellation::default(), None);
        let error = runtime
            .execute_batch(vec![call], ToolRunContext::new(host))
            .await
            .unwrap_err();
        assert!(error.message.contains("final input denied"));
        assert_eq!(hook_count.load(Ordering::SeqCst), 1);
        assert!(
            probe.events.lock().unwrap().is_empty(),
            "tool body must not run after final gate refuses"
        );
        assert_eq!(
            gate.seen.lock().unwrap()[0].2.get("command"),
            Some(&JsValue::from("rm file"))
        );
    }

    #[tokio::test]
    async fn managed_batch_hands_pending_sibling_to_owner_after_first_error() {
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        register(
            &registry,
            &parse_json(r#"{"name":"test","safe":true}"#).unwrap(),
            &probe,
        );
        let gate = Arc::new(FinalGateProbe {
            deny_danger: true,
            ..Default::default()
        });
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent)
            .with_final_admission(gate.clone());
        let calls = vec![
            ToolCall {
                id: "denied".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true,"command":"rm file"}"#).unwrap(),
            },
            ToolCall {
                id: "sibling".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true,"delay":50}"#).unwrap(),
            },
        ];
        let host = context(&probe, &ToolCancellation::default(), None);
        assert!(
            runtime
                .execute_batch(calls, ToolRunContext::new(host))
                .await
                .is_err()
        );
        assert_eq!(gate.detached.load(Ordering::SeqCst), 1);
        assert_eq!(
            probe.live.load(Ordering::SeqCst),
            1,
            "result error must not pretend the sibling exited"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while probe.live.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("tracked fixture sibling should finish");
    }

    #[tokio::test]
    async fn managed_failed_handoff_still_transfers_every_started_sibling() {
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        register(
            &registry,
            &parse_json(r#"{"name":"test","safe":true}"#).unwrap(),
            &probe,
        );
        let gate = Arc::new(FinalGateProbe {
            deny_danger: true,
            fail_handoff: true,
            ..Default::default()
        });
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent)
            .with_final_admission(gate.clone());
        let calls = vec![
            ToolCall {
                id: "denied".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true,"command":"rm file"}"#).unwrap(),
            },
            ToolCall {
                id: "first".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true,"delay":50}"#).unwrap(),
            },
            ToolCall {
                id: "second".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true,"delay":50}"#).unwrap(),
            },
        ];
        let host = context(&probe, &ToolCancellation::default(), None);
        let error = runtime
            .execute_batch(calls, ToolRunContext::new(host))
            .await
            .unwrap_err();
        assert!(error.message.contains("tracked handoff unavailable"));
        assert_eq!(
            gate.detached.load(Ordering::SeqCst),
            2,
            "every already-polled sibling must reach the owner even after a handoff error"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while probe.live.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn managed_child_producer_refusal_precedes_any_tool_poll() {
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        register(
            &registry,
            &parse_json(r#"{"name":"test","safe":true}"#).unwrap(),
            &probe,
        );
        let gate = Arc::new(FinalGateProbe {
            fail_child: true,
            ..Default::default()
        });
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent)
            .with_final_admission(gate.clone());
        let calls = vec![
            ToolCall {
                id: "one".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true}"#).unwrap(),
            },
            ToolCall {
                id: "two".into(),
                name: "test".into(),
                input: parse_json(r#"{"safe":true}"#).unwrap(),
            },
        ];
        let host = context(&probe, &ToolCancellation::default(), None);
        assert!(
            runtime
                .execute_batch(calls, ToolRunContext::new(host))
                .await
                .is_err()
        );
        assert!(probe.events.lock().unwrap().is_empty());
        assert!(gate.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn final_gate_refuses_real_write_file_before_creating_file() {
        let root = std::env::temp_dir().join(format!(
            "pd-write-final-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let probe = Probe::new();
        let registry = ToolRegistry::default();
        registry
            .register(Arc::new(
                pilotdeck_gateway::runtime::tools::builtin::write_file::WriteFileTool::new(),
            ))
            .unwrap();
        let gate = Arc::new(FinalGateProbe {
            deny_write: true,
            ..Default::default()
        });
        let runtime = ToolRuntime::new(registry, Arc::new(Permission), SchedulerMode::Concurrent)
            .with_final_admission(gate.clone());
        let host = context(
            &probe,
            &ToolCancellation::default(),
            Some(&obj([("cwd", JsValue::from(root.to_str().unwrap()))])),
        );
        let call = ToolCall {
            id: "write".into(),
            name: "write_file".into(),
            input: parse_json(r#"{"file_path":"created.txt","content":"must not be written"}"#)
                .unwrap(),
        };
        let result = runtime
            .execute_batch(vec![call], ToolRunContext::new(host))
            .await;
        assert!(result.is_err());
        assert_eq!(gate.seen.lock().unwrap().len(), 1);
        assert!(
            !root.join("created.txt").exists(),
            "denied tool must not create file"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
