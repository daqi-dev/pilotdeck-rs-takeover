#![cfg(all(unix, feature = "kernel-resident"))]

use futures_util::{SinkExt, StreamExt};
use pilotdeck_gateway::native_process::{
    BackendOwner, Listening, LocalGateway, LocalOptions, ServerOptions, load_session_owner, serve,
};
use pilotdeck_protocol::parse_json;
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::net::TcpStream as TokioTcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const WAIT: Duration = Duration::from_secs(30);
const CHILD_FLAG_ENV: &str = "PILOTDECK_FULLLATE_E2E_CHILD";
const WS_TOKEN: &str = "full-late-e2e-token";
const FILE_MARKER: &str = "FULL_LATE_FIXTURE_MARKER_V1";

type WsSocket = WebSocketStream<MaybeTlsStream<TokioTcpStream>>;

fn locate_native_loop_binary() -> PathBuf {
    if let Some(explicit) = std::env::var_os("PILOTDECK_IFACE_NATIVE_LOOP_BIN")
        .or_else(|| option_env!("CARGO_BIN_EXE_pilotdeck-native-loop").map(Into::into))
    {
        let path = PathBuf::from(explicit);
        assert!(
            path.is_absolute() && path.is_file(),
            "invalid pilotdeck-native-loop path: {}",
            path.display()
        );
        return path;
    }
    let exe = std::env::current_exe().expect("current_exe");
    let sibling = exe
        .parent()
        .and_then(|deps| deps.parent())
        .map(|target_dir| target_dir.join("pilotdeck-native-loop"))
        .expect("target dir parent");
    assert!(
        sibling.is_file(),
        "pilotdeck-native-loop binary not found at {}; set PILOTDECK_IFACE_NATIVE_LOOP_BIN or build pilotdeck-native-loop",
        sibling.display()
    );
    sibling
}

struct TempWorkspace {
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
    config: PathBuf,
}

impl TempWorkspace {
    fn new(prefix: &str) -> Self {
        let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let raw = PathBuf::from(format!("/tmp/{prefix}-{short}"));
        fs::create_dir_all(&raw).unwrap();
        let root = fs::canonicalize(&raw).unwrap_or(raw);
        let home = root.join("home");
        let project = root.join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("fixture.txt"), format!("{FILE_MARKER}\n")).unwrap();
        let chats =
            pilotdeck_gateway::session_chat_dir(&home, &project, &project.to_string_lossy());
        fs::create_dir_all(chats.parent().unwrap()).unwrap();
        fs::write(
            chats.parent().unwrap().join(".cwd"),
            project.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let config = home.join("pilotdeck.yaml");
        Self {
            root,
            home,
            project,
            config,
        }
    }

