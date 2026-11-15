//! 真实 resident → Native PilotDeck Loop → D96 → PilotDeck Model → 受管 HTTP。
//! 这是无工具、无 Context、无可选 hostModules 的模型组装验收，非完整产品入口。
#![forbid(unsafe_code)]

#[path = "managed_model/host.rs"]
mod host;
#[path = "managed_model/http.rs"]
mod http;
#[path = "managed_model/setup.rs"]
mod setup;
#[path = "managed_model/wire.rs"]
mod wire;

use serde_json::{Value, json};
use std::time::Duration;
const WAIT: Duration = Duration::from_secs(30);

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

fn verify() {
    http::verify_idle_connection_does_not_block_another_request_or_shutdown();
    let http = http::LocalHttp::start();
    let host = host::Resident::start_managed(http.addr);
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

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "--help") => {}
        Some("--isolated-child") => verify(),
        _ => {
            // Isolate production proxy/CA environment without mutating this
            // multi-threaded process or reading user proxy credentials.
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.arg("--isolated-child");
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
            assert!(
                status.success(),
                "isolated managed fixture failed: {status}"
            );
        }
    }
}
