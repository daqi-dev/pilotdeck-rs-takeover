//! 真实 resident → Native PilotDeck Loop → D96 → PilotDeck Model → 受管 HTTP。
//! 这是无工具、无 Context、无可选 hostModules 的模型组装验收，非完整产品入口。
#![forbid(unsafe_code)]

#[path = "managed_model/host.rs"]
mod host;
#[path = "managed_model/http.rs"]
mod http;
#[path = "managed_model/setup.rs"]
mod setup;
#[path = "managed_model/trace.rs"]
mod trace;
#[path = "managed_model/wire.rs"]
mod wire;

use serde_json::{Value, json};
use std::time::Duration;
const WAIT: Duration = Duration::from_secs(90);

fn payload(session: &str, turn: &str) -> Value {
    json!({"operationId":turn,"sessionId":session,"turnId":turn,"payload":{
        "provider":"p","model":"m","systemPrompt":"Preserve the fixture instruction.",
        "maxOutputTokens":123,"maxTurns":2,
        "messages":[{"role":"user","content":[{"type":"text","text":"Echo fixture input."}]}]
    }})
}

fn run(wire: &mut wire::Wire, sid: &str, turn: u64, payload: Value) -> Value {
    let accepted = wire.call(
        "execution.input",
        json!({"session_id":sid,
        "idempotency_key":format!("fixture-{turn}"),"turn":turn,"input":payload.to_string()}),
    );
    let call = accepted["call_ref"]
        .as_str()
        .expect("accepted call identity");
    let controller = wire.wait("execution.get", json!({"session_id":sid,"call_ref":call}));
    assert_eq!(
        controller["state"], "completed",
        "input controller: {controller}"
    );
    let child = controller["result"]["child_ref"]
        .as_str()
        .expect("real child Loop identity");
    let done = wire.wait("execution.get", json!({"session_id":sid,"call_ref":child}));
    // 同时验证公开结果投递，不能只从 execution.get 旁读已完成的 Loop。
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let events = wire.call("execution.events", json!({"session_id":sid,"after":0}));
        if let Some(publication) = events
            .as_array()
            .expect("execution event list")
            .iter()
            .find(|event| {
                event["payload"]["content"]["fixture_kind"] == "product_result"
                    && event["payload"]["content"]["child_ref"] == child
            })
        {
            assert_eq!(
                publication["payload"]["outcome"], done["state"],
                "result state changed at output port"
            );
            assert_eq!(
                publication["payload"]["content"]["result"], done["result"],
                "result changed at output port"
            );
            assert_eq!(
                publication["payload"]["error"], done["error"],
                "error changed at output port"
            );
            return done;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Loop result was not published by its authorized controller: {events}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn session(wire: &mut wire::Wire) -> String {
    let created = wire.call("session/new", json!({}));
    let sid = created["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let lease = wire.call(
        "session.lease_acquire",
        json!({"session_id":sid,"ttl_ms":300000}),
    );
    assert!(
        lease["data"]["epoch"]
            .as_u64()
            .is_some_and(|epoch| epoch > 0),
        "{lease}"
    );
    let opened = wire.call("execution.open", json!({"session_id":sid}));
    assert_eq!(opened["protocol"], agentcpm_scheduler_protocol::PROTOCOL);
    sid
}

fn source_events(events: &Value) -> Vec<Value> {
    events
        .as_array()
        .expect("execution event list")
        .iter()
        .filter(|event| event["payload"]["content"]["fixture_kind"] == "product_event")
        .map(|event| {
            serde_json::from_str(
                event["payload"]["content"]["event_json"]
                    .as_str()
                    .expect("original event text"),
            )
            .expect("original event JSON")
        })
        .collect()
}

// Flush each observation and flush again during unwinding so a failed check
// leaves the original wire frames and whatever HTTP state was observed.
struct HeldEvidence<'a> {
    host: &'a host::Resident,
    http: &'a http::LocalHttp,
    context: Value,
    observations: Vec<Value>,
}
impl HeldEvidence<'_> {
    fn record(&mut self, stage: &str, frame: Option<&str>) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|v| v.as_millis())
            .unwrap_or_default();
        self.observations
            .push(json!({"stage":stage,"observed_epoch_ms":timestamp_ms,
            "frame_raw":frame,"second_is_held":self.http.second_is_held()}));
        self.flush();
    }
    fn flush(&self) {
        let value = json!({"context":self.context,"observations":self.observations,
            "http_requests":self.http.requests_evidence(),"second_is_held":self.http.second_is_held()});
        if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
            let _ = std::fs::write(self.host.root.join("held-evidence.json"), bytes);
        }
    }
}
impl Drop for HeldEvidence<'_> {
    fn drop(&mut self) {
        self.record("scope_exit", None);
    }
}

