//! End-to-end verification against a real resident Kernel host,
//! `KernelSessionBinder`, `GatewayManagedModelSetup`, `NativeToolBatchSetup`,
//! `NativeProductReads`, `ProductController`, and the source-built
//! `--native-model-tool-loop` fixture role (`Model -> read_file -> Model`).
#![cfg(all(test, unix, feature = "kernel-resident"))]

use crate::{
    extensions::NativeExtensions,
    native_process::{
        DeploymentKernelConfig, GatewayManagedModelSetup, KernelSessionBinder, LocalGateway,
        LocalOptions, ManagedSessionBinder, NativeGatewayServices, NativeProductReads,
        NativeToolBatchSetup, OriginalProjectCatalogPolicy, OriginalProjectTurnAuthority,
    },
    runtime::turn::kernel_client::{KernelExecutionOptions, p2_fixture::FixtureBinder},
};
use agentcpm_host::{
    EmbedOptions, ExternalBackends, ResidentComposition, SchedulerAssembly, SchedulerComponent,
    SchedulerImplementation, StartupOptions, TrustedReadBinding, TrustedReadProvider,
    serve_resident_composed,
};
use agentcpm_scheduler_protocol as p;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(30);
const FILE_MARKER: &str = "PILOTDECK_REAL_MODEL_TOOL_E2E_MARKER";
const ASSISTANT_CONFIRMATION: &str = "E2E_READ_CONFIRMED: PILOTDECK_REAL_MODEL_TOOL_E2E_MARKER";

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
            "id": "e2e-title",
            "object": "chat.completion",
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"title\":\"E2E Fixture\"}"
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
                "id": "e2e-step-1",
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
                "id": "e2e-step-1",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
            json!({
                "id": "e2e-step-1",
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
                "id": "e2e-step-2",
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
                "id": "e2e-step-2",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            json!({
                "id": "e2e-step-2",
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

#[test]
fn real_host_kernel_session_runs_model_read_file_model_e2e() {
    if std::env::var_os("PILOTDECK_E2E_ISOLATED_CHILD").is_none() {
        let isolated =
            std::env::temp_dir().join(format!("pd-kernel-e2e-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&isolated).unwrap();
        use rustix::process::{Pid, Signal, kill_process_group};
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let receipt = isolated.join("finished");
        let log_path = isolated.join("e2e.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_process::kernel_e2e_model_tool_tests::real_host_kernel_session_runs_model_read_file_model_e2e",
                "--nocapture",
            ])
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

    let executable = PathBuf::from(
        std::env::var_os("PILOTDECK_IFACE_MANAGED_NATIVE_FIXTURE")
            .expect("source-built managed_native helper required"),
    );
    assert!(executable.is_absolute() && executable.is_file());

    let short_id = uuid::Uuid::new_v4().simple().to_string();
    let raw_root = std::env::temp_dir().join(format!("pe-{}", &short_id[..12]));
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

    std::fs::write(
        components.join("install.json"),
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

    let digest = format!("{:x}", Sha256::digest(std::fs::read(&executable).unwrap()));
    let component =
        |id: &str, kind: p::Kind, resources: &[&str], args: Vec<String>, controller: bool| {
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
                            ["pilotdeck".into()].into()
                        } else {
                            ["controller".into()].into()
                        },
                        publish: controller,
                        state_namespace: None,
                        resources: resources.iter().map(|v| (*v).into()).collect(),
                    },
                },
                provider: SchedulerImplementation::Native {
                    program: executable.clone(),
                    args,
                    sha256: digest.clone(),
                },
                controller,
            }
        };

    let assembly = SchedulerAssembly {
        protocol: p::PROTOCOL.into(),
        profile: p::SessionConfig {
            input_controller_ref: Some("controller".into()),
            default_loop_ref: "pilotdeck".into(),
            allowed_bindings: ["pilotdeck".into(), "controller".into()].into(),
        },
        bindings: vec![
            component(
                "pilotdeck",
                p::Kind::Loop,
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
                ],
                vec![
                    "--native-model-tool-loop".into(),
                    project.to_string_lossy().into_owned(),
                ],
                false,
            ),
            component(
                "controller",
                p::Kind::Service,
                &[],
                vec!["--p2-product-controller".into()],
                true,
            ),
        ],
    };

    let expected_loop = assembly.bindings[0]
        .effective_binding()
        .expect("fixture loop binding");
    let expected_controller = assembly.bindings[1]
        .effective_binding()
        .expect("fixture controller binding");

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

    let executor = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let setup = Arc::new(GatewayManagedModelSetup::new(
        None,
        Arc::new(OriginalProjectTurnAuthority),
        executor.handle().clone(),
    ));
    let composition = ResidentComposition::new(ExternalBackends {
        scheduler_assembly: Some(assembly),
        ..Default::default()
    })
    .with_trusted_reads(reads)
    .with_managed_model(setup.clone())
    .with_trusted_tool_batches(Arc::new(NativeToolBatchSetup));
    let (composition, shutdown) = composition.with_shutdown_control();

    let uid: u32 = String::from_utf8(
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse()
    .unwrap();
    let deployment = DeploymentKernelConfig {
        socket: root.join("k.s"),
        expected_uid: uid,
        principal: "gateway".into(),
        tenant: "fixture".into(),
        rpc_timeout_ms: 15000,
    };
    let mut options: EmbedOptions = deployment
        .host_options(StartupOptions {
            config: Some(config),
            data_dir: root.join("data"),
            components_dir: components,
        })
        .unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    options.on_ready = Some(Box::new(move |_| {
        ready_tx.send(()).unwrap();
    }));
    let thread = std::thread::spawn(move || serve_resident_composed(options, composition));
    let mut resident = ResidentGuard {
        stop: shutdown,
        thread: Some(thread),
    };
    ready_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("resident readiness");

    let production_binder = KernelSessionBinder {
        connections: Arc::new(deployment.clone()),
        project_sessions: setup.project_sessions().unwrap(),
        catalog: Arc::new(OriginalProjectCatalogPolicy),
        turn_options: Arc::new(|_, _| {
            Ok(KernelExecutionOptions {
                loop_binding: "pilotdeck".into(),
                controller_binding: "controller".into(),
                module_id: "kernel-native".into(),
                cleanup_timeout: Duration::from_secs(10),
            })
        }),
        resident_shutdown: None,
    };
    let gate_error = executor
        .block_on(production_binder.validate_installation())
        .expect_err(
            "KernelSessionBinder must fail closed while LOOP_SETTLEMENT_FEATURE is unadvertised",
        );
    assert!(
        gate_error
            .message
            .contains("Unsupported Kernel integration"),
        "unexpected fail-closed message: {}",
        gate_error.message
    );

    let binder = Arc::new(FixtureBinder {
        socket: deployment.socket,
        expected_principal: deployment.principal,
        expected_tenant: deployment.tenant,
        timeout: Duration::from_secs(15),
        required_features: vec![
            p::tool_batch::FEATURE.into(),
            p::model_session_resources::FEATURE.into(),
        ],
        projects: setup.project_sessions().unwrap(),
        catalog: Arc::new(OriginalProjectCatalogPolicy),
        loop_binding: expected_loop,
        controller_binding: expected_controller,
    });

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

    let gateway = LocalGateway::start_with_services(
        LocalOptions {
            pilot_home: home,
            project,
            os_home: Some(root.join("home")),
            sidecar: root.join("unused"),
            env,
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

    let request = pilotdeck_protocol::parse_json(
        r#"{"sessionKey":"e2e-session","channelKey":"test","message":"Read fixture.txt using read_file and echo the marker."}"#,
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = events.clone();
    gateway
        .submit_turn(
            request,
            Arc::new(move |event| capture.lock().unwrap().push(event)),
        )
        .expect("real host KernelSessionBackend E2E turn");

    let recorded_events = events.lock().unwrap().clone();
    assert!(
        !recorded_events
            .iter()
            .any(|event| event.get("type") == Some(&pilotdeck_protocol::JsValue::from("error"))),
        "turn emitted unexpected error event: {recorded_events:?}"
    );
    let event_blobs: String = recorded_events
        .iter()
        .filter_map(|event| pilotdeck_protocol::stringify_json(event).ok().flatten())
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

    let jsonl_files: Vec<PathBuf> = std::fs::read_dir(&chats)
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
    http.shutdown();
    drop(executor);
    std::fs::remove_dir_all(root).unwrap();
    let receipt = std::env::var_os("PILOTDECK_E2E_RECEIPT").expect("parent receipt path");
    std::fs::write(receipt, b"completed").unwrap();
}
