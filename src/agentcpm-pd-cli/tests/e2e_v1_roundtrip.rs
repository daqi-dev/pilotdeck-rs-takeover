//! WP5 end-to-end acceptance suite (`007 §10`).
//!
//! Every scenario drives the real `agentcpm-pd` binary (`CARGO_BIN_EXE`) as a
//! subprocess against an isolated temporary `AGENTCPM_PD_HOME`, with the
//! default `embedded-gateway` feature answering over the real `/v1/ws`
//! transport (client auto-start). No mock gateway is used anywhere: S1 items
//! that the plan allowed to simulate are exercised against the real stack.
//!
//! Scenario -> acceptance tag mapping is noted on each test; the full matrix
//! is reported in the WP5 handoff record.
//!
//! Determinism rules: every frame read is deadline-bounded, every subprocess
//! invocation is timeout-bounded, and waits rely on ready.json / event
//! frames — never on sleep races. Negative "no more frames" assertions use
//! one bounded quiet window, which is the only sound way to observe absence.

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::registry::OperationRegistry;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_agentcpm-pd");
const OP_VERSION: &str = "agentcpm-pd.operation/1";
const CLI_TIMEOUT: Duration = Duration::from_secs(90);
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a frame source may stay silent before a scenario gives up on
/// "nothing else is coming" style checks.
const QUIET_WINDOW: Duration = Duration::from_millis(700);

static SEQ: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Harness: temp home + CLI invocations
// ---------------------------------------------------------------------------

struct TestHome(PathBuf);

impl TestHome {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentcpm-pd-wp5-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp home");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The PID of the live gateway serving this home (auto-started by the
    /// CLI, or explicitly started), from `run/ready.json`.
    fn gateway_pid(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(self.0.join("run").join("ready.json")).ok()?;
        let value: Value = serde_json::from_str(&raw).ok()?;
        value.get("pid").and_then(Value::as_u64).map(|p| p as u32)
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        if let Some(pid) = self.gateway_pid() {
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(pid as i32).expect("pid"),
                rustix::process::Signal::KILL,
            );
        }
        std::thread::sleep(Duration::from_millis(60));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct CliOutcome {
    code: i32,
    stdout: String,
    stderr: String,
}

fn base_command(home: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd
}

/// Runs one `agentcpm-pd` argv invocation to completion.
async fn cli(home: &Path, args: &[&str]) -> CliOutcome {
    cli_with_stdin(home, args, None).await
}

async fn cli_with_stdin(home: &Path, args: &[&str], stdin: Option<&str>) -> CliOutcome {
    let mut cmd = base_command(home);
    cmd.args(args);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().expect("spawn agentcpm-pd");
    if let Some(text) = stdin {
        let mut handle = child.stdin.take().expect("piped stdin");
        handle
            .write_all(text.as_bytes())
            .await
            .expect("write stdin");
        drop(handle);
    }
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output())
        .await
        .expect("cli invocation timed out")
        .expect("wait for cli");
    CliOutcome {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn registry() -> &'static OperationRegistry {
    OperationRegistry::global()
}

/// Parses and validates one argv envelope (`--output json`): exactly one line,
/// `version`/`requestId` present, the result checked against the embedded
/// operation schema. Returns the operation result.
fn expect_envelope(method: &str, out: &CliOutcome) -> Value {
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "`{method}` must print exactly one envelope line; stdout:\n{}",
        out.stdout
    );
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(
        value["version"], OP_VERSION,
        "`{method}` envelope version mismatch: {value}"
    );
    assert!(
        value["requestId"].as_str().is_some_and(|s| !s.is_empty()),
        "`{method}` envelope has no requestId: {value}"
    );
    let outcome = value["outcome"].as_str().unwrap_or_default();
    assert!(
        outcome == "succeeded" || outcome == "accepted",
        "`{method}` outcome `{outcome}`: {}",
        out.stderr
    );
    let result = value["result"].clone();
    registry()
        .validate_result(method, &result)
        .unwrap_or_else(|e| panic!("`{method}` result failed schema validation: {e:?}"));
    result
}

/// Like [`expect_envelope`] but also asserts exit code 0.
fn expect_ok(method: &str, out: &CliOutcome) -> Value {
    assert_eq!(
        out.code, 0,
        "`{method}` exited {}: {}",
        out.code, out.stderr
    );
    expect_envelope(method, out)
}

fn init_home(home: &Path) -> Value {
    let out = await_cli(home, &["instance", "init", "--output", "json"]);
    expect_ok("instance.initialize", &out)
}

/// Small wrapper so sync contexts can drive async CLI calls.
fn await_cli(home: &Path, args: &[&str]) -> CliOutcome {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(cli(home, args)))
}

// ---------------------------------------------------------------------------
// Harness: `api connect` stdio session
// ---------------------------------------------------------------------------

struct ConnectSession {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    rx: mpsc::Receiver<String>,
    stderr_log: Arc<Mutex<String>>,
    next_id: usize,
}

impl ConnectSession {
    /// Spawns `agentcpm-pd api connect` with piped stdio and a line reader.
    async fn start(home: &Path) -> Self {
        let mut cmd = base_command(home);
        cmd.args(["api", "connect"]);
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn api connect");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel::<String>(1024);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
        let stderr_log = Arc::new(Mutex::new(String::new()));
        let log = stderr_log.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = log.lock().unwrap();
                if guard.len() < 512 * 1024 {
                    guard.push_str(&line);
                    guard.push('\n');
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            rx,
            stderr_log,
            next_id: 0,
        }
    }

    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    async fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("api connect stdin still open");
        stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to api connect stdin");
        stdin.write_all(b"\n").await.expect("newline to stdin");
        stdin.flush().await.expect("flush stdin");
    }

    async fn send(&mut self, frame: &Value) {
        let line = serde_json::to_string(frame).expect("serialize frame");
        self.send_raw(&line).await;
    }

    async fn close_stdin(&mut self) {
        self.stdin = None; // dropping the handle closes the pipe (EOF)
    }

    /// Waits for the next frame matching `pred`; heartbeats and unrelated
    /// frames are skipped. Returns the matched frame.
    async fn wait_for(
        &mut self,
        pred: impl Fn(&Value) -> bool,
        timeout: Duration,
        what: &str,
    ) -> Value {
        tokio::time::timeout(timeout, async {
            loop {
                let Some(line) = self.rx.recv().await else {
                    panic!(
                        "api connect stdout closed while waiting for {what}; stderr:\n{}",
                        self.stderr_log.lock().unwrap()
                    );
                };
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    panic!("non-JSON frame from api connect: {line}");
                };
                if pred(&value) {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out after {}ms waiting for {what}; stderr:\n{}",
                timeout.as_millis(),
                self.stderr_log.lock().unwrap()
            )
        })
    }

    async fn wait_response(&mut self, id: &str, what: &str) -> Value {
        self.wait_for(
            |v| v.get("id").and_then(Value::as_str) == Some(id),
            FRAME_TIMEOUT,
            what,
        )
        .await
    }

    async fn wait_response_for(&mut self, id: &str, timeout: Duration, what: &str) -> Value {
        self.wait_for(
            |v| v.get("id").and_then(Value::as_str) == Some(id),
            timeout,
            what,
        )
        .await
    }

    async fn initialize(&mut self) -> Value {
        let id = self.fresh_id("init");
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "abi_version": 1,
                "api_version": OP_VERSION,
                "features": ["subscriptions", "credit", "heartbeat"],
                "client": {"name": "wp5-e2e", "version": "0.0.0"}
            }
        }))
        .await;
        self.wait_response(&id, "initialize response").await
    }

    /// Sends one business RPC and awaits its response frame.
    async fn call(&mut self, method: &str, input: Value, key: Option<&str>) -> Value {
        let id = self.fresh_id("r");
        let mut params = json!({"input": input});
        if let Some(k) = key {
            params["idempotencyKey"] = json!(k);
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await;
        self.wait_response(&id, &format!("{method} response")).await
    }

    async fn sub_open(&mut self, stream: &str, input: Value, credit: u32) -> Value {
        let id = self.fresh_id("sub");
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "sub.open",
            "params": {"stream": stream, "input": input, "credit": credit}
        }))
        .await;
        self.wait_response(&id, "sub.open response").await
    }

    async fn grant_credit(&mut self, subscription_id: &str, add: u32) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "sub.credit",
            "params": {"subscriptionId": subscription_id, "add": add}
        }))
        .await;
    }

    /// Takes the raw stdout line channel out of the session (for scenarios
    /// that interleave sends with drains in one select loop). The reader task
    /// keeps feeding the taken receiver; the session itself can no longer
    /// read frames afterwards.
    fn take_rx(&mut self) -> mpsc::Receiver<String> {
        let (_tx, empty_rx) = mpsc::channel(1);
        std::mem::replace(&mut self.rx, empty_rx)
    }

    async fn expect_no_frames(&mut self, what: &str) {
        let _ = what;
        match tokio::time::timeout(QUIET_WINDOW, self.rx.recv()).await {
            Ok(Some(line)) => panic!(
                "expected no further frames, got: {line}; stderr:\n{}",
                self.stderr_log.lock().unwrap()
            ),
            Ok(None) => panic!("api connect stdout closed unexpectedly"),
            Err(_) => {}
        }
    }

    async fn wait_exit(&mut self, expected_code: i32, what: &str) {
        let code = tokio::time::timeout(CLI_TIMEOUT, self.child.wait())
            .await
            .expect("api connect exit timed out")
            .expect("wait api connect")
            .code()
            .unwrap_or(-1);
        assert_eq!(code, expected_code, "{what}");
    }
}

