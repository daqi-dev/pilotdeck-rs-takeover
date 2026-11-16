#![cfg(all(unix, feature = "kernel-resident"))]

use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(30);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const FILE_MARKER: &str = "PILOTDECK_CLI_KERNEL_OPT_IN_MARKER";
const ASSISTANT_CONFIRMATION: &str = "CLI_OPT_IN_CONFIRMED: PILOTDECK_CLI_KERNEL_OPT_IN_MARKER";

struct TempWorkspace {
    root: PathBuf,
    home: PathBuf,
    pilot_home: PathBuf,
    project: PathBuf,
    tmp_dir: PathBuf,
    config_path: PathBuf,
}

impl TempWorkspace {
    fn new(prefix: &str, openai_addr: SocketAddr) -> Self {
        let short_id = uuid::Uuid::new_v4().simple().to_string();
        let raw_root = PathBuf::from("/tmp").join(format!("{prefix}-{}", &short_id[..10]));
        fs::create_dir_all(&raw_root).unwrap();
        let root = fs::canonicalize(&raw_root).unwrap();
        let home = root.join("home");
        let pilot_home = root.join("pilot");
        let project = root.join("project");
        let tmp_dir = root.join("tmp");
        for dir in [&home, &pilot_home, &project, &tmp_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        let chats =
            pilotdeck_gateway::session_chat_dir(&pilot_home, &project, &project.to_string_lossy());
        fs::create_dir_all(chats.parent().unwrap()).unwrap();
        fs::write(
            chats.parent().unwrap().join(".cwd"),
            project.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(project.join("fixture.txt"), format!("{FILE_MARKER}\n")).unwrap();
        let config_path = pilot_home.join("pilotdeck.yaml");
        fs::write(
            &config_path,
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
                openai_addr
            ),
        )
        .unwrap();
        Self {
            root,
            home,
            pilot_home,
            project,
            tmp_dir,
            config_path,
        }
    }

    fn base_command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pilotdeck"));
        cmd.env_clear()
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("PILOT_HOME", &self.pilot_home)
            .env("PILOTDECK_CONFIG_PATH", &self.config_path)
            .env("TMPDIR", &self.tmp_dir)
            .env("LANG", "en_US.UTF-8")
            .env("PATH", "/usr/bin:/bin")
            .env("ANALYTICS_ENABLED", "0")
            .env("PILOTDECK_TELEMETRY_DISABLED", "1");
        cmd
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture_binary() -> PathBuf {
    let executable = PathBuf::from(
        std::env::var_os("PILOTDECK_IFACE_MANAGED_NATIVE_FIXTURE")
            .expect("PILOTDECK_IFACE_MANAGED_NATIVE_FIXTURE must point to built managed_native"),
    );
    assert!(
        executable.is_absolute() && executable.is_file(),
        "invalid fixture path: {}",
        executable.display()
    );
    executable
}

struct MockOpenAiServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockOpenAiServer {
    fn start(expected_file_path: PathBuf) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (observed, stopping) = (requests.clone(), stop.clone());
        let expected = expected_file_path.to_string_lossy().into_owned();
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_http(stream, &expected, &observed),
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
            let _ = thread.join();
        }
    }
}

