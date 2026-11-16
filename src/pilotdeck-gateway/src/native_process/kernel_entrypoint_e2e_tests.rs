//! Real-host entry-point E2E verification against `agentcpm-host` (`fb54c74a8`
//! scoped resident assembly) + source-built `--native-model-tool-loop` fixture
//! across:
//! 1. `LocalGateway` (`ScopedResidentSessionBinder`),
//! 2. `serve_with_resident_kernel_with_profile(..., ScopedResident)` over real
//!    WebSocket SDK/CLI protocol (`hello` -> `new_session` -> `resume_session`
//!    -> `submit_turn` -> graceful `SIGINT` shutdown -> host exit 0),
//! 3. `assembly::start_with_managed_services` / `NativeChannels` ->
//!    `FeishuTurnGateway` -> `SubmitterTurnGateway` (`ScopedResidentSessionBinder`).
#![cfg(all(test, unix, feature = "kernel-resident"))]

use crate::{
    channel::protocol::{
        BoxFuture, ChannelAdapter, ChannelError, ChannelHandle, ChannelKey, ChannelStartDeps,
        TurnEventStream, TurnGateway, TurnRequest,
    },
    config::Env,
    extensions::NativeExtensions,
    native_process::{
        DeploymentKernelConfig, GatewayManagedModelSetup, KernelExecutionProfile,
        KernelSessionBinder, KernelTurnOptions, Listening, LocalGateway, LocalOptions,
        ManagedModuleFailure, ManagedModuleModel, ManagedModulePrincipal, ManagedModuleResponse,
        ManagedSessionBinder, NativeGatewayServices, NativeLoopImage, NativeProductReads,
        NativeToolBatchSetup, Options, OriginalProjectCatalogPolicy, OriginalProjectTurnAuthority,
        ScopedResidentProductInputs, ScopedResidentSessionBinder, ServerOptions, assembly,
        serve_with_resident_kernel_with_profile, server::ensure_router_dir,
    },
    runtime::turn::kernel_client::KernelExecutionOptions,
};
use agentcpm_host::{
    EmbedOptions, ExternalBackends, ResidentComposition, SchedulerAssembly, SchedulerComponent,
    SchedulerImplementation, StartupOptions, TrustedReadBinding, TrustedReadProvider,
    serve_resident_composed,
};
use agentcpm_scheduler_protocol as p;
use futures_util::{SinkExt, StreamExt};
use pilotdeck_protocol::{JsValue, parse_json, stringify_json};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::net::TcpStream as TokioTcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const WAIT: Duration = Duration::from_secs(30);
const FILE_MARKER: &str = "PILOTDECK_ENTRYPOINT_E2E_MARKER";
const ASSISTANT_CONFIRMATION: &str = "ENTRYPOINT_E2E_CONFIRMED: PILOTDECK_ENTRYPOINT_E2E_MARKER";
const LOOP_BINDING: &str = "pilotdeck";
const CONTROLLER_BINDING: &str = "controller";
const WS_TOKEN: &str = "p5-ws-fixture-token";

type WsSocket = WebSocketStream<MaybeTlsStream<TokioTcpStream>>;

struct ResidentGuard {
    stop: agentcpm_host::ResidentShutdown,
    thread: Option<std::thread::JoinHandle<i32>>,
}

impl Drop for ResidentGuard {
    fn drop(&mut self) {
        let _ = self.stop.request_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct SendSigintOnDrop(bool);

impl SendSigintOnDrop {
    fn send_now(&mut self) {
        if !self.0 {
            self.0 = true;
            if let Some(pid) = rustix::process::Pid::from_raw(std::process::id() as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
            }
        }
    }
}

impl Drop for SendSigintOnDrop {
    fn drop(&mut self) {
        self.send_now();
    }
}

struct MockOpenAiServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockOpenAiServer {
    fn start(expected_file_path: String) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (observed, stopping) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_http(stream, &expected_file_path, &observed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock OpenAI HTTP accept: {error}"),
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for MockOpenAiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn read_chunked_body(reader: &mut BufReader<TcpStream>) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line).unwrap();
        let hex_len = size_line.trim().split(';').next().unwrap().trim();
        let chunk_len = usize::from_str_radix(hex_len, 16).unwrap();
        if chunk_len == 0 {
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }
        let mut chunk = vec![0u8; chunk_len];
        reader.read_exact(&mut chunk).unwrap();
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).unwrap();
    }
    body
}