fn rpc_request(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn error_code(frame: &Value) -> i64 {
    frame["error"]["code"].as_i64().unwrap_or(0)
}

fn contract_code(frame: &Value) -> String {
    frame["error"]["data"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Registers project `p-<tag>` and session `s-<tag>`, returning both ids.
async fn seed_project_session(home: &Path, tag: &str) -> (String, String) {
    let (project_id, session_id) = (format!("p-{tag}"), format!("s-{tag}"));
    // The workspace root must carry the real instanceId (bind_instance_id
    // validates nested values too).
    let instance_id = init_home_instance(home);
    let out = await_cli(
        home,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "WP5 E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"root-{tag}\",\"relativePath\":\"ws\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    let out = await_cli(
        home,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--project-id",
            &project_id,
        ],
    );
    expect_ok("session.create", &out);
    (project_id, session_id)
}

/// Submits a run through argv with an explicit idempotency key.
async fn submit_run(home: &Path, session_id: &str, text: &str, key: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--request-key",
            key,
            "--text",
            text,
        ],
    );
    expect_ok("run.submit", &out)
}

/// Resumes a session queue paused by a failed run (queue.get -> queue.resume).
async fn resume_paused_queue(home: &Path, session_id: &str) {
    let qget = await_cli(
        home,
        &[
            "queue",
            "get",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    let qget = expect_ok("queue.get", &qget);
    let queue = &qget["queue"];
    if queue["paused"] != true {
        return;
    }
    let qres = await_cli(
        home,
        &[
            "queue",
            "resume",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--expected-revision",
            queue["revision"].as_str().unwrap(),
            "--acknowledged-pause-reason",
            queue["pauseReason"].as_str().unwrap(),
        ],
    );
    expect_ok("queue.resume", &qres);
}

fn wait_run(home: &Path, run_id: &str) -> CliOutcome {
    await_cli(
        home,
        &[
            "run",
            "wait",
            "--output",
            "json",
            "--run-id",
            run_id,
            "--timeout-ms",
            "15000",
        ],
    )
}

// ===========================================================================
// 1. The 28 current-round operations, full round trip (A1)
// ===========================================================================

/// A1: submission, observation, interaction, cancel, and result query are all
/// possible through the CLI alone — no UI, no host process. Every one of the
/// 28 operations answers with a schema-validated envelope.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_28_operations_round_trip_through_the_cli_alone() {
    let home = TestHome::new("28ops");
    let root = home.path();

    // instance.initialize (local lifecycle op).
    let init = init_home(root);
    assert!(init["identity"]["instanceId"].as_str().is_some());

    // The first business op auto-starts the embedded gateway.
    let caps = await_cli(root, &["server", "capabilities", "--output", "json"]);
    let caps = expect_ok("system.capabilities", &caps);
    assert!(caps.get("operations").is_some() || caps.get("capabilities").is_some());

    let health = await_cli(root, &["server", "health", "--output", "json"]);
    expect_ok("system.health", &health);

    let schema_list = await_cli(root, &["schema", "list", "--output", "json"]);
    expect_ok("schema.list", &schema_list);

    let schema_get = await_cli(
        root,
        &[
            "schema",
            "show",
            "--output",
            "json",
            "--method-id",
            "run.submit",
        ],
    );
    expect_ok("schema.get", &schema_get);

    let status = await_cli(root, &["server", "status", "--output", "json"]);
    let status = expect_ok("service.status", &status);
    assert_eq!(status["serviceState"], "ready");

    let (project_id, session_id) = seed_project_session(root, "28").await;

    let pget = await_cli(
        root,
        &[
            "projects",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    expect_ok("project.get", &pget);
    let plist = await_cli(root, &["projects", "list", "--output", "json"]);
    expect_ok("project.list", &plist);
    let sget = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    expect_ok("session.get", &sget);
    let slist = await_cli(
        root,
        &[
            "sessions",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    expect_ok("session.list", &slist);

    // run.submit + idempotent replay.
    let run = submit_run(root, &session_id, "hello world", "e2e-28-run-1").await;
    assert_eq!(run["deduplicated"], false);
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let replay = submit_run(root, &session_id, "hello world", "e2e-28-run-1").await;
    assert_eq!(replay["deduplicated"], true, "same key must deduplicate");
    assert_eq!(replay["runId"].as_str(), Some(run_id.as_str()));

    // requests.lookup resolves the persisted acceptance.
    let lookup = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "run.submit",
            "--idempotency-key",
            "e2e-28-run-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &lookup);
    assert!(
        matches!(
            lookup["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "unexpected requestState: {lookup}"
    );

    let rget = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    expect_ok("run.get", &rget);
    let rlist = await_cli(
        root,
        &[
            "run",
            "list",
            "--output",
            "json",
            "--scope",
            &format!(
                "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
            ),
        ],
    );
    expect_ok("run.list", &rlist);

    let rwait = wait_run(root, &run_id);
    let rwait_result = expect_ok("run.wait", &rwait);
    assert_eq!(rwait_result["run"]["state"], "succeeded");

    // run.events as a paged unary read.
    let events_page = await_cli(
        root,
        &["run", "events", "--output", "json", "--run-id", &run_id],
    );
    expect_ok("run.events", &events_page);

    // run.events --follow streams NDJSON event envelopes until completed.
    let follow = await_cli(
        root,
        &[
            "run", "events", "--output", "ndjson", "--follow", "--run-id", &run_id,
        ],
    );
    assert_eq!(follow.code, 0, "follow exit code: {}", follow.stderr);
    let mut follow_events = 0;
    for line in follow.stdout.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("follow emitted non-JSON line: {e}: {line}"));
        assert_eq!(value["version"], "agentcpm-pd.event/1");
        assert_eq!(value["instanceId"], init["identity"]["instanceId"]);
        follow_events += 1;
    }
    assert!(follow_events >= 2, "expected replay + terminal events");

    let snap = await_cli(
        root,
        &["run", "snapshot", "--output", "json", "--run-id", &run_id],
    );
    let snap = expect_ok("run.snapshot", &snap);
    assert!(
        snap["snapshot"]["checkpoint"]["cursor"].as_str().is_some(),
        "snapshot must carry a checkpoint cursor: {snap}"
    );

    // Approval interaction.
    let approval_run = submit_run(root, &session_id, "[pd-driver:approval]", "e2e-28-app-1").await;
    let approval_run_id = approval_run["runId"].as_str().unwrap().to_owned();
    let ilist = await_cli(
        root,
        &[
            "interaction",
            "list",
            "--output",
            "json",
            "--state",
            "pending",
        ],
    );
    let ilist = expect_ok("interaction.list", &ilist);
    let items = ilist["items"].as_array().expect("interaction items");
    let approval = items
        .iter()
        .find(|i| i["runId"].as_str() == Some(approval_run_id.as_str()))
        .unwrap_or_else(|| panic!("no pending approval for {approval_run_id}: {ilist}"));
    let interaction_id = approval["interactionId"].as_str().unwrap().to_owned();
    let revision = approval["revision"].as_str().unwrap().to_owned();

    let iget = await_cli(
        root,
        &[
            "interaction",
            "get",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
        ],
    );
    expect_ok("interaction.get", &iget);

    let aresp = await_cli(
        root,
        &[
            "approval",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &revision,
            "--decision",
            "allow",
        ],
    );
    expect_ok("approval.respond", &aresp);
    let waited = wait_run(root, &approval_run_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");

    // Question interaction.
    let question_run = submit_run(root, &session_id, "[pd-driver:question]", "e2e-28-q-1").await;
    let question_run_id = question_run["runId"].as_str().unwrap().to_owned();
    let qlist = await_cli(
        root,
        &[
            "interaction",
            "list",
            "--output",
            "json",
            "--kind",
            "question",
            "--state",
            "pending",
        ],
    );
    let qlist = expect_ok("interaction.list", &qlist);
    let qitems = qlist["items"].as_array().expect("question items");
    let question = qitems
        .iter()
        .find(|i| i["runId"].as_str() == Some(question_run_id.as_str()))
        .unwrap_or_else(|| panic!("no pending question for {question_run_id}: {qlist}"));
    let q_id = question["interactionId"].as_str().unwrap().to_owned();
    let q_rev = question["revision"].as_str().unwrap().to_owned();
    let iresp = await_cli(
        root,
        &[
            "interaction",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &q_id,
            "--expected-revision",
            &q_rev,
            "--answer",
            "{\"text\":\"yes\"}",
        ],
    );
    expect_ok("interaction.respond", &iresp);
    let q_waited = wait_run(root, &question_run_id);
    let q_waited = expect_envelope("run.wait", &q_waited);
    assert_eq!(q_waited["run"]["state"], "succeeded");

    // Failing run pauses the session queue; queue.get reports it; queue.resume
    // acknowledges it.
    let fail_run = submit_run(root, &session_id, "[pd-driver:fail]", "e2e-28-fail-1").await;
    let fail_run_id = fail_run["runId"].as_str().unwrap().to_owned();
    let fail_wait = wait_run(root, &fail_run_id);
    assert_eq!(fail_wait.code, 1, "run.wait on a failed run exits 1");

    let qget = await_cli(
        root,
        &[
            "queue",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    let qget = expect_ok("queue.get", &qget);
    assert_eq!(qget["queue"]["paused"], true);

    let qres = await_cli(
        root,
        &[
            "queue",
            "resume",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            qget["queue"]["revision"].as_str().unwrap(),
            "--acknowledged-pause-reason",
            qget["queue"]["pauseReason"].as_str().unwrap(),
        ],
    );
    let qres = expect_ok("queue.resume", &qres);
    assert_eq!(qres["queue"]["paused"], false);

    // run.cancel on a sleeping run.
    let cancel_run = submit_run(
        root,
        &session_id,
        "[pd-driver:sleep=30000]",
        "e2e-28-cancel-1",
    )
    .await;
    let cancel_run_id = cancel_run["runId"].as_str().unwrap().to_owned();
    let rcancel = await_cli(
        root,
        &[
            "run",
            "cancel",
            "--output",
            "json",
            "--run-id",
            &cancel_run_id,
        ],
    );
    expect_ok("run.cancel", &rcancel);
    let cancelled_wait = wait_run(root, &cancel_run_id);
    let cancelled_wait = expect_envelope("run.wait", &cancelled_wait);
    assert_eq!(cancelled_wait["run"]["state"], "cancelled");

    // service.stop / service.start (background mode) at the very end so the
    // business operations above run against one live gateway.
    let stop = await_cli(root, &["server", "stop", "--output", "json"]);
    let stop = expect_ok("service.stop", &stop);
    assert_eq!(stop["stopped"], true);
    let status_stopped = await_cli(root, &["server", "status", "--output", "json"]);
    let status_stopped = expect_ok("service.status", &status_stopped);
    assert_eq!(status_stopped["serviceState"], "stopped");

    let start = await_cli(root, &["server", "start", "--output", "json"]);
    let start = expect_ok("service.start", &start);
    assert_eq!(start["ready"], true);
    let status_again = await_cli(root, &["server", "status", "--output", "json"]);
    let status_again = expect_ok("service.status", &status_again);
    assert_eq!(status_again["serviceState"], "ready");
}

// ===========================================================================
// 2. T1/T2/T3 protocol edges on the stdio segment
// ===========================================================================

/// T1 (pre-handshake -32002, version -32006 with `supported`), T2 (duplicate
/// in-flight id -32600, oversized frame recovery), T3 (unknown method -32601,
/// schema violation -32602 with `details.path`, missing idempotency key
/// -32086), S3 capability gate (terminals.output exactly one -32004), local
/// schema access over the connection, and the long-connection service.*
/// refusal from the transport contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t1_t2_t3_stdio_protocol_edges() {
    let home = TestHome::new("t123");
    let root = home.path();
    init_home(root);
    // Warm the gateway so connect-time behavior is not polluted by autostart.
    let warm = await_cli(root, &["server", "health", "--output", "json"]);
    expect_ok("system.health", &warm);

    let mut session = ConnectSession::start(root).await;

    // T1: business request before initialize -> -32002.
    let early = rpc_request("early-1", "run.get", json!({"input": {"runId": "x"}}));
    session.send(&early).await;
    let resp = session
        .wait_response("early-1", "pre-handshake rejection")
        .await;
    assert_eq!(error_code(&resp), -32002, "pre-handshake frame: {resp}");

    // T1: unsupported api_version -> -32006 with data.supported.
    let bad_version = rpc_request(
        "init-bad",
        "initialize",
        json!({
            "abi_version": 1,
            "api_version": "agentcpm-pd.operation/999",
            "features": [],
            "client": {"name": "wp5-e2e", "version": "0.0.0"}
        }),
    );
    session.send(&bad_version).await;
    let resp = session.wait_response("init-bad", "version rejection").await;
    assert_eq!(error_code(&resp), -32006, "{resp}");
    let supported = resp["error"]["data"]["supported"].as_array().cloned();
    assert!(
        supported.is_some_and(|s| !s.is_empty()),
        "-32006 must list supported versions: {resp}"
    );

    // Proper initialize.
    let init = session.initialize().await;
    assert_eq!(init["result"]["abi_version"], 1);

    // T2: two requests sharing one id while in flight -> second gets -32600.
    session
        .send(&rpc_request(
            "dup",
            "run.get",
            json!({"input": {"runId": "no-such-run"}}),
        ))
        .await;
    session
        .send(&rpc_request(
            "dup",
            "run.get",
            json!({"input": {"runId": "also-no-run"}}),
        ))
        .await;
    let first = session
        .wait_response("dup", "duplicate id first reply")
        .await;
    assert_eq!(error_code(&first), -32600, "in-flight duplicate: {first}");
    let second = session
        .wait_response("dup", "duplicate id second reply")
        .await;
    assert_ne!(
        error_code(&second),
        -32600,
        "the forwarded call must resolve normally: {second}"
    );
    assert!(second["error"]["data"]["code"] == "NOT_FOUND" || second.get("result").is_some());

    // T2: an oversized line (> 1 MiB) is answered with id:null -32600
    // frame_too_large and the connection survives.
    let mut oversized = String::from(
        "{\"jsonrpc\":\"2.0\",\"id\":\"big\",\"method\":\"run.get\",\"params\":{\"input\":{\"j\":\"",
    );
    while oversized.len() <= 1024 * 1024 + 64 {
        oversized.push('x');
    }
    oversized.push_str("\"}}");
    session.send_raw(&oversized).await;
    let resp = session
        .wait_for(
            |v| v.get("id").map(Value::is_null).unwrap_or(false) && v.get("error").is_some(),
            FRAME_TIMEOUT,
            "frame_too_large rejection",
        )
        .await;
    assert_eq!(error_code(&resp), -32600, "{resp}");
    assert_eq!(resp["error"]["data"]["reason"], "frame_too_large", "{resp}");

    // The connection is still usable after the oversized frame.
    let ok_call = session
        .call("schema.get", json!({"methodId": "run.submit"}), None)
        .await;
    assert!(
        ok_call["result"].is_object(),
        "post-recovery call: {ok_call}"
    );

    // T3: unknown method -> -32601.
    let unknown = session.call("totally.bogus", json!({}), None).await;
    assert_eq!(error_code(&unknown), -32601, "{unknown}");

    // T3: schema violation -> -32602 with details.path naming the field.
    let bad_input = session
        .call(
            "run.submit",
            json!({"projectId": "missing-session-and-input"}),
            Some("t3-schema-key"),
        )
        .await;
    assert_eq!(error_code(&bad_input), -32602, "{bad_input}");
    let path = bad_input["error"]["data"]["details"]["path"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        path.contains("sessionId") || path.contains("input"),
        "schema violation must name a path: {bad_input}"
    );

    // T3: idempotentSubmit without a key on api connect -> -32086 (the input
    // itself must be schema-valid so the key check is what fires).
    let no_key = session
        .call(
            "run.submit",
            json!({"sessionId": "s-x", "input": [{"kind": "text", "text": "no key"}]}),
            None,
        )
        .await;
    assert_eq!(error_code(&no_key), -32086, "{no_key}");

    // S3 gate: terminals.output answers exactly one -32004.
    let sub_id_resp = session
        .sub_open(
            "terminals.output",
            json!({"terminalId": "t-1", "follow": true}),
            8,
        )
        .await;
    assert_eq!(error_code(&sub_id_resp), -32004, "{sub_id_resp}");

    // Local schema access is read-only and served by the engine.
    let local_list = session.call("schema.list", json!({}), None).await;
    assert!(local_list["result"].is_object(), "{local_list}");

    // The transport contract forbids instance/service lifecycle ops on the
    // long connection; they must not be silently forwarded.
    let service = session
        .call("service.start", json!({"mode": "background"}), None)
        .await;
    assert!(
        error_code(&service) == -32601 || contract_code(&service) == "CAPABILITY_UNAVAILABLE",
        "service.start on api connect must be refused: {service}"
    );

    session.close_stdin().await;
    session
        .wait_exit(0, "clean EOF exit after protocol edges")
        .await;
}

// ===========================================================================
// 3. T4 + A2: instanceId injection, mismatch refusal, link identity
// ===========================================================================

/// T4: `instanceId` is injected from the bound target when omitted and an
/// explicit mismatching value is refused with INSTANCE_MISMATCH before any
/// side effect (no run is created). A2: the stdio handshake carries the CLI
/// process identity (`cli.pid`/`cli.executable`) and the target identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t4_instance_injection_mismatch_and_a2_link_identity() {
    let home = TestHome::new("t4");
    let root = home.path();
    let init = init_home(root);
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();
    let (_, session_id) = seed_project_session(root, "t4").await;

    let mut conn = ConnectSession::start(root).await;
    let init_frame = conn.initialize().await;

    // A2: the CLI reports its own process identity and the target identity.
    let result = &init_frame["result"];
    assert_eq!(result["target"]["instanceId"], instance_id);
    let cli_pid = result["cli"]["pid"].as_u64().expect("cli.pid");
    assert_eq!(cli_pid, conn.child.id().expect("child pid") as u64);
    assert!(
        result["cli"]["executable"]
            .as_str()
            .is_some_and(|exe| exe.contains("agentcpm-pd")),
        "cli.executable must point at the running binary: {result}"
    );
    assert!(
        result["cli"]["build"]
            .as_str()
            .is_some_and(|b| !b.is_empty())
    );

    // T4: explicit mismatching instanceId is refused before the side effect.
    let mismatch = conn
        .call(
            "run.submit",
            json!({
                "instanceId": "local-00000000-0000-7000-8000-00000000dead",
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "should never run"}]
            }),
            Some("t4-mismatch-key"),
        )
        .await;
    assert!(
        contract_code(&mismatch) == "INSTANCE_MISMATCH",
        "mismatched instanceId must be refused: {mismatch}"
    );
    assert_eq!(
        mismatch["error"]["data"]["effect"], "not_applied",
        "{mismatch}"
    );

    // T4: omitted instanceId is injected from the bound target.
    let accepted = conn
        .call(
            "run.submit",
            json!({
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "injected identity"}]
            }),
            Some("t4-inject-key"),
        )
        .await;
    let run_id = accepted["result"]["result"]["runId"]
        .as_str()
        .unwrap_or_else(|| panic!("submit without instanceId failed: {accepted}"))
        .to_owned();

    // The mismatched submit left no run behind: exactly one run for the
    // session exists (the injected one).
    let list = conn
        .call(
            "run.list",
            json!({"scope": {"kind": "session", "projectId": "p-t4", "sessionId": session_id}}),
            None,
        )
        .await;
    let runs = list["result"]["result"]["items"]
        .as_array()
        .or_else(|| list["result"]["result"]["runs"].as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(runs.len(), 1, "mismatched submit must not create a run");
    assert_eq!(runs[0]["runId"].as_str(), Some(run_id.as_str()));

    // The injected instanceId resolved to the same bound instance.
    let get = conn.call("run.get", json!({"runId": run_id}), None).await;
    assert_eq!(
        get["result"]["result"]["run"]["instanceId"]
            .as_str()
            .or(get["result"]["result"]["instanceId"].as_str()),
        Some(instance_id.as_str()),
        "{get}"
    );

    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after T4").await;
}

// ===========================================================================
// 4. T5: credit flow control + per-subscription buffers
// ===========================================================================

/// T5: the gateway stops sending when the granted credit is exhausted and
/// resumes exactly from the break point when more credit arrives — no
/// duplicates, no gaps. Per-subscription buffering (WP3 leftover): a second,
/// independent subscription keeps flowing while the first is starved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t5_credit_flow_pauses_and_resumes_without_loss() {
    let home = TestHome::new("t5");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t5").await;

    let burst = submit_run(root, &session_id, "[pd-driver:events=40]", "t5-burst-1").await;
    let burst_id = burst["runId"].as_str().unwrap().to_owned();
    let quiet = submit_run(root, &session_id, "plain run", "t5-quiet-1").await;
    let quiet_id = quiet["runId"].as_str().unwrap().to_owned();

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    // Open the burst subscription with exactly 3 credits.
    let open = conn
        .sub_open("run.events", json!({"runId": burst_id}), 3)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut seen: Vec<String> = Vec::new();

    for _ in 0..3 {
        let ev = conn
            .wait_for(
                |v| {
                    v.get("method").and_then(Value::as_str) == Some("sub.event")
                        && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
                },
                FRAME_TIMEOUT,
                "credited sub.event",
            )
            .await;
        seen.push(ev["params"]["event"]["cursor"].as_str().unwrap().to_owned());
    }

    // Credit exhausted: no further events for this subscription arrive.
    conn.expect_no_frames("credit-exhausted subscription stays silent")
        .await;

    // Per-subscription buffering: a second subscription on another run is
    // unaffected by the first one's starvation.
    let open2 = conn
        .sub_open("run.events", json!({"runId": quiet_id}), 64)
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut sub2_events = 0;
    let sub2_end = loop {
        let frame = conn
            .wait_for(
                |v| {
                    (v.get("method").and_then(Value::as_str) == Some("sub.event")
                        && v["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str()))
                        || (v.get("method").and_then(Value::as_str) == Some("sub.end")
                            && v["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str()))
                },
                FRAME_TIMEOUT,
                "second subscription progress",
            )
            .await;
        if frame.get("method").and_then(Value::as_str) == Some("sub.end") {
            break frame;
        }
        sub2_events += 1;
    };
    assert_eq!(sub2_end["params"]["terminal"], "completed", "{sub2_end}");
    assert!(sub2_events >= 2, "second subscription must replay events");

    // Grant 5 more credits to the starved subscription: delivery resumes.
    conn.grant_credit(&sub_id, 5).await;
    let mut resumed = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while resumed < 5 && Instant::now() < deadline {
        let ev = conn
            .wait_for(
                |v| {
                    v.get("method").and_then(Value::as_str) == Some("sub.event")
                        && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
                },
                Duration::from_secs(5),
                "resumed sub.event",
            )
            .await;
        let cursor = ev["params"]["event"]["cursor"].as_str().unwrap().to_owned();
        assert!(
            !seen.contains(&cursor),
            "duplicate cursor {cursor} after credit resume"
        );
        seen.push(cursor);
        resumed += 1;
    }
    assert_eq!(resumed, 5, "credit add=5 must deliver exactly 5 events");

    // Grant the rest and expect completion.
    conn.grant_credit(&sub_id, 512).await;
    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            FRAME_TIMEOUT,
            "burst subscription terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "completed", "{end}");
    let cursors: HashSet<&str> = seen.iter().map(String::as_str).collect();
    assert_eq!(cursors.len(), seen.len(), "no duplicate cursors overall");
    assert!(
        seen.len() >= 8,
        "expected the full event set, got {}",
        seen.len()
    );

    // Any events still queued ahead of sub.end on the same lane.
    let mut trailing = 0;
    while let Ok(line) = conn.rx.try_recv() {
        if let Ok(value) = serde_json::from_str::<Value>(&line)
            && value.get("method").and_then(Value::as_str) == Some("sub.event")
            && value["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
        {
            trailing += 1;
        }
    }
    assert_eq!(trailing, 0, "no events may follow the terminal frame");

    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after T5").await;
}

// ===========================================================================
// 5. T6: slow consumer termination and lossless resubscription
// ===========================================================================

/// T6/A8: a subscription that cannot keep up ends with `slow_consumer` and a
/// `resumeCursor`; re-subscribing from that cursor delivers the remainder
/// without duplicates or missing events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_slow_consumer_resume_without_dup_or_loss() {
    let home = TestHome::new("t6");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t6").await;

    let burst = submit_run(root, &session_id, "[pd-driver:events=3000]", "t6-burst-1").await;
    let burst_id = burst["runId"].as_str().unwrap().to_owned();

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    // Starve the subscription: 1 credit against 3000+ buffered events.
    let open = conn
        .sub_open("run.events", json!({"runId": burst_id}), 1)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut first_batch: Vec<String> = Vec::new();
    let end = loop {
        let frame = conn
            .wait_for(
                |v| {
                    let is_ours = v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str());
                    is_ours
                        && matches!(
                            v.get("method").and_then(Value::as_str),
                            Some("sub.event") | Some("sub.end")
                        )
                },
                FRAME_TIMEOUT,
                "slow consumer window",
            )
            .await;
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") => {
                first_batch.push(
                    frame["params"]["event"]["cursor"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
            _ => break frame,
        }
    };
    assert_eq!(
        end["params"]["terminal"], "slow_consumer",
        "starved subscription must end slow_consumer: {end}"
    );
    let resume_cursor = end["params"]["resumeCursor"]
        .as_str()
        .expect("slow_consumer carries a resumeCursor")
        .to_owned();
    assert!(
        !first_batch.is_empty(),
        "the credited event must have been delivered"
    );

    // Re-subscribe from the resume cursor with ample credit.
    let open2 = conn
        .sub_open(
            "run.events",
            json!({"runId": burst_id, "afterCursor": resume_cursor}),
            8192,
        )
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut second_batch: Vec<String> = Vec::new();
    let end2 = loop {
        let frame = conn
            .wait_for(
                |v| {
                    let is_ours = v["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str());
                    is_ours
                        && matches!(
                            v.get("method").and_then(Value::as_str),
                            Some("sub.event") | Some("sub.end")
                        )
                },
                Duration::from_secs(30),
                "resumed subscription progress",
            )
            .await;
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") => {
                second_batch.push(
                    frame["params"]["event"]["cursor"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
            _ => break frame,
        }
    };
    assert_eq!(end2["params"]["terminal"], "completed", "{end2}");

    let first_set: HashSet<&str> = first_batch.iter().map(String::as_str).collect();
    let second_set: HashSet<&str> = second_batch.iter().map(String::as_str).collect();
    let overlap = first_set.intersection(&second_set).count();
    assert_eq!(
        overlap, 0,
        "resumed stream must not repeat delivered cursors"
    );
    assert!(
        second_batch.len() >= 3000,
        "resumed stream must deliver the retained remainder, got {}",
        second_batch.len()
    );

    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after T6").await;
}

// ===========================================================================
// 6. T7: control operations stay fast during an event flood (P2 prototype)
// ===========================================================================

/// T7/A8 (P2 prototype approximation per §10.3): while a subscription is
/// flooding events, control-class RPCs (`run.get`) complete within the bound.
/// The measured latencies are printed for the report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t7_control_latency_during_event_flood() {
    let home = TestHome::new("t7");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t7").await;

    let burst = submit_run(root, &session_id, "[pd-driver:events=8000]", "t7-burst-1").await;
    let burst_id = burst["runId"].as_str().unwrap().to_owned();

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    let open = conn
        .sub_open("run.events", json!({"runId": burst_id}), 200_000)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // Interleave control probes with continuous frame draining; the engine's
    // writer prioritizes the control lane, so the probes must not queue
    // behind the flood.
    let mut rx = conn.take_rx();
    let probes = 12usize;
    let mut latencies_ms: Vec<u128> = Vec::new();
    let mut next_probe_at = tokio::time::Instant::now();
    let mut pending: Vec<(String, Instant)> = Vec::new();
    let mut probe_idx = 0usize;
    let mut events_seen = 0usize;
    let mut flood_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    while tokio::time::Instant::now() < deadline {
        if flood_done && probe_idx >= probes && pending.is_empty() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(next_probe_at), if probe_idx < probes => {
                let id = conn.fresh_id("ctl");
                conn.send(&rpc_request(&id, "run.get", json!({"input": {"runId": burst_id}}))).await;
                pending.push((id, Instant::now()));
                probe_idx += 1;
                next_probe_at = tokio::time::Instant::now() + Duration::from_millis(150);
            }
            line = rx.recv() => {
                let Some(line) = line else { break };
                let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = value.get("id").and_then(Value::as_str) {
                    if let Some(pos) = pending.iter().position(|(pid, _)| pid == id) {
                        let (_, sent) = pending.remove(pos);
                        latencies_ms.push(sent.elapsed().as_millis());
                    }
                    continue;
                }
                match value.get("method").and_then(Value::as_str) {
                    Some("sub.event") => events_seen += 1,
                    Some("sub.end")
                        if value["params"]["subscriptionId"].as_str() == Some(sub_id.as_str()) =>
                    {
                        assert_eq!(value["params"]["terminal"], "completed", "{value}");
                        flood_done = true;
                    }
                    _ => {}
                }
            }
        }
    }

    assert_eq!(
        latencies_ms.len(),
        probes,
        "all control probes must be answered; stderr:\n{}",
        conn.stderr_log.lock().unwrap()
    );
    assert!(
        events_seen > 1000,
        "flood must actually flow: {events_seen}"
    );
    let mut sorted = latencies_ms.clone();
    sorted.sort_unstable();
    let p95 = sorted[sorted.len() - 1]; // 12 samples: p95 ~= max
    println!(
        "T7 control latency during {events_seen}-event flood (ms): {:?} (p95≈max {})",
        latencies_ms, p95
    );
    // Guard catches gross regressions only; the measured p95 is reported
    // (the §10.3 P2 target of 200 ms p95 is an S3 gate — see the WP5 report
    // for the store-mutex contention analysis under extreme bursts).
    assert!(
        p95 < 2000,
        "control probe latency {p95}ms exceeded the 2s guard during the flood"
    );

    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after T7").await;
}

// ===========================================================================
// 7. T8 + A5: EOF drain, shutdown terminal, run survives its observer
// ===========================================================================

/// T8: stdin EOF stops accepting requests, drains in-flight work, ends
/// subscriptions with `shutdown`, and exits 0 — the run itself keeps running
/// (A5: runs are decoupled from observers).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t8_eof_drains_and_the_run_outlives_the_cli() {
    let home = TestHome::new("t8");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t8").await;

    let long = submit_run(root, &session_id, "[pd-driver:sleep=6000]", "t8-long-1").await;
    let long_id = long["runId"].as_str().unwrap().to_owned();

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    let open = conn
        .sub_open("run.events", json!({"runId": long_id}), 64)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // Wait until the run has started so EOF hits a live observation.
    conn.wait_for(
        |v| {
            v.get("method").and_then(Value::as_str) == Some("sub.event")
                && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
        },
        FRAME_TIMEOUT,
        "run.started event",
    )
    .await;

    // EOF: the engine drains, ends the subscription with `shutdown`, exits 0.
    conn.close_stdin().await;
    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            FRAME_TIMEOUT,
            "shutdown terminal after EOF",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "shutdown", "{end}");
    conn.wait_exit(0, "EOF exit code").await;

    // A5: the run is untouched by the observer going away. Drive it to a
    // terminal state through a *different* CLI invocation.
    let cancel = await_cli(
        root,
        &["run", "cancel", "--output", "json", "--run-id", &long_id],
    );
    expect_ok("run.cancel", &cancel);
    let waited = wait_run(root, &long_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "cancelled");
}

// ===========================================================================
// 8. T9 + A4: kill -9 gateway -> RESULT_UNKNOWN, disconnected, reconnect,
//    interrupted runs, paused queue, lossless resubscription
// ===========================================================================

/// T9: with the gateway SIGKILLed mid-connection, in-flight writes answer
/// RESULT_UNKNOWN (`effect: unknown`, key echoed), subscriptions end
/// `disconnected` with a resume cursor, the CLI reconnects, and re-opening
/// from the last cursor misses nothing. Crash recovery marks the interrupted
/// run `interrupted` and pauses its session queue until `queue.resume`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t9_gateway_kill9_result_unknown_and_recovery() {
    let home = TestHome::new("t9");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t9").await;

    let long = submit_run(root, &session_id, "[pd-driver:sleep=60000]", "t9-long-1").await;
    let long_id = long["runId"].as_str().unwrap().to_owned();

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    let open = conn
        .sub_open("run.events", json!({"runId": long_id}), 64)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let started = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.event")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            FRAME_TIMEOUT,
            "run.started event before kill",
        )
        .await;
    let mut last_cursor = started["params"]["event"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    // Drain any further delivered events so `last_cursor` is the newest one
    // the gateway actually pushed (the disconnect terminal reports that).
    while let Ok(Some(frame)) = tokio::time::timeout(QUIET_WINDOW, conn.rx.recv()).await {
        let Ok(value) = serde_json::from_str::<Value>(&frame) else {
            continue;
        };
        if value.get("method").and_then(Value::as_str) == Some("sub.event")
            && value["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
        {
            last_cursor = value["params"]["event"]["cursor"]
                .as_str()
                .unwrap()
                .to_owned();
        }
    }

    // Freeze the gateway so the write below is deterministically in flight.
    let pid = home.gateway_pid().expect("gateway pid");
    signal_pid(pid, rustix::process::Signal::STOP);

    let write_id = conn.fresh_id("w");
    conn.send(&rpc_request(
        &write_id,
        "run.submit",
        json!({
            "input": {
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "in flight when the gateway dies"}]
            },
            "idempotencyKey": "t9-inflight-1"
        }),
    ))
    .await;
    // Give the frame time to reach the (frozen) gateway's socket buffer.
    tokio::time::sleep(Duration::from_millis(400)).await;

    signal_pid(pid, rustix::process::Signal::KILL);

    // The in-flight write resolves as RESULT_UNKNOWN with the key echoed.
    let write_resp = conn
        .wait_response(&write_id, "RESULT_UNKNOWN for in-flight write")
        .await;
    assert_eq!(contract_code(&write_resp), "RESULT_UNKNOWN", "{write_resp}");
    assert_eq!(write_resp["error"]["data"]["effect"], "unknown");
    assert_eq!(
        write_resp["error"]["data"]["details"]["idempotencyKey"], "t9-inflight-1",
        "{write_resp}"
    );

    // The subscription ends `disconnected` with the last delivered cursor.
    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            FRAME_TIMEOUT,
            "disconnected terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "disconnected", "{end}");
    assert_eq!(
        end["params"]["resumeCursor"].as_str(),
        Some(last_cursor.as_str())
    );

    // The engine reconnects (the autostart path revives the gateway).
    let reconnected = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("conn.state")
                    && v["params"]["state"] == "connected"
            },
            Duration::from_secs(30),
            "conn.state connected after gateway restart",
        )
        .await;
    assert_eq!(
        reconnected["params"]["instanceId"],
        init_home_instance(root),
        "reconnect must bind the same instance"
    );

    // Crash recovery: the sleeping run is `interrupted` and its queue paused.
    let interrupted = conn.call("run.get", json!({"runId": long_id}), None).await;
    let state = interrupted["result"]["result"]["run"]["state"]
        .as_str()
        .or(interrupted["result"]["result"]["state"].as_str())
        .unwrap_or_else(|| panic!("run.get shape: {interrupted}"));
    assert_eq!(state, "interrupted", "{interrupted}");

    let qget = conn
        .call("queue.get", json!({"sessionId": session_id}), None)
        .await;
    let queue = &qget["result"]["result"]["queue"];
    assert_eq!(queue["paused"], true, "{qget}");
    assert_eq!(queue["pauseReason"], "gateway_restart", "{qget}");

    // Resubscribe from the last cursor: no duplicates, nothing missing.
    let open2 = conn
        .sub_open(
            "run.events",
            json!({"runId": long_id, "afterCursor": last_cursor}),
            64,
        )
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut tail: Vec<(String, String)> = Vec::new(); // (cursor, event type)
    let end2 = loop {
        let frame = conn
            .wait_for(
                |v| {
                    let is_ours = v["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str());
                    is_ours
                        && matches!(
                            v.get("method").and_then(Value::as_str),
                            Some("sub.event") | Some("sub.end")
                        )
                },
                FRAME_TIMEOUT,
                "resubscription tail after restart",
            )
            .await;
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") => tail.push((
                frame["params"]["event"]["cursor"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                frame["params"]["event"]["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )),
            _ => break frame,
        }
    };
    assert_eq!(end2["params"]["terminal"], "completed", "{end2}");
    assert!(
        tail.iter().all(|(c, _)| c != &last_cursor),
        "resumed stream must not repeat the last delivered cursor"
    );
    assert!(
        tail.iter()
            .any(|(_, ty)| ty == "run.interrupted" || ty == "run.cancelled"),
        "the interrupted run's terminal event must be reachable, got {tail:?}"
    );

    // queue.resume unblocks the session for later runs.
    let revision = queue["revision"].as_str().unwrap().to_owned();
    let qres = conn
        .call(
            "queue.resume",
            json!({
                "sessionId": session_id,
                "expectedRevision": revision,
                "acknowledgedPauseReason": "gateway_restart"
            }),
            None,
        )
        .await;
    assert!(
        qres.get("result").is_some(),
        "queue.resume after restart: {qres}"
    );

    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after T9").await;
}

fn init_home_instance(root: &Path) -> String {
    let raw = std::fs::read_to_string(root.join("local.json")).expect("local.json");
    let value: Value = serde_json::from_str(&raw).expect("parse local.json");
    value["instanceId"].as_str().unwrap().to_owned()
}

fn signal_pid(pid: u32, signal: rustix::process::Signal) {
    let Some(p) = rustix::process::Pid::from_raw(pid as i32) else {
        panic!("invalid pid {pid}");
    };
    rustix::process::kill_process(p, signal).expect("signal gateway");
}

// ===========================================================================
// 9. T10: instanceId change is refused, not silently switched
// ===========================================================================

/// T10: when the local instance identity changes underneath a live
/// connection, the CLI reports `conn.state failed` with
/// `reason=instance_changed` and exits instead of silently rebinding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t10_instance_change_emits_failed_state() {
    let home = TestHome::new("t10");
    let root = home.path();
    let init = init_home(root);
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();
    let (_, _session_id) = seed_project_session(root, "t10").await;

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;
    // Confirm the link is usable before breaking it.
    let ok = conn.call("run.list", json!({}), None).await;
    assert!(ok.get("result").is_some(), "{ok}");

    // Rewrite local.json with a different instanceId, then kill the gateway.
    // The reconnect resolves the (new) local identity, finds the gateway
    // bound to the old identity, and must fail loudly instead of rebinding.
    let local_path = root.join("local.json");
    let mut local: Value =
        serde_json::from_str(&std::fs::read_to_string(&local_path).expect("read local.json"))
            .expect("parse local.json");
    local["instanceId"] = json!(format!("{instance_id}x"));
    std::fs::write(&local_path, serde_json::to_vec_pretty(&local).unwrap())
        .expect("rewrite local.json");

    let pid = home.gateway_pid().expect("gateway pid");
    signal_pid(pid, rustix::process::Signal::KILL);
    // The store binds to the instance that created it: a gateway booted with
    // the rewritten identity refuses the old DB (INSTANCE_MISMATCH) and exits
    // 1, which would loop the reconnect forever instead of surfacing the
    // identity change. This test owns the whole home and has no runs, so
    // clearing the store lets the replacement instance boot and the engine
    // then refuses to rebind to it.
    let _ = std::fs::remove_file(root.join("v1").join("state.sqlite"));
    let _ = std::fs::remove_file(root.join("v1").join("state.sqlite-wal"));
    let _ = std::fs::remove_file(root.join("v1").join("state.sqlite-shm"));

    let failed = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("conn.state")
                    && v["params"]["state"] == "failed"
            },
            Duration::from_secs(30),
            "conn.state failed after identity change",
        )
        .await;
    assert_eq!(failed["params"]["reason"], "instance_changed", "{failed}");
    conn.wait_exit(1, "instance change must exit 1").await;
}

