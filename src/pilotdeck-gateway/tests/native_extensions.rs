//! An independent Rust consumer uses only public APIs and the production local
//! gateway assembly. Its factory calls a registered tool through normal host
//! capability/permission dispatch; no provider or sidecar executable is used.
use pilotdeck_gateway::{
    extensions::{NativeExtension, NativeExtensions},
    host::{HostError, modules::capability::ToolDefinition},
    native_process::{LocalGateway, LocalOptions},
    runtime::tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
};
use pilotdeck_protocol::{JsObject, JsValue, parse_json};
use pilotdeck_transport::{
    execute::{FactoryPayloadDecoder, OriginalFactoryPayloadDecoder},
    server::{ExecuteRequest, ExecutionContext, ExecutionFactory, ExecutionTerminal},
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Resource(Arc<AtomicUsize>);
impl Drop for Resource {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct CounterTool {
    fields: JsObject,
    revision: usize,
    calls: Arc<Mutex<Vec<usize>>>,
}
impl ToolDefinition for CounterTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}
impl ExecutableTool for CounterTool {
    fn execute(&self, _: JsValue, _: ToolRunContext) -> ToolFuture<ToolOutput> {
        self.calls.lock().unwrap().push(self.revision);
        Box::pin(async {
            Ok(ToolOutput {
                content: vec![parse_json(r#"{"type":"text","text":"native tool ran"}"#).unwrap()],
                supplemental_messages: None,
                data: None,
                metadata: None,
            })
        })
    }
}
struct Factory {
    prompts: Arc<Mutex<Vec<String>>>,
}
impl ExecutionFactory for Factory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> pilotdeck_agent::ports::PortResult<ExecutionTerminal> {
        let decoded = OriginalFactoryPayloadDecoder
            .decode(&request.boundary, &"/".into())
            .unwrap();
        let prompt = request
            .boundary
            .get("payload")
            .and_then(|p| p.get("agent"))
            .and_then(|a| a.get("appendSystemPrompt"))
            .and_then(JsValue::as_string)
            .map(|s| String::from_utf16_lossy(s.units()))
            .unwrap_or_default();
        self.prompts.lock().unwrap().push(prompt);
        pilotdeck_transport::default_factory::OriginalAgentFactory {
            clock: Arc::new(pilotdeck_transport::system_clock::SystemExecutionClock),
            process_cwd: decoded.config.cwd,
        }
        .execute(request, context)
    }
}
fn extension(
    revision: usize,
    calls: Arc<Mutex<Vec<usize>>>,
    prompts: Arc<Mutex<Vec<String>>>,
    dropped: Arc<AtomicUsize>,
) -> NativeExtension {
    NativeExtension {
        tools: vec![Arc::new(CounterTool { revision, calls, fields: parse_json(r#"{"name":"native_counter","description":"An embedding-owned read-only counter","inputSchema":{"type":"object"}}"#).unwrap().as_object().unwrap().clone() })],
        prompts: vec![format!("Extension generation {revision}")],
        factory: Some(Arc::new(Factory { prompts })),
        resources: vec![Arc::new(Resource(dropped))],
        ..Default::default()
    }
}
struct MockProvider {
    address: std::net::SocketAddr,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl MockProvider {
    fn start() -> Self {
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
                // macOS may inherit the listener's nonblocking flag. The
                // fixture reads a complete HTTP request using blocking I/O.
                socket.set_nonblocking(false).unwrap();
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
                    serde_json::json!({"role":"assistant", "tool_calls":[{"id":"native-call","type":"function","function":{"name":"native_counter","arguments":"{}"}}]})
                } else {
                    serde_json::json!({"role":"assistant","content":text})
                };
                let finish = if tool { "tool_calls" } else { "stop" };
                let (mime, response) = if body["stream"] == true {
                    let delta = if tool {
                        serde_json::json!({"tool_calls":[{"index":0,"id":"native-call","type":"function","function":{"name":"native_counter","arguments":"{}"}}]})
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
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().expect("mock worker is owned").join();
        if std::thread::panicking() {
            if result.is_err() {
                eprintln!("mock provider worker also panicked during cleanup");
            }
        } else {
            result.expect("mock provider worker must finish successfully");
        }
    }
}

#[test]
fn public_embedding_runs_updates_and_releases_native_extensions() {
    let scratch = Scratch(
        std::env::temp_dir().join(format!("pdrs-extension-consumer-{}", uuid::Uuid::new_v4())),
    );
    let home = scratch.0.join("home");
    let pilot = home.join(".pilotdeck");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&pilot).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let chat_dir =
        pilotdeck_gateway::session_chat_dir(&pilot, &project, &project.to_string_lossy());
    std::fs::create_dir_all(chat_dir.parent().unwrap()).unwrap();
    std::fs::write(
        chat_dir.parent().unwrap().join(".cwd"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let mock = MockProvider::start();
    let config = pilot.join("pilotdeck.yaml");
    std::fs::write(&config, format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: mock-only\n      models: {{mock: {{}}}}\n", mock.address)).unwrap();
    let env = [
        ("HOME", home.to_string_lossy().into_owned()),
        ("PILOT_HOME", pilot.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH",
            config.to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    let extensions = NativeExtensions::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::new(AtomicUsize::new(0));
    let old = extensions
        .register(
            "consumer",
            Some(&project),
            extension(1, calls.clone(), prompts.clone(), dropped.clone()),
        )
        .unwrap();
    let gateway = LocalGateway::start_with_extensions(
        LocalOptions {
            telemetry_context: None,
            pilot_home: pilot,
            project,
            os_home: Some(home),
            sidecar: scratch.0.join("does-not-exist"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
        },
        extensions.clone(),
    )
    .unwrap();
    let submit = |session: &str| {
        let events = Arc::new(Mutex::new(Vec::new()));
        let emitted = events.clone();
        gateway.submit_turn(parse_json(&format!(r#"{{"sessionKey":"{session}","channelKey":"test","message":"call native extension"}}"#)).unwrap(), Arc::new(move |event| emitted.lock().unwrap().push(event))).unwrap();
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.get("type") == Some(&JsValue::from("error"))),
            "{:?}",
            *events.lock().unwrap()
        );
    };
    submit("old-session");
    let current = extensions
        .update(
            "consumer",
            None,
            extension(2, calls.clone(), prompts.clone(), dropped.clone()),
        )
        .unwrap();
    drop(old);
    submit("old-session");
    submit("new-session");
    assert_eq!(
        *calls.lock().unwrap(),
        vec![1, 1, 2],
        "factory prompts: {:?}",
        *prompts.lock().unwrap()
    );
    assert_eq!(
        *prompts.lock().unwrap(),
        vec![
            "Extension generation 1",
            "Extension generation 1",
            "Extension generation 2"
        ]
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    for (index, (name, aliases, expected)) in [
        ("read_file", vec![], "Tool read_file is already registered."),
        (
            "Read",
            vec![],
            "Tool Read conflicts with an existing alias.",
        ),
        (
            "native_conflict",
            vec!["read_file"],
            "Alias read_file conflicts with an existing tool name.",
        ),
        (
            "native_conflict",
            vec!["Read"],
            "Alias Read is already registered.",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut fields =
            parse_json(r#"{"description":"conflicting tool","inputSchema":{"type":"object"}}"#)
                .unwrap()
                .as_object()
                .unwrap()
                .clone();
        fields.insert("name".into(), name.into());
        fields.insert(
            "aliases".into(),
            JsValue::Array(aliases.into_iter().map(JsValue::from).collect()),
        );
        let conflicting = extensions
            .register(
                "conflict",
                None,
                NativeExtension {
                    tools: vec![Arc::new(CounterTool {
                        revision: 99,
                        calls: calls.clone(),
                        fields,
                    })],
                    ..Default::default()
                },
            )
            .unwrap();
        let errors = Arc::new(Mutex::new(String::new()));
        let emitted = errors.clone();
        let conflict = gateway.submit_turn(
            parse_json(&format!(r#"{{"sessionKey":"conflicting-session-{index}","channelKey":"test","message":"must fail"}}"#)).unwrap(),
            Arc::new(move |event| {
                emitted.lock().unwrap().push_str(
                    &pilotdeck_protocol::stringify_json(&event)
                        .unwrap()
                        .unwrap_or_default(),
                )
            }),
        );
        let failure = format!("{conflict:?} {}", errors.lock().unwrap());
        assert!(
            failure.contains("Native extension tool registration failed"),
            "{failure}"
        );
        assert!(failure.contains(expected), "{failure}");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![1, 1, 2],
            "a rejected extension must never execute"
        );
        assert!(conflicting.unregister());
    }
    assert!(current.unregister());
    gateway.shutdown(Duration::from_secs(5)).unwrap();
    drop(gateway);
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}
