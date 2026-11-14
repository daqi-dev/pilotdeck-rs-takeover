//! Fixed-TS causal admission control. This file is intentionally not linked yet.
//! Future cfg(test)-only integration points are documented in the private handoff.
//! No gate or context tap may be compiled into a production binary.
use crate::{
    native_process::{LocalGateway, LocalOptions},
    runtime::tools::ToolRunContext,
};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

type SignalProbe = Arc<dyn Fn() -> bool + Send + Sync>;
struct Gate {
    entered: AtomicBool,
    released: AtomicBool,
    signal: Mutex<Option<SignalProbe>>,
    release: tokio::sync::Semaphore,
}
impl Default for Gate {
    fn default() -> Self {
        Self {
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
            signal: Mutex::new(None),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}
#[derive(Clone, Debug, serde::Serialize)]
struct HookObservation {
    source: &'static str,
    key: Option<String>,
    matched: bool,
    context_keys: Vec<String>,
}
static OBSERVATIONS: OnceLock<Mutex<Vec<HookObservation>>> = OnceLock::new();
fn note_hook(source: &'static str, key: Option<String>, matched: bool, context_keys: Vec<String>) {
    if gates().lock().unwrap().is_empty() {
        return;
    }
    let mut rows = OBSERVATIONS.get_or_init(Mutex::default).lock().unwrap();
    if rows.len() == 64 {
        rows.remove(0);
    }
    rows.push(HookObservation {
        source,
        key,
        matched,
        context_keys,
    });
}
static GATES: OnceLock<Mutex<HashMap<String, Weak<Gate>>>> = OnceLock::new();
fn gates() -> &'static Mutex<HashMap<String, Weak<Gate>>> {
    GATES.get_or_init(Mutex::default)
}
fn lookup(session: &str) -> Option<Arc<Gate>> {
    gates().lock().unwrap().get(session).and_then(Weak::upgrade)
}
/// Called only under cfg(test), before the real tool starts admission. It does
/// not forward the signal to the implementation or fix the behavior under test.
pub(crate) fn capture_context(context: &ToolRunContext) {
    let key = context
        .fields
        .get("sessionId")
        .and_then(JsValue::as_string)
        .map(|s| String::from_utf16_lossy(s.units()));
    let gate = key.as_deref().and_then(lookup);
    note_hook(
        "tool_context",
        key,
        gate.is_some(),
        context
            .fields
            .entries()
            .iter()
            .map(|(key, _)| String::from_utf16_lossy(key.units()))
            .collect(),
    );
    let Some(gate) = gate else {
        return;
    };
    let signal = context.host.abort_signal.clone();
    let probe: SignalProbe = Arc::new(move || {
        signal.as_ref().is_some_and(|s| {
            s.downcast_ref::<crate::host::Cancellation>()
                .is_some_and(crate::host::Cancellation::is_cancelled)
                || s.downcast_ref::<crate::runtime::tools::ToolCancellation>()
                    .is_some_and(crate::runtime::tools::ToolCancellation::is_cancelled)
        })
    });
    *gate.signal.lock().unwrap() = Some(probe);
}
/// Called only under cfg(test), at the actual SessionContinuationPort
/// materialize boundary, before acquiring/constructing the native child.
pub(crate) async fn before_materialize(parent: &JsString) {
    let key = String::from_utf16_lossy(parent.units());
    let gate = lookup(&key);
    note_hook("materialize", Some(key), gate.is_some(), Vec::new());
    let Some(gate) = gate else {
        return;
    };
    gate.entered.store(true, Ordering::SeqCst);
    gate.release.acquire().await.unwrap().forget();
    gate.released.store(true, Ordering::SeqCst);
}
struct Registration {
    session: String,
    gate: Arc<Gate>,
}
impl Registration {
    fn new(session: String) -> Self {
        let gate = Arc::new(Gate {
            release: tokio::sync::Semaphore::new(0),
            ..Gate::default()
        });
        assert!(
            gates()
                .lock()
                .unwrap()
                .insert(session.clone(), Arc::downgrade(&gate))
                .is_none()
        );
        Self { session, gate }
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.gate.release.add_permits(1);
        gates().lock().unwrap().remove(&self.session);
    }
}
fn js(value: Value) -> JsValue {
    parse_json(&value.to_string()).unwrap()
}
fn wire(value: &JsValue) -> Value {
    serde_json::from_str(&stringify_json(value).unwrap().unwrap()).unwrap()
}
fn until(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "causal admission prerequisite: {what}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
/// Same registered-project storage contract as the public LocalGateway
/// fixture in tests/host_owned_storage_management.rs:708. No invented RPC.
fn register_fixture_project(pilot: &Path, project: &Path) -> String {
    let project_key = project.to_string_lossy().into_owned();
    let chats = crate::session_chat_dir(pilot, project, &project_key);
    let registered = chats.parent().unwrap();
    std::fs::create_dir_all(registered).unwrap();
    std::fs::write(registered.join(".cwd"), project_key.as_bytes()).unwrap();
    project_key
}
struct Provider {
    address: std::net::SocketAddr,
    child_models: Arc<AtomicUsize>,
    child_read_seen: Arc<AtomicBool>,
    errors: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Provider {
    fn start(project: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        assert_ne!(address.port(), 18789);
        let child_models = Arc::new(AtomicUsize::new(0));
        let child_read_seen = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (count, read, faults, stopping) = (
            child_models.clone(),
            child_read_seen.clone(),
            errors.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // Darwin accepted sockets can inherit nonblocking mode.
                        socket.set_nonblocking(false).unwrap();
                        if let Err(error) = answer(&mut socket, &project, &count, &read, &recorded)
                        {
                            faults.lock().unwrap().push(error);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => {
                        faults.lock().unwrap().push(error.to_string());
                        break;
                    }
                }
            }
        });
        Self {
            address,
            child_models,
            child_read_seen,
            errors,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
fn answer(
    socket: &mut TcpStream,
    project: &Path,
    child: &AtomicUsize,
    read_seen: &AtomicBool,
    recorded: &Mutex<Vec<Value>>,
) -> Result<(), String> {
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let header_end;
    loop {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("mock request ended before headers".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(n) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
            header_end = n + 4;
            break;
        }
    }
    let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let length: usize = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .ok_or("mock request has no content-length")?;
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("mock request ended before body".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let body: Value = serde_json::from_slice(&bytes[header_end..header_end + length])
        .map_err(|e| e.to_string())?;
    let material = body["messages"].to_string();
    let branch = if body["stream"] != true {
        "title"
    } else if material.contains("You are a subagent") && material.contains("CHILD-GEN-CLOSE") {
        if material.contains("GEN-CAPABILITY-OK") {
            "child-after-read"
        } else {
            "child-read"
        }
    } else if material.contains("PARENT-GEN-CLOSE") {
        "parent-subagent"
    } else {
        "unexpected"
    };
    let last = body["messages"]
        .as_array()
        .and_then(|rows| rows.last())
        .cloned()
        .unwrap_or(Value::Null)
        .to_string();
    recorded.lock().unwrap().push(json!({
        "requestLine":header.lines().next(), "model":body["model"], "stream":body["stream"], "branch":branch,
        "lastMessage":last.chars().take(600).collect::<String>(),
        "toolNames":body["tools"].as_array().map(|tools|tools.iter().map(|tool|tool["function"]["name"].clone()).collect::<Vec<_>>())
    }));
    let response = if body["stream"] != true {
        json!({"choices":[{"message":{"role":"assistant","content":"{\"title\":\"Generation gate\"}"},"finish_reason":"stop"}]}).to_string()
    } else {
        let material = body["messages"].to_string();
        let (delta, reason) = if material.contains("You are a subagent")
            && material.contains("CHILD-GEN-CLOSE")
        {
            child.fetch_add(1, Ordering::SeqCst);
            if material.contains("GEN-CAPABILITY-OK") {
                read_seen.store(true, Ordering::SeqCst);
                (
                    json!({"role":"assistant","content":"authorized child complete"}),
                    "stop",
                )
            } else {
                (
                    json!({"role":"assistant","tool_calls":[{"index":0,"id":"child-read","type":"function","function":{"name":"read_file","arguments":json!({"file_path":project.join("capability.txt")}).to_string()}}]}),
                    "tool_calls",
                )
            }
        } else if material.contains("PARENT-GEN-CLOSE") {
            (
                json!({"role":"assistant","tool_calls":[{"index":0,"id":"parent-subagent","type":"function","function":{"name":"subagent","arguments":json!({"description":"Read owned capability","prompt":"CHILD-GEN-CLOSE: read capability.txt and then report the exact marker.","subagent_type":"general-purpose"}).to_string()}}]}),
                "tool_calls",
            )
        } else {
            return Err("unexpected model prompt".into());
        };
        let first = json!({"id":"gen-stream","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}).to_string();
        let last = json!({"id":"gen-stream","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}).to_string();
        let sse = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).map_err(|e| e.to_string())?;
        return socket.flush().map_err(|e| e.to_string());
    };
    write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).map_err(|e|e.to_string())?;
    socket.flush().map_err(|e| e.to_string())
}
fn child_completed(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if child_completed(&path) {
                return true;
            }
        } else if path.extension().is_some_and(|ext| ext == "jsonl")
            && path
                .components()
                .any(|part| part.as_os_str() == "subagents")
            && std::fs::read_to_string(&path).ok().is_some_and(|raw| {
                raw.lines().any(|line| {
                    serde_json::from_str::<Value>(line).ok().is_some_and(|row| {
                        row["type"] == "turn_result" && row["result"]["type"] == "success"
                    })
                })
            })
        {
            return true;
        }
    }
    false
}
fn run_case(cancel_pending: bool) {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pdrs-admission-{}", uuid::Uuid::new_v4())));
    let home = scratch.0.join("home");
    let pilot = home.join("pilot");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&pilot).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let project_key = register_fixture_project(&pilot, &project);
    std::fs::write(project.join("capability.txt"), "GEN-CAPABILITY-OK\n").unwrap();
    let provider = Provider::start(project.clone());
    let config = pilot.join("pilotdeck.yaml");
    std::fs::write(&config, format!("schemaVersion: 1\nagent: {{model: owned/default}}\nmodel:\n  providers:\n    owned:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: owned-dummy-no-real-key\n      models: {{default: {{}}}}\ntelemetry: {{enabled: false}}\n", provider.address)).unwrap();
    let env = [
        ("HOME", home.to_string_lossy().into_owned()),
        ("PILOT_HOME", pilot.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH",
            config.to_string_lossy().into_owned(),
        ),
        ("ANALYTICS_ENABLED", "false".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    let gateway = Arc::new(
        LocalGateway::start(LocalOptions {
            pilot_home: pilot,
            project: project.clone(),
            os_home: Some(home),
            sidecar: scratch.0.join("absent-sidecar"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
            telemetry_context: None,
        })
        .unwrap(),
    );
    let session = format!("gen-parent-{}", uuid::Uuid::new_v4());
    let run = uuid::Uuid::new_v4().to_string();
    let registration = Registration::new(session.clone());
    let rows = Arc::new(Mutex::new(Vec::new()));
    let emitted = rows.clone();
    let submitting = gateway.clone();
    let submit_session = session.clone();
    let submit_run = run.clone();
    let submit_project = project_key.clone();
    let operation = thread::spawn(move || {
        submitting.submit_turn(js(json!({"sessionKey":submit_session,"projectKey":submit_project,"runId":submit_run,"channelKey":"test","message":"PARENT-GEN-CLOSE: launch the owned child.","maxTurns":1})), Arc::new(move |v| emitted.lock().unwrap().push(wire(&v))))
    });
    let mut answered = false;
    let mut permission_queries = Vec::new();
    let mut last_permissions = Value::Null;
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        until(
            || {
                let requests = wire(
                    &gateway
                        .request(
                            "permission_list",
                            js(json!({"sessionKey":session,"projectKey":project_key})),
                        )
                        .unwrap(),
                );
                if requests != last_permissions {
                    permission_queries.push(json!({"method":"permission_list", "sessionKey":session, "projectKey":project_key, "result":requests}));
                    last_permissions = requests.clone();
                }
                for request in requests["requests"].as_array().unwrap() {
                    assert_eq!(request["toolName"], "subagent");
                    assert!(!answered, "one exact grant");
                    let decision = gateway.request("permission_decide", js(json!({"sessionKey":session,"requestId":request["requestId"],"decision":"allow","remember":false}))).unwrap();
                    permission_queries.push(json!({"method":"permission_decide","sessionKey":session,"requestId":request["requestId"],"result":wire(&decision)}));
                    answered = true;
                }
                registration.gate.entered.load(Ordering::SeqCst)
            },
            "real permission then actual materialize gate",
        );
        assert!(answered);
        assert_eq!(provider.child_models.load(Ordering::SeqCst), 0);
        if cancel_pending {
            let aborting = gateway.clone();
            let abort_session = session.clone();
            let abort = thread::spawn(move || {
                aborting.request("abort_turn", js(json!({"sessionKey":abort_session,"runId":run,"reason":"owned_pending_admission_cancel"})))
            });
            until(
                || {
                    registration
                        .gate
                        .signal
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|probe| probe())
                },
                "actual parent tool abort signal before materialize release",
            );
            registration.gate.release.add_permits(1);
            abort.join().unwrap().unwrap();
        } else {
            registration.gate.release.add_permits(1);
        }
    }));
    // Always remove the held prerequisite before cleanup, including assertion failures.
    registration.gate.release.add_permits(1);
    if observed.is_err() {
        let _ = gateway.request(
            "abort_turn",
            js(json!({"sessionKey":session,"reason":"failed_fixture_prerequisite"})),
        );
    }
    let submitted = operation.join().unwrap();
    let completion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if observed.is_ok() && !cancel_pending {
            submitted.as_ref().unwrap();
            until(
                || provider.child_read_seen.load(Ordering::SeqCst) && child_completed(&scratch.0),
                "normally admitted child reads actual granted capability",
            );
        }
    }));
    let closed = gateway.request(
        "close_session",
        js(json!({"sessionKey":session,"reason":"fixture_cleanup"})),
    );
    let shutdown = gateway.shutdown(Duration::from_secs(5));
    let faults = provider.errors.lock().unwrap().clone();
    let child_models = provider.child_models.load(Ordering::SeqCst);
    let events = rows.lock().unwrap().clone();
    let context_signal_captured = registration.gate.signal.lock().unwrap().is_some();
    let context_signal_aborted = registration
        .gate
        .signal
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|probe| probe());
    let hook_observations = OBSERVATIONS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .clone();
    eprintln!("ADMISSION_FIXTURE_DIAGNOSTIC {}", serde_json::to_string_pretty(&json!({
        "cancelPending":cancel_pending, "registeredSession":session, "registeredProject":project_key, "answered":answered,
        "gateEntered":registration.gate.entered.load(Ordering::SeqCst),
        "gateReleased":registration.gate.released.load(Ordering::SeqCst),
        "contextSignalCaptured":context_signal_captured, "contextSignalAborted":context_signal_aborted,
        "submitted":format!("{submitted:?}"), "close":format!("{closed:?}"), "shutdown":format!("{shutdown:?}"),
        "providerRequests":provider.requests.lock().unwrap().clone(), "providerErrors":faults,
        "childModels":child_models, "events":events, "hookObservations":hook_observations, "permissionQueries":permission_queries,
    })).unwrap());
    drop(gateway);
    drop(registration);
    drop(provider);
    if let Err(panic) = observed {
        std::panic::resume_unwind(panic);
    }
    if let Err(panic) = completion {
        std::panic::resume_unwind(panic);
    }
    assert!(closed.is_ok(), "session cleanup: {closed:?}");
    assert!(shutdown.is_ok(), "gateway cleanup: {shutdown:?}");
    assert!(faults.is_empty(), "provider errors: {faults:?}");
    if cancel_pending {
        assert_eq!(
            child_models, 0,
            "cancelled pre-admission must never start child model after release"
        );
        assert!(
            events
                .iter()
                .any(|v| v["type"] == "turn_completed" && v["finishReason"] == "aborted_streaming"),
            "{events:?}"
        );
    } else {
        assert!(
            events.iter().any(|v| v["type"] == "tool_call_finished"
                && v["toolName"] == "subagent"
                && v["ok"] == true
                && v["data"]["subagentId"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())),
            "normal admission must publish its actual child id: {events:?}"
        );
        assert_eq!(
            child_models, 2,
            "one native read_file round and one completion round"
        );
    }
}
#[test]
fn normal_admission_then_actual_child_capability_completes() {
    run_case(false);
}
#[test]
fn cancelled_pending_materialize_never_publishes_or_runs_child() {
    run_case(true);
}