impl Drop for MockOpenAiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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
            "id": "cli-e2e-title",
            "object": "chat.completion",
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"title\":\"CLI Kernel Opt-In E2E\"}"
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
                "id": "cli-step-1",
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
                "id": "cli-step-1",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
            json!({
                "id": "cli-step-1",
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
                "id": "cli-step-2",
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
                "id": "cli-step-2",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            json!({
                "id": "cli-step-2",
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

struct ChildProcessGroup {
    child: Option<Child>,
    stdout_log: Arc<Mutex<String>>,
    stderr_log: Arc<Mutex<String>>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl ChildProcessGroup {
    fn spawn_server(mut cmd: Command) -> (Self, mpsc::Receiver<String>) {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn pilotdeck server");
        let stdout = child.stdout.take().expect("server stdout");
        let stderr = child.stderr.take().expect("server stderr");

        let stdout_log = Arc::new(Mutex::new(String::new()));
        let stderr_log = Arc::new(Mutex::new(String::new()));
        let (line_tx, line_rx) = mpsc::channel();

        let out_buf = stdout_log.clone();
        let stdout_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']).to_owned();
                        out_buf.lock().unwrap().push_str(&line);
                        let _ = line_tx.send(trimmed);
                    }
                }
            }
        });

        let err_buf = stderr_log.clone();
        let stderr_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => err_buf.lock().unwrap().push_str(&line),
                }
            }
        });

        (
            Self {
                child: Some(child),
                stdout_log,
                stderr_log,
                stdout_thread: Some(stdout_thread),
                stderr_thread: Some(stderr_thread),
            },
            line_rx,
        )
    }

    fn dump_logs(&self) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            self.stdout_log.lock().unwrap(),
            self.stderr_log.lock().unwrap()
        )
    }

    fn send_sigint_and_wait(&mut self, timeout: Duration) -> i32 {
        let Some(child) = self.child.as_mut() else {
            panic!("server process already consumed");
        };
        let pid = Pid::from_raw(child.id() as i32).expect("server pid");
        kill_process(pid, Signal::INT).expect("send SIGINT to pilotdeck server");
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait().expect("poll server exit") {
                Some(status) => break status,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                None => {
                    let _ = kill_process_group(pid, Signal::KILL);
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "pilotdeck server timed out on SIGINT:\n{}",
                        self.dump_logs()
                    );
                }
            }
        };
        self.child = None;
        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        status.code().unwrap_or(-1)
    }
}

