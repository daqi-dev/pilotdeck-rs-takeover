//! 真实 resident → Native PilotDeck Loop → D96 → PilotDeck Model → 受管 HTTP。
//! 这是无工具、无 Context、无可选 hostModules 的模型组装验收，非完整产品入口。
#![forbid(unsafe_code)]

#[path = "resident_model/host.rs"]
mod host;
#[path = "resident_model/http.rs"]
mod http;
#[path = "resident_model/provider.rs"]
mod provider;
#[path = "resident_model/wire.rs"]
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

fn verify() {
    let http = http::LocalHttp::start();
    let host = host::Resident::start(http.addr);
    println!("resident_model artifacts: {}", host.root.display());
    let mut wire = wire::Wire::new(host.addr);
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
    assert_eq!(requests.len(), 1, "one logical request, no replay");
    http::check_request(&requests[0]).expect("exact provider request checks");
    // 负向控制：同一请求判定器必须捕捉破坏后的 provider 行为。
    let mut corrupted = requests[0].clone();
    corrupted["body"]["stream"] = json!(false);
    assert!(http::check_request(&corrupted).is_err());
    let events = wire.call("execution.events", json!({"session_id":sid,"after":0}));
    host.save("execution-events.json", &events);
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
    assert!(
        rejected
            .to_string()
            .contains("fixture does not install hostModules"),
        "{rejected}"
    );
    assert_eq!(
        http.requests().len(),
        1,
        "rejected admission must perform zero HTTP calls"
    );
    drop(wire);
    let root = host.shutdown();
    http.shutdown();
    std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({"harness_runs":1,"checks":3,"passed_checks":3,"scope":"model-only original preparation, real resident and HTTP; not full P4", "native_provider":"same source-built test executable", "host_default_features":false,"http_requests":1,"negative_control":"corrupted stream flag rejected","resident_shutdown":"confirmed"})).unwrap()).unwrap();
    println!(
        "resident_model: 3 checks passed (real model closure, request negative control, unsupported binding rejection)"
    );
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "--help") => {}
        Some("--native-provider") => provider::serve(false),
        Some("--native-controller") => provider::serve(true),
        _ => verify(),
    }
}
