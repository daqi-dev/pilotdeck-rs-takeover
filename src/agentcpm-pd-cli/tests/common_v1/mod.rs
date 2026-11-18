//! Shared harness for the v1 end-to-end suites (`WP5`, `007 §10`).
//!
//! Extracted from `e2e_v1_roundtrip.rs` in the S3 substrate refactor so every
//! domain suite (`tests/e2e_v1_<domain>.rs`) drives the same real-binary
//! harness: temp `AGENTCPM_PD_HOME`, real `agentcpm-pd` subprocesses over the
//! real `/v1/ws` transport (client auto-start), deadline-bounded reads, and
//! schema-validated envelopes. Scenario bodies stay in their own files.
//!
//! The harness requires the `embedded-gateway` feature (there is nothing to
//! drive without it); every consuming suite is gated the same way.

#![cfg(feature = "embedded-gateway")]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentcpm_pd_protocol::registry::OperationRegistry;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub const BIN: &str = env!("CARGO_BIN_EXE_agentcpm-pd");
pub const OP_VERSION: &str = "agentcpm-pd.operation/1";
pub const CLI_TIMEOUT: Duration = Duration::from_secs(90);
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a frame source may stay silent before a scenario gives up on
/// "nothing else is coming" style checks.
pub const QUIET_WINDOW: Duration = Duration::from_millis(700);

static SEQ: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Harness: temp home + CLI invocations
// ---------------------------------------------------------------------------

pub struct TestHome(PathBuf);

impl TestHome {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentcpm-pd-wp5-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp home");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The PID of the live gateway serving this home (auto-started by the
    /// CLI, or explicitly started), from `run/ready.json`.
    pub fn gateway_pid(&self) -> Option<u32> {
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

pub struct CliOutcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn base_command(home: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd
}

/// Runs one `agentcpm-pd` argv invocation to completion.
pub async fn cli(home: &Path, args: &[&str]) -> CliOutcome {
    cli_with_stdin(home, args, None).await
}

pub async fn cli_with_stdin(home: &Path, args: &[&str], stdin: Option<&str>) -> CliOutcome {
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

pub fn registry() -> &'static OperationRegistry {
    OperationRegistry::global()
}

/// Parses and validates one argv envelope (`--output json`): exactly one line,
/// `version`/`requestId` present, the result checked against the embedded
/// operation schema. Returns the operation result.
pub fn expect_envelope(method: &str, out: &CliOutcome) -> Value {
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
pub fn expect_ok(method: &str, out: &CliOutcome) -> Value {
    assert_eq!(
        out.code, 0,
        "`{method}` exited {}: {}",
        out.code, out.stderr
    );
    expect_envelope(method, out)
}

pub fn init_home(home: &Path) -> Value {
    let out = await_cli(home, &["instance", "init", "--output", "json"]);
    expect_ok("instance.initialize", &out)
}

/// Small wrapper so sync contexts can drive async CLI calls.
pub fn await_cli(home: &Path, args: &[&str]) -> CliOutcome {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(cli(home, args)))
}

// ---------------------------------------------------------------------------
// Harness: `api connect` stdio session
// ---------------------------------------------------------------------------

/// Every frame the session reads, plus explicit marks the scenario pushes,
/// in arrival order — the accounting base for exact-count assertions (T5
/// review P1: skipped frames must be recorded, never silently discarded).
/// The record is a compact projection (not a full `Value` clone): flood
/// scenarios journal thousands of frames, and the clone made the test
/// consumer slow enough to trip the engine's own slow-consumer guard.
#[derive(Clone, Debug)]
pub struct FrameRecord {
    pub method: Option<String>,
    pub subscription_id: Option<String>,
    pub cursor: Option<String>,
    pub terminal: Option<String>,
}

impl FrameRecord {
    fn is_sub_event_for(&self, sub_id: &str) -> bool {
        self.method.as_deref() == Some("sub.event")
            && self.subscription_id.as_deref() == Some(sub_id)
    }
}

fn frame_record(v: &Value) -> FrameRecord {
    FrameRecord {
        method: v.get("method").and_then(Value::as_str).map(str::to_owned),
        subscription_id: v["params"]["subscriptionId"].as_str().map(str::to_owned),
        cursor: v["params"]["event"]["cursor"].as_str().map(str::to_owned),
        terminal: v["params"]["terminal"].as_str().map(str::to_owned),
    }
}

#[derive(Clone, Debug)]
pub enum JournalEntry {
    Frame(FrameRecord),
    Mark(String),
}

pub type Journal = Arc<Mutex<Vec<JournalEntry>>>;

pub fn journal_frame(journal: &Journal, value: &Value) {
    journal
        .lock()
        .unwrap()
        .push(JournalEntry::Frame(frame_record(value)));
}

pub struct ConnectSession {
    pub child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    pub rx: mpsc::Receiver<String>,
    pub stderr_log: Arc<Mutex<String>>,
    next_id: usize,
    pub journal: Journal,
    opened_subs: Vec<String>,
}

impl ConnectSession {
    /// Spawns `agentcpm-pd api connect` with piped stdio and a line reader.
    pub async fn start(home: &Path) -> Self {
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
            journal: Arc::new(Mutex::new(Vec::new())),
            opened_subs: Vec::new(),
        }
    }

    pub fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    pub async fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("api connect stdin still open");
        stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to api connect stdin");
        stdin.write_all(b"\n").await.expect("newline to stdin");
        stdin.flush().await.expect("flush stdin");
    }

