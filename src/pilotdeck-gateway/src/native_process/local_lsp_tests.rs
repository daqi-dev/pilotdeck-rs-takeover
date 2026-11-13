//! Independent source-level consumer through the production LocalGateway.
use pilotdeck_gateway::{
    extensions::NativeExtensions,
    native_process::{LocalGateway, LocalOptions},
    runtime::{lsp::*, network::Cancellation},
};
use pilotdeck_protocol::parse_json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
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
struct Probe {
    generation: usize,
    queries: Arc<Mutex<Vec<LspProviderQuery>>>,
    disposed: Arc<AtomicUsize>,
    binds: Arc<AtomicUsize>,
}
impl LspProvider for Probe {
    fn id(&self) -> &str {
        "selected"
    }
    fn extension_to_language(&self) -> BTreeMap<String, String> {
        BTreeMap::from([(".ts".into(), "typescript".into())])
    }
    fn query(&self, request: LspProviderQuery, cancel: Cancellation) -> LspFuture<LspQueryResult> {
        let file = request.request.file_path.clone();
        self.queries.lock().unwrap().push(request);
        let generation = self.generation;
        Box::pin(async move {
            if file == "cancel.ts" {
                cancel.cancelled().await;
                return Err(LspError::new("ABORT_ERR", "LSP query aborted"));
            }
            if file == "error.ts" {
                return Err(LspError::new(
                    "LSP_MALFORMED_RESPONSE",
                    "selected provider fixture error",
                ));
            }
            Ok(LspQueryResult::Hover {
                hover: Some(
                    serde_json::json!({"contents":format!("selected provider result {generation}")}),
                ),
            })
        })
    }
    fn bind_session(
        &self,
        _execution: LspSessionExecution,
    ) -> Result<Arc<dyn LspProvider>, LspError> {
        self.binds.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Self {
            generation: self.generation,
            queries: self.queries.clone(),
            disposed: self.disposed.clone(),
            binds: self.binds.clone(),
        }))
    }
    fn dispose(&self) -> LspFuture<()> {
        self.disposed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
struct Factory {
    bind_sessions: bool,
    calls: Arc<AtomicUsize>,
    queries: Arc<Mutex<Vec<LspProviderQuery>>>,
    disposed: Arc<AtomicUsize>,
    binds: Arc<AtomicUsize>,
}
impl LspServiceFactory for Factory {
    fn session_execution(
        &self,
        project: &Path,
        _session: &str,
    ) -> Result<Option<LspSessionExecution>, LspError> {
        if self.bind_sessions {
            Ok(Some(LspSessionExecution {
                workspace_root: project.to_owned(),
                io: Arc::new(NativeLspExecutionPort::new(
                    Default::default(),
                    project.to_owned(),
                )?),
            }))
        } else {
            Ok(None)
        }
    }
    fn create(&self, _project: &Path) -> Result<Arc<LspService>, LspError> {
        let generation = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let s = Arc::new(LspService::new());
        s.register_provider(Arc::new(Probe {
            generation,
            queries: self.queries.clone(),
            disposed: self.disposed.clone(),
            binds: self.binds.clone(),
        }))?;
        Ok(s)
    }
}
struct MockProvider {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
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
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
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
                let body_text = body.to_string();
                let file = if body_text.contains("cancel-lsp") {
                    "cancel.ts"
                } else if body_text.contains("error-lsp") {
                    "error.ts"
                } else {
                    "src/index.ts"
                };
                let coordinate = if body_text.contains("large-lsp") {
                    4_294_967_298.0
                } else {
                    2.0
                };
                let arguments=serde_json::json!({"operation":"hover","file_path":file,"line":coordinate,"character":coordinate+1.0}).to_string();
                let message = if tool {
                    serde_json::json!({"role":"assistant", "tool_calls":[{"id":"native-call","type":"function","function":{"name":"lsp","arguments":arguments}}]})
                } else {
                    serde_json::json!({"role":"assistant","content":text})
                };
                let finish = if tool { "tool_calls" } else { "stop" };
                let (mime, response) = if body["stream"] == true {
                    let delta = if tool {
                        serde_json::json!({"tool_calls":[{"index":0,"id":"native-call","type":"function","function":{"name":"lsp","arguments":arguments}}]})
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
            requests,
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

#[derive(Default)]
struct Text(Mutex<Vec<serde_json::Value>>);
impl crate::runtime::connection::GatewayTextConnection for Text {
    fn send_text(&self, text: String) {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::from_str(&text).unwrap());
    }
    fn close(&self, _code: u16, _reason: &str) {}
}
#[test]
fn real_public_factory_survives_reload_with_old_generation_leased() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pdrs-lsp-consumer-{}", uuid::Uuid::new_v4())));
    let home = scratch.0.join("home");
    let pilot = home.join(".pilotdeck");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&pilot).unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/index.ts"), "const 中文 = 1;\n").unwrap();
    let chats = pilotdeck_gateway::session_chat_dir(&pilot, &project, &project.to_string_lossy());
    std::fs::create_dir_all(chats.parent().unwrap()).unwrap();
    std::fs::write(
        chats.parent().unwrap().join(".cwd"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let mock = MockProvider::start();
    let config = pilot.join("pilotdeck.yaml");
    std::fs::write(&config,format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: mock-only\n      models: {{mock: {{}}}}\n",mock.address)).unwrap();
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
    let calls = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(Mutex::new(Vec::new()));
    let disposed = Arc::new(AtomicUsize::new(0));
    let binds = Arc::new(AtomicUsize::new(0));
    let extensions = NativeExtensions::default();
    let factory = Arc::new(Factory {
        bind_sessions: false,
        calls: calls.clone(),
        queries: queries.clone(),
        disposed: disposed.clone(),
        binds: binds.clone(),
    });
    let gateway = LocalGateway::start_with_lsp_factory(
        LocalOptions {
            telemetry_context: None,
            pilot_home: pilot.clone(),
            project: project.clone(),
            os_home: Some(home),
            sidecar: scratch.0.join("absent-sidecar"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
        },
        extensions,
        factory,
    )
    .unwrap();

    let submit = |session: &str| {
        let events = Arc::new(Mutex::new(Vec::new()));
        let emitted = events.clone();
        gateway.submit_turn(parse_json(&format!(r#"{{"sessionKey":"{session}","channelKey":"test","message":"Inspect TypeScript symbol"}}"#)).unwrap(),Arc::new(move|e|emitted.lock().unwrap().push(e))).unwrap();
        let text = format!("{:?}", events.lock().unwrap());
        text
    };
    assert!(submit("old").contains("selected provider result 1"));
    let text = Arc::new(Text::default());
    let connection = gateway
        .host
        .connect(text.clone(), "lsp-test-token".into(), "fixture".into());
    gateway.runtime.block_on(async {
  connection.receive(serde_json::json!({"type":"hello","protocolVersion":pilotdeck_protocol::GATEWAY_PROTOCOL_VERSION,"token":"lsp-test-token","clientName":"test","clientVersion":"fixture"}).to_string()).await.unwrap();
  let original=std::fs::read_to_string(&config).unwrap();std::fs::write(&config,original.replace("agent: {model: fixture/mock}","agent: {model: fixture/mock, maxOutputTokens: 4096}")).unwrap();
  connection.receive(serde_json::json!({"type":"request","id":"lsp-reload","method":"reload_config","params":{}}).to_string()).await.unwrap();
 });
    assert!(
        text.0
            .lock()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "lsp-reload" && f["ok"] == true),
        "{:?}",
        text.0.lock().unwrap()
    );
    assert!(submit("new").contains("selected provider result 2"));
    assert!(submit("old").contains("selected provider result 1"));
    assert!(
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["messages"]
                .as_array()
                .is_some_and(
                    |messages| messages.iter().any(|message| message["role"] == "tool"
                        && message["content"]
                            .to_string()
                            .contains("selected provider result 2"))
                )),
        "new generation reaches actual next provider request"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        disposed.load(Ordering::SeqCst),
        0,
        "old and current generation leases both live"
    );
    gateway.runtime.block_on(connection.closed()).unwrap();
    gateway.shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(
        disposed.load(Ordering::SeqCst),
        2,
        "each project generation service disposed exactly once"
    );
}

#[test]
fn real_native_lsp_tool_cancel_keeps_shared_service_usable() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pdrs-lsp-consumer-{}", uuid::Uuid::new_v4())));
    let home = scratch.0.join("home");
    let pilot = home.join(".pilotdeck");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&pilot).unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/index.ts"), "const 中文 = 1;\n").unwrap();
    let chats = pilotdeck_gateway::session_chat_dir(&pilot, &project, &project.to_string_lossy());
    std::fs::create_dir_all(chats.parent().unwrap()).unwrap();
    std::fs::write(
        chats.parent().unwrap().join(".cwd"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let mock = MockProvider::start();
    let config = pilot.join("pilotdeck.yaml");
    std::fs::write(&config,format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: mock-only\n      models: {{mock: {{}}}}\n",mock.address)).unwrap();
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
    let calls = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(Mutex::new(Vec::new()));
    let disposed = Arc::new(AtomicUsize::new(0));
    let binds = Arc::new(AtomicUsize::new(0));
    let extensions = NativeExtensions::default();
    let factory = Arc::new(Factory {
        bind_sessions: false,
        calls: calls.clone(),
        queries: queries.clone(),
        disposed: disposed.clone(),
        binds: binds.clone(),
    });
    let gateway = LocalGateway::start_with_lsp_factory(
        LocalOptions {
            telemetry_context: None,
            pilot_home: pilot.clone(),
            project: project.clone(),
            os_home: Some(home),
            sidecar: scratch.0.join("absent-sidecar"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
        },
        extensions,
        factory,
    )
    .unwrap();

    let text = Arc::new(Text::default());
    let connection = gateway
        .host
        .connect(text.clone(), "lsp-test-token".into(), "fixture".into());
    gateway.runtime.block_on(async {
  connection.receive(serde_json::json!({"type":"hello","protocolVersion":pilotdeck_protocol::GATEWAY_PROTOCOL_VERSION,"token":"lsp-test-token","clientName":"test","clientVersion":"fixture"}).to_string()).await.unwrap();
  let pending=connection.enqueue(serde_json::json!({"type":"request","id":"lsp-pending","method":"submit_turn","params":{"sessionKey":"cancel","channelKey":"test","message":"cancel-lsp"}}).to_string()).await.unwrap();
  tokio::time::timeout(Duration::from_secs(5),async{loop{if queries.lock().unwrap().iter().any(|q|q.request.file_path=="cancel.ts"){break;}tokio::time::sleep(Duration::from_millis(2)).await;}}).await.unwrap();
  connection.receive(serde_json::json!({"type":"request","id":"lsp-abort","method":"abort_turn","params":{"sessionKey":"cancel"}}).to_string()).await.unwrap();
  tokio::time::timeout(Duration::from_secs(5),pending).await.unwrap().unwrap();
  connection.closed().await.unwrap();
 });
    assert!(
        text.0
            .lock()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "lsp-abort" && f["ok"] == true)
    );
    assert!(
        text.0
            .lock()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "lsp-pending" && f["final"] == true)
    );
    assert_eq!(
        disposed.load(Ordering::SeqCst),
        0,
        "cancelling a query must not dispose the shared service"
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let emitted = events.clone();
    gateway.submit_turn(parse_json(r#"{"sessionKey":"after-cancel","channelKey":"test","message":"Inspect TypeScript symbol"}"#).unwrap(),Arc::new(move|e|emitted.lock().unwrap().push(e))).unwrap();
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| format!("{e:?}").contains("selected provider result 1"))
    );
    gateway.shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
}

#[test]
fn explicit_session_execution_owns_only_its_bound_lsp_providers() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pdrs-lsp-consumer-{}", uuid::Uuid::new_v4())));
    let home = scratch.0.join("home");
    let pilot = home.join(".pilotdeck");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&pilot).unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/index.ts"), "const 中文 = 1;\n").unwrap();
    let chats = pilotdeck_gateway::session_chat_dir(&pilot, &project, &project.to_string_lossy());
    std::fs::create_dir_all(chats.parent().unwrap()).unwrap();
    std::fs::write(
        chats.parent().unwrap().join(".cwd"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let mock = MockProvider::start();
    let config = pilot.join("pilotdeck.yaml");
    std::fs::write(&config,format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: mock-only\n      models: {{mock: {{}}}}\n",mock.address)).unwrap();
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
    let calls = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(Mutex::new(Vec::new()));
    let disposed = Arc::new(AtomicUsize::new(0));
    let binds = Arc::new(AtomicUsize::new(0));
    let extensions = NativeExtensions::default();
    let factory = Arc::new(Factory {
        bind_sessions: true,
        calls: calls.clone(),
        queries: queries.clone(),
        disposed: disposed.clone(),
        binds: binds.clone(),
    });
    let gateway = LocalGateway::start_with_lsp_factory(
        LocalOptions {
            telemetry_context: None,
            pilot_home: pilot.clone(),
            project: project.clone(),
            os_home: Some(home),
            sidecar: scratch.0.join("absent-sidecar"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
        },
        extensions,
        factory,
    )
    .unwrap();

    for session in ["bound-a", "bound-b"] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let emitted = events.clone();
        gateway.submit_turn(parse_json(&format!(r#"{{"sessionKey":"{session}","channelKey":"test","message":"Inspect TypeScript symbol"}}"#)).unwrap(),Arc::new(move|e|emitted.lock().unwrap().push(e))).unwrap();
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| format!("{e:?}").contains("selected provider result 1"))
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one generation supplier");
    assert_eq!(
        binds.load(Ordering::SeqCst),
        2,
        "one selected execution binding per session"
    );
    assert_eq!(disposed.load(Ordering::SeqCst), 0);
    gateway.shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(
        disposed.load(Ordering::SeqCst),
        3,
        "two independently bound session providers and one generation provider"
    );
}