impl Drop for ChildProcessGroup {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = Pid::from_raw(child.id() as i32) {
                let _ = kill_process_group(pid, Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
    }
}

fn run_command_bounded(mut cmd: Command, timeout: Duration) -> Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().expect("spawn pilotdeck CLI command");
    let pid = Pid::from_raw(child.id() as i32);
    let stdout = child.stdout.take().expect("cmd stdout");
    let stderr = child.stderr.take().expect("cmd stderr");

    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = BufReader::new(stdout);
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().expect("poll pilotdeck CLI command") {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                if let Some(pid) = pid {
                    let _ = kill_process_group(pid, Signal::KILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                let out = out_handle.join().unwrap_or_default();
                let err = err_handle.join().unwrap_or_default();
                panic!(
                    "CLI command timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                    timeout,
                    String::from_utf8_lossy(&out),
                    String::from_utf8_lossy(&err)
                );
            }
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Output {
        status,
        stdout,
        stderr,
    }
}

fn assert_transcript_exists(pilot_home: &Path, project: &Path) {
    let chats =
        pilotdeck_gateway::session_chat_dir(pilot_home, project, &project.to_string_lossy());
    let jsonl_files: Vec<PathBuf> = fs::read_dir(&chats)
        .unwrap_or_else(|e| panic!("expected chats dir {}: {e}", chats.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    assert!(
        !jsonl_files.is_empty(),
        "expected .jsonl session transcript under {}",
        chats.display()
    );
    let transcript = jsonl_files
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
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
            "transcript missing `{expected}`: {transcript}"
        );
    }
}

#[test]
fn cli_server_plus_sdk_run_and_sessions_through_scoped_resident_kernel_e2e() {
    let fixture = fixture_binary();
    let ws = TempWorkspace::new("pd-cli-srv", "127.0.0.1:0".parse().unwrap());
    let http = MockOpenAiServer::start(ws.project.join("fixture.txt"));
    // Rewrite config with the real bound mock OpenAI port.
    fs::write(
        &ws.config_path,
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

    let mut srv_cmd = ws.base_command();
    srv_cmd
        .args(["server", "--port", "0", "--host", "127.0.0.1"])
        .env("PILOTDECK_KERNEL_EXECUTION_PROFILE", "scoped-resident")
        .env("PILOTDECK_KERNEL_LOOP_BIN", &fixture);

    let (mut server, line_rx) = ChildProcessGroup::spawn_server(srv_cmd);

    let mut ws_url: Option<String> = None;
    let mut token_path: Option<PathBuf> = None;
    let deadline = Instant::now() + WAIT;
    while ws_url.is_none() || token_path.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for server WebSocket/Token output:\n{}",
            server.dump_logs()
        );
        match line_rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(url) = line.strip_prefix("WebSocket: ") {
                    ws_url = Some(url.trim().to_owned());
                } else if let Some(path) = line.strip_prefix("Token: ") {
                    token_path = Some(PathBuf::from(path.trim()));
                }
            }
            Err(_) => {
                panic!(
                    "server stdout closed before WebSocket/Token:\n{}",
                    server.dump_logs()
                );
            }
        }
    }
    let ws_url = ws_url.unwrap();
    let token_path = token_path.unwrap();
    let auth_token = fs::read_to_string(&token_path)
        .unwrap_or_else(|e| panic!("failed reading {}: {e}", token_path.display()))
        .trim()
        .to_owned();
    assert!(!auth_token.is_empty());

    let project_str = ws.project.to_string_lossy().into_owned();
    let mut run_cmd = ws.base_command();
    run_cmd.args([
        "run",
        "Read fixture.txt using read_file and echo the marker.",
        "--gateway-url",
        &ws_url,
        "--auth-token",
        &auth_token,
        "--project",
        &project_str,
        "--json",
    ]);
    let run_out = run_command_bounded(run_cmd, PROCESS_TIMEOUT);
    assert_eq!(
        run_out.status.code(),
        Some(0),
        "pilotdeck run failed:\nstdout: {}\nstderr: {}\nserver:\n{}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr),
        server.dump_logs()
    );
    let run_stdout = String::from_utf8_lossy(&run_out.stdout);
    let run_json_line = run_stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("expected JSON envelope in run stdout:\n{run_stdout}"));
    let run_json: Value = serde_json::from_str(run_json_line).unwrap();
    let sid = run_json["sessionId"]
        .as_str()
        .expect("run JSON sessionId")
        .to_owned();
    assert_eq!(run_json["result"]["status"], "completed");
    let output_text = run_json["result"]["output"]
        .as_str()
        .expect("run JSON output string");
    assert!(
        output_text.contains(ASSISTANT_CONFIRMATION),
        "run JSON output missing confirmation: {run_json}"
    );

    let recorded = http.requests();
    assert_eq!(
        recorded.len(),
        2,
        "expected 2 model calls (tool request + final answer), got: {recorded:?}"
    );
    let second_body = serde_json::to_string(&recorded[1]["body"]).unwrap();
    assert!(
        second_body.contains(FILE_MARKER),
        "second OpenAI request missing read_file content marker: {second_body}"
    );

    let mut list_cmd = ws.base_command();
    list_cmd.args([
        "sessions",
        "list",
        "--gateway-url",
        &ws_url,
        "--auth-token",
        &auth_token,
        "--project",
        &project_str,
        "--json",
    ]);
    let list_out = run_command_bounded(list_cmd, PROCESS_TIMEOUT);
    assert_eq!(
        list_out.status.code(),
        Some(0),
        "pilotdeck sessions list failed: {}",
        String::from_utf8_lossy(&list_out.stderr)
    );
    let sessions_json: Value = serde_json::from_slice(&list_out.stdout).unwrap();
    let sessions = sessions_json.as_array().expect("sessions array");
    let sid_suffix = sid.rsplit(':').next().unwrap_or(&sid);
    assert!(
        sessions.iter().any(|item| {
            item["sessionId"]
                .as_str()
                .is_some_and(|id| id == sid || id.ends_with(sid_suffix))
        }),
        "sessions list missing {sid}: {sessions_json}"
    );

    let mut msg_cmd = ws.base_command();
    msg_cmd.args([
        "sessions",
        "messages",
        &sid,
        "--gateway-url",
        &ws_url,
        "--auth-token",
        &auth_token,
        "--project",
        &project_str,
        "--json",
    ]);
    let msg_out = run_command_bounded(msg_cmd, PROCESS_TIMEOUT);
    assert_eq!(
        msg_out.status.code(),
        Some(0),
        "pilotdeck sessions messages failed: {}",
        String::from_utf8_lossy(&msg_out.stderr)
    );
    let messages_json: Value = serde_json::from_slice(&msg_out.stdout).unwrap();
    assert!(!messages_json.as_array().expect("messages array").is_empty());
    let messages_blob = serde_json::to_string(&messages_json).unwrap();
    assert!(
        messages_blob.contains(ASSISTANT_CONFIRMATION),
        "sessions messages missing assistant confirmation: {messages_blob}"
    );
    assert!(
        messages_blob.contains(FILE_MARKER),
        "sessions messages missing tool marker: {messages_blob}"
    );

    let server_exit = server.send_sigint_and_wait(Duration::from_secs(20));
    assert_eq!(
        server_exit,
        0,
        "pilotdeck server did not exit 0 cleanly on SIGINT:\n{}",
        server.dump_logs()
    );

    assert_transcript_exists(&ws.pilot_home, &ws.project);
    http.shutdown();
}