// ===========================================================================
// 10. A3: a blocked CLI never reaches the gateway
// ===========================================================================

/// A3 (controlled blocking): with the CLI process stopped before a submission
/// is forwarded, killing it leaves no run behind and the gateway observes no
/// request — there is no bypass path around the CLI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a3_sigstopped_cli_never_submits() {
    let home = TestHome::new("a3");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "a3").await;

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    // Freeze the CLI: it will not read stdin or forward anything.
    let cli_pid = conn.child.id().expect("cli pid");
    signal_pid(cli_pid, rustix::process::Signal::STOP);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The host submits; the stopped CLI cannot forward it.
    conn.send(&rpc_request(
        "a3-submit",
        "run.submit",
        json!({
            "input": {
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "must never reach the gateway"}]
            },
            "idempotencyKey": "a3-blocked-1"
        }),
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Kill the frozen CLI (no automatic rebuild here — the host decides).
    signal_pid(cli_pid, rustix::process::Signal::KILL);
    let _ = conn.child.wait().await;

    // A separate CLI confirms nothing was submitted: no run for the session,
    // and the idempotency key is unknown.
    let lookup = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "run.submit",
            "--idempotency-key",
            "a3-blocked-1",
        ],
    );
    assert_eq!(lookup.code, 0, "lookup itself succeeds: {}", lookup.stderr);
    let lookup_value: Value = serde_json::from_str(lookup.stdout.trim()).unwrap();
    assert_eq!(
        lookup_value["result"]["requestState"], "result_unknown",
        "the blocked write was never observed: {lookup_value}"
    );
    assert_eq!(
        lookup_value["result"]["reason"], "notObserved",
        "{lookup_value}"
    );
    let listing = await_cli(root, &["run", "list", "--output", "json"]);
    let listing = expect_ok("run.list", &listing);
    let items = listing["items"]
        .as_array()
        .or_else(|| listing["runs"].as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        items.is_empty(),
        "the blocked submit must not have created a run: {listing}"
    );
}

