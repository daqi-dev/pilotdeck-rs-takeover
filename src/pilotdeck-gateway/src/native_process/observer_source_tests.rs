//! Actual four-model-round / three-tool native child regression.
//! Pinned original scene and mock input, with isolated home and local provider.
use super::*;
use crate::{
    native_process::{Options, assembly},
    runtime::connection::GatewayTextConnection,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

const SCENE_SHA: &str = "9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca";
const RUNNER_SHA: &str = "b5df7fb4a004d1a730b75e1e61ef30b477bc84f2b0af9b9e7759fa6e13cb1cfd";
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn required_path(name: &str) -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os(name).unwrap_or_else(|| panic!("diagnostic requires {name}")),
    );
    assert!(
        path.is_absolute(),
        "diagnostic path must be absolute: {name}"
    );
    path
}
fn substitute(value: &mut Value, substitutions: &[(&str, String)]) {
    match value {
        Value::String(text) => {
            for (from, to) in substitutions {
                *text = text.replace(from, to);
            }
        }
        Value::Array(values) => {
            for value in values {
                substitute(value, substitutions);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                substitute(value, substitutions);
            }
        }
        _ => {}
    }
}

struct Provider {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Value>>>,
    errors: Arc<Mutex<Vec<String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    changed: watch::Sender<usize>,
}
impl Provider {
    fn start(rules: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let faults = errors.clone();
        let changed = watch::channel(0).0;
        let progress = changed.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => {
                        faults.lock().unwrap().push(error.to_string());
                        break;
                    }
                };
                if let Err(error) = answer(&mut socket, &rules, &captured) {
                    faults.lock().unwrap().push(error);
                }
                progress.send_modify(|n| *n += 1);
            }
        });
        Self {
            address,
            stop,
            requests,
            errors,
            worker: Some(worker),
            changed,
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
fn answer(
    socket: &mut TcpStream,
    rules: &[Value],
    captured: &Mutex<Vec<Value>>,
) -> Result<(), String> {
    // Darwin may inherit O_NONBLOCK from the nonblocking accept listener.
    // The per-connection mock reader/writer intentionally uses blocking I/O.
    socket.set_nonblocking(false).map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let mut block = [0u8; 8192];
    let header_end = loop {
        let n = socket.read(&mut block).map_err(|e| e.to_string())?;
        if n == 0 && bytes.is_empty() {
            return Ok(());
        }
        if n == 0 {
            return Err("mock request headers truncated".into());
        }
        bytes.extend_from_slice(&block[..n]);
        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() > 2_000_000 {
            return Err("mock request too large".into());
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .ok_or("mock requires Content-Length")?;
    if length > 2_000_000 {
        return Err("mock body too large".into());
    }
    while bytes.len() < header_end + length {
        let n = socket.read(&mut block).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("mock body truncated".into());
        }
        bytes.extend_from_slice(&block[..n]);
    }
    let raw = String::from_utf8(bytes[header_end..header_end + length].to_vec())
        .map_err(|e| e.to_string())?;
    let body: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let rule = rules
        .iter()
        .find(|rule| {
            rule["when"]
                .as_array()
                .is_none_or(|terms| terms.iter().all(|s| raw.contains(s.as_str().unwrap())))
                && rule["unless"]
                    .as_array()
                    .is_none_or(|terms| terms.iter().all(|s| !raw.contains(s.as_str().unwrap())))
        })
        .ok_or_else(|| {
            format!(
                "unexpected model request: {}",
                raw.chars().take(120).collect::<String>()
            )
        })?;
    let reply = &rule["reply"];
    let request_index = {
        let mut requests = captured.lock().unwrap();
        let index = requests.len();
        requests.push(json!({"tag":rule["name"],"body":body,"requestRaw":raw}));
        index
    };
    let mut calls = Vec::new();
    if reply["tool"].is_object() {
        calls.push(reply["tool"].clone());
    }
    if let Some(more) = reply["tools"].as_array() {
        calls.extend(more.clone());
    }
    let calls: Vec<Value> = calls.iter().enumerate().map(|(i, tool)| json!({
        "id":tool.get("id").cloned().unwrap_or(json!(format!("call_bbx_{}",i+1))),
        "type":"function", "function":{"name":tool["name"],"arguments":tool["rawArguments"].as_str().map(str::to_owned).unwrap_or_else(||serde_json::to_string(tool.get("arguments").unwrap_or(&json!({}))).unwrap())}
    })).collect();
    let text = reply["text"].as_str().map(str::to_owned).or_else(|| {
        if !calls.is_empty() {
            None
        } else {
            Some("BBX_OK".into())
        }
    });
    let finish = reply["finish"].as_str().unwrap_or(if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    });
    let usage = reply
        .get("usage")
        .cloned()
        .unwrap_or(json!({"prompt_tokens":12,"completion_tokens":4,"total_tokens":16}));
    let stream = if body["stream"] == true {
        let mut deltas = Vec::new();
        if let Some(reasoning) = reply["reasoning"].as_str() {
            deltas.push(json!({"reasoning_content":reasoning}));
        }
        if let Some(text) = &text {
            deltas.push(json!({"content":text}));
        }
        if !calls.is_empty() {
            deltas.push(json!({"tool_calls":calls.iter().enumerate().map(|(i, call)| { let mut c=call.clone();c["index"]=json!(i);c }).collect::<Vec<_>>()}));
        }
        if let Some(first) = deltas.first_mut() {
            first["role"] = json!("assistant");
        }
        let mut frames: Vec<String> = deltas
            .into_iter()
            .map(|delta| {
                format!(
                    "data: {}\n\n",
                    json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
                )
            })
            .collect();
        frames.push(format!(
            "data: {}\n\n",
            json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":usage})
        ));
        frames.push("data: [DONE]\n\n".into());
        Some(frames)
    } else {
        None
    };
    if let Some(frames) = stream {
        // Same three SSE messages as the original BBX OpenAI fixture. Each
        // message is a separate HTTP chunk, with no fabricated model events.
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
        for frame in frames {
            write!(socket, "{:x}\r\n{}\r\n", frame.len(), frame).map_err(|e| e.to_string())?;
            socket.flush().map_err(|e| e.to_string())?;
        }
        socket.write_all(b"0\r\n\r\n").map_err(|e| e.to_string())?;
    } else {
        let mut message = json!({"role":"assistant","content":text});
        if !calls.is_empty() {
            message["tool_calls"] = json!(calls);
        }
        let response = json!({"id":"chatcmpl-bbx","model":body["model"],"choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":usage}).to_string();
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).map_err(|e| e.to_string())?;
    }
    let result = socket.flush().map_err(|e| e.to_string());
    captured.lock().unwrap()[request_index]["responseWritten"] = json!(result.is_ok());
    result
}

