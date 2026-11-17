#![cfg(all(unix, feature = "kernel-resident"))]

use pilotdeck_gateway::native_process::{BackendOwner, load_session_owner};
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
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(60);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(120);
const FILE_MARKER: &str = "CLI_DEFAULT_FULLLATE_MARKER_V1";
const ASSISTANT_CONFIRMATION: &str = "CLI_DEFAULT_CONFIRMED: CLI_DEFAULT_FULLLATE_MARKER_V1";

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
    let cli_bin = PathBuf::from(env!("CARGO_BIN_EXE_pilotdeck"));
    let sibling = cli_bin
        .parent()
        .map(|dir| dir.join("pilotdeck-native-loop"))
        .expect("cli target dir");
    assert!(
        sibling.is_file(),
        "pilotdeck-native-loop not found beside {}; set PILOTDECK_IFACE_NATIVE_LOOP_BIN",
        cli_bin.display()
    );
    sibling
}

struct StagedCliWorkspace {
    root: PathBuf,
    bin_dir: PathBuf,
    staged_cli: PathBuf,
    home: PathBuf,
    pilot_home: PathBuf,
    project: PathBuf,
    tmp_dir: PathBuf,
    config_path: PathBuf,
}

impl StagedCliWorkspace {
    fn new(prefix: &str, native_loop_bin: &Path) -> Self {
        let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let raw_root = PathBuf::from(format!("/tmp/{prefix}-{short}"));
        fs::create_dir_all(&raw_root).unwrap();
        let root = fs::canonicalize(&raw_root).unwrap_or(raw_root);
        let bin_dir = root.join("bin");
        let home = root.join("home");
        let pilot_home = root.join("pilot");
        let project = root.join("project");
        let tmp_dir = root.join("tmp");
        for dir in [&bin_dir, &home, &pilot_home, &project, &tmp_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        let staged_cli = bin_dir.join("pilotdeck");
        let staged_loop = bin_dir.join("pilotdeck-native-loop");
        let real_cli = PathBuf::from(env!("CARGO_BIN_EXE_pilotdeck"));
        fs::copy(&real_cli, &staged_cli).unwrap();
        fs::copy(native_loop_bin, &staged_loop).unwrap();

        fs::write(project.join("fixture.txt"), format!("{FILE_MARKER}\n")).unwrap();
        let chats =
            pilotdeck_gateway::session_chat_dir(&pilot_home, &project, &project.to_string_lossy());
        fs::create_dir_all(chats.parent().unwrap()).unwrap();
        fs::write(
            chats.parent().unwrap().join(".cwd"),
            project.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let config_path = pilot_home.join("pilotdeck.yaml");
        Self {
            root,
            bin_dir,
            staged_cli,
            home,
            pilot_home,
            project,
            tmp_dir,
            config_path,
        }
    }

    fn write_config(&self, openai_addr: SocketAddr) {
        fs::write(
            &self.config_path,
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
    }

    /// Clean environment with ZERO `PILOTDECK_KERNEL_*` overrides so P7.default auto-discovers
    /// `bin_dir/pilotdeck-native-loop` next to `bin_dir/pilotdeck`.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.staged_cli);
        cmd.env_clear()
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("PILOT_HOME", &self.pilot_home)
            .env("PILOTDECK_CONFIG_PATH", &self.config_path)
            .env("TMPDIR", &self.tmp_dir)
            .env("LANG", "en_US.UTF-8")
            .env("PATH", "/usr/bin:/bin");
        let _ = &self.bin_dir;
        cmd
    }
}

impl Drop for StagedCliWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct MockOpenAiServer {
    addr: SocketAddr,
    request_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockOpenAiServer {
    fn start(expected_file: PathBuf) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let req_ref = request_count.clone();
        let stop_ref = stop.clone();
        let file_str = expected_file.to_string_lossy().into_owned();
        let thread = std::thread::spawn(move || {
            while !stop_ref.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_http(stream, &file_str, &req_ref),
                    Err(_) => {
                        // WouldBlock (empty accept queue) is the steady state, and
                        // transient errors must be retried too: under parallel load a
                        // queued client connection can be reset before accept
                        // (ECONNABORTED) or fd pressure can surface EMFILE. Breaking
                        // here would drop the listener and fail every later model
                        // request with `fetch failed`.
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        });
        Self {
            addr,
            request_count,
            stop,
            thread: Some(thread),
        }
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
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

/// `None` signals an aborted connection or malformed chunk framing: the mock
/// must drop that request without panicking, because the accept loop runs
/// `handle_http` inline and a panic would tear down the whole listener.
fn read_chunked_body(reader: &mut BufReader<TcpStream>) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line).ok()?;
        let hex_len = size_line.trim().split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(hex_len, 16).ok()?;
        if chunk_len == 0 {
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }
        let mut chunk = vec![0u8; chunk_len];
        reader.read_exact(&mut chunk).ok()?;
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).ok()?;
    }
    Some(body)
}

fn handle_http(mut stream: TcpStream, expected_file: &str, request_count: &AtomicUsize) {
    let _ = stream.set_read_timeout(Some(WAIT));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
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
        let Some(body) = read_chunked_body(&mut reader) else {
            return;
        };
        body
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        buf
    } else {
        Vec::new()
    };
    let body: Value = serde_json::from_slice(&raw_body).unwrap_or(Value::Null);
    if body.to_string().contains("Generate a concise title") || body["stream"] != true {
        let payload = json!({
            "id": "cli-title",
            "object": "chat.completion",
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "{\"title\":\"CliDefault\"}" },
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
    request_count.fetch_add(1, Ordering::SeqCst);
    let current_turn = body["messages"]
        .as_array()
        .and_then(|msgs| {
            let idx = msgs.iter().rposition(|m| m["role"] == "user")?;
            serde_json::to_string(&msgs[idx..]).ok()
        })
        .unwrap_or_else(|| body.to_string());
    let frames = if !current_turn.contains(FILE_MARKER) && !current_turn.contains("Follow up") {
        let args = json!({ "file_path": expected_file }).to_string();
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
                            "id": "call_cli_read_1",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": args }
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "cli-step-1",
                "model": "mock",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
            }),
        ]
    } else {
        vec![
            json!({
                "id": "cli-step-2",
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "delta": { "role": "assistant", "content": ASSISTANT_CONFIRMATION },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "cli-step-2",
                "model": "mock",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
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
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

struct ChildProcessGroup {
    child: Option<Child>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl ChildProcessGroup {
    fn spawn_server(mut cmd: Command) -> (Self, mpsc::Receiver<String>) {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn staged pilotdeck server");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let stdout_thread = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let stderr_thread =
            std::thread::spawn(
                move || {
                    for _ in BufReader::new(stderr).lines().map_while(Result::ok) {}
                },
            );
        (
            Self {
                child: Some(child),
                stdout_thread: Some(stdout_thread),
                stderr_thread: Some(stderr_thread),
            },
            rx,
        )
    }

    fn shutdown_sigint(mut self) -> i32 {
        let mut child = self.child.take().unwrap();
        if let Some(pid) = Pid::from_raw(child.id() as i32) {
            let _ = kill_process(pid, Signal::INT);
        }
        let deadline = Instant::now() + WAIT;
        let code = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            assert!(
                Instant::now() < deadline,
                "server SIGINT shutdown timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        code
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
    let mut child = cmd.spawn().expect("spawn CLI");
    let pid = Pid::from_raw(child.id() as i32);
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            if let Some(pid) = pid {
                let _ = kill_process_group(pid, Signal::KILL);
            }
            panic!("command timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cli_one_shot_auto_discovers_sibling_native_loop_without_env_vars() {
    let native_loop = locate_native_loop_binary();
    let ws = StagedCliWorkspace::new("pd-cli-dflt", &native_loop);
    let http = MockOpenAiServer::start(ws.project.join("fixture.txt"));
    ws.write_config(http.addr);

    let mut cmd = ws.base_command();
    cmd.arg("Read fixture.txt using read_file and echo marker.");
    let out = run_command_bounded(cmd, PROCESS_TIMEOUT);
    assert_eq!(
        out.status.code(),
        Some(0),
        "staged CLI default FullLate failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(ASSISTANT_CONFIRMATION),
        "missing assistant confirmation on stdout: {stdout}"
    );

    let session_key =
        pilotdeck_gateway::channel::cli::default_session_key(&ws.project.to_string_lossy());
    let rec = load_session_owner(&ws.pilot_home, &session_key)
        .unwrap()
        .expect("default CLI session pinned in ownership ledger");
    assert_eq!(rec.owner, BackendOwner::KernelFullLate);

    // Explicit `legacy` must refuse to mix with the KernelFullLate-pinned session.
    // CliChannel one-shot preserves baseline stream error behavior (exit 0 + rendered
    // `[error:gateway_submit_failed]` containing `MIGRATION_OWNERSHIP_CONFLICT`, zero model calls).
    let before_legacy_reqs = http.request_count();
    let mut legacy_cmd = ws.base_command();
    legacy_cmd
        .arg("Follow up on pinned session")
        .env("PILOTDECK_KERNEL_EXECUTION_PROFILE", "legacy");
    let legacy_out = run_command_bounded(legacy_cmd, PROCESS_TIMEOUT);
    let legacy_stdout = String::from_utf8_lossy(&legacy_out.stdout);
    let legacy_stderr = String::from_utf8_lossy(&legacy_out.stderr);
    assert_eq!(
        legacy_out.status.code(),
        Some(0),
        "CliChannel one-shot expected baseline exit 0 on error event:\nstdout={legacy_stdout}\nstderr={legacy_stderr}"
    );
    assert!(
        legacy_stdout.contains("[error:gateway_submit_failed]")
            && legacy_stdout.contains("MIGRATION_OWNERSHIP_CONFLICT")
            && legacy_stdout.contains("cannot route or reassign to legacy_runtime")
            && !legacy_stdout.contains(ASSISTANT_CONFIRMATION),
        "legacy override did not fail closed with MIGRATION_OWNERSHIP_CONFLICT:\nstdout={legacy_stdout}\nstderr={legacy_stderr}"
    );
    assert_eq!(
        http.request_count(),
        before_legacy_reqs,
        "legacy override must not execute any model request on a KernelFullLate-pinned session"
    );
    assert_eq!(
        load_session_owner(&ws.pilot_home, &session_key)
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::KernelFullLate
    );

    http.shutdown();
}

fn spawn_server_ready(srv_cmd: Command) -> (ChildProcessGroup, String, String) {
    let (server, line_rx) = ChildProcessGroup::spawn_server(srv_cmd);
    let mut ws_url = None;
    let mut token_path = None;
    let deadline = Instant::now() + WAIT;
    while ws_url.is_none() || token_path.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "server startup timeout");
        let line = line_rx.recv_timeout(remaining).unwrap();
        if let Some(url) = line.strip_prefix("WebSocket:") {
            ws_url = Some(url.trim().to_owned());
        } else if let Some(path) = line.strip_prefix("Token:") {
            token_path = Some(PathBuf::from(path.trim()));
        }
    }
    let ws_url = ws_url.unwrap();
    let token = fs::read_to_string(token_path.unwrap())
        .unwrap()
        .trim()
        .to_owned();
    (server, ws_url, token)
}

fn parse_last_json_line(stdout: &[u8]) -> Value {
    let text = String::from_utf8_lossy(stdout);
    let json_line = text
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("expected JSON envelope in stdout:\n{text}"));
    serde_json::from_str(json_line)
        .unwrap_or_else(|e| panic!("invalid JSON line `{json_line}`: {e}"))
}

#[test]
fn cli_server_auto_discovers_full_late_runs_sdk_run_resume_and_clean_sigint() {
    let native_loop = locate_native_loop_binary();
    let ws = StagedCliWorkspace::new("pd-cli-sdflt", &native_loop);
    let http = MockOpenAiServer::start(ws.project.join("fixture.txt"));
    ws.write_config(http.addr);
    let project_str = ws.project.to_string_lossy().into_owned();

    let mut srv_cmd = ws.base_command();
    srv_cmd.args(["server", "--port", "0", "--host", "127.0.0.1"]);
    let (server, ws_url, token) = spawn_server_ready(srv_cmd);

    let mut run_cmd = ws.base_command();
    run_cmd.args([
        "run",
        "Read fixture.txt using read_file.",
        "--gateway-url",
        &ws_url,
        "--auth-token",
        &token,
        "--project",
        &project_str,
        "--json",
    ]);
    let run_out = run_command_bounded(run_cmd, PROCESS_TIMEOUT);
    assert_eq!(
        run_out.status.code(),
        Some(0),
        "pilotdeck run failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_json = parse_last_json_line(&run_out.stdout);
    assert_eq!(run_json["result"]["status"], "completed", "{run_json}");
    let session_id = run_json["sessionId"]
        .as_str()
        .expect("run --json sessionId")
        .to_owned();
    assert_eq!(
        load_session_owner(&ws.pilot_home, &session_id)
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::KernelFullLate
    );

    let mut resume_cmd = ws.base_command();
    resume_cmd.args([
        "resume",
        &session_id,
        "Follow up after resume",
        "--gateway-url",
        &ws_url,
        "--auth-token",
        &token,
        "--project",
        &project_str,
        "--json",
    ]);
    let resume_out = run_command_bounded(resume_cmd, PROCESS_TIMEOUT);
    assert_eq!(
        resume_out.status.code(),
        Some(0),
        "pilotdeck resume failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&resume_out.stdout),
        String::from_utf8_lossy(&resume_out.stderr)
    );
    let resume_json = parse_last_json_line(&resume_out.stdout);
    assert_eq!(
        resume_json["result"]["status"], "completed",
        "{resume_json}"
    );

    assert_eq!(server.shutdown_sigint(), 0);

    // Explicit legacy server must refuse resuming a KernelFullLate-pinned session with non-zero exit.
    let mut legacy_srv_cmd = ws.base_command();
    legacy_srv_cmd
        .args(["server", "--port", "0", "--host", "127.0.0.1"])
        .env("PILOTDECK_KERNEL_EXECUTION_PROFILE", "legacy");
    let (legacy_server, legacy_ws_url, legacy_token) = spawn_server_ready(legacy_srv_cmd);

    let before_legacy_reqs = http.request_count();
    let mut legacy_resume_cmd = ws.base_command();
    legacy_resume_cmd.args([
        "resume",
        &session_id,
        "Legacy server must refuse",
        "--gateway-url",
        &legacy_ws_url,
        "--auth-token",
        &legacy_token,
        "--project",
        &project_str,
        "--json",
    ]);
    let legacy_resume_out = run_command_bounded(legacy_resume_cmd, PROCESS_TIMEOUT);
    assert_eq!(
        legacy_resume_out.status.code(),
        Some(1),
        "legacy server resume should exit 1 on MIGRATION_OWNERSHIP_CONFLICT:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&legacy_resume_out.stdout),
        String::from_utf8_lossy(&legacy_resume_out.stderr)
    );
    let legacy_resume_json = parse_last_json_line(&legacy_resume_out.stdout);
    assert_eq!(
        legacy_resume_json["result"]["status"], "failed",
        "{legacy_resume_json}"
    );
    assert_eq!(http.request_count(), before_legacy_reqs);
    assert_eq!(
        load_session_owner(&ws.pilot_home, &session_id)
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::KernelFullLate
    );
    assert_eq!(legacy_server.shutdown_sigint(), 0);
    http.shutdown();
}