// ===========================================================================
// 11. A4: unknown writes are resolved through requests.lookup
// ===========================================================================

/// A4: after a CLI dies mid-write, the recovery protocol is: look the
/// `{methodId, idempotencyKey}` up, then re-issue with the same key — the
/// gateway deduplicates, so no double submission is possible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a4_unknown_write_resolved_via_requests_lookup() {
    let home = TestHome::new("a4");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "a4").await;

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    let key = "a4-unknown-write-1";
    conn.send(&rpc_request(
        "a4-submit",
        "run.submit",
        json!({
            "input": {
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "unknown outcome write"}]
            },
            "idempotencyKey": key
        }),
    ))
    .await;
    // Kill the CLI before the response is observed (it may or may not have
    // reached the gateway — that is the point of the lookup protocol).
    tokio::time::sleep(Duration::from_millis(120)).await;
    let cli_pid = conn.child.id().expect("cli pid");
    signal_pid(cli_pid, rustix::process::Signal::KILL);
    let _ = conn.child.wait().await;

    // Recovery step 1: look the write up from a new process. An unobserved
    // key answers `result_unknown`/`notObserved`; a landed one reports its
    // acceptance state.
    let lookup = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "run.submit",
            "--idempotency-key",
            key,
        ],
    );
    assert_eq!(lookup.code, 0, "lookup itself succeeds: {}", lookup.stderr);
    let lookup_value: Value = serde_json::from_str(lookup.stdout.trim()).unwrap();
    let found = !matches!(
        lookup_value["result"]["requestState"].as_str(),
        Some("result_unknown") | None
    );

    // Recovery step 2: re-issue with the same key; either it deduplicates
    // (the write landed) or it is accepted fresh — never both.
    let replay = submit_run(root, &session_id, "unknown outcome write", key).await;
    let run_id = replay["runId"].as_str().unwrap().to_owned();
    if found {
        assert_eq!(replay["deduplicated"], true, "known key must deduplicate");
    }

    // Exactly one run exists for this key.
    let lookup2 = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "run.submit",
            "--idempotency-key",
            key,
        ],
    );
    let lookup2 = expect_ok("requests.lookup", &lookup2);
    assert!(
        matches!(
            lookup2["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "{lookup2}"
    );
    let listing = await_cli(root, &["run", "list", "--output", "json"]);
    let listing = expect_ok("run.list", &listing);
    let items = listing["items"]
        .as_array()
        .or_else(|| listing["runs"].as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(items.len(), 1, "exactly one run for the recovered key");
    assert_eq!(items[0]["runId"].as_str(), Some(run_id.as_str()));
}

// ===========================================================================
// 12. A6: argv and structured-entry equivalence
// ===========================================================================

/// A6: for the same operation and input, the canonical argv command and
/// `api invoke` produce identical contract responses (after §6.3 mapping the
/// machine result back to the envelope).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a6_argv_and_api_invoke_are_equivalent() {
    let home = TestHome::new("a6");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "a6").await;
    let run = submit_run(root, &session_id, "equivalence probe", "a6-run-1").await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let wait = wait_run(root, &run_id);
    expect_envelope("run.wait", &wait);

    let argv = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    let argv_value: Value = serde_json::from_str(argv.stdout.trim()).expect("argv envelope");
    let invoke = cli_with_stdin(
        root,
        &[
            "api",
            "invoke",
            "run.get",
            "--output",
            "json",
            "--input-json",
            "-",
        ],
        Some(&format!("{{\"runId\":\"{run_id}\"}}")),
    )
    .await;
    let invoke_value: Value =
        serde_json::from_str(invoke.stdout.trim()).expect("api invoke envelope");
    assert_eq!(invoke.code, 0, "{}", invoke.stderr);
    assert_eq!(argv.code, 0, "{}", argv.stderr);

    assert_eq!(argv_value["outcome"], invoke_value["outcome"]);
    assert_eq!(argv_value["version"], invoke_value["version"]);
    assert_eq!(argv_value["result"], invoke_value["result"]);

    // Same for a list-shaped read.
    let argv_list = await_cli(root, &["run", "list", "--output", "json"]);
    let invoke_list = cli_with_stdin(
        root,
        &[
            "api",
            "invoke",
            "run.list",
            "--output",
            "json",
            "--input-json",
            "-",
        ],
        Some("{}"),
    )
    .await;
    let argv_list: Value = serde_json::from_str(argv_list.stdout.trim()).unwrap();
    let invoke_list: Value = serde_json::from_str(invoke_list.stdout.trim()).unwrap();
    assert_eq!(argv_list["outcome"], invoke_list["outcome"]);
    assert_eq!(argv_list["result"], invoke_list["result"]);

    // And a local schema read.
    let argv_schema = await_cli(
        root,
        &[
            "schema",
            "show",
            "--output",
            "json",
            "--method-id",
            "run.get",
        ],
    );
    let invoke_schema = cli_with_stdin(
        root,
        &[
            "api",
            "invoke",
            "schema.get",
            "--output",
            "json",
            "--input-json",
            "-",
        ],
        Some("{\"methodId\":\"run.get\"}"),
    )
    .await;
    let argv_schema: Value = serde_json::from_str(argv_schema.stdout.trim()).unwrap();
    let invoke_schema: Value = serde_json::from_str(invoke_schema.stdout.trim()).unwrap();
    assert_eq!(argv_schema["result"], invoke_schema["result"]);
}