struct Text {
    rows: Mutex<Vec<Value>>,
    changed: watch::Sender<usize>,
}
impl Default for Text {
    fn default() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
            changed: watch::channel(0).0,
        }
    }
}
impl GatewayTextConnection for Text {
    fn send_text(&self, text: String) {
        self.rows
            .lock()
            .unwrap()
            .push(serde_json::from_str(&text).unwrap());
        self.changed.send_modify(|n| *n += 1);
    }
    fn close(&self, code: u16, reason: &str) {
        self.rows
            .lock()
            .unwrap()
            .push(json!({"diagnosticClose":code,"reason":reason}));
        self.changed.send_modify(|n| *n += 1);
    }
}
fn output(path: &Path, name: &str, value: &Value) {
    std::fs::write(path.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "actual native fixture requires explicit pinned inputs, Node and owned output"]
async fn actual_four_model_three_tool_observer_keeps_final_assistant() {
    let out = required_path("PILOTDECK_OBSERVER_DIAGNOSTIC_OUT");
    std::fs::create_dir(&out).expect("diagnostic output must be fresh and owned");
    let out = std::fs::canonicalize(out).unwrap();
    let scene_path = required_path("PILOTDECK_OBSERVER_DIAGNOSTIC_SCENE");
    let runner_path = required_path("PILOTDECK_OBSERVER_DIAGNOSTIC_RUNNER");
    let baseline = required_path("PILOTDECK_OBSERVER_DIAGNOSTIC_BASELINE");
    let node = required_path("PILOTDECK_OBSERVER_DIAGNOSTIC_NODE");
    assert!(node.is_file());
    let scene_bytes = std::fs::read(&scene_path).unwrap();
    let runner_bytes = std::fs::read(&runner_path).unwrap();
    assert_eq!(
        hash(&scene_bytes),
        SCENE_SHA,
        "original four-round fixture identity"
    );
    assert_eq!(
        hash(&runner_bytes),
        RUNNER_SHA,
        "original mock framing/default config identity"
    );
    let mut scene: Value = serde_json::from_slice(&scene_bytes).unwrap();
    let root = out.join("root");
    let home = root.join("home");
    let pilot = home.join(".pilotdeck");
    let project = root.join("proj");
    for dir in [&pilot, &project, &root.join("tmp")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    substitute(
        &mut scene,
        &[
            ("{ROOT}", root.display().to_string()),
            ("{HOME}", home.display().to_string()),
            ("{PILOT_HOME}", pilot.display().to_string()),
            ("{PROJECT}", project.display().to_string()),
        ],
    );
    for (relative, content) in scene["files"].as_object().unwrap() {
        let target = root.join(relative);
        assert!(target.starts_with(&root));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(
            target,
            content.as_str().expect("original fork fixture text seeds"),
        )
        .unwrap();
    }
    let provider = Provider::start(scene["provider"].as_array().unwrap().clone());
    let runner = String::from_utf8(runner_bytes).unwrap();
    let default_config = runner
        .split("const DEFAULT_CONFIG = `")
        .nth(1)
        .unwrap()
        .split("`;")
        .next()
        .unwrap();
    let config = scene["config"]
        .as_str()
        .unwrap_or(default_config)
        .replace("{MOCK}", &format!("http://{}", provider.address));
    std::fs::write(pilot.join("pilotdeck.yaml"), config).unwrap();
    let env = [
        ("HOME", home.display().to_string()),
        ("PILOT_HOME", pilot.display().to_string()),
        ("TMPDIR", root.join("tmp").display().to_string()),
        ("TZ", "UTC".into()),
        ("LANG", "en_US.UTF-8".into()),
        (
            "PATH",
            format!("{}:/usr/bin:/bin", node.parent().unwrap().display()),
        ),
        (
            "PILOTDECK_CONFIG_PATH",
            pilot.join("pilotdeck.yaml").display().to_string(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let options = Options {
        home: pilot.clone(),
        project: pilot.clone(),
        project_root: project,
        listen: "127.0.0.1:0".parse().unwrap(),
        token_file: pilot.join("token"),
        sidecar: root.join("absent-sidecar"),
        sidecar_args: Vec::new(),
        drain_budget: Duration::from_secs(10),
        storage_root: None,
        os_home: Some(home),
        builtin_skills_root: Some(baseline.join("skills")),
        builtin_plugins_root: Some(baseline.join("dist/src/extension/plugins/builtin")),
        in_process_cli: false,
        telemetry: None,
        telemetry_context: None,
        telemetry_reload: false,
    };
    let host = assembly::start(options, env).await.unwrap();
    let text = Arc::new(Text::default());
    let connection = Arc::new(host.connect(
        text.clone(),
        "diagnostic-token".into(),
        "diagnostic-not-parity".into(),
    ));
    connection.receive(json!({"type":"hello","protocolVersion":pilotdeck_protocol::GATEWAY_PROTOCOL_VERSION,"token":"diagnostic-token","clientName":"web","clientVersion":"fixture"}).to_string()).await.unwrap();
    let mut changed = text.changed.subscribe();
    let mut provider_changed = provider.changed.subscribe();
    let submit = scene["steps"][0]["params"].clone();
    let submitting = connection.clone();
    let operation = tokio::spawn(async move {
        submitting.receive(json!({"type":"request","id":"diagnostic-submit","method":"submit_turn","params":submit}).to_string()).await
    });
    let mut answered = false;
    let observed = tokio::time::timeout(Duration::from_secs(45),async {
        loop {
            changed.borrow_and_update();
            provider_changed.borrow_and_update();
            let frames = text.rows.lock().unwrap().clone();
            for frame in &frames {
                if !answered && frame["event"]["type"] == "permission_request" {
                    answered = true;
                    connection.receive(json!({"type":"request","id":"diagnostic-answer","method":"permission_decide","params":{"sessionKey":"agent03-parent","requestId":frame["event"]["requestId"],"decision":"allow"}}).to_string()).await.unwrap();
                }
            }
            let completed = frames.iter().any(|f|f["event"]["type"]=="turn_completed");
            let observer = provider.requests.lock().unwrap().iter().any(|r|r["tag"]=="observer" && r["responseWritten"] == true);
            if completed && observer { break; }
            tokio::select! {
                ready = changed.changed() => ready.unwrap(),
                ready = provider_changed.changed() => ready.unwrap(),
            };
        }
    }).await;
    let closed = connection.closed().await;
    let shutdown = host.shutdown(Duration::from_secs(10)).await;
    let operation_abort = operation.abort_handle();
    let submitted = tokio::time::timeout(Duration::from_secs(5), operation).await;
    if submitted.is_err() {
        operation_abort.abort();
    }
    let requests = provider.requests.lock().unwrap().clone();
    let faults = provider.errors.lock().unwrap().clone();
    output(&out, "provider.json", &Value::Array(requests.clone()));
    output(
        &out,
        "gateway.json",
        &Value::Array(text.rows.lock().unwrap().clone()),
    );
    assert!(
        observed.is_ok(),
        "diagnostic did not finish real fixture; retained output {}",
        out.display()
    );
    assert!(closed.is_ok(), "connection cleanup: {closed:?}");
    assert!(shutdown.is_ok(), "gateway cleanup: {shutdown:?}");
    assert!(
        matches!(submitted, Ok(Ok(Ok(())))),
        "submit failed: {submitted:?}"
    );
    assert!(answered, "real child MCP permission was not answered");
    assert!(faults.is_empty(), "mock errors: {faults:?}");
    for tag in [
        "child-alpha",
        "child-beta",
        "child-mcp",
        "child-after-mcp",
        "observer",
        "parent-after",
    ] {
        assert_eq!(
            requests.iter().filter(|r| r["tag"] == tag).count(),
            1,
            "actual fixture stage {tag}"
        );
    }
    let worker_id = requests.iter().find(|r|r["tag"]=="child-alpha").unwrap()["body"]["metadata"]["subagentId"].as_str().unwrap();
    let observer = requests
        .iter()
        .find(|row| row["tag"] == "observer")
        .unwrap();
    let prompt = observer["body"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let digest = prompt
        .split("Activity:\n")
        .nth(1)
        .unwrap()
        .split("\nOutcome:")
        .next()
        .unwrap();
    assert_eq!(
        digest.matches("- assistant response emitted").count(),
        4,
        "all four actual worker assistant yields reach observer before cap"
    );
    assert_eq!(digest.matches("- model request:").count(), 4);
    assert!(
        digest.trim_end().ends_with("- assistant response emitted"),
        "final assistant comes from the collected loop event, after fourth request"
    );
    fn transcripts(root: &Path, name: &str, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                transcripts(&path, name, found);
            } else if path.file_name().and_then(|s| s.to_str()) == Some(name)
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    == Some("subagents")
            {
                found.push(path);
            }
        }
    }
    let mut worker_files = Vec::new();
    transcripts(
        &pilot.join("projects"),
        &format!("{worker_id}.jsonl"),
        &mut worker_files,
    );
    assert_eq!(worker_files.len(), 1, "one real worker durable transcript");
    let durable: Vec<Value> = std::fs::read_to_string(&worker_files[0])
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        durable
            .iter()
            .filter(|event| event["type"] == "assistant_message")
            .count(),
        4,
        "actual completed four-round worker, independently of collector candidates"
    );
    assert!(
        durable
            .iter()
            .any(|event| event["type"] == "turn_result" && event["result"]["type"] == "success")
    );
    let mcp_rows: Vec<Value> = std::fs::read_to_string(pilot.join("agent03-mcp-log.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let calls: Vec<_> = mcp_rows
        .iter()
        .filter(|row| row["event"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1, "real child-only MCP call witness");
    assert_eq!(calls[0]["args"]["text"], "AG03-MCP-CALL");
    assert!(mcp_rows.iter().any(|row| row["event"] == "initialize"));
    assert!(
        mcp_rows.iter().any(|row| row["event"] == "exit"),
        "owned MCP actually exits on teardown"
    );
    let results: Vec<_> = text
        .rows
        .lock()
        .unwrap()
        .iter()
        .filter(|f| f["event"]["event"] == "subagent_tool_result")
        .cloned()
        .collect();
    assert_eq!(results.len(), 3, "actual child tool executions");
    let encoded = serde_json::to_string(&results).unwrap();
    assert!(
        encoded.contains("AG03-ALPHA-CONTENT")
            && encoded.contains("not found. Available skills: agent03-alpha")
            && encoded.contains("AG03-MCP-RESULT:AG03-MCP-CALL")
    );
    eprintln!(
        "actual native observer fixture completed; retained evidence {}",
        out.display()
    );
}

#[test]
fn custom_native_and_external_children_keep_legacy_merged_activity() {
    use crate::runtime::turn::host_factory::Deployment;
    assert!(owns_native_loop_activity(&Deployment::Native, false));
    assert!(!owns_native_loop_activity(&Deployment::Native, true));
    assert!(!owns_native_loop_activity(
        &Deployment::Tcp {
            host: "127.0.0.1".into(),
            port: 1,
            connect_timeout_ms: None
        },
        false
    ));
    assert!(!owns_native_loop_activity(
        &Deployment::Stdio {
            command: "fixture".into(),
            args: vec![],
            cwd: PathBuf::from("/fixture"),
            env: Default::default()
        },
        false
    ));
    let mut legacy = Vec::new();
    for kind in [
        "turn_started",
        "input_accepted",
        "user_prompt_submitted",
        "session_started",
        "pre_tool_use",
        "assistant_message",
    ] {
        collect_observer_activity(
            &mut legacy,
            &JsValue::Object(object([("type", kind.into())])),
        );
    }
    assert_eq!(
        legacy
            .iter()
            .map(|event| text(event.get("type").unwrap().as_string().unwrap()))
            .collect::<Vec<_>>(),
        ["session_started", "pre_tool_use", "assistant_message"]
    );
}