fn handle_http(stream: TcpStream, expected_file_path: &str, requests: &Mutex<Vec<Value>>) {
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap() == 0 {
        return;
    }
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "HTTP headers terminated unexpectedly"
        );
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.trim_end().split_once(':').expect("HTTP header");
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    let raw_body = if chunked {
        read_chunked_body(&mut reader)
    } else {
        let len = content_length.expect("HTTP request needs Content-Length or chunked encoding");
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).unwrap();
        buf
    };
    let body: Value = serde_json::from_slice(&raw_body).unwrap();
    if body.to_string().contains("Generate a concise title") || body["stream"] != true {
        let payload = json!({
            "id": "p5-e2e-title",
            "object": "chat.completion",
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"title\":\"Entrypoint E2E\"}"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )
        .unwrap();
        stream.flush().unwrap();
        return;
    }
    let call_index = {
        let mut guard = requests.lock().unwrap();
        let idx = guard.len();
        guard.push(json!({
            "request_line": request_line.trim_end(),
            "body": body,
        }));
        idx
    };

    let frames: Vec<Value> = if call_index == 0 {
        let tool_args = json!({ "file_path": expected_file_path }).to_string();
        vec![
            json!({
                "id": "p5-step-1",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": tool_args,
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "p5-step-1",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
            json!({
                "id": "p5-step-1",
                "model": "mock",
                "choices": [],
                "usage": {
                    "prompt_tokens": 20,
                    "completion_tokens": 10,
                    "total_tokens": 30
                }
            }),
        ]
    } else {
        vec![
            json!({
                "id": "p5-step-2",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": ASSISTANT_CONFIRMATION
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "p5-step-2",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            json!({
                "id": "p5-step-2",
                "model": "mock",
                "choices": [],
                "usage": {
                    "prompt_tokens": 35,
                    "completion_tokens": 12,
                    "total_tokens": 47
                }
            }),
        ]
    };

    let bytes = format!(
        "{}data: [DONE]\n\n",
        frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>()
    )
    .into_bytes();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )
    .unwrap();
    stream.write_all(&bytes).unwrap();
    stream.flush().unwrap();
}

fn short_temp_root(prefix: &str) -> PathBuf {
    let short_id = uuid::Uuid::new_v4().simple().to_string();
    let base = if cfg!(target_os = "macos") && std::env::temp_dir().as_os_str().len() > 24 {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    base.join(format!("{prefix}-{}", &short_id[..12]))
}

fn run_in_isolated_child(exact_test: &str, body: impl FnOnce()) {
    if std::env::var_os("PILOTDECK_E2E_ISOLATED_CHILD").is_none() {
        let isolated = short_temp_root("p5-home");
        std::fs::create_dir_all(&isolated).unwrap();
        use rustix::process::{Pid, Signal, kill_process_group};
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let receipt = isolated.join("finished");
        let log_path = isolated.join("e2e.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", exact_test, "--nocapture"])
            .env("PILOTDECK_E2E_ISOLATED_CHILD", "1")
            .env("PILOTDECK_E2E_RECEIPT", &receipt)
            .env("HOME", &isolated)
            .env("PILOT_HOME", &isolated)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .process_group(0)
            .spawn()
            .expect("start isolated E2E fixture");
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let status = loop {
            match child.try_wait().expect("poll isolated E2E fixture") {
                Some(status) => break status,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                None => {
                    if let Some(group) = Pid::from_raw(child.id() as i32) {
                        let _ = kill_process_group(group, Signal::KILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "isolated E2E fixture timed out; retained log: {}",
                        log_path.display()
                    );
                }
            }
        };
        if !status.success()
            && let Some(group) = Pid::from_raw(child.id() as i32)
        {
            let _ = kill_process_group(group, Signal::KILL);
        }
        assert!(
            status.success(),
            "isolated E2E fixture failed: {status}; retained log: {}",
            log_path.display()
        );
        assert_eq!(
            std::fs::read(&receipt).expect("test child did not publish completion receipt"),
            b"completed"
        );
        std::fs::remove_dir_all(isolated).unwrap();
        return;
    }

    body();
    let receipt = std::env::var_os("PILOTDECK_E2E_RECEIPT").expect("parent receipt path");
    std::fs::write(receipt, b"completed").unwrap();
}

struct TestHostFixture {
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
    chats: PathBuf,
    components: PathBuf,
    config: PathBuf,
    executable: PathBuf,
    digest: String,
    deployment: DeploymentKernelConfig,
    env: Env,
    http: Option<MockOpenAiServer>,
}

impl TestHostFixture {
    fn build(prefix: &str) -> Self {
        let executable = PathBuf::from(
            std::env::var_os("PILOTDECK_IFACE_MANAGED_NATIVE_FIXTURE")
                .expect("source-built managed_native helper required"),
        );
        assert!(executable.is_absolute() && executable.is_file());

        let raw_root = short_temp_root(prefix);
        std::fs::create_dir_all(&raw_root).unwrap();
        let root = std::fs::canonicalize(&raw_root).unwrap();
        let home = root.join("home");
        let project = root.join("project");
        let components = root.join("components");
        for path in [
            &home,
            &project,
            &components.join("fixture-iface"),
            &root.join("data"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let chats = crate::session_chat_dir(&home, &project, &project.to_string_lossy());
        std::fs::create_dir_all(chats.parent().unwrap()).unwrap();
        std::fs::write(
            chats.parent().unwrap().join(".cwd"),
            project.to_string_lossy().as_bytes(),
        )
        .unwrap();

        let file = project.join("fixture.txt");
        std::fs::write(&file, format!("{FILE_MARKER}\n")).unwrap();
        let http = MockOpenAiServer::start(file.to_string_lossy().into_owned());

        let ledger_path = components.join("install.json");
        std::fs::write(
            &ledger_path,
            json!({
                "schema": "agentcpm.kernel/install@1",
                "packages": [{
                    "name": "fixture-iface",
                    "version": "0.0.0",
                    "kind": "iface",
                    "dir": "fixture-iface",
                    "provides": { "interfaces": ["env"] },
                    "sha256": {}
                }]
            })
            .to_string(),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ledger_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let config = root.join("kernel.toml");
        std::fs::write(
            &config,
            concat!(
                "storage_dir = \"storage\"\n",
                "guard = \"deny-all\"\n",
                "exec_backend = \"none\"\n",
                "allow_ops = \"session.new,session.create,session.close,session.lease_acquire,",
                "session.lease_renew,session.get,session.deliver,session.stream_open,",
                "execution.open,execution.input,execution.get,execution.events,",
                "execution.output_ack,execution.control,tool.exec,",
                "model.session_open,model.session_get,model.input_append,model.input_commit,",
                "model.events_read,model.delivery_advance,model.session_close,model.session_abort,",
                "model.response_interrupt,product.plan_todo.read,product.model.metadata\"\n",
            ),
        )
        .unwrap();

        let pilot_config = home.join("pilotdeck.yaml");
        std::fs::write(
            &pilot_config,
            format!(
                concat!(
                    "schemaVersion: 1\n",
                    "agent: {{model: fixture/mock}}\n",
                    "model:\n",
                    "  providers:\n",
                    "    fixture:\n",
                    "      protocol: openai\n",
                    "      url: http://{}/v1\n",
                    "      apiKey: fixture-local-only\n",
                    "      models: {{mock: {{capabilities: {{supportsToolUse: true}}}}}}\n",
                    "telemetry: {{enabled: false}}\n",
                ),
                http.addr
            ),
        )
        .unwrap();
        let env = [
            ("HOME", home.to_string_lossy().into_owned()),
            ("PILOT_HOME", home.to_string_lossy().into_owned()),
            (
                "PILOTDECK_CONFIG_PATH",
                pilot_config.to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();

        let digest = format!("{:x}", Sha256::digest(std::fs::read(&executable).unwrap()));
        let socket = root.join("k.s");
        assert!(
            socket.as_os_str().len() < 100,
            "UDS path `{}` exceeds Unix socket length safety margin",
            socket.display()
        );
        let deployment = DeploymentKernelConfig {
            socket,
            expected_uid: rustix::process::geteuid().as_raw(),
            principal: "gateway".into(),
            tenant: "fixture".into(),
            rpc_timeout_ms: 15000,
        };

        Self {
            root,
            home,
            project,
            chats,
            components,
            config,
            executable,
            digest,
            deployment,
            env,
            http: Some(http),
        }
    }

    fn scheduler_component(
        &self,
        id: &str,
        kind: p::Kind,
        resources: &[&str],
        args: Vec<String>,
        controller: bool,
    ) -> SchedulerComponent {
        SchedulerComponent {
            binding: p::Binding {
                id: id.into(),
                revision: "fixture-v1".into(),
                kind,
                effect: if controller {
                    p::Effect::State
                } else {
                    p::Effect::ToolWrite
                },
                permissions: p::Permissions {
                    invoke: if controller {
                        [LOOP_BINDING.into()].into()
                    } else {
                        [CONTROLLER_BINDING.into()].into()
                    },
                    publish: controller,
                    state_namespace: None,
                    resources: resources.iter().map(|v| (*v).into()).collect(),
                },
            },
            provider: SchedulerImplementation::Native {
                program: self.executable.clone(),
                args,
                sha256: self.digest.clone(),
            },
            controller,
        }
    }

    fn loop_resources() -> &'static [&'static str] {
        &[
            "model.session_open",
            "model.session_get",
            "model.input_append",
            "model.input_commit",
            "model.events_read",
            "model.delivery_advance",
            "model.session_close",
            "model.session_abort",
            p::tool_batch::PREPARE,
            p::tool_batch::STATUS,
            p::tool_batch::CLOSE,
            p::product_reads::PLAN_TODO,
            p::product_reads::MODEL_METADATA,
        ]
    }

    fn loop_args(&self) -> Vec<String> {
        vec![
            "--native-model-tool-loop".into(),
            self.project.to_string_lossy().into_owned(),
        ]
    }

    fn controller_component(&self) -> SchedulerComponent {
        self.scheduler_component(
            CONTROLLER_BINDING,
            p::Kind::Service,
            &[],
            vec!["--p2-product-controller".into()],
            true,
        )
    }

    fn base_profile(&self) -> p::SessionConfig {
        p::SessionConfig {
            input_controller_ref: Some(CONTROLLER_BINDING.into()),
            default_loop_ref: LOOP_BINDING.into(),
            allowed_bindings: [LOOP_BINDING.into(), CONTROLLER_BINDING.into()].into(),
        }
    }

    fn turn_options() -> KernelTurnOptions {
        Arc::new(|_, _| {
            Ok(KernelExecutionOptions {
                loop_binding: LOOP_BINDING.into(),
                controller_binding: CONTROLLER_BINDING.into(),
                module_id: "kernel-native".into(),
                cleanup_timeout: Duration::from_secs(10),
            })
        })
    }

    fn startup_options(&self) -> StartupOptions {
        StartupOptions {
            config: Some(self.config.clone()),
            data_dir: self.root.join("data"),
            components_dir: self.components.clone(),
        }
    }

    fn spawn_standalone_host(
        &self,
        runtime_handle: tokio::runtime::Handle,
    ) -> (ResidentGuard, Arc<ScopedResidentSessionBinder>) {
        let loop_component = self.scheduler_component(
            LOOP_BINDING,
            p::Kind::Loop,
            Self::loop_resources(),
            self.loop_args(),
            false,
        );
        let controller_component = self.controller_component();
        let expected_loop = loop_component.effective_binding().unwrap();
        let assembly = SchedulerAssembly {
            protocol: p::PROTOCOL.into(),
            profile: self.base_profile(),
            bindings: vec![loop_component, controller_component],
        };

        let provider: Arc<dyn TrustedReadProvider> = Arc::new(NativeProductReads);
        let reads = [
            p::product_reads::PLAN_TODO,
            p::product_reads::MODEL_METADATA,
        ]
        .into_iter()
        .map(|method| TrustedReadBinding {
            method: method.into(),
            callers: vec![expected_loop.clone()],
            profiles: vec![pilotdeck_protocol::boundary_codec::PROFILE.into()],
            provider: provider.clone(),
        })
        .collect();

        let setup = Arc::new(GatewayManagedModelSetup::new(
            None,
            Arc::new(OriginalProjectTurnAuthority),
            runtime_handle.clone(),
        ));
        let composition = ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly),
            ..Default::default()
        })
        .with_trusted_reads(reads)
        .with_managed_model(setup.clone())
        .with_trusted_tool_batches(Arc::new(NativeToolBatchSetup));
        let (composition, shutdown) = composition.with_shutdown_control();

        let mut options: EmbedOptions = self
            .deployment
            .host_options(self.startup_options())
            .unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        options.on_ready = Some(Box::new(move |_| {
            ready_tx.send(()).unwrap();
        }));
        let thread = std::thread::spawn(move || serve_resident_composed(options, composition));
        let guard = ResidentGuard {
            stop: shutdown,
            thread: Some(thread),
        };
        ready_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("resident readiness");

        let project_sessions = setup.project_sessions().unwrap();
        let strict_binder = KernelSessionBinder {
            connections: Arc::new(self.deployment.clone()),
            project_sessions: project_sessions.clone(),
            catalog: Arc::new(OriginalProjectCatalogPolicy),
            turn_options: Self::turn_options(),
        };
        let gate_err = runtime_handle
            .block_on(strict_binder.validate_installation())
            .expect_err(
                "KernelSessionBinder must stay fail-closed without LOOP_SETTLEMENT_FEATURE",
            );
        assert!(
            gate_err.message.contains("Unsupported Kernel integration"),
            "unexpected FullLate gate message: {}",
            gate_err.message
        );

        let scoped_binder = Arc::new(ScopedResidentSessionBinder::new(
            self.deployment.clone(),
            project_sessions,
            Arc::new(OriginalProjectCatalogPolicy),
            Self::turn_options(),
            LOOP_BINDING,
            CONTROLLER_BINDING,
        ));
        (guard, scoped_binder)
    }

    fn finish(mut self) {
        if let Some(http) = self.http.take() {
            http.shutdown();
        }
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn assert_turn_outcome_and_transcript(http: &MockOpenAiServer, chats: &Path, events: &[JsValue]) {
    assert!(
        !events
            .iter()
            .any(|event| event.get("type") == Some(&JsValue::from("error"))),
        "turn emitted unexpected error event: {events:?}"
    );
    let event_blobs: String = events
        .iter()
        .filter_map(|event| stringify_json(event).ok().flatten())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        event_blobs.contains(ASSISTANT_CONFIRMATION),
        "streamed events missing assistant confirmation: {event_blobs}"
    );

    let recorded_requests = http.requests();
    assert_eq!(
        recorded_requests.len(),
        2,
        "expected exactly 2 model calls (tool request + final answer), got: {recorded_requests:?}"
    );
    let second_body = serde_json::to_string(&recorded_requests[1]["body"]).unwrap();
    assert!(
        second_body.contains(FILE_MARKER),
        "second model request missing read_file content marker: {second_body}"
    );

    let jsonl_files: Vec<PathBuf> = std::fs::read_dir(chats)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    assert!(
        !jsonl_files.is_empty(),
        "expected persistent session .jsonl under {}",
        chats.display()
    );
    let transcript: String = jsonl_files
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "call_read_1",
        "read_file",
        FILE_MARKER,
        ASSISTANT_CONFIRMATION,
    ] {
        assert!(
            transcript.contains(expected),
            "persisted session transcript missing `{expected}`: {transcript}"
        );
    }
}

#[test]
fn gateway_local_runs_through_scoped_resident_binder_e2e() {
    run_in_isolated_child(
        "native_process::kernel_entrypoint_e2e_tests::gateway_local_runs_through_scoped_resident_binder_e2e",
        || {
            let fixture = TestHostFixture::build("p5-local");
            let executor = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let (mut resident, binder) = fixture.spawn_standalone_host(executor.handle().clone());

            let gateway = LocalGateway::start_with_services(
                LocalOptions {
                    pilot_home: fixture.home.clone(),
                    project: fixture.project.clone(),
                    os_home: Some(fixture.home.clone()),
                    sidecar: fixture.root.join("unused"),
                    env: fixture.env.clone(),
                    builtin_skills_root: None,
                    builtin_plugins_root: None,
                    telemetry_context: None,
                },
                NativeExtensions::default(),
                NativeGatewayServices {
                    kernel_execution: Some(binder),
                    ..Default::default()
                },
            )
            .unwrap();

            let request = parse_json(
                r#"{"sessionKey":"p5-local-session","channelKey":"cli","message":"Read fixture.txt using read_file and echo the marker."}"#,
            )
            .unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let capture = events.clone();
            gateway
                .submit_turn(
                    request,
                    Arc::new(move |event| capture.lock().unwrap().push(event)),
                )
                .expect("LocalGateway ScopedResident E2E turn");

            let recorded_events = events.lock().unwrap().clone();
            let http = fixture.http.as_ref().unwrap();
            assert_turn_outcome_and_transcript(http, &fixture.chats, &recorded_events);

            gateway
                .shutdown(Duration::from_secs(15))
                .expect("gateway cleanup");
            drop(gateway);
            let _ = resident.stop.request_shutdown();
            assert_eq!(
                resident
                    .thread
                    .take()
                    .unwrap()
                    .join()
                    .expect("resident thread"),
                0
            );
            drop(executor);
            fixture.finish();
        },
    );
}

struct UnusedModuleModel;
impl ManagedModuleModel for UnusedModuleModel {
    fn call<'a>(
        &'a self,
        _principal: &'a ManagedModulePrincipal,
        _operation: &'a str,
        _input: &'a JsValue,
    ) -> Pin<
        Box<dyn Future<Output = Result<ManagedModuleResponse, ManagedModuleFailure>> + Send + 'a>,
    > {
        Box::pin(async {
            Err(ManagedModuleFailure {
                status: Some(403),
                code: Some("UNUSED_IN_TURN_E2E".into()),
                message: Some("module-host model RPC is unused in turn E2E".into()),
            })
        })
    }
}

async fn ws_send_json(socket: &mut WsSocket, value: &Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn ws_recv_json(socket: &mut WsSocket) -> Value {
    loop {
        let msg = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("WS receive timeout")
            .expect("WS closed unexpectedly")
            .expect("WS frame error");
        if let Message::Text(text) = msg {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn ws_connect_and_hello(ws_url: &str, token: &str) -> WsSocket {
    let (mut socket, _) = tokio::time::timeout(WAIT, connect_async(ws_url))
        .await
        .expect("WS connect timeout")
        .expect("WS connect failed");
    ws_send_json(
        &mut socket,
        &json!({
            "type": "hello",
            "protocolVersion": pilotdeck_protocol::GATEWAY_PROTOCOL_VERSION,
            "clientName": "sdk",
            "clientVersion": "pilotdeck-cli",
            "token": token,
        }),
    )
    .await;
    loop {
        let frame = ws_recv_json(&mut socket).await;
        if frame["type"] == "hello_ok" {
            return socket;
        }
    }
}

async fn ws_request(socket: &mut WsSocket, method: &str, params: Value) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    ws_send_json(
        socket,
        &json!({
            "type": "request",
            "id": id,
            "method": method,
            "params": params,
        }),
    )
    .await;
    loop {
        let frame = ws_recv_json(socket).await;
        if frame["type"] == "response" && frame["id"] == id {
            assert_eq!(frame["ok"], true, "WS RPC `{method}` failed: {frame}");
            return frame["result"].clone();
        }
    }
}

#[test]
fn gateway_ws_sdk_cli_protocol_runs_through_resident_kernel_e2e() {
    run_in_isolated_child(
        "native_process::kernel_entrypoint_e2e_tests::gateway_ws_sdk_cli_protocol_runs_through_resident_kernel_e2e",
        || {
            let fixture = TestHostFixture::build("p5-ws");
            let loop_component = fixture.scheduler_component(
                LOOP_BINDING,
                p::Kind::Loop,
                TestHostFixture::loop_resources(),
                fixture.loop_args(),
                false,
            );
            let assembly = SchedulerAssembly {
                protocol: p::PROTOCOL.into(),
                profile: fixture.base_profile(),
                bindings: vec![fixture.controller_component()],
            };
            let inputs = ScopedResidentProductInputs {
                deployment: fixture.deployment.clone(),
                startup: fixture.startup_options(),
                external: ExternalBackends {
                    scheduler_assembly: Some(assembly),
                    ..Default::default()
                },
                native_loop: NativeLoopImage {
                    binding: loop_component.binding,
                    program: fixture.executable.clone(),
                    sha256: fixture.digest.clone(),
                },
                loop_args: fixture.loop_args(),
                loop_binding: LOOP_BINDING.into(),
                controller_binding: CONTROLLER_BINDING.into(),
                model_authority: Arc::new(OriginalProjectTurnAuthority),
                tool_batches: Arc::new(NativeToolBatchSetup),
                catalog: Arc::new(OriginalProjectCatalogPolicy),
                turn_options: TestHostFixture::turn_options(),
                module_model: Arc::new(UnusedModuleModel),
                proxy: None,
                ready_timeout: Duration::from_secs(60),
                shutdown_timeout: Duration::from_secs(20),
            };
            let server_options = ServerOptions {
                telemetry_context: crate::telemetry::TelemetryRuntimeContext {
                    installation_id: "p5-ws-install".into(),
                    instance_id: "p5-ws-instance".into(),
                    deployment_mode: "test".into(),
                    commit_hash: "0".into(),
                    app_version: "0".into(),
                    platform: "unix".into(),
                },
                telemetry_enabled: false,
                pilot_home: fixture.home.clone(),
                project: fixture.project.clone(),
                os_home: Some(fixture.home.clone()),
                host: Some("127.0.0.1".into()),
                port: 0.0,
                allow_remote_host: false,
                token: Some(WS_TOKEN.into()),
                token_path: fixture.home.join("server-token"),
                sidecar: fixture.root.join("unused"),
                env: fixture.env.clone(),
                builtin_skills_root: None,
                builtin_plugins_root: None,
            };

            let project_str = fixture.project.to_string_lossy().into_owned();
            let client_handle: Arc<Mutex<Option<std::thread::JoinHandle<Vec<JsValue>>>>> =
                Arc::new(Mutex::new(None));
            let client_slot = client_handle.clone();

            serve_with_resident_kernel_with_profile(
                server_options,
                inputs,
                KernelExecutionProfile::ScopedResident,
                move |listening: &Listening| {
                    let ws_url = listening.ws_url.clone();
                    let project_key = project_str.clone();
                    let handle = std::thread::spawn(move || {
                        let mut sigint_guard = SendSigintOnDrop(false);
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        let streamed = rt.block_on(async move {
                            let mut control = ws_connect_and_hello(&ws_url, WS_TOKEN).await;
                            let created = ws_request(
                                &mut control,
                                "new_session",
                                json!({
                                    "projectKey": project_key,
                                    "channelKey": "api_server",
                                }),
                            )
                            .await;
                            let session_key = created["sessionKey"]
                                .as_str()
                                .expect("new_session sessionKey")
                                .to_owned();

                            let mut query = ws_connect_and_hello(&ws_url, WS_TOKEN).await;
                            let _ = ws_request(
                                &mut query,
                                "resume_session",
                                json!({ "sessionKey": session_key }),
                            )
                            .await;

                            let submit_id = uuid::Uuid::new_v4().to_string();
                            let run_id = uuid::Uuid::new_v4().to_string();
                            ws_send_json(
                                &mut query,
                                &json!({
                                    "type": "request",
                                    "id": submit_id,
                                    "method": "submit_turn",
                                    "params": {
                                        "sessionKey": session_key,
                                        "channelKey": "api_server",
                                        "projectKey": project_key,
                                        "message": "Read fixture.txt using read_file and echo the marker.",
                                        "canPrompt": false,
                                        "canElicit": false,
                                        "sdkSessionConfig": {},
                                        "runId": run_id,
                                    },
                                }),
                            )
                            .await;

                            let mut events = Vec::new();
                            let mut expected_seq: i64 = 0;
                            loop {
                                let frame = ws_recv_json(&mut query).await;
                                if frame["type"] == "response" && frame["id"] == submit_id {
                                    panic!("submit_turn returned unexpected error frame: {frame}");
                                }
                                if frame["type"] == "event" && frame["id"] == submit_id {
                                    assert_eq!(
                                        frame["seq"].as_i64(),
                                        Some(expected_seq),
                                        "WS event seq mismatch: {frame}"
                                    );
                                    expected_seq += 1;
                                    let event_json = serde_json::to_string(&frame["event"]).unwrap();
                                    events.push(parse_json(&event_json).unwrap());
                                    if frame["final"] == true {
                                        break;
                                    }
                                }
                            }
                            let _ = query.close(None).await;
                            let _ = control.close(None).await;
                            events
                        });
                        sigint_guard.send_now();
                        streamed
                    });
                    *client_slot.lock().unwrap() = Some(handle);
                },
            )
            .expect("serve_with_resident_kernel_with_profile graceful SIGINT exit 0");

            let streamed_events = client_handle
                .lock()
                .unwrap()
                .take()
                .expect("listening callback spawned WS client")
                .join()
                .expect("WS client assertions succeeded");
            let http = fixture.http.as_ref().unwrap();
            assert_turn_outcome_and_transcript(http, &fixture.chats, &streamed_events);
            fixture.finish();
        },
    );
}

struct TestChannelHandle;
impl ChannelHandle for TestChannelHandle {
    fn stop(&self, _reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}

struct TestChannelAdapter {
    key: ChannelKey,
    captured_gateway: Arc<Mutex<Option<Arc<dyn TurnGateway>>>>,
}

impl ChannelAdapter for TestChannelAdapter {
    fn channel_key(&self) -> &ChannelKey {
        &self.key
    }

    fn start(
        &self,
        deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        let gateway = deps.turn_gateway;
        let slot = self.captured_gateway.clone();
        Box::pin(async move {
            *slot.lock().unwrap() = Some(
                gateway
                    .ok_or_else(|| ChannelError::error("ChannelStartDeps missing TurnGateway"))?,
            );
            Ok(Arc::new(TestChannelHandle) as Arc<dyn ChannelHandle>)
        })
    }
}

async fn drain_channel_stream(
    mut stream: Box<dyn TurnEventStream>,
) -> Result<Vec<JsValue>, ChannelError> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await? {
        events.push(event);
    }
    Ok(events)
}

#[test]
fn channel_submitter_runs_through_scoped_resident_binder_e2e() {
    run_in_isolated_child(
        "native_process::kernel_entrypoint_e2e_tests::channel_submitter_runs_through_scoped_resident_binder_e2e",
        || {
            let fixture = TestHostFixture::build("p5-chan");
            let executor = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let (mut resident, binder) = fixture.spawn_standalone_host(executor.handle().clone());

            let options = Options {
                home: fixture.home.clone(),
                project: fixture.project.clone(),
                project_root: fixture.project.clone(),
                listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                token_file: PathBuf::new(),
                sidecar: fixture.root.join("unused"),
                sidecar_args: Vec::new(),
                drain_budget: Duration::from_secs(15),
                storage_root: None,
                os_home: Some(fixture.home.clone()),
                builtin_skills_root: None,
                builtin_plugins_root: None,
                in_process_cli: true,
                telemetry: None,
                telemetry_context: None,
                telemetry_reload: false,
            };

            let captured_gateway: Arc<Mutex<Option<Arc<dyn TurnGateway>>>> =
                Arc::new(Mutex::new(None));
            let project_str = fixture.project.to_string_lossy().into_owned();
            let streamed_events = executor.block_on(async {
                ensure_router_dir(&fixture.home);
                let (host_gateway, _webhook, mut switch_handles) =
                    assembly::start_with_managed_services(
                        options,
                        fixture.env.clone(),
                        NativeExtensions::default(),
                        true,
                        assembly::NativeHostServices::default(),
                        binder,
                    )
                    .await
                    .expect("assembly::start_with_managed_services");

                if let Some(activation) = switch_handles.channel_activation.take() {
                    activation.await.expect("channel activation");
                }
                switch_handles.channels.server().publish_startup();
                switch_handles
                    .channels
                    .server()
                    .hot_start_channel(Arc::new(TestChannelAdapter {
                        key: ChannelKey::new("test-channel"),
                        captured_gateway: captured_gateway.clone(),
                    }))
                    .await
                    .expect("hot_start_channel");

                let channel_gateway = captured_gateway
                    .lock()
                    .unwrap()
                    .take()
                    .expect("ChannelStartDeps received FeishuTurnGateway -> TurnSubmitter");
                let stream = channel_gateway
                    .submit(TurnRequest {
                        session_key: "p5-channel-session".into(),
                        channel_key: ChannelKey::new("test-channel"),
                        message: "Read fixture.txt using read_file and echo the marker.".into(),
                        project_key: Some(project_str.into()),
                        ..Default::default()
                    })
                    .await
                    .expect("channel TurnGateway::submit");
                let events = drain_channel_stream(stream)
                    .await
                    .expect("channel TurnEventStream");

                switch_handles
                    .channels
                    .close()
                    .await
                    .expect("NativeChannels close");
                host_gateway
                    .shutdown(Duration::from_secs(15))
                    .await
                    .expect("TurnGateway shutdown");
                drop(switch_handles);
                drop(host_gateway);
                events
            });

            let http = fixture.http.as_ref().unwrap();
            assert_turn_outcome_and_transcript(http, &fixture.chats, &streamed_events);

            let _ = resident.stop.request_shutdown();
            assert_eq!(
                resident
                    .thread
                    .take()
                    .unwrap()
                    .join()
                    .expect("resident thread"),
                0
            );
            drop(executor);
            fixture.finish();
        },
    );
}