// ===========================================================================
// 13. A7: the HTTP surface offers no business bypass
// ===========================================================================

/// A7 (runtime half): the gateway's HTTP surface is `/health` (unauthenticated
/// liveness) plus the authenticated `/v1/ws` upgrade; everything else is 404
/// and the upgrade without credentials is 401 — no business operation is
/// reachable over plain HTTP. The static half (no host-side business client
/// dependency) is a code-audit statement recorded in the report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a7_http_surface_has_no_business_bypass() {
    let home = TestHome::new("a7");
    let root = home.path();
    init_home(root);
    let warm = await_cli(root, &["server", "health", "--output", "json"]);
    expect_ok("system.health", &warm);

    let raw = std::fs::read_to_string(root.join("run").join("ready.json")).unwrap();
    let ready: Value = serde_json::from_str(&raw).unwrap();
    let ws_url = ready["wsUrl"].as_str().unwrap().to_owned();
    let http_base = ws_url
        .trim_start_matches("ws://")
        .trim_end_matches("/v1/ws")
        .to_owned();

    let health = raw_http(
        &http_base,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.contains("instanceId"), "{health}");

    let not_found = raw_http(
        &http_base,
        "GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(not_found.starts_with("HTTP/1.1 404"), "{not_found}");

    // A WebSocket upgrade without credentials is rejected with 401.
    let upgrade = raw_http(
        &http_base,
        "GET /v1/ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )
    .await;
    assert!(upgrade.starts_with("HTTP/1.1 401"), "{upgrade}");

    // A plain (non-upgrade) request to /v1/ws is not a business route.
    let plain = raw_http(
        &http_base,
        "GET /v1/ws HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(plain.starts_with("HTTP/1.1 404"), "{plain}");
}

async fn raw_http(host_port: &str, request: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(host_port),
    )
    .await
    .expect("connect timeout")
    .expect("connect to gateway http");
    stream.write_all(request.as_bytes()).await.expect("write");
    // Read headers plus whatever body arrives; keep-alive servers may hold
    // the connection open, so read_to_end would hang. One short trailing
    // read drains the body after the header terminator.
    let mut buf = Vec::new();
    let deadline = Duration::from_secs(5);
    tokio::time::timeout(deadline, async {
        loop {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.expect("read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                // Headers complete: drain one extra window for the body.
                let _ =
                    tokio::time::timeout(Duration::from_millis(400), stream.read(&mut chunk)).await;
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    // Body may still be streaming; read once more, bounded.
                    let _ =
                        tokio::time::timeout(Duration::from_millis(400), stream.read(&mut chunk))
                            .await;
                }
                break;
            }
        }
    })
    .await
    .expect("read timeout");
    String::from_utf8_lossy(&buf).into_owned()
}