    pub async fn send(&mut self, frame: &Value) {
        let line = serde_json::to_string(frame).expect("serialize frame");
        self.send_raw(&line).await;
    }

    pub async fn close_stdin(&mut self) {
        self.stdin = None; // dropping the handle closes the pipe (EOF)
    }

    /// Waits for the next frame matching `pred`. Non-matching frames are
    /// *recorded in the journal* (never silently discarded — T5 review P1),
    /// so exact-count and end-of-test accounting can still see them.
    pub async fn wait_for(
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
                journal_frame(&self.journal, &value);
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

    pub async fn wait_response(&mut self, id: &str, what: &str) -> Value {
        self.wait_for(
            |v| v.get("id").and_then(Value::as_str) == Some(id),
            FRAME_TIMEOUT,
            what,
        )
        .await
    }

    pub async fn wait_response_for(&mut self, id: &str, timeout: Duration, what: &str) -> Value {
        self.wait_for(
            |v| v.get("id").and_then(Value::as_str) == Some(id),
            timeout,
            what,
        )
        .await
    }

    pub async fn initialize(&mut self) -> Value {
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
    pub async fn call(&mut self, method: &str, input: Value, key: Option<&str>) -> Value {
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

    pub async fn sub_open(&mut self, stream: &str, input: Value, credit: u32) -> Value {
        let id = self.fresh_id("sub");
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "sub.open",
            "params": {"stream": stream, "input": input, "credit": credit}
        }))
        .await;
        let response = self.wait_response(&id, "sub.open response").await;
        if let Some(sub_id) = response["result"]["subscriptionId"].as_str() {
            self.opened_subs.push(sub_id.to_owned());
        }
        response
    }

    /// Registers a subscription id with the accounting when its sub.open was
    /// issued through a raw send (flood loops that re-subscribe in place)
    /// instead of `sub_open`.
    pub fn note_opened_sub(&mut self, sub_id: &str) {
        self.opened_subs.push(sub_id.to_owned());
    }

