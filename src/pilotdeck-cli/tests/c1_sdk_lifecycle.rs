//! C1: formal CLI entry points release failed queries instead of retaining a socket.
//! Baseline cc7c4e6: sdkCli.ts:43-86 and SDK client.ts:1247-1256,2001-2011.
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinSet, time::timeout};
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Clone, Copy, Debug)]
enum Case {
    MissingProject,
    HelloTimeout,
    QueryRejected,
    ModelAmbiguous,
    StreamRejected,
    Completed,
    ModelCompleted,
    Failed,
}

struct Runtime(PathBuf);
impl Runtime {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("c1-sdk-{}", uuid::Uuid::new_v4()));
        for name in ["home", "pilot", "project"] {
            fs::create_dir_all(path.join(name)).expect("create isolated CLI runtime");
        }
        Self(path)
    }

    fn command(&self, endpoint: &str, resume: bool, case: Case) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pilotdeck"));
        command
            .env_clear()
            .current_dir(self.0.join("project"))
            .env("HOME", self.0.join("home"))
            .env("PILOT_HOME", self.0.join("pilot"))
            .env("PILOTDECK_SDK_GATEWAY_URL", endpoint)
            .env("PILOTDECK_SDK_AUTH_TOKEN", "c1-fixture")
            .env("ANALYTICS_ENABLED", "0")
            .env("DOCKER_CONTAINER", "1")
            .env("PATH", "")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if resume {
            command.args(["resume", "c1-session"]);
        } else {
            command.arg("run");
        }
        command.arg("fixture prompt");
        match case {
            Case::MissingProject => {
                command.args(["--model", "fixture/model"]);
            }
            Case::ModelAmbiguous | Case::ModelCompleted => {
                command.args(["--project", "c1-project", "--model", "fixture/model"]);
            }
            _ => {}
        }
        command
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Test-only phase evidence: no I/O within an exercise's original deadline.
// Retain the latest 256 bounded entries and explicitly count older evictions.
struct Observation {
    start: std::time::Instant,
    case: String,
    events: Mutex<(std::collections::VecDeque<Value>, usize)>,
    flushed: std::sync::atomic::AtomicBool,
}
impl Observation {
    fn new(resume: bool, case: Case) -> Arc<Self> {
        Arc::new(Self {
            start: std::time::Instant::now(),
            case: format!("{}-{case:?}", if resume { "resume" } else { "run" }),
            events: Mutex::new((std::collections::VecDeque::new(), 0)),
            flushed: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn event(&self, phase: &str, data: Value) {
        let data = match serde_json::to_string(&data) {
            Ok(encoded) if encoded.len() > 8192 => {
                let mut end = 8192;
                while !encoded.is_char_boundary(end) {
                    end -= 1;
                }
                json!({"truncated":true,"originalBytes":encoded.len(),"prefix":&encoded[..end]})
            }
            _ => data,
        };
        let mut stored = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stored.0.len() == 256 {
            stored.0.pop_front();
            stored.1 += 1;
        }
        stored.0.push_back(json!({"elapsedMs":self.start.elapsed().as_secs_f64()*1000.0,"phase":phase,"data":data}));
    }
    fn flush_failure(&self) {
        if self
            .flushed
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        // Logging failure must never panic or prevent the original child cleanup.
        let stored = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = json!({"case":self.case,"testPid":std::process::id(),"evictedEntries":stored.1,"events":stored.0});
        drop(stored);
        if let Ok(encoded) = serde_json::to_vec(&record) {
            use std::io::Write;
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(b"C1_SDK_LIFECYCLE_FAILURE_PHASE ");
            let _ = stderr.write_all(&encoded);
            let _ = stderr.write_all(b"\n");
        }
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.flush_failure();
        }
    }
}

struct OwnedChild(Option<Child>, Arc<Observation>);
impl OwnedChild {
    async fn output(&mut self, bound: Duration) -> Output {
        timeout(bound, async {
            self.1.event("child-pid-at-timeout-start", json!({"pid":self.0.as_ref().map(Child::id)}));
            loop {
                if self
                    .0
                    .as_mut()
                    .expect("owned child")
                    .try_wait()
                    .expect("poll CLI")
                    .is_some()
                {
                    let output = self.0.take().expect("exited child").wait_with_output().expect("CLI output");
                    self.1.event("child-exited", json!({"code":output.status.code(),"status":format!("{}",output.status),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)}));
                    return output;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .inspect_err(|_| { self.1.event("original-5-or-15s-child-deadline", json!({"pid":self.0.as_ref().map(Child::id),"boundMs":bound.as_millis()})); })
        .expect("CLI must exit without requiring the mock to close its query socket")
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let pid = child.id();
            let kill = child.kill();
            let wait = child.wait();
            self.1.event(
                "failure-owned-cleanup",
                json!({"pid":pid,"kill":format!("{kill:?}"),"wait":format!("{wait:?}")}),
            );
        }
        if std::thread::panicking() {
            self.1.flush_failure();
        }
    }
}

type Wire = Arc<Mutex<Vec<(usize, Value)>>>;

async fn serve(listener: TcpListener, case: Case, wire: Wire, observation: Arc<Observation>) {
    let mut clients = JoinSet::new();
    for connection in 1..=2 {
        observation.event("accept-wait", json!({"connection":connection}));
        let (socket, peer) = listener
            .accept()
            .await
            .expect("accept owned CLI connection");
        observation.event(
            "accepted",
            json!({"connection":connection,"peer":peer.to_string()}),
        );
        let wire = Arc::clone(&wire);
        let observation = Arc::clone(&observation);
        clients.spawn(async move {
            observation.event("handshake-begin", json!({"connection":connection}));
            let mut ws = accept_async(socket).await.expect("CLI websocket handshake");
            observation.event("handshake-complete", json!({"connection":connection}));
            let mut closed = false;
            while let Some(message) = ws.next().await {
                if let Err(error) = &message { observation.event("socket-read-error", json!({"connection":connection,"error":format!("{error:?}")})); }
                let message = message.expect("CLI websocket frame");
                if matches!(message, Message::Close(_)) {
                    closed = true;
                    observation.event("close-received", json!({"connection":connection}));
                    let flushed = ws.flush().await;
                    observation.event("close-flush", json!({"connection":connection,"result":format!("{flushed:?}")}));
                    break;
                }
                let Message::Text(text) = message else { continue };
                let request: Value = serde_json::from_str(&text).expect("CLI JSON request");
                wire.lock().expect("wire lock").push((connection, request.clone()));
                observation.event("frame-received", json!({"connection":connection,"frame":request}));
                if matches!(case, Case::HelloTimeout)
                    && connection == 2
                    && request["type"] == "hello"
                {
                    continue;
                }
                let response = if request["type"] == "hello" {
                    json!({"type":"hello_ok","protocolVersion":"1.2","serverInfo":{"capabilities":[]}})
                } else {
                    let method = request["method"].as_str().expect("RPC method");
                    if matches!(case, Case::QueryRejected) && connection == 2 && method == "resume_session"
                        || matches!(case, Case::StreamRejected) && method == "submit_turn"
                    {
                        json!({"type":"response","id":request["id"],"ok":false,"error":{"code":"server_error","message":"c1 fixture rejected"}})
                    } else if method == "submit_turn" {
                        if matches!(case, Case::Failed) {
                            json!({"type":"event","id":request["id"],"seq":0,"final":true,"event":{"type":"error","code":"server_error","message":"c1 failed"}})
                        } else {
                            observation.event("text-delta-send-begin", json!({"connection":connection,"requestId":request["id"]}));
                            ws.send(Message::Text(json!({"type":"event","id":request["id"],"seq":0,"final":false,"event":{"type":"assistant_text_delta","text":"c1 success"}}).to_string().into())).await.expect("send text event");
                            observation.event("text-delta-send-complete", json!({"connection":connection}));
                            json!({"type":"event","id":request["id"],"seq":1,"final":true,"event":{"type":"turn_completed","usage":{}}})
                        }
                    } else {
                        let result = if method == "model_catalog_list" {
                            if matches!(case, Case::ModelCompleted) {
                                json!({"items":[{"id":"fixture/model","provider":"fixture","model":"model"}]})
                            } else {
                                json!({"items":[]})
                            }
                        } else {
                            json!({"sessionKey":"c1-session"})
                        };
                        json!({"type":"response","id":request["id"],"ok":true,"result":result})
                    }
                };
                observation.event("reply-begin", json!({"connection":connection,"frame":response}));
                ws.send(Message::Text(response.to_string().into())).await.expect("send mock reply");
                observation.event("reply-complete", json!({"connection":connection}));
            }
            observation.event("socket-loop-ended", json!({"connection":connection,"closed":closed}));
            assert!(closed, "connection {connection} must receive an explicit close frame");
        });
    }
    while let Some(result) = clients.join_next().await {
        observation.event(
            "mock-task-complete",
            json!({"result":format!("{result:?}")}),
        );
        result.expect("mock client completed");
    }
}

async fn exercise(resume: bool, case: Case) {
    let observation = Observation::new(resume, case);
    observation.event(
        "case-begin",
        json!({"resume":resume,"case":format!("{case:?}"),"testPid":std::process::id()}),
    );
    let runtime = Runtime::new();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("random owned mock port");
    let endpoint = format!("ws://{}/ws", listener.local_addr().expect("mock address"));
    let wire = Arc::new(Mutex::new(Vec::new()));
    observation.event(
        "spawn-begin",
        json!({"endpoint":endpoint,"runtime":runtime.0,"CLI":env!("CARGO_BIN_EXE_pilotdeck")}),
    );
    let mut child = OwnedChild(
        Some(
            runtime
                .command(&endpoint, resume, case)
                .spawn()
                .expect("spawn formal CLI"),
        ),
        Arc::clone(&observation),
    );
    let bound = Duration::from_secs(if matches!(case, Case::HelloTimeout) {
        15
    } else {
        5
    });
    let (output, ()) = tokio::join!(child.output(bound), async {
        timeout(
            bound + Duration::from_secs(1),
            serve(listener, case, Arc::clone(&wire), Arc::clone(&observation)),
        )
        .await
        .expect("both CLI sockets released");
    });
    observation.event(
        "output-assertions-begin",
        json!({"code":output.status.code()}),
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    match case {
        Case::Completed | Case::ModelCompleted => {
            assert_eq!(output.status.code(), Some(0), "{resume:?} {case:?}");
            assert_eq!(stdout, "c1 success\n");
            assert_eq!(stderr, "");
        }
        Case::Failed => {
            assert_eq!(output.status.code(), Some(1));
            assert_eq!(
                serde_json::from_str::<Value>(&stdout).expect("failed result")["status"],
                "failed"
            );
            assert_eq!(stderr, "");
        }
        _ => {
            assert_eq!(output.status.code(), Some(1), "{resume:?} {case:?}");
            assert_eq!(stdout, "");
            let expected = match case {
                Case::MissingProject => "projectKey is required when options.model is set.\n",
                Case::HelloTimeout => "Gateway hello timed out.\n",
                Case::ModelAmbiguous => "Model is not uniquely resolvable: fixture/model\n",
                _ => "c1 fixture rejected\n",
            };
            assert_eq!(stderr, expected);
        }
    }
    let wire = wire.lock().expect("wire lock");
    let methods: Vec<_> = wire
        .iter()
        .map(|(connection, request)| (*connection, request["method"].as_str().unwrap_or("hello")))
        .collect();
    let mut expected = vec![
        (1, "hello"),
        (
            1,
            if resume {
                "resume_session"
            } else {
                "new_session"
            },
        ),
        (2, "hello"),
        (2, "resume_session"),
    ];
    if matches!(case, Case::HelloTimeout) {
        expected.pop();
    }
    if matches!(case, Case::ModelAmbiguous | Case::ModelCompleted) {
        expected.push((2, "model_catalog_list"));
    }
    if matches!(
        case,
        Case::StreamRejected | Case::Completed | Case::ModelCompleted | Case::Failed
    ) {
        expected.push((2, "submit_turn"));
    }
    assert_eq!(
        methods, expected,
        "validation must retain baseline RPC order"
    );
    if let Some((_, submitted)) = wire
        .iter()
        .find(|(_, request)| request["method"] == "submit_turn")
    {
        assert_eq!(submitted["params"]["sessionKey"], "c1-session");
        assert_eq!(submitted["params"]["message"], "fixture prompt");
        if matches!(case, Case::ModelCompleted) {
            assert_eq!(
                submitted["params"]["modelOverride"],
                json!({"mode":"model","provider":"fixture","model":"model"})
            );
        }
    }
    assert_eq!(
        fs::read_dir(runtime.0.join("pilot"))
            .expect("isolated pilot home")
            .count(),
        0,
        "CLI must not write local state"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_and_resume_release_failed_queries() {
    for resume in [false, true] {
        for case in [
            Case::MissingProject,
            Case::QueryRejected,
            Case::ModelAmbiguous,
            Case::StreamRejected,
        ] {
            exercise(resume, case).await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn run_and_resume_preserve_success_and_failed_results() {
    for resume in [false, true] {
        for case in [Case::Completed, Case::ModelCompleted, Case::Failed] {
            exercise(resume, case).await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn run_and_resume_release_queries_after_hello_timeout() {
    for resume in [false, true] {
        exercise(resume, Case::HelloTimeout).await;
    }
}