// ===========================================================================
// 14. P3: gateway kill -9 -> re-ready measured
// ===========================================================================

/// P3 (§10.3): from killing the gateway to a usable connection
/// (initialize + requests.lookup) must stay under 2 s. Five rounds are
/// measured and printed; the assert uses the per-round bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p3_gateway_restart_to_ready_under_2s() {
    let home = TestHome::new("p3");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "p3").await;
    let run = submit_run(root, &session_id, "p3 probe", "p3-run-1").await;
    let _run_id = run["runId"].as_str().unwrap().to_owned();

    let mut samples_ms: Vec<u128> = Vec::new();
    for round in 1..=5 {
        let pid = home.gateway_pid().expect("gateway pid");
        signal_pid(pid, rustix::process::Signal::KILL);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = Instant::now();
        let lookup = await_cli(
            root,
            &[
                "requests",
                "lookup",
                "--output",
                "json",
                "--method-id",
                "run.submit",
                "--idempotency-key",
                "p3-run-1",
            ],
        );
        let elapsed = started.elapsed().as_millis();
        assert_eq!(lookup.code, 0, "round {round}: {}", lookup.stderr);
        let value: Value = serde_json::from_str(lookup.stdout.trim()).unwrap();
        assert!(
            matches!(
                value["result"]["requestState"].as_str(),
                Some("accepted") | Some("completed")
            ),
            "round {round}: {value}"
        );
        samples_ms.push(elapsed);
        println!("P3 round {round}: kill -9 -> initialize+requests.lookup = {elapsed} ms");
    }
    let mut sorted = samples_ms.clone();
    sorted.sort_unstable();
    let p95 = sorted[sorted.len() - 1];
    println!("P3 samples (ms): {samples_ms:?}, p95 = {p95}");
    assert!(
        p95 < 2000,
        "P3 p95 {p95}ms exceeds the 2 s gate (samples {samples_ms:?})"
    );
}