    fn write_config(&self, openai_addr: SocketAddr) {
        fs::write(
            &self.config,
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
                    "      models: {{mock: {{contextWindow: 4096, capabilities: {{supportsToolUse: true}}}}}}\n",
                    "telemetry: {{enabled: false}}\n",
                ),
                openai_addr
            ),
        )
        .unwrap();
    }

    fn write_ask_bash_rules(&self) {
        fs::write(
            self.home.join("permissions.json"),
            r#"{"askTools":["bash"]}"#,
        )
        .unwrap();
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct MockOpenAiServer {
    addr: SocketAddr,
    stall_next: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockOpenAiServer {
    fn start(expected_file: PathBuf) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let req_ref = Arc::new(Mutex::new(Vec::new()));
        let stall_next = Arc::new(AtomicBool::new(false));
        let overflow_injected = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (stall_ref, overflow_ref, stop_ref) =
            (stall_next.clone(), overflow_injected.clone(), stop.clone());
        let file_str = expected_file.to_string_lossy().into_owned();
        let thread = std::thread::spawn(move || {
            while !stop_ref.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_http(
                            stream,
                            &file_str,
                            &req_ref,
                            &stall_ref,
                            &overflow_ref,
                            &stop_ref,
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            stall_next,
            stop,
            thread: Some(thread),
        }
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for MockOpenAiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
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

fn write_sse_frames(stream: &mut TcpStream, frames: Vec<Value>) {
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

fn sse_text_frames(id: &str, text: &str) -> Vec<Value> {
    vec![
        json!({
            "id": id,
            "model": "mock",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": text },
                "finish_reason": null
            }]
        }),
        json!({
            "id": id,
            "model": "mock",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        }),
        json!({
            "id": id,
            "model": "mock",
            "choices": [],
            "usage": { "prompt_tokens": 20, "completion_tokens": 8, "total_tokens": 28 }
        }),
    ]
}

fn sse_tool_call_frames(id: &str, call_id: &str, tool_name: &str, args: Value) -> Vec<Value> {
    vec![
        json!({
            "id": id,
            "model": "mock",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": tool_name,
                            "arguments": args.to_string(),
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        json!({
            "id": id,
            "model": "mock",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        }),
        json!({
            "id": id,
            "model": "mock",
            "choices": [],
            "usage": { "prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30 }
        }),
    ]
}

fn handle_http(
    mut stream: TcpStream,
    expected_file_path: &str,
    requests: &Mutex<Vec<Value>>,
    stall_next: &AtomicBool,
    overflow_injected: &AtomicBool,
    stop: &AtomicBool,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(WAIT));
    let _ = stream.set_write_timeout(Some(WAIT));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    let raw_body = if chunked {
        read_chunked_body(&mut reader)
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).unwrap();
        buf
    } else {
        Vec::new()
    };
    let body: Value = serde_json::from_slice(&raw_body).unwrap_or(Value::Null);
    if body.to_string().contains("Generate a concise title") || body["stream"] != true {
        let payload = json!({
            "id": "e2e-title",
            "object": "chat.completion",
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "{\"title\":\"FullLate\"}" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.flush();
        return;
    }

    requests.lock().unwrap().push(body.clone());
    let serialized = body.to_string();

    if stall_next.swap(false, Ordering::SeqCst) {
        let chunk = format!(
            "data: {}\n\n",
            json!({
                "id": "fl-stall",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": { "role": "assistant", "content": "STREAMING_BEFORE_ABORT " },
                    "finish_reason": null
                }]
            })
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n",
            chunk.len(),
            chunk
        );
        let _ = stream.flush();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        return;
    }

    if serialized.contains("TRIGGER_OVERFLOW") && !overflow_injected.swap(true, Ordering::SeqCst) {
        let err = json!({
            "error": {
                "message": "This model's maximum context length is 4096 tokens. However, your messages resulted in 4900 tokens.",
                "type": "invalid_request_error",
                "code": "context_length_exceeded"
            }
        })
        .to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            err.len(),
            err
        );
        let _ = stream.flush();
        return;
    }

    let frames = if serialized.contains("TRIGGER_READ_FILE") && !serialized.contains(FILE_MARKER) {
        sse_tool_call_frames(
            "fl-read",
            "call_fl_read_1",
            "read_file",
            json!({ "file_path": expected_file_path }),
        )
    } else if serialized.contains("TRIGGER_READ_FILE") {
        sse_text_frames("fl-read-done", &format!("FULL_LATE_READ_OK: {FILE_MARKER}"))
    } else if serialized.contains("TRIGGER_DENIED_BASH")
        && !serialized.contains("operator denied")
        && !serialized.contains("denied")
    {
        sse_tool_call_frames(
            "fl-bash-deny",
            "call_fl_bash_deny",
            "bash",
            json!({ "command": "echo MUST_NOT_RUN" }),
        )
    } else if serialized.contains("TRIGGER_DENIED_BASH") {
        sse_text_frames("fl-bash-denied-ok", "FULL_LATE_DENY_HANDLED")
    } else if serialized.contains("TRIGGER_ALLOW_BASH")
        && !serialized.contains("FULL_LATE_BASH_EXECUTED")
    {
        sse_tool_call_frames(
            "fl-bash-allow",
            "call_fl_bash_allow",
            "bash",
            json!({ "command": "echo FULL_LATE_BASH_EXECUTED" }),
        )
    } else if serialized.contains("TRIGGER_ALLOW_BASH") {
        sse_text_frames("fl-bash-allow-ok", "FULL_LATE_ALLOW_HANDLED")
    } else if serialized.contains("TRIGGER_OVERFLOW") {
        sse_text_frames("fl-recover-ok", "FULL_LATE_RECOVERY_CONFIRMED")
    } else {
        sse_text_frames("fl-resume-ok", "FULL_LATE_RESUMED_OK")
    };
    write_sse_frames(&mut stream, frames);
}

async fn ws_send_json(socket: &mut WsSocket, value: &Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("WS send");
}

async fn ws_recv_json(socket: &mut WsSocket) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let next = tokio::time::timeout_at(deadline, socket.next())
            .await
            .expect("WS recv timeout")
            .expect("WS closed unexpectedly")
            .expect("WS frame error");
        match next {
            Message::Text(text) => {
                return serde_json::from_str(&text).expect("valid JSON WS frame");
            }
            Message::Ping(payload) => {
                let _ = socket.send(Message::Pong(payload)).await;
            }
            Message::Close(frame) => panic!("unexpected WS close: {frame:?}"),
            _ => {}
        }
    }
}

async fn ws_connect_and_hello(ws_url: &str, token: &str) -> WsSocket {
    let (mut socket, _) = tokio::time::timeout(WAIT, connect_async(ws_url))
        .await
        .expect("WS connect timeout")
        .expect("WS connect");
    ws_send_json(
        &mut socket,
        &json!({
            "type": "hello",
            "protocolVersion": pilotdeck_protocol::GATEWAY_PROTOCOL_VERSION,
            "clientName": "sdk",
            "clientVersion": "pilotdeck-fulllate-e2e",
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

async fn ws_submit_and_collect(
    socket: &mut WsSocket,
    session_key: &str,
    project_key: &str,
    message: &str,
    can_prompt: bool,
) -> Vec<Value> {
    let submit_id = uuid::Uuid::new_v4().to_string();
    let run_id = uuid::Uuid::new_v4().to_string();
    ws_send_json(
        socket,
        &json!({
            "type": "request",
            "id": submit_id,
            "method": "submit_turn",
            "params": {
                "sessionKey": session_key,
                "channelKey": "api_server",
                "projectKey": project_key,
                "message": message,
                "canPrompt": can_prompt,
                "canElicit": false,
                "sdkSessionConfig": {},
                "runId": run_id,
            }
        }),
    )
    .await;
    let mut events = Vec::new();
    let mut expected_seq = 0_i64;
    loop {
        let frame = ws_recv_json(socket).await;
        if frame["type"] == "response" && frame["id"] == submit_id {
            panic!("submit_turn error frame: {frame}");
        }
        if frame["type"] == "event" && frame["id"] == submit_id {
            assert_eq!(frame["seq"].as_i64(), Some(expected_seq));
            expected_seq += 1;
            events.push(frame["event"].clone());
            if frame["final"] == true {
                return events;
            }
        }
    }
}

fn run_isolated_child(test_name: &str, case_id: &str, body: impl FnOnce()) {
    if std::env::var_os(CHILD_FLAG_ENV).as_deref() == Some(std::ffi::OsStr::new(case_id)) {
        body();
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let status = Command::new(exe)
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CHILD_FLAG_ENV, case_id)
        .status()
        .expect("spawn isolated FullLate child test");
    assert!(
        status.success(),
        "FullLate E2E child `{case_id}` failed: {status}"
    );
}

struct SendSigintOnDrop(bool);
impl SendSigintOnDrop {
    fn send_now(&mut self) {
        if !self.0 {
            self.0 = true;
            if let Some(pid) = Pid::from_raw(std::process::id() as i32) {
                let _ = kill_process(pid, Signal::INT);
            }
        }
    }
}
impl Drop for SendSigintOnDrop {
    fn drop(&mut self) {
        self.send_now();
    }
}

#[test]
fn full_late_ws_5_context_roles_wait_approval_cancel_resume_e2e() {
    run_isolated_child(
        "full_late_ws_5_context_roles_wait_approval_cancel_resume_e2e",
        "ws-fulllate",
        || {
            let native_loop = locate_native_loop_binary();
            let ws = TempWorkspace::new("pd-fl-ws");
            let server = MockOpenAiServer::start(ws.project.join("fixture.txt"));
            ws.write_config(server.addr);
            ws.write_ask_bash_rules();

            let mut env = pilotdeck_gateway::config::Env::from([
                ("HOME".into(), ws.home.to_string_lossy().into_owned()),
                ("PILOT_HOME".into(), ws.home.to_string_lossy().into_owned()),
                (
                    "PILOTDECK_CONFIG_PATH".into(),
                    ws.config.to_string_lossy().into_owned(),
                ),
                (
                    "PILOTDECK_KERNEL_EXECUTION_PROFILE".into(),
                    "full-late".into(),
                ),
                (
                    "PILOTDECK_KERNEL_LOOP_BIN".into(),
                    native_loop.to_string_lossy().into_owned(),
                ),
            ]);
            let _ = &mut env;

            let server_options = ServerOptions {
                telemetry_context: pilotdeck_gateway::telemetry::TelemetryRuntimeContext {
                    installation_id: "fl-e2e".into(),
                    instance_id: "fl-e2e-1".into(),
                    deployment_mode: "test".into(),
                    commit_hash: "0".into(),
                    app_version: "0".into(),
                    platform: "unix".into(),
                },
                telemetry_enabled: false,
                pilot_home: ws.home.clone(),
                project: ws.project.clone(),
                os_home: Some(ws.home.clone()),
                host: Some("127.0.0.1".into()),
                port: 0.0,
                allow_remote_host: false,
                token: Some(WS_TOKEN.into()),
                token_path: ws.home.join("server-token"),
                sidecar: ws.root.join("unused"),
                env,
                builtin_skills_root: None,
                builtin_plugins_root: None,
            };

            let project_str = ws.project.to_string_lossy().into_owned();
            let stall_next = server.stall_next.clone();
            let client_slot = Arc::new(Mutex::new(None::<std::thread::JoinHandle<String>>));
            let client_out = client_slot.clone();

            serve(server_options, move |listening: &Listening| {
                let ws_url = listening.ws_url.clone();
                let project_key = project_str.clone();
                let stall_flag = stall_next.clone();
                let handle = std::thread::spawn(move || {
                    let mut sigint = SendSigintOnDrop(false);
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let session_key = rt.block_on(async move {
                        let mut control = ws_connect_and_hello(&ws_url, WS_TOKEN).await;
                        let created = ws_request(
                            &mut control,
                            "new_session",
                            json!({ "projectKey": project_key, "channelKey": "api_server" }),
                        )
                        .await;
                        let session_key = created["sessionKey"].as_str().unwrap().to_owned();

                        let mut query = ws_connect_and_hello(&ws_url, WS_TOKEN).await;
                        let _ = ws_request(
                            &mut query,
                            "resume_session",
                            json!({ "sessionKey": session_key }),
                        )
                        .await;

                        // 1. Approved tool + PrepareForModel + ApplyToolResults + CaptureTurn.
                        let t1 = ws_submit_and_collect(
                            &mut query,
                            &session_key,
                            &project_key,
                            "TRIGGER_READ_FILE",
                            false,
                        )
                        .await;
                        assert!(serde_json::to_string(&t1).unwrap().contains(FILE_MARKER));

                        // 2. Wait / Approval: deny then allow.
                        for (prompt, decision) in [
                            ("TRIGGER_DENIED_BASH", "deny"),
                            ("TRIGGER_ALLOW_BASH", "allow"),
                        ] {
                            let submit_id = uuid::Uuid::new_v4().to_string();
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
                                        "message": prompt,
                                        "canPrompt": true,
                                        "canElicit": false,
                                        "sdkSessionConfig": {},
                                        "runId": uuid::Uuid::new_v4().to_string(),
                                    }
                                }),
                            )
                            .await;

                            let mut settled = Vec::new();
                            let mut decided = false;
                            loop {
                                let frame = ws_recv_json(&mut query).await;
                                if frame["type"] == "event" && frame["id"] == submit_id {
                                    let ev = &frame["event"];
                                    if !decided && ev["type"] == "permission_request" {
                                        let reqs = ws_request(
                                            &mut control,
                                            "permission_list",
                                            json!({ "sessionKey": session_key }),
                                        )
                                        .await;
                                        let req_id = reqs["requests"][0]["requestId"]
                                            .as_str()
                                            .expect("pending requestId")
                                            .to_owned();
                                        let res = ws_request(
                                            &mut control,
                                            "permission_decide",
                                            json!({
                                                "sessionKey": session_key,
                                                "requestId": req_id,
                                                "decision": decision,
                                                "reason": "operator decision",
                                            }),
                                        )
                                        .await;
                                        assert_eq!(res["delivered"], true);
                                        decided = true;
                                    }
                                    settled.push(ev.clone());
                                    if frame["final"] == true {
                                        break;
                                    }
                                }
                            }
                            assert!(decided, "permission_request did not trigger for {prompt}");
                            let joined = serde_json::to_string(&settled).unwrap();
                            if decision == "deny" {
                                assert!(joined.contains("FULL_LATE_DENY_HANDLED"));
                            } else {
                                assert!(joined.contains("FULL_LATE_ALLOW_HANDLED"));
                            }
                        }

                        // 3. Overflow recovery (RecoverFromModelError).
                        let t4 = ws_submit_and_collect(
                            &mut query,
                            &session_key,
                            &project_key,
                            "TRIGGER_OVERFLOW",
                            false,
                        )
                        .await;
                        assert!(
                            serde_json::to_string(&t4)
                                .unwrap()
                                .contains("FULL_LATE_RECOVERY_CONFIRMED")
                        );

                        // 4. Cancel / Interrupt + Resume on the same sessionKey.
                        stall_flag.store(true, Ordering::SeqCst);
                        let abort_submit_id = uuid::Uuid::new_v4().to_string();
                        ws_send_json(
                            &mut query,
                            &json!({
                                "type": "request",
                                "id": abort_submit_id,
                                "method": "submit_turn",
                                "params": {
                                    "sessionKey": session_key,
                                    "channelKey": "api_server",
                                    "projectKey": project_key,
                                    "message": "TRIGGER_ABORT_TURN",
                                    "canPrompt": false,
                                    "canElicit": false,
                                    "sdkSessionConfig": {},
                                    "runId": uuid::Uuid::new_v4().to_string(),
                                }
                            }),
                        )
                        .await;
                        let mut aborted = false;
                        loop {
                            let frame = ws_recv_json(&mut query).await;
                            if frame["type"] == "event" && frame["id"] == abort_submit_id {
                                if !aborted
                                    && frame["event"]
                                        .to_string()
                                        .contains("STREAMING_BEFORE_ABORT")
                                {
                                    let _ = ws_request(
                                        &mut control,
                                        "abort_turn",
                                        json!({
                                            "sessionKey": session_key,
                                            "reason": "e2e_cancel",
                                        }),
                                    )
                                    .await;
                                    aborted = true;
                                }
                                if frame["final"] == true {
                                    break;
                                }
                            }
                        }
                        assert!(aborted, "expected mid-stream abort_turn");

                        let t6 = ws_submit_and_collect(
                            &mut query,
                            &session_key,
                            &project_key,
                            "RESUME_AFTER_ABORT",
                            false,
                        )
                        .await;
                        assert!(
                            serde_json::to_string(&t6)
                                .unwrap()
                                .contains("FULL_LATE_RESUMED_OK")
                        );

                        let _ = query.close(None).await;
                        let _ = control.close(None).await;
                        session_key
                    });
                    sigint.send_now();
                    session_key
                });
                *client_out.lock().unwrap() = Some(handle);
            })
            .expect("FullLate server clean SIGINT exit 0");

            let session_key = client_slot.lock().unwrap().take().unwrap().join().unwrap();
            let rec = load_session_owner(&ws.home, &session_key)
                .unwrap()
                .expect("session pinned in ownership ledger");
            assert_eq!(rec.owner, BackendOwner::KernelFullLate);
            server.shutdown();
        },
    );
}

#[test]
fn full_late_local_gateway_and_channel_auto_discovery_e2e() {
    let native_loop = locate_native_loop_binary();
    let ws = TempWorkspace::new("pd-fl-loc");
    let server = MockOpenAiServer::start(ws.project.join("fixture.txt"));
    ws.write_config(server.addr);

    let env = pilotdeck_gateway::config::Env::from([
        ("HOME".into(), ws.home.to_string_lossy().into_owned()),
        ("PILOT_HOME".into(), ws.home.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH".into(),
            ws.config.to_string_lossy().into_owned(),
        ),
        (
            "PILOTDECK_KERNEL_LOOP_BIN".into(),
            native_loop.to_string_lossy().into_owned(),
        ),
    ]);

    let gateway = LocalGateway::start(LocalOptions {
        pilot_home: ws.home.clone(),
        project: ws.project.clone(),
        os_home: Some(ws.home.clone()),
        sidecar: ws.root.join("unused"),
        env,
        builtin_skills_root: None,
        builtin_plugins_root: None,
        telemetry_context: None,
    })
    .expect("LocalGateway auto-discovers FullLate without explicit profile env");

    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    gateway
        .submit_turn(
            parse_json(
                r#"{"sessionKey":"fl-local-chan","channelKey":"feishu","message":"TRIGGER_READ_FILE"}"#,
            )
            .unwrap(),
            Arc::new(move |ev| sink.lock().unwrap().push(ev)),
        )
        .expect("FullLate channel submit_turn");

    let count = AtomicUsize::new(events.lock().unwrap().len());
    assert!(count.load(Ordering::SeqCst) > 0);
    let rec = load_session_owner(&ws.home, "fl-local-chan")
        .unwrap()
        .expect("session ownership pinned");
    assert_eq!(rec.owner, BackendOwner::KernelFullLate);

    gateway
        .shutdown(Duration::from_secs(15))
        .expect("leak-free FullLate LocalGateway shutdown");
    server.shutdown();
}