fn verify_interrupted_local_release(host: &host::Resident, http: &http::LocalHttp) {
    use agentcpm_iface::model_session::{
        MODEL_EVENT_PROFILE_V1, MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1, MODEL_REQUEST_PROFILE_V1,
        REQUIRED_MODEL_EVENT_PROFILE, REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE,
        REQUIRED_MODEL_REQUEST_PROFILE,
    };
    let mut evidence = HeldEvidence {
        host,
        http,
        context: json!({}),
        observations: Vec::new(),
    };
    http.hold_second_request();
    evidence.record("hold_requested", None);
    let mut control = wire::Wire::new_with_local_resource_facts(host.addr);
    let sid = control.call("session/new", json!({}))["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    evidence.context["session_id"] = json!(sid);
    evidence.record("session_created", None);
    let epoch = control.call(
        "session.lease_acquire",
        json!({"session_id":sid,"ttl_ms":300000}),
    )["data"]["epoch"]
        .as_u64()
        .unwrap();
    let opened = control.call("model.session_open", json!({"session_id":sid,"epoch":epoch,"mode":"request","model":"m","endpoint_ref":"p", "idempotency_key":"held-open", "extensions":{ REQUIRED_MODEL_EVENT_PROFILE:MODEL_EVENT_PROFILE_V1, REQUIRED_MODEL_REQUEST_PROFILE:MODEL_REQUEST_PROFILE_V1, REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE:MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1 }}));
    let model = opened["model_session_id"].as_str().unwrap().to_owned();
    evidence.context["model_session_id"] = json!(model);
    evidence.context["input_id"] = json!("held-input");
    evidence.record("model_opened", None);
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let state = control.call("model.session_get", json!({"model_session_id":model}));
        if state["state"] == "ready" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "model did not become ready: {state}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut view = wire::Wire::new_with_local_resource_facts(host.addr);
    view.call(
        "session.stream_open",
        json!({"session_id":sid,"view":"model_session","model_session_id":model}),
    );
    let request = br#"{"provider":"p","model":"m","messages":[{"role":"user","content":[{"type":"text","text":"held"}]}]}"#;
    let content_hex = request
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    control.call("model.input_append", json!({"model_session_id":model,"client_message_id":"held-1","input_id":"held-input","kind":"request_snapshot","content_type":MODEL_REQUEST_PROFILE_V1,"content_hex":content_hex,"chunk_seq":0,"last_chunk":false,"epoch":epoch,"idempotency_key":"held-append"}));
    let commit = control.call("model.input_commit", json!({"model_session_id":model,"input_id":"held-input","epoch":epoch,"idempotency_key":"held-commit"}));
    let job = commit["job_id"].as_str().expect("admitted job").to_owned();
    evidence.context = json!({"session_id":sid,"model_session_id":model,"input_id":"held-input","job_id":job,"response_id":null});
    evidence.record("committed", None);
    let response = loop {
        let (frame, raw) = view.next_stream_frame(deadline);
        evidence.record("stream_frame", Some(&raw));
        let data = &frame["params"]["data"];
        if data["kind"] == "response_started" {
            break data["response_id"].as_str().unwrap().to_owned();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "response did not start"
        );
    };
    evidence.context["response_id"] = json!(response);
    evidence.record("response_bound", None);
    while http.requests().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "held request never reached loopback provider"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    evidence.record("second_request_observed", None);
    while !http.second_is_held() {
        assert!(
            std::time::Instant::now() < deadline,
            "provider did not reach held-body barrier"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    evidence.record("second_body_held", None);
    control.call("model.response_interrupt", json!({"model_session_id":model,"response_id":response,"epoch":epoch,"idempotency_key":"held-interrupt"}));
    let mut saw_interrupted = false;
    loop {
        let (frame, raw) = view.next_stream_frame(deadline);
        evidence.record("stream_frame", Some(&raw));
        let data = &frame["params"]["data"];
        match data["kind"].as_str() {
            Some("response_interrupted") => {
                assert_eq!(data["backend_stop"], "unknown");
                saw_interrupted = true;
            }
            Some("local_resources_released") => {
                assert!(saw_interrupted, "late fact must follow interrupted outcome");
                assert!(
                    http.second_is_held(),
                    "receipt must not depend on remote provider ending its response"
                );
                assert_eq!(data["job_id"], job);
                assert_eq!(data["response_id"], response);
                assert!(data.get("end_kind").is_none());
                break;
            }
            Some("response_completed") => panic!("interrupted generation became success"),
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no late local release"
        );
    }
    // The fake remote is still deliberately held: this receipt only speaks to
    // the host-owned attempt/pool, never remote provider computation.
    assert_eq!(http.requests().len(), 2);
    host.assert_lease_drops(2);
    control.call(
        "model.session_close",
        json!({"model_session_id":model,"epoch":epoch,"idempotency_key":"held-close"}),
    );
    loop {
        let (frame, raw) = view.next_stream_frame(deadline);
        evidence.record("stream_frame", Some(&raw));
        let data = &frame["params"]["data"];
        if data["kind"] == "session_closed" {
            assert_eq!(
                data["terminal"], "unknown",
                "local cleanup is not remote stop proof"
            );
            break;
        }
        assert_ne!(
            data["kind"], "response_completed",
            "interrupted generation became success"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "no session close after late local release"
        );
    }
    assert!(http.second_is_held());
    evidence.record("before_remote_release", None);
    http.release_second_request();
    evidence.record("remote_released", None);
}

fn verify(demand: bool) {
    http::verify_idle_connection_does_not_block_another_request_or_shutdown();
    let http = http::LocalHttp::start();
    let host = if demand {
        host::Resident::start_managed_demand(http.addr)
    } else {
        host::Resident::start_managed(http.addr)
    };
    println!("resident_model artifacts: {}", host.root.display());
    let mut wire = wire::Wire::new_with_local_resource_facts(host.addr);
    let sid = session(&mut wire);
    let done = run(&mut wire, &sid, 7, payload(&sid, "product-turn-7"));
    host.save("success.json", &done);
    let requests = http.requests();
    host.save("http-requests.json", &json!(requests));
    assert_eq!(done["state"], "completed", "{done}");
    let output: Value = serde_json::from_str(
        done["result"]
            .as_str()
            .expect("opaque original result JSON"),
    )
    .unwrap();
    assert_eq!(output["result"]["type"], "success", "{output}");
    assert_eq!(output["result"]["stopReason"], "completed");
    assert_eq!(output["result"]["sessionId"], sid);
    assert_eq!(output["result"]["turnId"], "product-turn-7");
    assert_eq!(
        output["result"]["finalMessage"]["content"][0]["text"],
        "Hello fixture."
    );
    assert_eq!(output["result"]["usage"]["inputTokens"], 11);
    assert_eq!(output["result"]["usage"]["outputTokens"], 3);
    assert_eq!(output["result"]["usage"]["totalTokens"], 14);
    assert_eq!(output["messages"].as_array().unwrap().len(), 2);
    assert!(output.get("seedState").is_some() && output.get("modelState").is_some());
    host.assert_model_closed();
    let audit_path = host
        .root
        .join("storage")
        .join("sessions")
        .join(&sid)
        .join("events.tsv");
    let audit = std::fs::read_to_string(&audit_path).expect("resident model audit");
    let release = audit
        .find("model_session.local_resources_released")
        .expect("actual host accepted original generation release fact");
    let completed = audit
        .find("model_session.response_completed")
        .expect("response completion audit");
    assert!(
        release < completed,
        "local resource receipt precedes response completion"
    );
    assert_eq!(
        audit
            .matches("model_session.local_resources_released")
            .count(),
        1,
        "one generation release fact"
    );
    assert_eq!(requests.len(), 1, "one logical request, no replay");
    http::check_request(&requests[0]).expect("exact provider request checks");
    // 负向控制：同一请求判定器必须捕捉破坏后的 provider 行为。
    let mut corrupted = requests[0].clone();
    corrupted["body"]["stream"] = json!(false);
    assert!(http::check_request(&corrupted).is_err());
    let events = wire.call("execution.events", json!({"session_id":sid,"after":0}));
    host.save("execution-events.json", &events);
    assert!(
        !events.to_string().contains("fixture-secret"),
        "registered secret escaped in public events"
    );
    assert!(
        !done.to_string().contains("fixture-secret"),
        "registered secret escaped in result"
    );
    let events_list = events.as_array().unwrap();
    assert!(
        events_list
            .iter()
            .filter(|event| event["kind"] == "call.result")
            .all(|event| event["payload"]["error"].is_null()),
        "normal execution must not hide failed controller calls: {events}"
    );
    assert!(
        events_list
            .iter()
            .any(|event| event["payload"]["content"]["fixture_kind"] == "preparation_finished")
    );
    assert!(
        events_list
            .iter()
            .filter(|event| event["payload"]["content"].get("fixture_kind").is_some())
            .all(|event| event["payload"]["content"]["trusted_turn"] == 7)
    );
    let events = source_events(&events);
    assert!(events.iter().any(|event| event["type"] == "turn_completed"));
    let model_events: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "model_event")
        .collect();
    let end = model_events
        .iter()
        .position(|event| event["event"]["type"] == "message_end")
        .expect("source message_end");
    let usage = model_events
        .iter()
        .position(|event| event["event"]["type"] == "usage")
        .expect("source usage");
    assert!(end < usage, "post-message_end usage must remain observable");
    // 未安装的生命周期不能用空实现冒充；必须在模型派发前明确失败。
    let rejected_sid = session(&mut wire);
    let mut rejected_payload = payload(&rejected_sid, "unsupported-hook");
    rejected_payload["payload"]["hostModules"] = json!({"lifecycle":{"methods":["dispatch"]}});
    let rejected = run(&mut wire, &rejected_sid, 9, rejected_payload);
    host.save("unsupported-binding.json", &rejected);
    assert_eq!(rejected["state"], "failed", "{rejected}");
    let failure: agentcpm_scheduler_protocol::Failure =
        serde_json::from_value(rejected["error"].clone()).expect("public Scheduler failure");
    let reason = "fixture does not install hostModules";
    assert_eq!(failure.code, "PILOTDECK_PORT_FAILURE");
    assert_eq!(failure.message_key, "diagnostic.external");
    assert_eq!(failure.params["pilotdeck_error_kind"], "invalid_response");
    assert_eq!(failure.params["detail"], reason);
    assert_eq!(failure.params["detail_encoding"], "utf8");
    assert_eq!(
        failure.params["message_utf16"],
        json!(reason.encode_utf16().collect::<Vec<_>>())
    );
    assert_eq!(failure.params["stop_reason"], "host_port_failure");
    assert_eq!(
        http.requests().len(),
        1,
        "rejected admission must perform zero HTTP calls"
    );
    verify_interrupted_local_release(&host, &http);
    assert_eq!(
        http.requests().len(),
        2,
        "one success and one interrupted provider request"
    );
    drop(wire);
    let root = host.shutdown();
    http.shutdown();
    std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({"harness_runs":1,"checks":4,"passed_checks":4,"scope":"test-only snapshot/lease supplier; real managed resident, issuer, guarded pull and loopback HTTP; not production project lease or full P4", "native_provider":"source-built managed_native example", "host_default_features":false,"http_requests":2,"negative_control":"corrupted stream flag rejected","resident_shutdown":"confirmed"})).unwrap()).unwrap();
    println!(
        "managed_model: 4 checks passed (success, request negative control, unsupported binding, interrupted late release)"
    );
}