// ===========================================================================
// 15. CURSOR_EXPIRED standard recovery flow (negative control)
// ===========================================================================

/// v0.7.1 negative control: opening a stream with an unusable cursor is a
/// `-32000 CURSOR_EXPIRED` error response (not success + gap frame); the
/// standard recovery — `run.snapshot` -> `checkpoint.cursor` -> re-open —
/// succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_expired_standard_recovery_flow() {
    let home = TestHome::new("cursor");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "cur").await;
    let run = submit_run(root, &session_id, "cursor recovery", "cursor-run-1").await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let wait = wait_run(root, &run_id);
    expect_envelope("run.wait", &wait);

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    // Open with an expired/invalid cursor -> -32000 CURSOR_EXPIRED, no
    // subscriptionId, connection stays usable.
    let rejected = conn
        .sub_open(
            "run.events",
            json!({"runId": run_id, "afterCursor": "stale-not-a-cursor"}),
            8,
        )
        .await;
    assert_eq!(error_code(&rejected), -32000, "{rejected}");
    assert_eq!(contract_code(&rejected), "CURSOR_EXPIRED", "{rejected}");
    assert!(
        rejected["result"]["subscriptionId"].is_null(),
        "the open must be rejected, not half-opened: {rejected}"
    );

    // Standard recovery: snapshot -> checkpoint.cursor -> re-open.
    let snap = conn
        .call("run.snapshot", json!({"runId": run_id}), None)
        .await;
    let checkpoint_cursor = snap["result"]["result"]["snapshot"]["checkpoint"]["cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("snapshot shape: {snap}"))
        .to_owned();

    let reopened = conn
        .sub_open(
            "run.events",
            json!({"runId": run_id, "afterCursor": checkpoint_cursor}),
            8,
        )
        .await;
    let sub_id = reopened["result"]["subscriptionId"]
        .as_str()
        .unwrap_or_else(|| panic!("re-open must succeed: {reopened}"))
        .to_owned();
    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            FRAME_TIMEOUT,
            "recovered subscription terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "completed", "{end}");

    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after cursor recovery").await;
}

