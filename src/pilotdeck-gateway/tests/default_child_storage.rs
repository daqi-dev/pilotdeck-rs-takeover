//! Default filesystem child spill and scoped skills, compared with fixed TS
//! `_tmp/20261114-default-child-storage-audit/oracle-result.json`.
use pilotdeck_gateway::{
    extensions::{NativeExtension, NativeExtensions},
    host::{HostError, modules::capability::ToolDefinition},
    native_process::{LocalGateway, LocalOptions},
    runtime::tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
};
use pilotdeck_protocol::{JsObject, JsValue, parse_json, stringify_json};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

fn js(value: Value) -> JsValue {
    parse_json(&value.to_string()).unwrap()
}
fn value(value: &JsValue) -> Value {
    serde_json::from_str(&stringify_json(value).unwrap().unwrap()).unwrap()
}
struct Large {
    fields: JsObject,
    payload: String,
    calls: Arc<AtomicUsize>,
}
impl ToolDefinition for Large {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}
impl ExecutableTool for Large {
    fn execute(&self, _: JsValue, _: ToolRunContext) -> ToolFuture<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let payload = self.payload.clone();
        Box::pin(async move {
            Ok(ToolOutput {
                content: vec![js(json!({"type":"text","text":payload}))],
                supplemental_messages: None,
                data: None,
                metadata: None,
            })
        })
    }
}
struct Provider {
    address: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Provider {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (stopping, seen) = (stop.clone(), requests.clone());
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("provider accept: {e}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 8192];
                let (start, length) = loop {
                    let n = socket.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(at) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..at]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                    .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (at + 4, length);
                    }
                };
                while bytes.len() < start + length {
                    let n = socket.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let body: Value = serde_json::from_slice(&bytes[start..start + length]).unwrap();
                seen.lock().unwrap().push(body.clone());
                let messages = body["messages"].as_array().unwrap();
                let child = messages.iter().any(|m| {
                    m["role"] == "user" && m["content"].to_string().contains("DEFAULT_CHILD_SPILL")
                });
                let called = |name: &str| {
                    messages.iter().any(|m| {
                        m["tool_calls"].as_array().is_some_and(|calls| {
                            calls.iter().any(|c| c["function"]["name"] == name)
                        })
                    })
                };
                let delta = if body.to_string().contains("Generate a concise title") {
                    json!({"content":"{\"title\":\"Owned child\"}"})
                } else if child && !called("owned_big") {
                    json!({"tool_calls":[{"index":0,"id":"child-big","type":"function","function":{"name":"owned_big","arguments":"{}"}}]})
                } else if child && !called("read_file") {
                    json!({"tool_calls":[{"index":0,"id":"child-read","type":"function","function":{"name":"read_file","arguments":json!({"file_path":".pilotdeck/tool-results/refs/result-0001.txt","offset":1,"limit":2}).to_string()}}]})
                } else if child {
                    json!({"content":"Scope: child\nResult: read done\nKey files: none\nFiles changed: none\nIssues: none"})
                } else if !called("agent") {
                    json!({"tool_calls":[{"index":0,"id":"launch","type":"function","function":{"name":"agent","arguments":json!({"description":"Owned child","prompt":"DEFAULT_CHILD_SPILL: use owned_big then read its output.","subagent_type":"reviewer"}).to_string()}}]})
                } else {
                    json!({"content":"parent complete"})
                };
                let finish = if delta.get("tool_calls").is_some() {
                    "tool_calls"
                } else {
                    "stop"
                };
                let (mime, response) = if body["stream"] == true {
                    let first = json!({"id":"owned","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                    let last = json!({"id":"owned","choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
                    (
                        "text/event-stream",
                        format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
                    )
                } else {
                    ("application/json",json!({"id":"owned","choices":[{"index":0,"message":{"role":"assistant","content":"Owned child"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string())
                };
                let wire = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = socket.write_all(wire.as_bytes());
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
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn default_child_scoped_skills_spills_to_parent_and_reads_alias() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pdrs-default-child-{}", uuid::Uuid::new_v4())));
    let project = scratch.0.join("project");
    let pilot = scratch.0.join("home/.pilotdeck");
    let skills = scratch.0.join("skills");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&pilot).unwrap();
    for (name, description) in [
        ("review", "SELECTED REVIEW SKILL"),
        ("deploy", "HIDDEN DEPLOY SKILL"),
    ] {
        let dir = skills.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n"),
        )
        .unwrap();
    }
    let provider = Provider::start();
    let config = format!(
        "schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: {}/v1\n      apiKey: mock-only\n      models: {{mock: {{}}}}\n",
        provider.address
    );
    std::fs::write(pilot.join("pilotdeck.yaml"), config).unwrap();
    let chat = pilotdeck_gateway::session_chat_dir(&pilot, &project, &project.to_string_lossy());
    std::fs::create_dir_all(chat.parent().unwrap()).unwrap();
    std::fs::write(
        chat.parent().unwrap().join(".cwd"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let payload = format!(
        "CHILD-SPILL-START\n{}CHILD-SPILL-END\n",
        "payload line 1234567890 abcdefghijklmnopqrstuvwxyz\n".repeat(5000)
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let extensions = NativeExtensions::default();
    let registration=extensions.register("default-child-spill",Some(&project),NativeExtension{tools:vec![Arc::new(Large{fields:js(json!({"name":"owned_big","description":"Return owned large output","inputSchema":{"type":"object","properties":{},"additionalProperties":false}})).as_object().unwrap().clone(),payload:payload.clone(),calls:calls.clone()})],..Default::default()}).unwrap();
    let env = [
        ("HOME", scratch.0.join("home")),
        ("PILOT_HOME", pilot.clone()),
        ("PILOTDECK_CONFIG_PATH", pilot.join("pilotdeck.yaml")),
    ]
    .into_iter()
    .map(|(key, path)| (key.into(), path.to_string_lossy().into_owned()))
    .collect();
    let gateway = Arc::new(
        LocalGateway::start_with_extensions(
            LocalOptions {
                telemetry_context: None,
                pilot_home: pilot.clone(),
                project: project.clone(),
                os_home: Some(scratch.0.join("home")),
                sidecar: scratch.0.join("unused"),
                env,
                builtin_skills_root: Some(skills),
                builtin_plugins_root: None,
            },
            extensions,
        )
        .unwrap(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = events.clone();
    let params = js(
        json!({"sessionKey":"parent-default","projectKey":project,"channelKey":"test","message":"Delegate owned child.","mode":"bypassPermissions","sdkSessionConfig":{"agents":{"reviewer":{"description":"Owned scoped reviewer","prompt":"OWNED CHILD SCOPE","tools":["owned_big","read_file"],"skills":["review"]}}}}),
    );
    thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let running = gateway.clone();
        let worker = scope.spawn(move || {
            let result = running.submit_turn(
                params,
                Arc::new(move |event| capture.lock().unwrap().push(value(&event))),
            );
            let _ = send.send(result);
        });
        let result = receive.recv_timeout(Duration::from_secs(30));
        if result.is_err() {
            gateway.shutdown(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
            panic!("child turn timed out");
        }
        result.unwrap().unwrap();
        worker.join().unwrap();
    });
    let rows = events.lock().unwrap().clone();
    let requests = provider.requests.lock().unwrap().clone();
    let child_request = requests
        .iter()
        .find(|request| {
            request["messages"]
                .to_string()
                .contains("DEFAULT_CHILD_SPILL")
                && request["messages"]
                    .to_string()
                    .contains("CHILD-SPILL-START")
        })
        .expect("child provider received budget reference");
    let child_output = requests.iter().find(|request| {
        request["messages"]
            .to_string()
            .contains("DEFAULT_CHILD_SPILL")
            && request["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|m| {
                    m["role"] == "tool"
                        && m["tool_call_id"] == "child-read"
                        && m["content"].to_string().contains("CHILD-SPILL-START")
                })
            })
    });
    let parent_transcript = std::fs::read_to_string(chat.join("parent-default.jsonl")).unwrap();
    let started = parent_transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry["type"] == "subagent_started")
        .expect("actual child sidechain");
    let relative = started["transcriptRelativePath"].as_str().unwrap();
    let child_path = chat.join(relative);
    let transcript = std::fs::read_to_string(&child_path).unwrap();
    let reference = transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .flat_map(|entry| {
            entry["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .find(|block| {
            block["type"] == "tool_result_reference" && block["toolCallId"] == "child-big"
        })
        .expect("actual child reference");
    let actual = PathBuf::from(reference["path"].as_str().unwrap());
    let expected = project.join(".pilotdeck/tool-results/parent-default");
    eprintln!(
        "DEFAULT_CHILD_REFERENCE actual={} expected_parent={} read_alias={} child_transcript={}",
        actual.display(),
        expected.display(),
        reference["readFilePath"],
        child_path.display()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        rows.iter().any(|event| event["type"] == "turn_completed"),
        "{rows:?}"
    );
    assert!(
        child_request.to_string().contains("result-0001.txt"),
        "provider should see the read alias"
    );
    assert!(
        child_output.is_some(),
        "child read_file returned first marker; requests={requests:?}"
    );
    let prompt = child_request["messages"].to_string();
    assert!(prompt.contains("SELECTED REVIEW SKILL"), "{prompt}");
    assert!(!prompt.contains("HIDDEN DEPLOY SKILL"), "{prompt}");
    assert_eq!(std::fs::read(&actual).unwrap(), payload.as_bytes());
    assert_eq!(
        actual.parent(),
        Some(expected.as_path()),
        "fixed TS uses parent spill root; observed {}",
        actual.display()
    );
    gateway.shutdown(Duration::from_secs(5)).unwrap();
    drop(registration);
}