// Gate-only evidence remains independent of the legacy/demand receipts. During
// the credit experiment it records only in memory; disk I/O happens before
// network startup and after shutdown, or during stack unwinding.
struct GateEvidence {
    root: std::path::PathBuf,
    wire: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    http: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
    context: Value,
    observations: Vec<Value>,
    completed: Vec<&'static str>,
    passed: bool,
}
impl GateEvidence {
    fn new() -> Self {
        use sha2::{Digest, Sha256};
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pilotdeck-gate-evidence-{}-{now}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("create unique independent gate evidence root");
        println!("gate-child evidence: {}", root.display());
        let mut evidence = Self {
            root,
            wire: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            http: None,
            context: json!({
                "role":"gate-child",
                "kernel_identity":"linked into the identified harness; no Git revision inferred",
                "api_version":agentcpm_iface::API_VERSION,
                "scheduler_protocol":agentcpm_scheduler_protocol::PROTOCOL,
                "requested_abi_version":2,
                "native_provider_role":"--native-provider-demand",
                "assembly_roles_note":"assembly.json role list is legacy fixture metadata; gate starts the demand role named here",
            }),
            observations: Vec::new(),
            completed: Vec::new(),
            passed: false,
        };
        evidence
            .persist("running")
            .expect("write initial gate receipt");
        let executable = std::env::current_exe().expect("gate harness path");
        let executable_bytes = std::fs::read(&executable).expect("gate harness bytes");
        evidence.context["harness_path"] = json!(executable);
        evidence.context["harness_sha256"] =
            json!(format!("{:x}", Sha256::digest(&executable_bytes)));
        evidence.context["harness_bytes"] = json!(executable_bytes.len());
        evidence
    }
    fn initialize_observed(&self, connection: &str) -> Option<Value> {
        self.wire
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|entry| {
                entry["connection"] == connection
                    && entry["direction"] == "receive"
                    && entry["frame"]["id"] == 1
            })
            .and_then(|entry| entry["frame"].get("result"))
            .cloned()
    }
    fn mark(&mut self, name: &'static str, supplier: Option<usize>, requests: Option<usize>) {
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default();
        self.completed.push(name);
        self.observations
            .push(json!({"checkpoint":name,"observed_epoch_ms":epoch_ms,
            "supplier_calls":supplier,"http_requests":requests}));
    }
    fn persist(&self, status: &str) -> std::io::Result<()> {
        let wire = self
            .wire
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let http = self.http.as_ref().map(|requests| {
            requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        });
        let assembly = self
            .context
            .get("host_root")
            .and_then(Value::as_str)
            .and_then(|root| std::fs::read(std::path::Path::new(root).join("assembly.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let receipt = json!({
            "schema":"pilotdeck.fixture.gate-evidence/1", "status":status,
            "failure_detail":"panic payload, if any, remains in process stderr; receipt records the unwind state",
            "context":self.context, "assembly":assembly,
            "check_groups":{"expected":7,"completed":self.completed.len(),"names":self.completed},
            "observations":self.observations, "wire_frames":wire, "http_requests":http,
            "measurement":"Gate wire tracing adds timestamps and in-memory mutex operations at each send/receive; checkpoint snapshots add atomic/mutex reads. Disk writes are limited to before startup and after shutdown or on unwind. No independent driver-poll count is claimed.",
        });
        let data = serde_json::to_vec_pretty(&receipt).map_err(std::io::Error::other)?;
        let temp = self.root.join("receipt.tmp");
        std::fs::write(&temp, data)?;
        std::fs::rename(temp, self.root.join("receipt.json"))
    }
    fn finish(&mut self) {
        assert_eq!(self.completed.len(), 7, "gate evidence groups incomplete");
        self.persist("passed")
            .expect("persist successful gate receipt");
        self.passed = true;
        println!(
            "gate-child: 7 checkpoint groups passed; receipt: {}",
            self.root.join("receipt.json").display()
        );
    }
}
impl Drop for GateEvidence {
    fn drop(&mut self) {
        if !self.passed {
            let status = if std::thread::panicking() {
                "failed_unwind"
            } else {
                "incomplete"
            };
            if let Err(error) = self.persist(status) {
                eprintln!(
                    "gate receipt write failed at {}: {error}",
                    self.root.display()
                );
            }
        }
    }
}