    pub async fn grant_credit(&mut self, subscription_id: &str, add: u32) {
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
    /// read frames afterwards. Every line received through the wrapper is
    /// journaled, so accounting still sees what the scenario consumed.
    pub fn take_rx(&mut self) -> JournalingRx {
        let (_tx, empty_rx) = mpsc::channel(1);
        JournalingRx {
            rx: std::mem::replace(&mut self.rx, empty_rx),
            journal: self.journal.clone(),
        }
    }

    /// Auxiliary quiet-window probe: any frame that arrives within the
    /// window fails the test immediately (and is journaled first).
    pub async fn expect_no_frames(&mut self, what: &str) {
        match tokio::time::timeout(QUIET_WINDOW, self.rx.recv()).await {
            Ok(Some(line)) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    journal_frame(&self.journal, &value);
                }
                panic!(
                    "expected no further frames ({what}), got: {line}; stderr:\n{}",
                    self.stderr_log.lock().unwrap()
                );
            }
            Ok(None) => panic!("api connect stdout closed unexpectedly"),
            Err(_) => {}
        }
    }

    /// Synchronously drains frames already buffered in the session channel
    /// into the journal, so a mark taken right after sees them on its
    /// "before" side.
    pub fn drain_channel_to_journal(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                journal_frame(&self.journal, &value);
            }
        }
    }

    /// Pushes a journal mark; pair with the count helpers below.
    pub fn mark(&mut self, label: &str) {
        self.journal
            .lock()
            .unwrap()
            .push(JournalEntry::Mark(label.to_owned()));
    }

    /// Number of `sub.event` frames for `sub_id` journaled after the (last)
    /// mark with `label`.
    pub fn sub_events_since(&self, label: &str, sub_id: &str) -> usize {
        let journal = self.journal.lock().unwrap();
        let start = journal
            .iter()
            .rposition(|e| matches!(e, JournalEntry::Mark(l) if l == label))
            .unwrap_or_else(|| panic!("journal mark `{label}` must exist"));
        journal
            .iter()
            .skip(start + 1)
            .filter(|entry| matches!(entry, JournalEntry::Frame(f) if f.is_sub_event_for(sub_id)))
            .count()
    }

    /// Total `sub.event` frames for `sub_id` in the journal.
    pub fn sub_event_count(&self, sub_id: &str) -> usize {
        let journal = self.journal.lock().unwrap();
        journal
            .iter()
            .filter(|entry| matches!(entry, JournalEntry::Frame(f) if f.is_sub_event_for(sub_id)))
            .count()
    }

    /// Every `sub.event` cursor for `sub_id` in journal order.
    pub fn sub_event_cursors(&self, sub_id: &str) -> Vec<String> {
        let journal = self.journal.lock().unwrap();
        journal
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::Frame(f) if f.is_sub_event_for(sub_id) => f.cursor.clone(),
                _ => None,
            })
            .collect()
    }

    /// End-of-scenario accounting over the whole journal: every subscription
    /// frame belongs to a subscription this session opened, each subscription
    /// ends at most once, and no event may follow a `completed` terminal
    /// (completed rides the event lane, so ordering is guaranteed). Locally
    /// synthesized terminals (slow_consumer, disconnected, shutdown, closed)
    /// ride the control lane by design (007 §6.4 priority), so events already
    /// queued ahead of them may legitimately arrive after — the recovery flow
    /// re-subscribes from the terminal's resume cursor instead.
    pub fn assert_subscription_accounting(&self) {
        let journal = self.journal.lock().unwrap();
        let mut completed: HashSet<String> = HashSet::new();
        let mut ends: HashMap<String, usize> = HashMap::new();
        for entry in journal.iter() {
            let JournalEntry::Frame(f) = entry else {
                continue;
            };
            if !matches!(f.method.as_deref(), Some("sub.event") | Some("sub.end")) {
                continue;
            }
            let Some(sub_id) = f.subscription_id.as_deref() else {
                panic!("sub frame without subscriptionId: {f:?}");
            };
            assert!(
                self.opened_subs.iter().any(|s| s == sub_id),
                "frame for a subscription this session never opened ({sub_id}): {f:?}"
            );
            match f.method.as_deref() {
                Some("sub.end") => {
                    *ends.entry(sub_id.to_owned()).or_insert(0) += 1;
                    if f.terminal.as_deref() == Some("completed") {
                        completed.insert(sub_id.to_owned());
                    }
                }
                Some("sub.event") => {
                    assert!(
                        !completed.contains(sub_id),
                        "sub.event after the completed terminal for {sub_id}: {f:?}"
                    );
                }
                _ => unreachable!(),
            }
        }
        for (sub_id, count) in ends {
            assert_eq!(count, 1, "subscription {sub_id} ended {count} times");
        }
    }

    pub async fn wait_exit(&mut self, expected_code: i32, what: &str) {
        let code = tokio::time::timeout(CLI_TIMEOUT, self.child.wait())
            .await
            .expect("api connect exit timed out")
            .expect("wait api connect")
            .code()
            .unwrap_or(-1);
        assert_eq!(code, expected_code, "{what}");
    }
}

/// Channel wrapper that journals every received line (T7's select loop
/// reads frames directly; without the wrapper they would escape accounting).
pub struct JournalingRx {
    rx: mpsc::Receiver<String>,
    journal: Journal,
}

impl JournalingRx {
    /// Receives, journals, and returns the next parsed frame.
    pub async fn recv_frame(&mut self) -> Option<Value> {
        let line = self.rx.recv().await?;
        serde_json::from_str::<Value>(&line)
            .ok()
            .inspect(|value| journal_frame(&self.journal, value))
    }
}

