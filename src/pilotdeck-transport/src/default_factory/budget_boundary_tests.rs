//! Actual factory/owning budget loop/EventSink over an isolated socket pair.
//! Host replies supply deterministic model usage and cost; no provider or child.
use super::*;
use crate::server::ExecutionServer;
use crate::stdio::ConnectionOwner;
use pilotdeck_protocol::admission::SidecarBinding;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

struct Clock;
impl ExecutionClock for Clock {
    fn now_epoch_ms(&self) -> f64 {
        1000.0
    }
    fn now_iso(&self) -> JsString {
        "1970-01-01T00:00:01.000Z".into()
    }
    fn parse_date(&self, _: &JsString) -> Result<Option<f64>, JsString> {
        Ok(None)
    }
}
fn json(value: &str) -> JsValue {
    pilotdeck_protocol::parse_json(value).unwrap()
}
fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value
        .get(key)
        .unwrap_or_else(|| panic!("missing {key}: {value:?}"))
}
fn label(value: &JsValue, key: &str) -> String {
    field(value, key).as_string().unwrap().to_utf8().unwrap()
}
fn read(io: &UnixStream) -> JsValue {
    let mut line = String::new();
    BufReader::with_capacity(1, io)
        .read_line(&mut line)
        .unwrap();
    json(&line)
}
fn send(io: &mut UnixStream, value: &JsValue) {
    writeln!(
        io,
        "{}",
        pilotdeck_protocol::stringify_json(value).unwrap().unwrap()
    )
    .unwrap();
}
fn reply(io: &mut UnixStream, request: &JsValue, payload: &str) {
    let mut response = json(payload).as_object().unwrap().clone();
    response.insert("kind".into(), "response".into());
    response.insert("messageId".into(), "host-ack".into());
    response.insert("inReplyTo".into(), field(request, "messageId").clone());
    send(io, &JsValue::Object(response));
}
fn durable_fallback(event: &JsValue) -> JsValue {
    // Exact TS sidecarStatusMessage's precedence, used only to expose the
    // observable result of the real factory output below.
    event.get("text").cloned().unwrap_or_else(|| {
        event
            .get("detail")
            .and_then(|d| d.get("message"))
            .filter(|v| v.as_string().is_some())
            .cloned()
            .unwrap_or_else(|| field(event, "event").clone())
    })
}
enum FactoryEntry {
    Sidecar,
    Native,
    ObservedNative,
}
fn run_budget(entry: FactoryEntry, task: bool) -> (Vec<JsValue>, JsValue) {
    let owner = ConnectionOwner::new(
        SidecarBinding {
            module_id: "pilotdeck-agent-loop".into(),
            module_instance_id: "instance".into(),
            connection_generation: "generation".into(),
            capabilities_version: "1".into(),
        },
        32,
    );
    let clock = Arc::new(Clock);
    let factory = OriginalAgentFactory {
        clock: clock.clone(),
        process_cwd: "/controlled/project".into(),
    };
    // Exercise production entrypoints, not a direct exposure enum invocation.
    let factory: Arc<dyn ExecutionFactory> = match entry {
        FactoryEntry::Native => Arc::new(factory),
        FactoryEntry::ObservedNative => {
            Arc::new(factory.observed(Arc::new(NativeLoopActivity::default())))
        }
        FactoryEntry::Sidecar => Arc::new(factory.sidecar_public()),
    };
    let server = ExecutionServer::new(owner, factory, clock, 4);
    let (mut io, remote) = UnixStream::pair().unwrap();
    io.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let input = BufReader::new(remote.try_clone().unwrap());
    let worker =
        std::thread::spawn(move || server.serve(input, remote, Duration::from_millis(500)));
    let mut execute = json(
        r#"{"kind":"request","messageId":"execute","method":"execute","runId":"run","operationId":"op","requestId":"initial","payload":{"provider":"test","model":"model","task":{"prompt":"budget boundary"},"hostModules":{"model":{"methods":["stream_next","close_stream"]},"context":{"methods":["try_auto_compact","capture_turn"]},"budget":{"methods":["estimate_usage_cost"]}}}}"#,
    );
    let JsValue::Object(fields) = &mut execute else {
        unreachable!()
    };
    let mut payload = fields.get("payload").unwrap().as_object().unwrap().clone();
    payload.insert(
        if task {
            "taskBudgetUsd"
        } else {
            "maxBudgetUsd"
        }
        .into(),
        JsValue::Number(0.5),
    );
    fields.insert("payload".into(), JsValue::Object(payload));
    send(&mut io, &execute);
    assert_eq!(field(&read(&io), "ok"), &JsValue::Bool(true));
    let mut events = Vec::new();
    let mut host_calls = Vec::new();
    let terminal = loop {
        let message = read(&io);
        if label(&message, "kind") == "event" {
            if field(&message, "final") == &JsValue::Bool(true) {
                break message;
            }
            events.push(message);
            continue;
        }
        let operation = label(&message, "operation");
        host_calls.push(operation.clone());
        match operation.as_str() {
            "try_auto_compact" => reply(
                &mut io,
                &message,
                r#"{"ok":true,"payload":{"result":{"type":"skipped","snapshot":{"tokens":1,"maxContextTokens":100000,"warningRatio":0.8,"blockingRatio":1,"ratio":0.00001,"state":"ok"}}}}"#,
            ),
            "stream_next" => reply(
                &mut io,
                &message,
                r#"{"ok":true,"payload":{"events":[{"type":"request_started","provider":"test","model":"model"},{"type":"message_start","role":"assistant"},{"type":"usage","usage":{"inputTokens":1000,"outputTokens":20,"totalTokens":1020}},{"type":"message_end","finishReason":"stop"}],"done":true}}"#,
            ),
            "estimate_usage_cost" => {
                reply(&mut io, &message, r#"{"ok":true,"payload":{"costUsd":1}}"#)
            }
            "capture_turn" => {
                assert_eq!(
                    field(field(field(&message, "payload"), "input"), "errored"),
                    &JsValue::Bool(true)
                );
                reply(
                    &mut io,
                    &message,
                    r#"{"ok":true,"payload":{"result":null}}"#,
                );
            }
            "close_stream" => reply(
                &mut io,
                &message,
                r#"{"ok":true,"payload":{"closed":true}}"#,
            ),
            _ => panic!("unexpected effect: {operation}"),
        }
    };
    assert_eq!(host_calls.iter().filter(|x| *x == "stream_next").count(), 1);
    assert_eq!(
        host_calls
            .iter()
            .filter(|x| *x == "estimate_usage_cost")
            .count(),
        1
    );
    assert_eq!(
        host_calls.iter().filter(|x| *x == "capture_turn").count(),
        1
    );
    assert_eq!(field(&terminal, "outcome"), &JsValue::from("failed"));
    io.shutdown(std::net::Shutdown::Both).unwrap();
    drop(io);
    assert!(worker.join().unwrap().complete());
    (events, terminal)
}
#[test]
fn actual_budget_loop_separates_sidecar_public_and_native_durable_status() {
    for task in [false, true] {
        let (public, public_terminal) = run_budget(FactoryEntry::Sidecar, task);
        let (native, native_terminal) = run_budget(FactoryEntry::Native, task);
        let (observed, observed_terminal) = run_budget(FactoryEntry::ObservedNative, task);
        let event_name = if task {
            "task_budget_reached"
        } else {
            "max_budget_reached"
        };
        let budget_name = if task {
            "taskBudget.total"
        } else {
            "maxBudgetUsd"
        };
        let human = JsValue::from(format!(
            "Reached Gateway-owned {budget_name} ($0.500000) after spending $1.000000."
        ));
        let status = |events: &[JsValue]| {
            events
                .iter()
                .map(|e| field(e, "payload").clone())
                .find(|p| p.get("event") == Some(&JsValue::from(event_name)))
                .unwrap()
        };
        let p = status(&public);
        let n = status(&native);
        let keys: Vec<_> = p
            .as_object()
            .unwrap()
            .entries()
            .iter()
            .map(|(k, _)| k.to_utf8().unwrap())
            .collect();
        assert_eq!(keys, ["type", "sessionId", "turnId", "event", "detail"]);
        assert_eq!(field(&p, "type"), &JsValue::from("agent_status"));
        assert_eq!(durable_fallback(&p), JsValue::from(event_name));
        assert_eq!(field(&n, "text"), &human);
        assert_eq!(status(&observed), n);
        assert_eq!(field(&p, "detail"), field(&n, "detail"));
        assert!(field(&p, "detail").get("message").is_none());
        assert_eq!(
            field(field(&p, "detail"), "spentBudgetUsd"),
            &JsValue::Number(1.0)
        );
        let result = |events: &[JsValue]| {
            events
                .iter()
                .find_map(|e| {
                    let p = field(e, "payload");
                    (p.get("type") == Some(&JsValue::from("turn_completed")))
                        .then(|| field(p, "result").clone())
                })
                .unwrap()
        };
        let pr = result(&public);
        assert_eq!(pr, result(&native));
        assert_eq!(pr, result(&observed));
        assert_eq!(field(&pr, "type"), &JsValue::from("error"));
        let JsValue::Array(errors) = field(&pr, "errors") else {
            panic!("errors")
        };
        assert_eq!(field(&errors[0], "message"), &human);
        assert_eq!(
            field(&public_terminal, "payload"),
            field(&native_terminal, "payload")
        );
        assert_eq!(
            field(&native_terminal, "payload"),
            field(&observed_terminal, "payload")
        );
    }
}
#[test]
fn public_projection_selects_source_fields_for_nonbudget_status_only() {
    for detail in [
        json(r#"{"message":"detail human"}"#),
        json(r#"{"count":1}"#),
    ] {
        let mut payload = json(r#"{"type":"agent_status","sessionId":"s","turnId":"t","event":"custom_status","kind":"error","text":"private text","privateExtra":true}"#).as_object().unwrap().clone();
        payload.insert("detail".into(), detail.clone());
        let public = JsValue::Object(
            StatusExposure::SidecarPublic.project(&"agent_status".into(), payload.clone()),
        );
        assert!(
            public.get("kind").is_none()
                && public.get("text").is_none()
                && public.get("privateExtra").is_none()
        );
        assert_eq!(field(&public, "detail"), &detail);
        assert_eq!(
            durable_fallback(&public),
            detail
                .get("message")
                .cloned()
                .unwrap_or_else(|| "custom_status".into())
        );
        assert_eq!(
            StatusExposure::NativeDurable.project(&"agent_status".into(), payload.clone()),
            payload
        );
        // A runner-owned durable status or other event is not an AgentLoop
        // agent_status source event and must retain its own kind/text/fields.
        let mut runner = payload;
        runner.insert("type".into(), "agent_status_message".into());
        assert_eq!(
            StatusExposure::SidecarPublic.project(&"agent_status_message".into(), runner.clone()),
            runner
        );
    }
}