#[test]
fn cli_local_in_process_turn_through_scoped_resident_kernel_e2e() {
    let fixture = fixture_binary();
    let ws = TempWorkspace::new("pd-cli-loc", "127.0.0.1:0".parse().unwrap());
    let http = MockOpenAiServer::start(ws.project.join("fixture.txt"));
    fs::write(
        &ws.config_path,
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

    let mut cmd = ws.base_command();
    cmd.arg("Read fixture.txt using read_file and echo the marker.")
        .env("PILOTDECK_KERNEL_EXECUTION_PROFILE", "scoped-resident")
        .env("PILOTDECK_KERNEL_LOOP_BIN", &fixture);

    let out = run_command_bounded(cmd, PROCESS_TIMEOUT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "pilotdeck local turn failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("using in-process mode"),
        "expected CliChannel in-process fallback notice on stderr:\n{stderr}"
    );
    assert!(
        stdout.contains(ASSISTANT_CONFIRMATION),
        "local CLI stdout missing assistant confirmation:\n{stdout}"
    );

    let recorded = http.requests();
    assert_eq!(
        recorded.len(),
        2,
        "expected 2 model calls (tool request + final answer), got: {recorded:?}"
    );
    let second_body = serde_json::to_string(&recorded[1]["body"]).unwrap();
    assert!(
        second_body.contains(FILE_MARKER),
        "second OpenAI request missing read_file content marker: {second_body}"
    );

    assert_transcript_exists(&ws.pilot_home, &ws.project);
    http.shutdown();
}

#[test]
fn cli_opt_in_fails_closed_on_unknown_profile_or_missing_loop_bin() {
    let ws = TempWorkspace::new("pd-cli-fc", "127.0.0.1:0".parse().unwrap());
    let http = MockOpenAiServer::start(ws.project.join("fixture.txt"));
    fs::write(
        &ws.config_path,
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

    // 1. Unknown profile on both `pilotdeck server` and local `pilotdeck "<prompt>"`.
    for (label, args) in [
        (
            "server-unknown",
            vec!["server", "--port", "0", "--host", "127.0.0.1"],
        ),
        ("local-unknown", vec!["hello"]),
    ] {
        let mut cmd = ws.base_command();
        cmd.args(&args)
            .env("PILOTDECK_KERNEL_EXECUTION_PROFILE", "bogus-profile");
        let out = run_command_bounded(cmd, Duration::from_secs(20));
        assert_ne!(
            out.status.code(),
            Some(0),
            "{label} must fail closed instead of falling back"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("PILOTDECK_KERNEL_EXECUTION_PROFILE"),
            "{label} missing explicit profile error on stderr: {stderr}"
        );
    }

    // 2. full-late profile without explicit ResidentProductInputs fails closed.
    for (label, args) in [
        (
            "server-full-late",
            vec!["server", "--port", "0", "--host", "127.0.0.1"],
        ),
        ("local-full-late", vec!["hello"]),
    ] {
        let mut cmd = ws.base_command();
        cmd.args(&args)
            .env("PILOTDECK_KERNEL_EXECUTION_PROFILE", "full-late");
        let out = run_command_bounded(cmd, Duration::from_secs(20));
        assert_ne!(out.status.code(), Some(0), "{label} must fail closed");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("FullLate"),
            "{label} missing FullLate error on stderr: {stderr}"
        );
    }

    // 3. scoped-resident with missing PILOTDECK_KERNEL_LOOP_BIN fails closed on both entries.
    for (label, args) in [
        (
            "server-missing-bin",
            vec!["server", "--port", "0", "--host", "127.0.0.1"],
        ),
        ("local-missing-bin", vec!["hello"]),
    ] {
        let mut cmd = ws.base_command();
        cmd.args(&args)
            .env("PILOTDECK_KERNEL_EXECUTION_PROFILE", "scoped-resident");
        let out = run_command_bounded(cmd, Duration::from_secs(20));
        assert_ne!(out.status.code(), Some(0), "{label} must fail closed");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("PILOTDECK_KERNEL_LOOP_BIN"),
            "{label} missing PILOTDECK_KERNEL_LOOP_BIN error on stderr: {stderr}"
        );
    }

    assert!(
        http.requests().is_empty(),
        "fail-closed opt-in failures must never fall back to legacy model runtime"
    );
    http.shutdown();
}