pub fn rpc_request(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn error_code(frame: &Value) -> i64 {
    frame["error"]["code"].as_i64().unwrap_or(0)
}

pub fn contract_code(frame: &Value) -> String {
    frame["error"]["data"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Registers project `p-<tag>` and session `s-<tag>`, returning both ids.
pub async fn seed_project_session(home: &Path, tag: &str) -> (String, String) {
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
pub async fn submit_run(home: &Path, session_id: &str, text: &str, key: &str) -> Value {
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

/// Reads the run's complete event history through the paged `run.events`
/// unary command (the default page limit is 100, so flood-sized runs need
/// pagination). Returns every event cursor, in order.
pub async fn all_run_event_cursors(home: &Path, run_id: &str) -> Vec<String> {
    let mut cursors = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..64 {
        let mut args = vec![
            "run",
            "events",
            "--output",
            "json",
            "--run-id",
            run_id,
            "--page",
            "{\"limit\":500}",
        ];
        if let Some(cursor) = &after {
            args.push("--after-cursor");
            args.push(cursor);
        }
        let page = await_cli(home, &args);
        let page = expect_ok("run.events", &page);
        let frame = &page["frame"];
        let events = frame["events"].as_array().cloned().unwrap_or_default();
        let count = events.len();
        for event in events {
            cursors.push(event["cursor"].as_str().unwrap().to_owned());
        }
        if frame["caughtUp"] == true {
            return cursors;
        }
        after = frame["nextCursor"].as_str().map(str::to_owned);
        assert!(count > 0, "pagination stalled: {frame}");
    }
    panic!("run.events pagination did not converge for {run_id}");
}

/// Resumes a session queue paused by a failed run (queue.get -> queue.resume).
pub async fn resume_paused_queue(home: &Path, session_id: &str) {
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

pub fn wait_run(home: &Path, run_id: &str) -> CliOutcome {
    await_cli(
        home,
        &[
            "run",
            "wait",
            "--output",
            "json",
            "--run-id",
            run_id,
            // The offline driver writes each event with its own fsync;
            // flood-sized runs take tens of seconds under concurrent suite
            // load, so the observation window is generous.
            "--timeout-ms",
            "60000",
        ],
    )
}

pub fn init_home_instance(root: &Path) -> String {
    let raw = std::fs::read_to_string(root.join("local.json")).expect("local.json");
    let value: Value = serde_json::from_str(&raw).expect("parse local.json");
    value["instanceId"].as_str().unwrap().to_owned()
}

pub fn signal_pid(pid: u32, signal: rustix::process::Signal) {
    let Some(p) = rustix::process::Pid::from_raw(pid as i32) else {
        panic!("invalid pid {pid}");
    };
    rustix::process::kill_process(p, signal).expect("signal gateway");
}

/// The first `execution='stream'` operation this build has not registered a
/// gateway handler for — mirrored in-process from the same embedded domain
/// table the gateway binary registers (same code, same compile), so the
/// `-32004` stream-gate scenarios stay registry-derived instead of pinning a
/// method id. Returns `None` once every stream op is implemented (S3
/// complete), and the probe is skipped then.
pub fn unimplemented_stream_op() -> Option<&'static str> {
    pilotdeck_gateway::public_v1::domains::first_unimplemented_operation_where(|op| {
        op.execution == agentcpm_pd_protocol::registry::ExecutionMode::Stream
    })
}

/// A schema-valid minimal input for every `execution='stream'` operation, so
/// a stream-gate probe passes the `api connect` engine's client-side schema
/// pre-check (it validates against the requested stream's schema before
/// forwarding) and reaches the gateway's `-32004` admission gate. The
/// engine injects `follow=true` and `instanceId` itself.
pub fn stream_probe_input(op: &str) -> Value {
    match op {
        "run.events" => json!({"runId": "probe-run"}),
        "operations.events" => json!({"operationId": "probe-operation"}),
        "terminals.output" => json!({"terminalId": "probe-terminal"}),
        "resources.watch" => json!({"resources": [{"kind": "project"}]}),
        "host.stream_model" => json!({
            "scope": {"kind": "global"},
            "preparedRequestId": "probe-prepared-request",
            "requestHash": "probe-request-hash",
            "executionContext": {
                "parentRunId": "probe-run",
                "parentInvocationId": "probe-invocation",
                "consumerKind": "runtimeTool",
                "grantRef": "probe-grant"
            }
        }),
        "notifications.events" => json!({"scope": {"kind": "global"}}),
        _ => json!({}),
    }
}