// Direct public-wire probe controls the precise consumer credit boundary. The
// separate demand Native run above proves the product adapter uses that wire.
fn verify_demand_gate() {
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_EVENT_PROFILE_V1,
        MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1, MODEL_REQUEST_PROFILE_V1,
        REQUIRED_MODEL_CONSUMER_DEMAND_PROFILE, REQUIRED_MODEL_EVENT_PROFILE,
        REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE, REQUIRED_MODEL_REQUEST_PROFILE,
    };
    let mut evidence = GateEvidence::new();
    let http = http::LocalHttp::start();
    evidence.http = Some(http.requests_evidence_handle());
    let host = host::Resident::start_managed_demand(http.addr);
    // Move the guard after the host in drop order: a gate assertion unwind
    // records its frames before resident shutdown can block or be killed.
    let mut evidence = evidence;
    evidence.context["host_root"] = json!(host.root);
    evidence.context["http_listen"] = json!(http.addr.to_string());
    let mut control = wire::Wire::new_traced(host.addr, "control", evidence.wire.clone());
    let control_initialize = evidence.initialize_observed("control");
    evidence.context["control_initialize_result"] = json!(control_initialize);
    let sid = control.call("session/new", json!({}))["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    evidence.context["session_id"] = json!(sid);
    let epoch = control.call(
        "session.lease_acquire",
        json!({"session_id":sid,"ttl_ms":300000}),
    )["data"]["epoch"]
        .as_u64()
        .unwrap();
    evidence.context["epoch"] = json!(epoch);
    let opened = control.call("model.session_open", json!({"session_id":sid,"epoch":epoch,"mode":"request","model":"m","endpoint_ref":"p","idempotency_key":"demand-open","extensions":{
        REQUIRED_MODEL_EVENT_PROFILE: MODEL_EVENT_PROFILE_V1,
        REQUIRED_MODEL_REQUEST_PROFILE: MODEL_REQUEST_PROFILE_V1,
        REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE: MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
        REQUIRED_MODEL_CONSUMER_DEMAND_PROFILE: MODEL_CONSUMER_DEMAND_PROFILE_V1}}));
    assert!(
        opened["negotiated_profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == MODEL_CONSUMER_DEMAND_PROFILE_V1)
    );
    let model = opened["model_session_id"].as_str().unwrap().to_owned();
    evidence.context["model_session_id"] = json!(model);
    evidence.context["input_id"] = json!("gate-input");
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let state = control.call("model.session_get", json!({"model_session_id":model}));
        if state["state"] == "ready" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "model not ready: {state}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut view = wire::Wire::new_traced(host.addr, "view", evidence.wire.clone());
    let view_initialize = evidence.initialize_observed("view");
    evidence.context["view_initialize_result"] = json!(view_initialize);
    view.call(
        "session.stream_open",
        json!({"session_id":sid,"view":"model_session","model_session_id":model}),
    );
    let request = br#"{"provider":"p","model":"m","messages":[{"role":"user","content":[{"type":"text","text":"gate"}]}]}"#;
    let content_hex = request
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    control.call("model.input_append", json!({"model_session_id":model,"client_message_id":"gate-1","input_id":"gate-input","kind":"request_snapshot","content_type":MODEL_REQUEST_PROFILE_V1,"content_hex":content_hex,"chunk_seq":0,"last_chunk":false,"epoch":epoch,"idempotency_key":"gate-append"}));
    let commit = control.call("model.input_commit", json!({"model_session_id":model,"input_id":"gate-input","epoch":epoch,"idempotency_key":"gate-commit"}));
    let job = commit["job_id"].as_str().unwrap().to_owned();
    evidence.context["job_id"] = json!(job);
    // Consume a public history frame before credit; observation must remain inert.
    let (observed, _) = view.next_stream_frame(deadline);
    assert_ne!(observed["params"]["data"]["kind"], "structured");
    // No call to advance: neither observation nor the closed-gate commit starts work.
    for _ in 0..5 {
        control.call("model.session_get", json!({"model_session_id":model}));
        assert_eq!(host.supplier_calls(), 0, "read/commit started supplier");
        assert!(http.requests().is_empty(), "read/commit started HTTP");
        std::thread::sleep(Duration::from_millis(10));
    }
    evidence.mark(
        "pre_credit_observation",
        Some(host.supplier_calls()),
        Some(http.requests().len()),
    );
    let initial = json!({"model_session_id":model,"job_id":job,"input_id":"gate-input","previous_event_seq":null,"epoch":epoch,"idempotency_key":"gate-first"});
    control.call("model.delivery_advance", initial.clone());
    let (sequence, response) = loop {
        let (frame, _) = view.next_stream_frame(deadline);
        let e = &frame["params"]["data"];
        if e["kind"] == "structured" {
            assert_eq!(
                e["event_type"], "request_started",
                "first credit must stop at the original request_started yield"
            );
            break (
                e["event_seq"].as_str().unwrap().to_owned(),
                e["response_id"].as_str().unwrap().to_owned(),
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no first source yield"
        );
    };
    assert_eq!(host.supplier_calls(), 1);
    assert!(
        http.requests().is_empty(),
        "first request_started yield precedes HTTP"
    );
    evidence.context["response_id"] = json!(response);
    evidence.context["first_host_event_seq"] = json!(sequence);
    evidence.mark(
        "first_credit_request_started",
        Some(host.supplier_calls()),
        Some(http.requests().len()),
    );
    // Exact repeated initial ack is idempotent, even after the first event.
    control.call("model.delivery_advance", initial);
    assert_eq!(host.supplier_calls(), 1);
    // Drain any independent facts; the replay must not yield another source.
    let quiet_until = std::time::Instant::now() + Duration::from_millis(100);
    while std::time::Instant::now() < quiet_until {
        match view
            .try_stream_frame(quiet_until.saturating_duration_since(std::time::Instant::now()))
        {
            Some(frame) => assert_ne!(
                frame["params"]["data"]["kind"], "structured",
                "duplicate ack minted a second yield"
            ),
            None => break,
        }
    }
    let wrong_job = control.expect_error("model.delivery_advance", json!({"model_session_id":model,"job_id":"different-job","input_id":"gate-input","previous_event_seq":sequence,"epoch":epoch,"idempotency_key":"gate-wrong-job"}));
    assert!(!wrong_job.is_null());
    let wrong_order = control.expect_error("model.delivery_advance", json!({"model_session_id":model,"job_id":job,"input_id":"gate-input","previous_event_seq":"999999","epoch":epoch,"idempotency_key":"gate-wrong-order"}));
    assert!(!wrong_order.is_null());
    assert_eq!(host.supplier_calls(), 1);
    assert!(
        http.requests().is_empty(),
        "replayed or rejected credit must not start HTTP"
    );
    evidence.mark(
        "replay_and_rejected_credit",
        Some(host.supplier_calls()),
        Some(http.requests().len()),
    );
    let next = json!({"model_session_id":model,"job_id":job,"input_id":"gate-input","previous_event_seq":sequence,"epoch":epoch,"idempotency_key":"gate-next"});
    control.call("model.delivery_advance", next);
    let next_sequence = loop {
        let (frame, _) = view.next_stream_frame(deadline);
        let e = &frame["params"]["data"];
        if e["kind"] == "structured" {
            assert_eq!(
                e["event_type"], "message_start",
                "the second credit delivers the first provider source event"
            );
            break e["event_seq"].as_str().unwrap().to_owned();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no second source yield"
        );
    };
    assert_ne!(sequence, next_sequence);
    assert_eq!(
        http.requests().len(),
        1,
        "second credit starts one HTTP request"
    );
    evidence.context["second_host_event_seq"] = json!(next_sequence);
    evidence.mark(
        "second_credit_message_start",
        Some(host.supplier_calls()),
        Some(http.requests().len()),
    );
    // Interrupt without acknowledging the outstanding second yield.
    control.call("model.response_interrupt", json!({"model_session_id":model,"response_id":response,"epoch":epoch,"idempotency_key":"gate-interrupt"}));
    loop {
        let (frame, _) = view.next_stream_frame(deadline);
        let e = &frame["params"]["data"];
        if e["kind"] == "local_resources_released" {
            assert_eq!(e["job_id"], job);
            break;
        }
        assert_ne!(
            e["kind"], "structured",
            "cleanup granted unsolicited credit"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "no release after cancellation"
        );
    }
    evidence.mark(
        "matching_local_release",
        Some(host.supplier_calls()),
        Some(http.requests().len()),
    );
    host.assert_lease_drops(1);
    evidence.mark(
        "lease_drop_confirmed",
        Some(host.supplier_calls()),
        Some(http.requests().len()),
    );
    drop(view);
    drop(control);
    host.shutdown();
    http.shutdown();
    evidence.mark("host_and_http_shutdown", None, None);
    evidence.finish();
}

// Host contract test: real session.create/claim/bind, real Native source,
// reservation and guarded port; fixture owner/credential are synthetic.
fn verify_project_forgery_rejection() {
    http::verify_idle_connection_does_not_block_another_request_or_shutdown();
    let http = http::LocalHttp::start();
    let host = host::Resident::start_managed_project(http.addr);
    let mut wire = wire::Wire::new_with_local_resource_facts(host.addr);
    let created = wire.call(
        "session.create",
        json!({"project_owner":true,"idempotency_key":"forged-project-create"}),
    );
    let reference = created["data"]["creation_ref"]
        .as_str()
        .expect("Host minted creation reference");
    let project = host.bind_created_project(reference);
    let sid = project.session_id().to_owned();
    assert_eq!(created["data"]["session_id"].as_str(), Some(sid.as_str()));
    let lease = wire.call(
        "session.lease_acquire",
        json!({"session_id":sid,"ttl_ms":300000}),
    );
    assert!(
        lease["data"]["epoch"]
            .as_u64()
            .is_some_and(|epoch| epoch > 0)
    );
    let opened = wire.call("execution.open", json!({"session_id":sid}));
    assert_eq!(opened["protocol"], agentcpm_scheduler_protocol::PROTOCOL);
    let result = run(&mut wire, &sid, 7, payload(&sid, "project-forgery-turn"));
    assert_eq!(
        result["state"], "completed",
        "valid project source positive control: {result}"
    );
    host.save("forged-project-evidence.json", &json!({"real_source_result":result["state"], "host_forgery_rejections":host.project_checks(), "raw_transport_starts":host.raw_starts(), "http_requests":http.requests().len()}));
    assert_eq!(
        host.project_checks(),
        6,
        "two forgery variants must each fail resolve, reservation and guarded start"
    );
    assert_eq!(
        host.raw_starts(),
        2,
        "one unpolled genuine preparation and one admitted driver attempt"
    );
    assert_eq!(
        http.requests().len(),
        1,
        "forgeries must not reach loopback HTTP"
    );
    http::check_request(&http.requests()[0])
        .expect("positive source reached original provider wire");
    host.assert_model_closed();
    project.revoke();
    let closed = wire.call(
        "session.close",
        json!({"session_id":sid,"generation":project.generation(),"reason":"fixture complete"}),
    );
    assert_eq!(closed["data"]["closed"], true);
    let deadline = std::time::Instant::now() + WAIT;
    while !project.is_released() {
        assert!(
            std::time::Instant::now() < deadline,
            "real project reservation did not release"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(project);
    drop(wire);
    host.shutdown();
    http.shutdown();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        if !args.iter().any(|arg| arg == "--ignored") {
            println!("managed_model: test");
        }
        return;
    }
    match args.first().map(String::as_str) {
        Some("--version" | "--help") => {}
        Some("--project-forged-child") => verify_project_forgery_rejection(),
        Some("--idle-selfcheck-only") => {
            http::verify_idle_connection_does_not_block_another_request_or_shutdown()
        }
        // Preserve the original isolated invocation and its failure verdict.
        Some("--isolated-child") => verify(false),
        Some("--demand-child") => verify(true),
        Some("--gate-child") => verify_demand_gate(),
        _ => {
            let mut failures = Vec::new();
            for role in ["--isolated-child", "--demand-child", "--gate-child"] {
                // Each source-built fixture runs separately, so a retained legacy
                // first failure cannot suppress demand-gate evidence.
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command.arg(role);
                for key in pilotdeck_http_transport::transport::PROXY_ENV_KEYS {
                    command.env_remove(key);
                }
                for key in [
                    "NODE_OPTIONS",
                    "SSL_CERT_FILE",
                    "SSL_CERT_DIR",
                    "NODE_EXTRA_CA_CERTS",
                ] {
                    command.env_remove(key);
                }
                let status = command.status().expect("start isolated managed fixture");
                if !status.success() {
                    failures.push(format!("{role}: {status}"));
                }
            }
            assert!(
                failures.is_empty(),
                "isolated managed fixture failed: {}",
                failures.join(", ")
            );
        }
    }
}
