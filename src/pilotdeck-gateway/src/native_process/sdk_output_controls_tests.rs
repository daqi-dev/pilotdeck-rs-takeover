//! Production SDK output controls: real local gateway and model/tool execution.
use super::*;
use crate::native_process::{LocalGateway, LocalOptions};
use crate::{
    extensions::{NativeExtension, NativeExtensions},
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
};
use pilotdeck_protocol::parse_json;
use pilotdeck_transport::{
    execute::{FactoryPayloadDecoder, OriginalFactoryPayloadDecoder},
    server::{ExecuteRequest, ExecutionContext, ExecutionFactory, ExecutionTerminal},
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
pub(in crate::native_process) struct ControlProvider {
    pub(in crate::native_process) address: std::net::SocketAddr,
    stop: Arc<std::sync::atomic::AtomicBool>,
    pub(in crate::native_process) requests: Arc<Mutex<Vec<serde_json::Value>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl ControlProvider {
    pub(in crate::native_process) fn start(tool_name: &str) -> Self {
        Self::start_with_arguments(
            tool_name,
            if tool_name == "structured_output" {
                r#"{"value":{"status":"ok"}}"#
            } else {
                "{}"
            },
        )
    }
    pub(in crate::native_process) fn start_with_arguments(
        tool_name: &str,
        arguments: &str,
    ) -> Self {
        let tool_name = tool_name.to_owned();
        let tool_arguments = arguments.to_owned();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("mock accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 8192];
                let (start, length) = loop {
                    let count = socket.read(&mut chunk).unwrap();
                    if count == 0 {
                        panic!("mock request ended before headers");
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..index]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                    .map(|(_, length)| length.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (index + 4, length);
                    }
                };
                while bytes.len() < start + length {
                    let count = socket.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[start..start + length]).unwrap();
                captured.lock().unwrap().push(body.clone());
                let title = body.to_string().contains("Generate a concise title");
                let after_tool = body["messages"]
                    .as_array()
                    .and_then(|messages| messages.last())
                    .is_some_and(|last| last["role"] == "tool");
                let text = if title {
                    "{\"title\":\"Native extension\"}"
                } else {
                    "done"
                };
                let tool = !title && !after_tool;
                let message = if tool {
                    serde_json::json!({"role":"assistant", "tool_calls":[{"id":"native-call","type":"function","function":{"name":tool_name,"arguments":tool_arguments}}]})
                } else {
                    serde_json::json!({"role":"assistant","content":text})
                };
                let finish = if tool { "tool_calls" } else { "stop" };
                let (mime, response) = if body["stream"] == true {
                    let delta = if tool {
                        serde_json::json!({"tool_calls":[{"index":0,"id":"native-call","type":"function","function":{"name":tool_name,"arguments":tool_arguments}}]})
                    } else {
                        serde_json::json!({"content":text})
                    };
                    let event = serde_json::json!({"id":"mock","object":"chat.completion.chunk","model":"mock","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                    let end = serde_json::json!({"id":"mock","object":"chat.completion.chunk","model":"mock","choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
                    (
                        "text/event-stream",
                        format!("data: {event}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
                    )
                } else {
                    ("application/json", serde_json::json!({"id":"mock","object":"chat.completion","model":"mock","choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for ControlProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct ProgressProbe {
    fields: JsObject,
    executions: Arc<AtomicUsize>,
}
impl ToolDefinition for ProgressProbe {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}
impl ExecutableTool for ProgressProbe {
    fn execute(&self, _: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        if let Some(progress) = &context.host.progress {
            let mut event = JsObject::new();
            for (key, value) in [
                ("toolCallId", JsValue::from("native-call")),
                ("toolName", JsValue::from("progress_probe")),
                ("message", JsValue::from("working")),
                ("createdAt", JsValue::from("2026-11-13T00:00:00.000Z")),
            ] {
                event.insert(key.into(), value);
            }
            progress(event);
        }
        Box::pin(async {
            Ok(ToolOutput {
                content: vec![parse_json(r#"{"type":"text","text":"probe done"}"#).unwrap()],
                supplemental_messages: None,
                data: None,
                metadata: None,
            })
        })
    }
}
struct ChildEventFactory;
impl ExecutionFactory for ChildEventFactory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> pilotdeck_agent::ports::PortResult<ExecutionTerminal> {
        for kind in ["text_delta", "thinking_delta"] {
            let payload=parse_json(&format!(r#"{{"subagentId":"child-1","subagentType":"general","event":{{"type":"{kind}","text":"child fragment"}}}}"#)).unwrap().as_object().unwrap().clone();
            context.emit("subagent_model_event".into(), payload)?;
        }
        let decoded = OriginalFactoryPayloadDecoder
            .decode(&request.boundary, &"/".into())
            .expect("SDK control native payload");
        pilotdeck_transport::default_factory::OriginalAgentFactory {
            clock: Arc::new(pilotdeck_transport::system_clock::SystemExecutionClock),
            process_cwd: decoded.config.cwd,
        }
        .execute(request, context)
    }
}
struct Run {
    events: Vec<JsValue>,
    requests: Vec<Value>,
    executions: usize,
}
fn run_control(config: Value, tool: &str, emit_child: bool) -> Run {
    let scratch = Scratch::new();
    let home = scratch.0.join("home");
    let project = scratch.0.join("project");
    let provider = ControlProvider::start(tool);
    let config_path = home.join("pilotdeck.yaml");
    std::fs::write(&config_path,format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: fixture-local-only\n      models: {{mock: {{capabilities: {{supportsToolUse: true}}}}}}\ntelemetry: {{enabled: false}}\n",provider.address)).unwrap();
    let env = [
        ("HOME", home.to_string_lossy().into_owned()),
        ("PILOT_HOME", home.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH",
            config_path.to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect();
    let extensions = NativeExtensions::default();
    let executions = Arc::new(AtomicUsize::new(0));
    let registration=extensions.register("sdk-control-probe",Some(&project),NativeExtension{
        tools:vec![Arc::new(ProgressProbe{fields:parse_json(r#"{"name":"progress_probe","description":"Emit one trusted host progress event","inputSchema":{"type":"object"}}"#).unwrap().as_object().unwrap().clone(),executions:executions.clone()})],
        factory:emit_child.then(||Arc::new(ChildEventFactory) as Arc<dyn ExecutionFactory>),..Default::default()
    }).unwrap();
    let gateway = LocalGateway::start_with_extensions(
        LocalOptions {
            pilot_home: home.clone(),
            project,
            os_home: Some(home),
            sidecar: scratch.0.join("absent-sidecar"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
            telemetry_context: None,
        },
        extensions,
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = events.clone();
    gateway.submit_turn(js(&json!({"sessionKey":"sdk-output-controls","channelKey":"test","message":"call the requested tool once","sdkSessionConfig":config})),Arc::new(move |event|capture.lock().unwrap().push(event))).unwrap();
    gateway.shutdown(Duration::from_secs(10)).unwrap();
    drop(registration);
    let events = events.lock().unwrap().clone();
    assert!(
        !events
            .iter()
            .any(|event| event.get("type") == Some(&JsValue::from("error"))),
        "{events:?}"
    );
    let requests = provider.requests.lock().unwrap().clone();
    Run {
        events,
        requests,
        executions: executions.load(Ordering::SeqCst),
    }
}
fn has_type(event: &JsValue, name: &str) -> bool {
    event.get("type") == Some(&JsValue::from(name))
}
#[test]
fn sdk_output_format_drives_real_structured_tool_termination() {
    let requested = json!({"outputFormat":{"type":"json_schema","schema":{"type":"object","required":["status"],"properties":{"status":{"type":"string"}}}}});
    let enabled = run_control(requested, "structured_output", false);
    let disabled = run_control(json!({}), "structured_output", false);
    let count = |run: &Run| {
        run.requests
            .iter()
            .filter(|request| request["stream"] == true)
            .count()
    };
    assert_eq!(
        count(&enabled),
        1,
        "structured output stops before a second model request"
    );
    assert_eq!(
        count(&disabled),
        2,
        "without format the same tool result continues the loop"
    );
    let structured = enabled
        .events
        .iter()
        .find(|event| has_type(event, "structured_output"))
        .expect("structured payload gateway event");
    assert_eq!(
        structured.get("payload"),
        Some(&js(&json!({"status":"ok"})))
    );
    assert!(
        enabled
            .events
            .iter()
            .any(|event| has_type(event, "turn_completed"))
    );
    assert!(
        enabled
            .requests
            .iter()
            .all(|request| request.get("response_format").is_none()),
        "fixed baseline selects structured_output termination; it does not send provider response_format"
    );
}
#[test]
fn sdk_progress_flag_gates_real_tool_context_emission() {
    for enabled in [false, true] {
        let run = run_control(
            json!({"agentProgressSummaries":enabled}),
            "progress_probe",
            false,
        );
        assert_eq!(
            run.executions, 1,
            "tool executed even when progress is disabled"
        );
        let progress: Vec<_> = run
            .events
            .iter()
            .filter(|event| has_type(event, "tool_progress"))
            .collect();
        assert_eq!(progress.len(), usize::from(enabled));
        if enabled {
            assert_eq!(progress[0].get("message"), Some(&JsValue::from("working")));
        }
    }
}
#[test]
fn sdk_forward_subagent_flag_routes_trusted_child_events_through_submit() {
    for enabled in [false, true] {
        let run = run_control(
            json!({"forwardSubagentText":enabled}),
            "progress_probe",
            true,
        );
        assert_eq!(
            run.events
                .iter()
                .filter(|event| has_type(event, "subagent_text_delta"))
                .count(),
            usize::from(enabled)
        );
        let status = |kind: &str| {
            run.events
                .iter()
                .filter(|event| {
                    has_type(event, "agent_status")
                        && event.get("event") == Some(&JsValue::from(kind))
                })
                .count()
        };
        assert_eq!(status("subagent_text_delta"), usize::from(!enabled));
        assert_eq!(
            status("subagent_thinking_delta"),
            1,
            "thinking remains a status event for either option"
        );
        assert!(
            run.events
                .iter()
                .any(|event| has_type(event, "turn_completed"))
        );
    }
}
