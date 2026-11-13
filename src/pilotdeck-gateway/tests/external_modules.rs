//! Independent consumers of production LocalGateway assembly, using only owned
//! HTTP peers. Contract: fixed cc7c4e6 external-session-e2e plus its real wire.
//! No adapter is injected and no private assembly or test-only guard is bypassed.
use pilotdeck_gateway::native_process::{LocalGateway, LocalOptions};
use pilotdeck_protocol::{JsValue, parse_json, stringify_json};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const FINAL: &str = "Five configured HTTP slots completed.";
const CHILD: &str = "EXTERNAL-CHILD-DIRECTIVE";
const TOOL_TEXT: &str = "remote lookup result from owned HTTP module";
const SKILL_TEXT: &str = "# Approval Guide\nUse a citation for every approval.";
const CONTEXT_TEXT: &str = "EXTERNAL-CONTEXT-SELECTED";

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// LocalGateway accepts an explicit environment, but the production ledger lock
// also uses std::env::temp_dir(). Run the same test in an owned child rather than
// mutating process-global environment while other Rust tests run concurrently.
fn isolated(name: &str, body: fn(&Path)) {
    if std::env::var("PDRS_EXTERNAL_MODULE_CASE").ok().as_deref() == Some(name) {
        let root = PathBuf::from(std::env::var_os("PDRS_EXTERNAL_MODULE_ROOT").unwrap());
        assert!(root.is_absolute());
        for key in [
            "HOME",
            "PILOT_HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "XDG_CACHE_HOME",
        ] {
            let p = PathBuf::from(std::env::var_os(key).unwrap());
            assert!(p.starts_with(&root), "{key} must be owned by this case");
        }
        body(&root);
        return;
    }
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pdrs-external-e2e-{}", uuid::Uuid::new_v4())));
    for p in ["home/.pilotdeck", "tmp", "cache", "project"] {
        std::fs::create_dir_all(scratch.0.join(p)).unwrap();
    }
    let stdout = std::fs::File::create(scratch.0.join("stdout.log")).unwrap();
    let stderr = std::fs::File::create(scratch.0.join("stderr.log")).unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env("PDRS_EXTERNAL_MODULE_CASE", name)
        .env("PDRS_EXTERNAL_MODULE_ROOT", &scratch.0)
        .env("HOME", scratch.0.join("home"))
        .env("PILOT_HOME", scratch.0.join("home/.pilotdeck"))
        .env("TMPDIR", scratch.0.join("tmp"))
        .env("TMP", scratch.0.join("tmp"))
        .env("TEMP", scratch.0.join("tmp"))
        .env("XDG_CACHE_HOME", scratch.0.join("cache"))
        .env("PATH", "/usr/bin:/bin")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // Preserve only dynamic loader search paths needed to execute this same
    // test binary; no provider, credential, proxy or user config environment.
    for key in [
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("owned external-module test timed out: {name}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = std::fs::read_to_string(scratch.0.join("stdout.log")).unwrap();
    let stderr = std::fs::read_to_string(scratch.0.join("stderr.log")).unwrap();
    println!("{stdout}");
    assert!(status.success(), "{name}: {status}\n{stderr}");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    FiveSlots,
    KnowledgeOnly,
    Disabled,
    Denied,
    Disconnect,
    Cancel,
    SkillFailure,
    HookUpdated,
}
#[derive(Clone)]
struct Wire {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
    request_text: String,
    response: Value,
    response_text: String,
    action: &'static str,
}
struct State {
    profile: Profile,
    stop: AtomicBool,
    wires: Mutex<Vec<Wire>>,
    errors: Mutex<Vec<String>>,
    connections: AtomicU64,
    project: PathBuf,
}
struct Mock {
    address: SocketAddr,
    state: Arc<State>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Mock {
    fn start(profile: Profile, project: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(State {
            profile,
            stop: AtomicBool::new(false),
            wires: Mutex::new(vec![]),
            errors: Mutex::new(vec![]),
            connections: AtomicU64::new(0),
            project,
        });
        let owner = state.clone();
        let worker = thread::spawn(move || {
            let mut children = vec![];
            while !owner.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        owner.connections.fetch_add(1, Ordering::SeqCst);
                        let state = owner.clone();
                        children.push(thread::spawn(move || {
                            if let Err(e) = serve(socket, &state) {
                                state.errors.lock().unwrap().push(e);
                            }
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => {
                        owner.errors.lock().unwrap().push(e.to_string());
                        break;
                    }
                }
            }
            for child in children {
                if child.join().is_err() {
                    owner
                        .errors
                        .lock()
                        .unwrap()
                        .push("HTTP worker panicked".into());
                }
            }
        });
        Self {
            address,
            state,
            worker: Some(worker),
        }
    }
    fn wires(&self) -> Vec<Wire> {
        self.state.wires.lock().unwrap().clone()
    }
    fn calls(&self, slot: &str, operation: &str) -> Vec<Wire> {
        self.wires()
            .into_iter()
            .filter(|w| {
                w.method == "POST"
                    && w.path == format!("/call/{slot}")
                    && (slot == "tools" || w.body["payload"]["operation"] == operation)
            })
            .collect()
    }
    fn manifest_count(&self, slot: &str) -> usize {
        self.wires()
            .iter()
            .filter(|w| w.method == "GET" && w.path == format!("/manifest/{slot}"))
            .count()
    }
    fn receipt(&self) -> Value {
        json!({"profile":format!("{:?}",self.state.profile),"connections":self.state.connections.load(Ordering::SeqCst),"wire":self.wires().iter().map(|w|json!({"method":w.method,"path":w.path,"headers":w.headers,"request":w.body,"requestText":w.request_text,"response":w.response,"responseText":w.response_text,"action":w.action})).collect::<Vec<_>>()})
    }
    fn assert_healthy(&self) {
        let errors = self.state.errors.lock().unwrap().clone();
        assert!(errors.is_empty(), "{errors:?}");
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        if thread::panicking() {
            println!("EXTERNAL_MODULE_E2E_FAILURE_WIRE {}", self.receipt());
        }
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn serve(mut socket: TcpStream, state: &State) -> Result<(), String> {
    socket.set_nonblocking(false).map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![];
    let mut chunk = [0; 8192];
    let end = loop {
        let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("HTTP request ended before headers".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("bounded HTTP request exceeded".into());
        }
        if let Some(p) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break p;
        }
    };
    let head = String::from_utf8(bytes[..end].to_vec()).map_err(|e| e.to_string())?;
    let mut lines = head.lines();
    let mut first = lines
        .next()
        .ok_or("missing request line")?
        .split_whitespace();
    let method = first.next().ok_or("missing HTTP method")?.to_owned();
    let path = first.next().ok_or("missing HTTP path")?.to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .map(|v| v.parse::<usize>().map_err(|e| e.to_string()))
        .transpose()?
        .unwrap_or(0);
    if length > 8 * 1024 * 1024 {
        return Err("bounded HTTP body exceeded".into());
    }
    while bytes.len() < end + 4 + length {
        let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("short HTTP body".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[end + 4..end + 4 + length]).map_err(|e| e.to_string())?
    };
    let mut wire = Wire {
        method,
        path,
        headers,
        body,
        request_text: String::from_utf8(bytes[end + 4..end + 4 + length].to_vec())
            .map_err(|e| e.to_string())?,
        response: Value::Null,
        response_text: String::new(),
        action: "response",
    };
    if wire.path.starts_with("/permission-hook?") {
        wire.response = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"key":"approved-only","approved":true}}}});
    } else if wire.path == "/v1/chat/completions" {
        wire.response = native_response(&wire.body, state.profile);
    } else if wire.method == "GET" {
        let slot = wire
            .path
            .strip_prefix("/manifest/")
            .ok_or("unknown manifest route")?;
        let (id, contract, methods) = match slot {
            "model" => (
                "modelProvider",
                "pilotdeck.model/v1",
                vec!["prepare", "stream"],
            ),
            "tools" => ("tools", "pilotdeck.tools/v1", vec!["execute"]),
            "context" => ("context", "pilotdeck.context/v1", vec!["prepare_for_model"]),
            "skills" => ("skills", "pilotdeck.skills/v1", vec!["list", "read"]),
            "knowledge" => ("knowledge", "staffdeck.knowledge/v1", vec!["query"]),
            _ => return Err("unknown slot".into()),
        };
        wire.response = json!({"protocolVersion":"2.0","implementationId":format!("example.{id}"),"contract":contract,"transport":"module-http-v2","methods":methods});
    } else {
        if wire.body["kind"] != "request" || wire.body["method"] != "module_call" {
            return Err("not module-http-v2".into());
        }
        let slot = wire
            .path
            .strip_prefix("/call/")
            .ok_or("unknown module call route")?;
        if slot == "tools" && matches!(state.profile, Profile::Disconnect | Profile::Cancel) {
            wire.action = if state.profile == Profile::Cancel {
                "cancel-held-dispatch"
            } else {
                "disconnect-after-dispatch"
            };
            state.wires.lock().unwrap().push(wire);
            if state.profile == Profile::Cancel {
                while !state.stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            return Ok(());
        }
        let b = &wire.body;
        let payload = &b["payload"];
        if slot == "skills"
            && state.profile == Profile::SkillFailure
            && payload["operation"] == "list"
        {
            wire.response = json!({"kind":"response","messageId":"skill-list-failure","inReplyTo":b["messageId"],"requestId":b["requestId"],"ok":false,"code":"REMOTE_SKILLS_DOWN","error":{"code":"REMOTE_SKILLS_DOWN","message":"owned remote skill list unavailable","retryability":"safe"},"outcome":"failed"});
        } else {
            let result = match slot {
                "skills" if payload["operation"] == "list" => {
                    json!({"result":{"builtin":[],"user":[],"project":[],"projectPath":state.project,"items":[{"slug":"approval-guide","name":"Approval Guide","description":"Approval guidance","skillFile":"/external/approval-guide/SKILL.md"}]}})
                }
                "skills" => {
                    json!({"result":{"content":SKILL_TEXT,"scope":"project","slug":"approval-guide","skill":null}})
                }
                "knowledge" => {
                    json!({"result":{"hits":[{"id":"chunk-1","text":"Approval requires evidence.","citationId":"citation-1"}]}})
                }
                "context" => {
                    json!({"result":{"messages":payload["input"]["messages"],"tools":payload["input"]["tools"],"systemPromptParts":[CONTEXT_TEXT],"diagnostics":[],"boundaries":[]}})
                }
                "tools" => {
                    if state.profile == Profile::HookUpdated {
                        json!({"type":"success","toolCallId":payload["toolCallId"],"toolName":payload["name"],"content":[{"type":"text","text":"HOOK-APPROVED-INPUT-USED"}],"data":payload["arguments"],"startedAt":"2026-09-18T00:00:00.000Z","completedAt":"2026-09-18T00:00:00.001Z"})
                    } else {
                        json!({"type":"success","toolCallId":payload["toolCallId"],"toolName":payload["name"],"content":[{"type":"text","text":TOOL_TEXT}],"startedAt":"2026-09-18T00:00:00.000Z","completedAt":"2026-09-18T00:00:00.001Z"})
                    }
                }
                "model" if payload["operation"] == "prepare" => {
                    json!({"prepared":{"request":payload["request"],"provider":"remote","model":"default"}})
                }
                "model" => json!({"events":model_events(&payload["request"], state.profile)}),
                _ => return Err("unknown call slot".into()),
            };
            wire.response = json!({"kind":"response","messageId":format!("response-{}",b["messageId"].as_str().unwrap_or("missing")),"inReplyTo":b["messageId"],"requestId":b["requestId"],"ok":true,"payload":result});
        }
    }
    let native_stream = wire.path == "/v1/chat/completions" && wire.body["stream"] == true;
    let (mime, body) = if native_stream {
        ("text/event-stream", native_sse(&wire.body, state.profile))
    } else {
        ("application/json", wire.response.to_string())
    };
    wire.response_text = body.clone();
    state.wires.lock().unwrap().push(wire);
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Cancellation can legitimately close its owned socket before this write.
    let _ = socket.write_all(reply.as_bytes());
    Ok(())
}
fn last_user(request: &Value) -> String {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .unwrap_or_default()
}
fn tool_result_present(request: &Value) -> bool {
    request["messages"].to_string().contains("tool_result")
        || request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|m| m["role"] == "tool")
}
fn planned_calls(request: &Value, profile: Profile) -> Vec<(&'static str, &'static str, Value)> {
    if tool_result_present(request)
        || last_user(request).contains("HISTORY-CHECK")
        || profile == Profile::Disabled
    {
        return vec![];
    }
    if matches!(
        profile,
        Profile::Denied | Profile::Disconnect | Profile::Cancel | Profile::HookUpdated
    ) {
        return vec![(
            "external-tool-call",
            "remote_lookup",
            json!({"key":"approval"}),
        )];
    }
    if profile == Profile::KnowledgeOnly {
        return vec![(
            "knowledge-call",
            "knowledge_query",
            json!({"query":"approval evidence"}),
        )];
    }
    let child = last_user(request).contains(CHILD);
    let mut result = vec![
        (
            if child { "child-skill" } else { "parent-skill" },
            "read_skill",
            json!({"skillName":"approval-guide"}),
        ),
        (
            if child {
                "child-lookup"
            } else {
                "parent-lookup"
            },
            "remote_lookup",
            json!({"key":"approval"}),
        ),
        (
            if child {
                "child-knowledge"
            } else {
                "parent-knowledge"
            },
            "knowledge_query",
            json!({"query":"approval evidence"}),
        ),
    ];
    if child {
        result.push((
            "child-native-read",
            "read_file",
            json!({"file_path":"owned.txt"}),
        ));
    } else {
        result.push(("parent-agent","agent",json!({"description":"Inspect owned evidence","prompt":CHILD,"subagent_type":"general-purpose"})));
    }
    result
}
fn model_events(request: &Value, profile: Profile) -> Vec<Value> {
    let calls = planned_calls(request, profile);
    let mut events = vec![
        json!({"type":"request_started","provider":"remote","model":"default"}),
        json!({"type":"message_start","role":"assistant"}),
    ];
    if calls.is_empty() {
        events.push(json!({"type":"text_delta","text":if last_user(request).contains(CHILD) {"Child external modules completed."}else{FINAL}}));
    } else {
        for (id, name, input) in &calls {
            events.push(json!({"type":"tool_call_start","id":id,"name":name}));
            events.push(
                json!({"type":"tool_call_end","toolCall":{"id":id,"name":name,"input":input}}),
            );
        }
    }
    events
        .push(json!({"type":"usage","usage":{"inputTokens":17,"outputTokens":5,"totalTokens":22}}));
    events.push(
        json!({"type":"message_end","finishReason":if calls.is_empty(){"stop"}else{"tool_call"}}),
    );
    events
}
fn native_response(request: &Value, profile: Profile) -> Value {
    let title = request.to_string().contains("Generate a concise title");
    let calls = if title {
        vec![]
    } else {
        planned_calls(request, profile)
    };
    let message = if calls.is_empty() {
        json!({"role":"assistant","content":if title{"{\"title\":\"External modules\"}"}else{FINAL}})
    } else {
        json!({"role":"assistant","tool_calls":calls.iter().map(|(id,name,input)|json!({"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})).collect::<Vec<_>>()})
    };
    json!({"id":"owned-native-response","object":"chat.completion","model":"default","choices":[{"index":0,"message":message,"finish_reason":if calls.is_empty(){"stop"}else{"tool_calls"}}],"usage":{"prompt_tokens":17,"completion_tokens":5,"total_tokens":22}})
}
fn native_sse(request: &Value, profile: Profile) -> String {
    let response = native_response(request, profile);
    let message = &response["choices"][0]["message"];
    let delta = if let Some(calls) = message["tool_calls"].as_array() {
        json!({"tool_calls":calls.iter().enumerate().map(|(i,c)|json!({"index":i,"id":c["id"],"type":"function","function":c["function"]})).collect::<Vec<_>>()})
    } else {
        json!({"content":message["content"]})
    };
    let first = json!({"id":"owned-native-response","object":"chat.completion.chunk","model":"default","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let last = json!({"id":"owned-native-response","object":"chat.completion.chunk","model":"default","choices":[{"index":0,"delta":{},"finish_reason":response["choices"][0]["finish_reason"]}],"usage":response["usage"]});
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    pilot: PathBuf,
    project: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new(root: &Path) -> Self {
        let f = Self {
            root: root.into(),
            home: root.join("home"),
            pilot: root.join("home/.pilotdeck"),
            project: root.join("project"),
            config: root.join("home/.pilotdeck/pilotdeck.yaml"),
        };
        std::fs::write(f.project.join("owned.txt"), "OWNED-NATIVE-READ-EVIDENCE\n").unwrap();
        let chats =
            pilotdeck_gateway::session_chat_dir(&f.pilot, &f.project, &f.project.to_string_lossy());
        std::fs::create_dir_all(chats.parent().unwrap()).unwrap();
        std::fs::write(
            chats.parent().unwrap().join(".cwd"),
            f.project.to_string_lossy().as_bytes(),
        )
        .unwrap();
        f
    }
    fn configure(&self, mock: &Mock, profile: Profile) {
        let endpoint = format!("http://{}", mock.address);
        let mut config = format!(
            "schemaVersion: 1\nagent:\n  model: remote/default\n  maxContextTokens: 65536\n  maxOutputTokens: 4096\nmodel:\n  providers:\n    remote:\n      protocol: openai\n      url: {endpoint}/v1\n      apiKey: owned-mock-only\n      models:\n        default: {{}}\ntelemetry:\n  enabled: false\nmodules:\n  agentLoop: {{enabled: true, provider: pilotdeck}}\n"
        );
        let mut binding = |slot: &str, id: &str, contract: &str, methods: &str| {
            config.push_str(&format!("  {slot}:\n    enabled: true\n    implementationId: example.{id}\n    contract: {contract}\n    transport: module-http-v2\n    endpoint: {endpoint}\n    manifestPath: /manifest/{slot}\n    callPath: /call/{slot}\n    methods: [{methods}]\n"));
        };
        // The modelProvider slot's public path is intentionally /model, just
        // as in the fixed TS test, not a route guessed from the slot name.
        if profile != Profile::KnowledgeOnly {
            binding(
                "modelProvider",
                "modelProvider",
                "pilotdeck.model/v1",
                "prepare, stream",
            );
            config = config
                .replace("/manifest/modelProvider", "/manifest/model")
                .replace("/call/modelProvider", "/call/model");
        }
        if matches!(
            profile,
            Profile::FiveSlots
                | Profile::Denied
                | Profile::Disconnect
                | Profile::Cancel
                | Profile::HookUpdated
        ) {
            config.push_str(&format!("  tools:\n    enabled: true\n    implementationId: example.tools\n    contract: pilotdeck.tools/v1\n    transport: module-http-v2\n    endpoint: {endpoint}\n    manifestPath: /manifest/tools\n    callPath: /call/tools\n    methods: [execute]\n    catalog:\n      - name: remote_lookup\n        description: Look up an approval record.\n        inputSchema: {{type: object}}\n        readOnly: {}\n        concurrencySafe: true\n",profile==Profile::FiveSlots));
        }
        if profile == Profile::FiveSlots {
            config.push_str(&format!("  context:\n    enabled: true\n    implementationId: example.context\n    contract: pilotdeck.context/v1\n    transport: module-http-v2\n    endpoint: {endpoint}\n    manifestPath: /manifest/context\n    callPath: /call/context\n    methods: [prepare_for_model]\n"));
        }
        if matches!(profile, Profile::FiveSlots | Profile::SkillFailure) {
            config.push_str(&format!("  skills:\n    enabled: true\n    implementationId: example.skills\n    contract: pilotdeck.skills/v1\n    transport: module-http-v2\n    endpoint: {endpoint}\n    manifestPath: /manifest/skills\n    callPath: /call/skills\n    methods: [list, read]\n"));
        } else {
            config.push_str("  skills: {enabled: false}\n");
        }
        if matches!(profile, Profile::FiveSlots | Profile::KnowledgeOnly) {
            config.push_str(&format!("  knowledge:\n    enabled: true\n    implementationId: example.knowledge\n    contract: staffdeck.knowledge/v1\n    transport: module-http-v2\n    endpoint: {endpoint}\n    manifestPath: /manifest/knowledge\n    callPath: /call/knowledge\n    methods: [query]\n    tenantId: tenant-owned\n    actorUserId: actor-owned\n    agentId: agent-owned\n"));
        } else {
            config.push_str("  knowledge: {enabled: false}\n");
        }
        assert!(!config.contains("credentialEnv"));
        std::fs::write(&self.config, config).unwrap();
    }
    fn start(&self) -> Result<LocalGateway, pilotdeck_gateway::ProxyError> {
        let mut env = pilotdeck_gateway::config::Env::new();
        for (key, value) in [
            ("HOME", self.home.clone()),
            ("PILOT_HOME", self.pilot.clone()),
            ("PILOTDECK_CONFIG_PATH", self.config.clone()),
            ("TMPDIR", self.root.join("tmp")),
            ("TMP", self.root.join("tmp")),
            ("TEMP", self.root.join("tmp")),
            ("XDG_CACHE_HOME", self.root.join("cache")),
        ] {
            env.insert(key.into(), value.to_string_lossy().into_owned());
        }
        env.insert("PILOTDECK_AGENT_LOOP_TRANSPORT".into(), "native".into());
        // A hook decision remains first even when the ordinary gateway prompt
        // callback later times out. This is per-consumer private environment.
        if self.root.join("hook-permission-timeout").is_file() {
            env.insert("PILOTDECK_PERMISSION_TIMEOUT_MS".into(), "20".into());
        }
        LocalGateway::start(LocalOptions {
            pilot_home: self.pilot.clone(),
            project: self.project.clone(),
            os_home: Some(self.home.clone()),
            sidecar: self.root.join("absent-sidecar-not-used"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
            telemetry_context: None,
        })
    }
    fn transcript(&self, session: &str) -> Vec<Value> {
        let path = pilotdeck_gateway::session_chat_dir(
            &self.pilot,
            &self.project,
            &self.project.to_string_lossy(),
        )
        .join(format!("{session}.jsonl"));
        read_jsonl(&path)
    }
    fn all_transcripts(&self) -> Vec<(PathBuf, Vec<Value>)> {
        fn walk(root: &Path, out: &mut Vec<(PathBuf, Vec<Value>)>) {
            for e in std::fs::read_dir(root).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "jsonl")
                    && !p.file_name().unwrap().to_string_lossy().contains("events")
                {
                    let rows = read_jsonl(&p);
                    if rows.iter().any(|r| r["type"] == "accepted_input") {
                        out.push((p, rows));
                    }
                }
            }
        }
        let mut out = vec![];
        walk(&self.pilot, &mut out);
        out
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !thread::panicking() {
            return;
        }
        fn collect(path: &Path, files: &mut Vec<Value>) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, files);
                } else if path.extension().is_some_and(|x| x == "jsonl")
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    files.push(json!({"path":path,"raw":text}));
                }
            }
        }
        let mut files = vec![];
        collect(&self.pilot, &mut files);
        println!("EXTERNAL_MODULE_E2E_FAILURE_DURABLE {}", json!(files));
    }
}
fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read actual transcript {}: {e}", path.display()))
        .lines()
        .filter(|s| !s.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn event_json(event: &JsValue) -> Value {
    serde_json::from_str(&stringify_json(event).unwrap().unwrap()).unwrap()
}
fn submit(
    gateway: &LocalGateway,
    session: &str,
    message: &str,
    mode: &str,
) -> (Result<(), String>, Vec<Value>) {
    let capture: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = capture.clone();
    let input = json!({"sessionKey":session,"channelKey":"test","message":message,"mode":mode,"canPrompt":false});
    let result = gateway.submit_turn(
        parse_json(&input.to_string()).unwrap(),
        Arc::new(move |e| sink.lock().unwrap().push(event_json(&e))),
    );
    let events = capture.lock().unwrap().clone();
    (result, events)
}
fn successful(events: &[Value]) {
    let terminals: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "turn_completed")
        .collect();
    assert_eq!(terminals.len(), 1, "{events:?}");
    assert_eq!(terminals[0]["finishReason"], "completed");
    assert!(!events.iter().any(|e| e["type"] == "turn_failed"));
    assert!(
        events.iter().any(|e| e.to_string().contains(FINAL)),
        "{events:?}"
    );
}
fn strict_chain(rows: &[Value]) {
    assert!(!rows.is_empty());
    let mut ids = BTreeSet::new();
    let session = rows[0]["sessionId"].clone();
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["sequence"], json!(i + 1));
        assert_eq!(row["sessionId"], session);
        assert!(ids.insert(row["entryId"].as_str().expect("real entry id").to_owned()));
        assert_eq!(
            row["parentEntryId"],
            if i == 0 {
                Value::Null
            } else {
                rows[i - 1]["entryId"].clone()
            }
        );
    }
}
fn assert_module_wire(mock: &Mock) {
    for w in mock.wires().iter().filter(|w| w.path.starts_with("/call/")) {
        assert!(
            !w.headers.contains_key("authorization"),
            "no credentialEnv in these generic bindings"
        );
        assert_eq!(w.body["kind"], "request");
        assert_eq!(w.body["method"], "module_call");
        assert!(
            w.body["messageId"]
                .as_str()
                .unwrap()
                .starts_with("module-http-")
        );
        assert!(!w.body["requestId"].as_str().unwrap().is_empty());
        if !w.response.is_null() {
            assert_eq!(w.response["kind"], "response");
            assert_eq!(w.response["inReplyTo"], w.body["messageId"]);
            assert_eq!(w.response["requestId"], w.body["requestId"]);
        }
        assert!(w.body.get("abortSignal").is_none());
        assert!(w.body["payload"].get("abortSignal").is_none());
    }
}
fn tool_names(request: &Value) -> BTreeSet<String> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            t["name"]
                .as_str()
                .or_else(|| t["function"]["name"].as_str())
        })
        .map(str::to_owned)
        .collect()
}
fn evidence(case: &str, mock: &Mock, events: &[Value], transcripts: &[(PathBuf, Vec<Value>)]) {
    println!(
        "EXTERNAL_MODULE_E2E_RECEIPT {}",
        json!({"case":case,"http":mock.receipt(),"events":events,"transcripts":transcripts.iter().map(|(p,r)|json!({"path":p,"rows":r})).collect::<Vec<_>>()})
    );
}
fn child_and_parent_wire(mock: &Mock) {
    for slot in ["model", "tools", "context"] {
        assert_eq!(
            mock.manifest_count(slot),
            2,
            "{slot} parent and child must own different HTTP clients"
        );
        let calls: Vec<_> = mock
            .wires()
            .into_iter()
            .filter(|w| w.path == format!("/call/{slot}"))
            .collect();
        assert_eq!(
            calls
                .iter()
                .filter(|w| w.body["messageId"] == "module-http-1")
                .count(),
            2,
            "{slot} starts client sequence twice"
        );
    }
    for slot in ["skills", "knowledge"] {
        assert_eq!(
            mock.manifest_count(slot),
            1,
            "{slot} belongs to project/domain owner"
        );
        let calls: Vec<_> = mock
            .wires()
            .into_iter()
            .filter(|w| w.path == format!("/call/{slot}"))
            .collect();
        let ids: BTreeSet<_> = calls
            .iter()
            .map(|w| w.body["messageId"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids.len(),
            calls.len(),
            "{slot} must share client sequence, no child reset"
        );
    }
    assert_eq!(
        mock.calls("skills", "list").len(),
        1,
        "child inherits domain contribution registry without re-list"
    );
    let model = mock.calls("model", "prepare");
    let runs: BTreeSet<_> = model
        .iter()
        .map(|w| w.body["runId"].as_str().unwrap())
        .collect();
    assert!(
        runs.len() >= 3,
        "parent two turns and child each use real run identity"
    );
    let read = mock.calls("skills", "read");
    assert_eq!(read.len(), 2);
    let request_ids: BTreeSet<_> = read
        .iter()
        .map(|w| w.body["requestId"].as_str().unwrap())
        .collect();
    assert_eq!(
        request_ids.len(),
        2,
        "child cannot reset domain request sequence"
    );
    for w in mock.calls("knowledge", "query") {
        assert_eq!(w.body["payload"]["input"]["tenantId"], "tenant-owned");
        assert_eq!(w.body["payload"]["input"]["actorUserId"], "actor-owned");
        assert_eq!(w.body["payload"]["input"]["agentId"], "agent-owned");
        assert!(w.body["payload"].get("authorityContext").is_none());
    }
}

#[test]
fn external_modules_five_slots_and_real_child_preserve_owner_and_durable_chain() {
    isolated(
        "external_modules_five_slots_and_real_child_preserve_owner_and_durable_chain",
        |root| {
            let fixture = Fixture::new(root);
            let mock = Mock::start(Profile::FiveSlots, fixture.project.clone());
            fixture.configure(&mock, Profile::FiveSlots);
            let gateway = fixture
                .start()
                .expect("five slots must pass production profile validation");
            let (result, mut events) = submit(
                &gateway,
                "external-main",
                "Use external approval evidence and delegate once.",
                "bypassPermissions",
            );
            result.unwrap();
            successful(&events);
            let (result, history) = submit(
                &gateway,
                "external-main",
                "HISTORY-CHECK: remember the prior completed turn.",
                "bypassPermissions",
            );
            result.unwrap();
            successful(&history);
            events.extend(history);
            assert_eq!(
                mock.calls("model", "prepare").len(),
                5,
                "two parent model steps, two child steps, one history step"
            );
            assert_eq!(mock.calls("model", "stream").len(), 5);
            assert_eq!(mock.calls("context", "prepare_for_model").len(), 5);
            assert_eq!(mock.calls("tools", "").len(), 2);
            assert_eq!(mock.calls("skills", "read").len(), 2);
            assert_eq!(mock.calls("knowledge", "query").len(), 2);
            child_and_parent_wire(&mock);
            assert_module_wire(&mock);
            for w in mock.calls("tools", "") {
                assert_eq!(w.body["module"], "capability");
                assert!(w.body["payload"].get("operation").is_none());
                assert_eq!(w.body["payload"]["name"], "remote_lookup");
                assert_eq!(w.body["payload"]["arguments"], json!({"key":"approval"}));
            }
            for w in mock.calls("model", "prepare") {
                let names = tool_names(&w.body["payload"]["request"]);
                for name in [
                    "remote_lookup",
                    "read_skill",
                    "knowledge_query",
                    "read_file",
                ] {
                    assert!(
                        names.contains(name),
                        "selected external + native tools coexist: {names:?}"
                    );
                }
                assert!(
                    w.body["payload"]["request"]
                        .to_string()
                        .contains(CONTEXT_TEXT),
                    "external context result reaches actual selected model"
                );
            }
            assert!(
                mock.calls("model", "prepare").iter().any(|w| last_user(
                    &w.body["payload"]["request"]
                )
                .contains("HISTORY-CHECK")
                    && w.body["payload"]["request"]["messages"]
                        .to_string()
                        .contains(FINAL)),
                "next real model request retains history"
            );
            assert!(
                mock.wires()
                    .iter()
                    .filter(|w| w.path == "/v1/chat/completions")
                    .all(|w| w.body.to_string().contains("Generate a concise title")),
                "selected external model never falls back to native provider for business turn"
            );
            gateway.shutdown(Duration::from_secs(5)).unwrap();
            let transcripts = fixture.all_transcripts();
            assert_eq!(
                transcripts.len(),
                2,
                "one parent and one actual child transcript"
            );
            for (_, rows) in &transcripts {
                strict_chain(rows);
                for t in [
                    "accepted_input",
                    "model_request",
                    "assistant_message",
                    "turn_result",
                ] {
                    assert!(rows.iter().any(|r| r["type"] == t), "durable {t}");
                }
                assert!(rows.iter().any(|r| r["type"] == "tool_result"));
            }
            let all_rows: Vec<_> = transcripts.iter().flat_map(|(_, r)| r.iter()).collect();
            for w in mock.calls("model", "prepare") {
                assert_eq!(
                    all_rows
                        .iter()
                        .filter(|r| r["type"] == "model_request"
                            && r["turnId"] == w.body["runId"]
                            && r["request"] == w.body["payload"]["request"])
                        .count(),
                    1,
                    "actual selected model request recorded exactly once"
                );
            }
            assert!(
                all_rows.iter().any(|r| r["type"] == "tool_result"
                    && r["result"]
                        .to_string()
                        .contains("OWNED-NATIVE-READ-EVIDENCE")),
                "native tool remains executable beside external owner"
            );
            for r in all_rows.iter().filter(|r| r["type"] == "turn_result") {
                assert_eq!(r["result"]["type"], "success");
                let n = mock
                    .calls("model", "stream")
                    .iter()
                    .filter(|w| w.body["runId"] == r["turnId"])
                    .count();
                assert!(n > 0);
                assert_eq!(r["result"]["usage"]["inputTokens"], json!(17 * n));
                assert_eq!(r["result"]["usage"]["outputTokens"], json!(5 * n));
            }
            let requests_before = mock.wires().len();
            assert!(
                submit(
                    &gateway,
                    "after-cleanup",
                    "must not dispatch",
                    "bypassPermissions"
                )
                .0
                .is_err()
            );
            assert_eq!(
                mock.wires().len(),
                requests_before,
                "shutdown cannot resurrect clients or dispatch late business"
            );
            mock.assert_healthy();
            evidence("five-slots-and-child", &mock, &events, &transcripts);
        },
    );
}

#[test]
fn external_modules_knowledge_only_keeps_native_model_and_disables_skills() {
    isolated(
        "external_modules_knowledge_only_keeps_native_model_and_disables_skills",
        |root| {
            let f = Fixture::new(root);
            let m = Mock::start(Profile::KnowledgeOnly, f.project.clone());
            f.configure(&m, Profile::KnowledgeOnly);
            let g = f.start().unwrap();
            let (result, events) = submit(
                &g,
                "knowledge-native",
                "Find approval evidence.",
                "bypassPermissions",
            );
            result.unwrap();
            successful(&events);
            g.shutdown(Duration::from_secs(5)).unwrap();
            assert_eq!(m.calls("knowledge", "query").len(), 1);
            assert_eq!(m.manifest_count("knowledge"), 1);
            for s in ["model", "tools", "context", "skills"] {
                assert_eq!(
                    m.manifest_count(s),
                    0,
                    "unconfigured/disabled {s} must not be contacted"
                );
            }
            let native: Vec<_> = m
                .wires()
                .into_iter()
                .filter(|w| {
                    w.path == "/v1/chat/completions"
                        && !w.body.to_string().contains("Generate a concise title")
                })
                .collect();
            assert_eq!(native.len(), 2);
            for w in &native {
                let names = tool_names(&w.body);
                assert!(names.contains("knowledge_query"));
                assert!(!names.contains("read_skill"));
                assert!(names.contains("read_file"));
            }
            assert!(
                native[1].body["messages"]
                    .to_string()
                    .contains("citation-1")
            );
            let rows = f.transcript("knowledge-native");
            strict_chain(&rows);
            assert!(
                rows.iter().any(|r| r["type"] == "tool_result"
                    && r["result"].to_string().contains("citation-1"))
            );
            assert_module_wire(&m);
            m.assert_healthy();
            evidence(
                "knowledge-only-native-model",
                &m,
                &events,
                &[(PathBuf::from("knowledge-native"), rows)],
            );
        },
    );
}

#[test]
fn external_modules_disabled_domains_are_absent_from_selected_model() {
    isolated(
        "external_modules_disabled_domains_are_absent_from_selected_model",
        |root| {
            let f = Fixture::new(root);
            let m = Mock::start(Profile::Disabled, f.project.clone());
            f.configure(&m, Profile::Disabled);
            let g = f.start().unwrap();
            let (result, events) = submit(
                &g,
                "disabled-domains",
                "Finish without domain tools.",
                "bypassPermissions",
            );
            result.unwrap();
            successful(&events);
            g.shutdown(Duration::from_secs(5)).unwrap();
            assert_eq!(m.calls("model", "prepare").len(), 1);
            for w in m.calls("model", "prepare") {
                let names = tool_names(&w.body["payload"]["request"]);
                assert!(!names.contains("read_skill"));
                assert!(!names.contains("knowledge_query"));
                assert!(names.contains("read_file"));
            }
            for s in ["skills", "knowledge", "tools", "context"] {
                assert_eq!(m.manifest_count(s), 0);
            }
            let rows = f.transcript("disabled-domains");
            strict_chain(&rows);
            m.assert_healthy();
            evidence(
                "disabled-domains",
                &m,
                &events,
                &[(PathBuf::from("disabled-domains"), rows)],
            );
        },
    );
}

#[test]
fn external_modules_permission_deny_precedes_remote_dispatch() {
    isolated(
        "external_modules_permission_deny_precedes_remote_dispatch",
        |root| {
            let f = Fixture::new(root);
            let m = Mock::start(Profile::Denied, f.project.clone());
            f.configure(&m, Profile::Denied);
            std::fs::write(
                f.pilot.join("permissions.json"),
                r#"{"disallowedTools":["remote_lookup"]}"#,
            )
            .unwrap();
            let g = f.start().unwrap();
            let (result, events) = submit(
                &g,
                "permission-denied",
                "Try the configured remote lookup.",
                "default",
            );
            result.unwrap();
            successful(&events);
            g.shutdown(Duration::from_secs(5)).unwrap();
            assert_eq!(
                m.calls("tools", "").len(),
                0,
                "deny must precede side effect HTTP POST"
            );
            assert!(
                m.calls("model", "prepare")
                    .iter()
                    .any(|w| tool_names(&w.body["payload"]["request"]).contains("remote_lookup")),
                "denial tests authorization, not a missing catalog tool"
            );
            let rows = f.transcript("permission-denied");
            strict_chain(&rows);
            let denied: Vec<_> = rows
                .iter()
                .filter(|r| {
                    r["type"] == "tool_result" && r["result"]["toolName"] == "remote_lookup"
                })
                .collect();
            assert_eq!(denied.len(), 1);
            assert_ne!(denied[0]["result"]["type"], "success");
            assert!(denied[0].to_string().contains("den"), "{denied:?}");
            m.assert_healthy();
            evidence(
                "permission-deny",
                &m,
                &events,
                &[(PathBuf::from("permission-denied"), rows)],
            );
        },
    );
}

#[test]
fn external_modules_dispatched_tool_disconnect_is_unknown_and_never_replayed() {
    isolated(
        "external_modules_dispatched_tool_disconnect_is_unknown_and_never_replayed",
        |root| {
            let f = Fixture::new(root);
            let m = Mock::start(Profile::Disconnect, f.project.clone());
            f.configure(&m, Profile::Disconnect);
            let g = f.start().unwrap();
            let (result, events) = submit(
                &g,
                "unknown-tool",
                "Try the remote lookup once.",
                "bypassPermissions",
            );
            result.unwrap();
            successful(&events);
            g.shutdown(Duration::from_secs(5)).unwrap();
            assert_eq!(
                m.calls("tools", "").len(),
                1,
                "unknown side effect must not auto-replay"
            );
            let rows = f.transcript("unknown-tool");
            strict_chain(&rows);
            let failed: Vec<_> = rows
                .iter()
                .filter(|r| {
                    r["type"] == "tool_result" && r["result"]["toolName"] == "remote_lookup"
                })
                .collect();
            assert_eq!(failed.len(), 1);
            let result = &failed[0]["result"];
            assert_eq!(result["type"], "error");
            assert_eq!(result["error"]["code"], "tool_execution_failed");
            assert_eq!(
                result["error"]["details"]["moduleCode"],
                "MODULE_TRANSPORT_UNAVAILABLE"
            );
            assert_eq!(result["error"]["details"]["outcome"], "result_unknown");
            assert_eq!(result["error"]["details"]["retryability"], "unsafe");
            assert!(
                m.calls("model", "prepare").iter().any(|w| {
                    w.body["payload"]["request"]["messages"]
                        .to_string()
                        .contains("result_unknown")
                }),
                "following actual model sees structured unknown result"
            );
            m.assert_healthy();
            evidence(
                "tool-disconnect-unknown",
                &m,
                &events,
                &[(PathBuf::from("unknown-tool"), rows)],
            );
        },
    );
}

#[test]
fn external_modules_remote_skill_list_error_cannot_silently_fall_back() {
    isolated(
        "external_modules_remote_skill_list_error_cannot_silently_fall_back",
        |root| {
            let f = Fixture::new(root);
            let m = Mock::start(Profile::SkillFailure, f.project.clone());
            f.configure(&m, Profile::SkillFailure);
            let local = f.project.join(".pilotdeck/skills/local-fallback");
            std::fs::create_dir_all(&local).unwrap();
            std::fs::write(local.join("SKILL.md"),"---\nname: local-fallback\ndescription: MUST NOT SUBSTITUTE REMOTE\n---\nLOCAL-FALLBACK-SENTINEL\n").unwrap();
            let g = f.start().unwrap();
            let (result, events) = submit(
                &g,
                "failed-remote-list",
                "Use the configured remote skills.",
                "bypassPermissions",
            );
            let combined = format!("{result:?} {events:?}");
            assert!(
                result.is_err()
                    || events
                        .iter()
                        .any(|e| e["type"] == "turn_failed" || e["finishReason"] == "failed"),
                "remote-list failure must fail the production turn: {combined}"
            );
            assert!(
                combined.contains("owned remote skill list unavailable")
                    || combined.contains("REMOTE_SKILLS_DOWN"),
                "remote error is observable: {combined}"
            );
            g.shutdown(Duration::from_secs(5)).unwrap();
            assert_eq!(m.calls("skills", "list").len(), 1);
            assert_eq!(
                m.calls("model", "prepare").len(),
                0,
                "remote list failure cannot continue with native/empty skills"
            );
            assert_eq!(m.calls("model", "stream").len(), 0);
            assert!(!events.iter().any(|e| e.to_string().contains(FINAL)));
            m.assert_healthy();
            evidence(
                "remote-skill-list-failure",
                &m,
                &events,
                &f.all_transcripts(),
            );
        },
    );
}

#[test]
fn external_modules_shutdown_cancels_dispatched_http_wait_without_replay() {
    isolated(
        "external_modules_shutdown_cancels_dispatched_http_wait_without_replay",
        |root| {
            let f = Fixture::new(root);
            let m = Mock::start(Profile::Cancel, f.project.clone());
            f.configure(&m, Profile::Cancel);
            let gateway = Arc::new(f.start().unwrap());
            let worker_gateway = gateway.clone();
            let worker = thread::spawn(move || {
                submit(
                    &worker_gateway,
                    "cancelled-http",
                    "Wait for the remote lookup.",
                    "bypassPermissions",
                )
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while m.calls("tools", "").is_empty() {
                assert!(
                    Instant::now() < deadline,
                    "real side effect POST must precede cancellation"
                );
                thread::sleep(Duration::from_millis(2));
            }
            let stopped = gateway.shutdown(Duration::from_secs(5));
            let (result, events) = worker.join().unwrap();
            stopped.unwrap();
            m.state.stop.store(true, Ordering::SeqCst);
            assert_eq!(m.calls("tools", "").len(), 1);
            assert!(
                result.is_err()
                    || events
                        .iter()
                        .any(|e| e["finishReason"] == "cancelled" || e["type"] == "turn_failed"),
                "shutdown must not report pending remote execution as completed: {result:?} {events:?}"
            );
            assert!(
                !events
                    .iter()
                    .any(|e| e["type"] == "turn_completed" && e["finishReason"] == "completed")
            );
            let before = m.wires().len();
            assert!(
                submit(&gateway, "late", "late request", "bypassPermissions")
                    .0
                    .is_err()
            );
            assert_eq!(m.wires().len(), before);
            let transcripts = f.all_transcripts();
            for (_, r) in &transcripts {
                strict_chain(r);
                assert!(
                    !r.iter()
                        .any(|e| e["type"] == "tool_result" && e["result"]["type"] == "success"),
                    "cancelled unknown remote call cannot become success"
                );
            }
            m.assert_healthy();
            evidence(
                "shutdown-cancels-local-http-wait",
                &m,
                &events,
                &transcripts,
            );
        },
    );
}

#[test]
fn external_modules_permission_hook_updates_real_dispatched_input_once() {
    isolated(
        "external_modules_permission_hook_updates_real_dispatched_input_once",
        |root| {
            let f = Fixture::new(root);
            let m = Mock::start(Profile::HookUpdated, f.project.clone());
            f.configure(&m, Profile::HookUpdated);
            // User ask rules exercise the actual native Permissions path. The
            // configured tool remains visible and is not an injected Decision port.
            std::fs::write(
                f.pilot.join("permissions.json"),
                r#"{"askTools":["remote_lookup"]}"#,
            )
            .unwrap();
            std::fs::write(root.join("hook-permission-timeout"), "owned-only").unwrap();
            let g = f.start().unwrap();
            let input = json!({
                "sessionKey":"hook-approved", "channelKey":"test",
                "message":"Ask before the one remote lookup.", "mode":"default", "canPrompt":true,
                "sdkSessionConfig":{
                    "includeHookEvents":true,
                    "hooks":{"url":format!("http://{}/permission-hook",m.address),
                        "events":{"PermissionRequest":[{"matcher":"remote_lookup"}]}}
                }
            });
            let capture: Arc<Mutex<Vec<Value>>> = Arc::default();
            let sink = capture.clone();
            g.submit_turn(
                parse_json(&input.to_string()).unwrap(),
                Arc::new(move |e| sink.lock().unwrap().push(event_json(&e))),
            )
            .unwrap();
            let events = capture.lock().unwrap().clone();
            successful(&events);
            let wire = m.wires();
            let hooks: Vec<_> = wire
                .iter()
                .filter(|w| w.path.starts_with("/permission-hook?"))
                .collect();
            assert_eq!(hooks.len(), 1, "one actual PermissionRequest hook dispatch");
            assert_eq!(hooks[0].method, "POST");
            assert!(hooks[0].path.contains("event=PermissionRequest"));
            assert!(hooks[0].path.contains("matcher=0"));
            assert_eq!(hooks[0].body["toolName"], "remote_lookup");
            assert_eq!(
                hooks[0].body["toolInput"],
                json!({"key":"approval"}),
                "hook receives model-authored input"
            );
            assert!(
                events
                    .iter()
                    .any(|e| e["type"] == "hook_started" && e["hookEvent"] == "PermissionRequest"),
                "real configured hook projected onto public stream"
            );
            assert!(
                events
                    .iter()
                    .any(|e| e["type"] == "hook_response" && e["hookEvent"] == "PermissionRequest")
            );
            assert!(
                events.iter().any(|e| e["type"] == "permission_request"),
                "ordinary callback reaches ask; hook first-decision must survive its timeout"
            );
            let tools = m.calls("tools", "");
            assert_eq!(tools.len(), 1, "approved side effect dispatched once");
            assert_eq!(
                tools[0].body["payload"]["arguments"],
                json!({"key":"approved-only","approved":true})
            );
            assert_eq!(tools[0].body["payload"]["toolCallId"], "external-tool-call");
            let hook_at = wire
                .iter()
                .position(|w| w.path.starts_with("/permission-hook?"))
                .unwrap();
            let tool_at = wire.iter().position(|w| w.path == "/call/tools").unwrap();
            assert!(
                hook_at < tool_at,
                "authorization hook result precedes actual remote POST"
            );
            assert_eq!(m.calls("model", "prepare").len(), 2);
            assert!(
                m.calls("model", "prepare").iter().any(|w| {
                    w.body["payload"]["request"]["messages"]
                        .to_string()
                        .contains("HOOK-APPROVED-INPUT-USED")
                }),
                "next real provider receives selected tool result"
            );
            g.shutdown(Duration::from_secs(5)).unwrap();
            let rows = f.transcript("hook-approved");
            strict_chain(&rows);
            let calls: Vec<_> = rows
                .iter()
                .filter(|r| r["type"] == "tool_call" && r["call"]["id"] == "external-tool-call")
                .collect();
            assert_eq!(calls.len(), 1, "original model tool call persisted once");
            assert_eq!(
                calls[0]["call"]["input"],
                json!({"key":"approval"}),
                "approval does not rewrite original model-authored durable call"
            );
            let results: Vec<_> = rows
                .iter()
                .filter(|r| {
                    r["type"] == "tool_result" && r["result"]["toolCallId"] == "external-tool-call"
                })
                .collect();
            assert_eq!(
                results.len(),
                1,
                "actual approved remote result persisted once"
            );
            assert_eq!(results[0]["result"]["type"], "success");
            assert_eq!(
                results[0]["result"]["data"],
                json!({"key":"approved-only","approved":true})
            );
            assert_eq!(
                results[0]["turnId"], tools[0].body["runId"],
                "actual wire and durable result belong to same turn"
            );
            assert_module_wire(&m);
            m.assert_healthy();
            evidence(
                "permission-hook-updated-input",
                &m,
                &events,
                &[(PathBuf::from("hook-approved"), rows)],
            );
        },
    );
}