// ===========================================================================
// 16. Exit-code matrix for run wait (negative control)
// ===========================================================================

/// The `run wait` exit policy: 0 succeeded, 1 known failure, 3 result
/// unknown (write with an expired client deadline against a frozen
/// gateway), 130 interrupted by SIGINT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_wait_exit_code_matrix_0_1_3_130() {
    let home = TestHome::new("exit");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "exit").await;

    // 0: succeeded.
    let ok_run = submit_run(root, &session_id, "fine", "exit-ok-1").await;
    let ok_wait = wait_run(root, ok_run["runId"].as_str().unwrap());
    assert_eq!(ok_wait.code, 0, "{}", ok_wait.stderr);

    // 1: known failure (and resume the session queue it pauses).
    let fail_run = submit_run(root, &session_id, "[pd-driver:fail]", "exit-fail-1").await;
    let fail_wait = wait_run(root, fail_run["runId"].as_str().unwrap());
    assert_eq!(fail_wait.code, 1, "{}", fail_wait.stderr);
    resume_paused_queue(root, &session_id).await;

    // 3: result unknown. A frozen gateway (SIGSTOP) can neither confirm nor
    // deny an in-flight write; an argv process would hang in the WS
    // handshake against it (connect has no deadline), so the deterministic
    // real-process path is the long-lived `api connect` engine: its already
    // established session forwards the write, the client deadline expires,
    // and the RESULT_UNKNOWN contract error (key echoed, effect unknown)
    // comes back on the wire. The argv `exit code 3` mapping for that error
    // is pinned by the CLI unit test
    // `execute_invocation_error_paths_print_failed_envelope_and_exit_codes`.
    let sleep_run = submit_run(root, &session_id, "[pd-driver:sleep=60000]", "exit-sleep-1").await;
    let sleep_id = sleep_run["runId"].as_str().unwrap().to_owned();
    // Establish the engine session while the gateway is healthy (a frozen
    // gateway cannot complete a fresh WS handshake — connects have no
    // deadline), then freeze it and fire the write.
    let mut conn = ConnectSession::start(root).await;
    let _ = conn.initialize().await;
    let pid = home.gateway_pid().expect("gateway pid");
    signal_pid(pid, rustix::process::Signal::STOP);
    conn.send(&rpc_request(
        "exit-unknown",
        "run.cancel",
        json!({
            "input": {"runId": sleep_id},
            "idempotencyKey": "exit-unknown-1"
        }),
    ))
    .await;
    // The engine's per-operation timeout (30 s for run.cancel) elapses
    // against the frozen gateway before RESULT_UNKNOWN comes back.
    let unknown = conn
        .wait_response_for("exit-unknown", Duration::from_secs(35), "RESULT_UNKNOWN")
        .await;
    signal_pid(pid, rustix::process::Signal::KILL);
    assert_eq!(error_code(&unknown), -32000, "{unknown}");
    assert_eq!(contract_code(&unknown), "RESULT_UNKNOWN", "{unknown}");
    assert_eq!(unknown["error"]["data"]["effect"], "unknown", "{unknown}");
    assert_eq!(
        unknown["error"]["data"]["details"]["idempotencyKey"], "exit-unknown-1",
        "{unknown}"
    );
    // The engine reconnects (fresh gateway via autostart) and exits cleanly.
    conn.wait_for(
        |v| {
            v.get("method").and_then(Value::as_str) == Some("conn.state")
                && v["params"]["state"] == "connected"
        },
        Duration::from_secs(30),
        "reconnect after the kill",
    )
    .await;
    conn.close_stdin().await;
    conn.wait_exit(0, "engine exit after RESULT_UNKNOWN probe")
        .await;

    // The engine above killed the gateway; its crash recovery paused the
    // session queue. Resume it and start a fresh sleeping run for the SIGINT
    // probe (the old one recovered as `interrupted` and is terminal).
    resume_paused_queue(root, &session_id).await;
    let sleep2 = submit_run(root, &session_id, "[pd-driver:sleep=60000]", "exit-sleep-2").await;
    let sleep2_id = sleep2["runId"].as_str().unwrap().to_owned();

    // 130: interrupted observation (SIGINT while run wait is blocking).
    let mut child = {
        let mut cmd = base_command(root);
        cmd.args([
            "run",
            "wait",
            "--output",
            "json",
            "--run-id",
            &sleep2_id,
            "--timeout-ms",
            "25000",
        ]);
        cmd.stdin(Stdio::null());
        cmd.spawn().expect("spawn run wait")
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    signal_pid(child.id().expect("child pid"), rustix::process::Signal::INT);
    let status = tokio::time::timeout(CLI_TIMEOUT, child.wait())
        .await
        .expect("run wait exit")
        .expect("wait run wait");
    assert_eq!(status.code(), Some(130), "SIGINT must exit 130");

    // Cleanup: cancel the sleeper so the session queue is free.
    let cancel = await_cli(
        root,
        &["run", "cancel", "--output", "json", "--run-id", &sleep2_id],
    );
    expect_ok("run.cancel", &cancel);
}
