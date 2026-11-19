#![allow(dead_code)] // test harness: different suites use different subsets
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
/// How long the stdout reader must pull nothing new off the pipe before
/// [`ConnectSession::drain_channel_to_journal`] considers the stream
/// settled (P1 minimax concurrency review): frames are journaled at read
/// time, so this counter-stability window — not a channel drain — is what
/// proves every arrived frame is accounted before the caller takes a mark
/// or asserts an exact count.
const DRAIN_GRACE: Duration = QUIET_WINDOW;
/// Upper bound for one `drain_channel_to_journal` call: a continuously
/// streaming peer keeps resetting the settle timer, and the caller's
/// exact-count assertion then fails against a live stream instead of the
/// helper waiting forever.
const DRAIN_MAX: Duration = Duration::from_secs(3);

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

/// Runs one `agentcpm-pd` argv invocation to completion from a synchronous
/// context — a plain `std::process::Command` implementation, independent of
/// any tokio runtime flavor (P2 minimax concurrency review: the previous
/// `block_in_place` bridge panicked on the current_thread runtime that a
/// default `#[tokio::test]` builds, so t005-style suites must be able to
/// call this from any flavor). Same `AGENTCPM_PD_HOME` isolation and the
/// same 90 s bound as [`cli_with_stdin`]; a timeout kills the child and
/// fails loudly, exactly like the async twin.
pub fn await_cli(home: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = std::process::Command::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.args(args);
    let mut child = cmd.spawn().expect("spawn agentcpm-pd");
    // Drain both pipes on helper threads: a full pipe would otherwise
    // deadlock the bounded wait below.
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_handle = std::thread::spawn(move || read_pipe_to_end(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe_to_end(stderr_pipe));
    let deadline = std::time::Instant::now() + CLI_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait agentcpm-pd") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("cli invocation timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_handle.join().expect("read cli stdout");
    let stderr = stderr_handle.join().expect("read cli stderr");
    CliOutcome {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

/// Reads one child pipe to EOF on its helper thread.
fn read_pipe_to_end(mut pipe: impl std::io::Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
    buf
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

// ---------------------------------------------------------------------------
// Harness: `api connect` stdio session
// ---------------------------------------------------------------------------

/// Every frame the session's reader pulls off the pipe, plus explicit marks
/// the scenario pushes, in arrival order — the accounting base for
/// exact-count assertions (T5 review P1: skipped frames must be recorded,
/// never silently discarded; minimax concurrency review P1: journaling
/// happens at read time in the reader task, so nothing a consumer never
/// reads escapes accounting). The record is a compact projection (not a
/// full `Value` clone): flood scenarios journal thousands of frames, and
/// the clone made the test consumer slow enough to trip the engine's own
/// slow-consumer guard.
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

/// Compatibility shim (P1 minimax concurrency review): frames enter the
/// journal when the stdout reader pulls them off the pipe — arrival order,
/// nothing a consumer path can miss or double-book — so consumer-side
/// journaling is redundant. Kept because pre-substrate scenario bodies
/// still call it; it is deliberately a no-op.
pub fn journal_frame(_journal: &Journal, _value: &Value) {}

/// Reader-side arrival ledger (P1 minimax concurrency review): the spawned
/// stdout reader journals every frame the moment it is pulled off the pipe
/// (a frame no consumer ever reads still reaches the journal, and marks
/// interleave with frames in true arrival order) and bumps `lines_read`
/// BEFORE the channel handoff, so quiet-window checks can observe the
/// read-but-not-yet-sent span that the channel alone cannot see.
struct ReaderLedger {
    journal: Journal,
    lines_read: AtomicUsize,
}

impl ReaderLedger {
    fn lines_read(&self) -> usize {
        self.lines_read.load(Ordering::Relaxed)
    }
}

pub struct ConnectSession {
    pub child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    pub rx: mpsc::Receiver<String>,
    pub stderr_log: Arc<Mutex<String>>,
    next_id: usize,
    pub journal: Journal,
    reader: Arc<ReaderLedger>,
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
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let reader = Arc::new(ReaderLedger {
            journal: journal.clone(),
            lines_read: AtomicUsize::new(0),
        });
        let ledger = reader.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // P1: journal at read time and count before the channel
                // handoff, so the ledger's counter covers the in-flight
                // window and the journal never misses a frame.
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    ledger
                        .journal
                        .lock()
                        .unwrap()
                        .push(JournalEntry::Frame(frame_record(&value)));
                }
                ledger.lines_read.fetch_add(1, Ordering::Relaxed);
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
            journal,
            reader,
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
    /// already recorded in the journal by the reader at arrival (T5 review
    /// P1 / minimax concurrency review P1), so exact-count and end-of-test
    /// accounting see every frame regardless of who consumes it.
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
    /// read frames afterwards. Frames are journaled by the reader at
    /// arrival, so accounting still sees everything the scenario consumes
    /// through the wrapper.
    pub fn take_rx(&mut self) -> JournalingRx {
        let (_tx, empty_rx) = mpsc::channel(1);
        JournalingRx {
            rx: std::mem::replace(&mut self.rx, empty_rx),
        }
    }

    /// Auxiliary quiet-window probe: any frame that arrives within the
    /// window fails the test immediately. The window covers both the
    /// channel and the reader's in-flight span — a frame pulled off the
    /// pipe but not yet sent is invisible to a channel-only check (P1
    /// minimax concurrency review) — via the reader's monotonic read
    /// counter: the window is quiet only if the counter never moves and
    /// the channel stays empty. A failing frame is already in the journal
    /// (read-time journaling).
    pub async fn expect_no_frames(&mut self, what: &str) {
        let started = self.reader.lines_read();
        let deadline = tokio::time::Instant::now() + QUIET_WINDOW;
        loop {
            match self.rx.try_recv() {
                Ok(line) => {
                    panic!(
                        "expected no further frames ({what}), got: {line}; stderr:\n{}",
                        self.stderr_log.lock().unwrap()
                    );
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    panic!("api connect stdout closed unexpectedly");
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            if self.reader.lines_read() != started {
                // A frame was pulled off the pipe within the window. Give
                // the channel handoff a moment so the failure can name it;
                // it is already journaled either way.
                let named = tokio::time::timeout(Duration::from_millis(50), self.rx.recv())
                    .await
                    .ok()
                    .flatten()
                    .map(|line| format!("got: {line}"))
                    .unwrap_or_else(|| {
                        "the reader pulled frame(s) off the pipe inside the quiet window \
                         (journaled at read time, not yet delivered)"
                            .to_owned()
                    });
                panic!(
                    "expected no further frames ({what}), {named}; stderr:\n{}",
                    self.stderr_log.lock().unwrap()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Settles the frame stream and empties the channel so a mark or an
    /// exact-count assertion taken right after sees every arrived frame on
    /// its "before" side. Frames are journaled by the reader at arrival
    /// (P1), so this no longer moves anything into the journal: it drains
    /// already-delivered frames out of the channel and proves nothing is
    /// in flight — the reader's monotonic `lines_read` counter moves the
    /// moment a frame is pulled off the pipe, before the channel send a
    /// `try_recv` could observe. Settled means the counter held steady for
    /// [`DRAIN_GRACE`], bounded overall by [`DRAIN_MAX`]. The blocking
    /// sleeps are fine: every scenario using this helper runs a
    /// multi_thread runtime, so the reader keeps making progress on
    /// another worker.
    pub fn drain_channel_to_journal(&mut self) {
        let hard_deadline = std::time::Instant::now() + DRAIN_MAX;
        let mut last_read = self.reader.lines_read();
        let mut settled_since = std::time::Instant::now();
        loop {
            // Already journaled at read time; only the channel buffer
            // needs emptying so later probes start from a clean base.
            while self.rx.try_recv().is_ok() {}
            let read = self.reader.lines_read();
            if read != last_read {
                last_read = read;
                settled_since = std::time::Instant::now();
                continue;
            }
            if settled_since.elapsed() >= DRAIN_GRACE || std::time::Instant::now() >= hard_deadline
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
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
        // P3 (review): resolve the mark index before panicking — a panic
        // while holding the journal guard would poison the mutex for every
        // later reader in this process.
        let start = {
            let journal = self.journal.lock().unwrap();
            journal
                .iter()
                .rposition(|e| matches!(e, JournalEntry::Mark(l) if l == label))
        };
        let Some(start) = start else {
            panic!("journal mark `{label}` must exist");
        };
        let journal = self.journal.lock().unwrap();
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

/// Pass-through wrapper over the taken stdout channel (T7's select loop
/// reads frames directly). Frames are journaled by the reader at arrival,
/// so the wrapper only parses and forwards.
pub struct JournalingRx {
    rx: mpsc::Receiver<String>,
}

impl JournalingRx {
    /// Receives and returns the next parsed frame (`None` on EOF or on a
    /// non-JSON line, matching the pre-substrate wrapper).
    pub async fn recv_frame(&mut self) -> Option<Value> {
        let line = self.rx.recv().await?;
        serde_json::from_str::<Value>(&line).ok()
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
        // P3 (dsh boundary review): an unknown stream op must fail loudly —
        // an empty fallback input would be rejected by the engine's schema
        // pre-check as a misleading -32602 instead of exercising the -32004
        // gate.
        _ => panic!(
            "no schema-valid probe input is known for stream operation `{op}` — add one to \
             `stream_probe_input` so the -32004 gate probe can pass the engine's client-side \
             schema pre-check"
        ),
    }
}

// ---------------------------------------------------------------------------
// Harness: local dual-home "remote" pair (S5 W0 `RemoteTarget` fixture)
// ---------------------------------------------------------------------------

use std::process::Command as StdCommand;

/// How long [`serve_remote`] polls `<home_b>/run/ready.json` before giving up
/// on the serve subprocess (the first start includes the gateway bootstrap).
pub const SERVE_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long [`RemoteServe::stop`] waits after SIGTERM before escalating to
/// SIGKILL (the graceful drain is bounded server-side; this is headroom).
const SERVE_STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// Spawns `server serve --data-root <home_b>` as a real subprocess and waits
/// for its `run/ready.json` — the explicit server side of the local
/// "simulated remote" topology (the J8 same-machine shape). The data root
/// must already be initialized (`instance init`); the bind stays the serve
/// default (loopback, system-assigned port) unless a target profile steered
/// it, and the resulting `wsUrl` is read back from `run/ready.json`.
///
/// The returned guard owns the child: `Drop` stops it (SIGTERM first, SIGKILL
/// escalation after [`SERVE_STOP_TIMEOUT`]) and reaps it. Only the guard's
/// own child is ever signalled — never a pid matched by name or read off
/// disk, and never `~/.pilotdeck` (W0 safety boundary).
pub fn serve_remote(home_b: &Path) -> RemoteServe {
    let data_root = home_b.display().to_string();
    let mut cmd = StdCommand::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home_b);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.args(["server", "serve", "--data-root", data_root.as_str()]);
    let mut child = cmd.spawn().expect("spawn server serve");

    let stdout_log = drain_pipe_to_capped_log(child.stdout.take().expect("piped serve stdout"));
    let stderr_log = drain_pipe_to_capped_log(child.stderr.take().expect("piped serve stderr"));

    // Accept only a ready file our own child wrote: the pid must match the
    // spawned process, so a stale file from an earlier gateway never passes.
    let expected_pid = child.id();
    let ready_path = home_b.join("run").join("ready.json");
    let deadline = std::time::Instant::now() + SERVE_READY_TIMEOUT;
    let ready = loop {
        if let Ok(raw) = std::fs::read_to_string(&ready_path)
            && let Ok(value) = serde_json::from_str::<Value>(&raw)
            && value.get("pid").and_then(Value::as_u64) == Some(u64::from(expected_pid))
            && value
                .get("wsUrl")
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty())
        {
            break value;
        }
        if let Some(status) = child.try_wait().expect("wait server serve") {
            panic!(
                "`server serve` exited before ready ({status}); stderr:\n{}",
                stderr_log.lock().unwrap()
            );
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "`server serve` was not ready within {}ms; stderr:\n{}",
                SERVE_READY_TIMEOUT.as_millis(),
                stderr_log.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    RemoteServe {
        home: home_b.to_path_buf(),
        child: Some(child),
        ready,
        stdout_log,
        stderr_log,
    }
}

/// Reads one serve pipe into a capped log buffer on a helper thread — the
/// pipe must be drained or the child would eventually block on a full
/// buffer, and the log is what failure diagnostics print.
fn drain_pipe_to_capped_log(mut pipe: impl std::io::Read + Send + 'static) -> Arc<Mutex<String>> {
    let log = Arc::new(Mutex::new(String::new()));
    let sink = log.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut pipe, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = sink.lock().unwrap();
                    if guard.len() < 512 * 1024 {
                        guard.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            }
        }
    });
    log
}

/// Owns one `server serve` subprocess (see [`serve_remote`]).
pub struct RemoteServe {
    home: PathBuf,
    child: Option<std::process::Child>,
    ready: Value,
    stdout_log: Arc<Mutex<String>>,
    stderr_log: Arc<Mutex<String>>,
}

impl RemoteServe {
    /// The serving gateway PID — always the guard's own child (`ready.json`
    /// `pid`), never a pid matched by name.
    pub fn pid(&self) -> u32 {
        self.ready["pid"].as_u64().expect("ready pid") as u32
    }

    /// The gateway `wsUrl` from `run/ready.json` — loopback with a
    /// system-assigned port unless a target profile steered the bind.
    pub fn ws_url(&self) -> String {
        self.ready["wsUrl"]
            .as_str()
            .expect("ready wsUrl")
            .to_owned()
    }

    /// The serving instance's instanceId.
    pub fn instance_id(&self) -> String {
        self.ready["instanceId"]
            .as_str()
            .expect("ready instanceId")
            .to_owned()
    }

    /// The parsed `run/ready.json` (pid / wsUrl / instanceId / generation).
    pub fn ready(&self) -> &Value {
        &self.ready
    }

    /// The serve subprocess's accumulated stderr (failure diagnostics).
    pub fn stderr_log(&self) -> String {
        self.stderr_log.lock().unwrap().clone()
    }

    /// The data root this serve carries (home B).
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Stops the serve subprocess: SIGTERM (the foreground serve drains and
    /// exits 0), then SIGKILL escalation after [`SERVE_STOP_TIMEOUT`].
    /// Idempotent, so a `Drop` after an explicit stop is a no-op.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        soft_signal(child.id(), rustix::process::Signal::TERM);
        let deadline = std::time::Instant::now() + SERVE_STOP_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        soft_signal(child.id(), rustix::process::Signal::KILL);
        let _ = child.wait();
    }
}

impl Drop for RemoteServe {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Signals a process the harness owns; failures are ignored (the process may
/// have exited between the check and the signal).
fn soft_signal(pid: u32, signal: rustix::process::Signal) {
    if let Some(p) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process(p, signal);
    }
}

/// The local "simulated remote" pair (W0 `RemoteTarget` 挂具): home A is the
/// client home, home B the server data root behind [`serve_remote`], and
/// `target_id` names the profile home A uses to reach home B's gateway over a
/// real ws:// hop. Home A stays deliberately uninitialized — remote commands
/// carry home B's identity, which is exactly what the journey-side "A 不落"
/// assertions rely on.
pub struct RemoteTargetPair {
    // Declared first so the serve child stops before the homes' own Drop
    // cleanup runs.
    serve: RemoteServe,
    pub home_a: TestHome,
    pub home_b: TestHome,
    /// The profile id home A uses for home B (`rt-<tag>`).
    pub target_id: String,
    /// The `credentials.json` key holding home B's local token in home A.
    pub credential_ref: String,
}

impl RemoteTargetPair {
    /// Home B's gateway `wsUrl`.
    pub fn ws_url(&self) -> String {
        self.serve.ws_url()
    }

    /// Home B's instanceId (the identity the profile pins).
    pub fn instance_id_b(&self) -> String {
        self.serve.instance_id()
    }

    /// Home B's serve subprocess PID (fault-injection handle; the guard
    /// tolerates an already-dead child on stop).
    pub fn serve_pid(&self) -> u32 {
        self.serve.pid()
    }

    /// Stops home B's serve subprocess (graceful); both homes stay usable
    /// for post-mortem disk assertions.
    pub fn stop_serve(&mut self) {
        self.serve.stop();
    }

    /// The serve guard (ready snapshot, diagnostics).
    pub fn serve(&self) -> &RemoteServe {
        &self.serve
    }
}

/// One-step constructor for the local dual-home "remote" topology (S5 W0):
///
/// 1. home A — isolated client home, left uninitialized (the remote hop
///    carries home B's identity; no gateway ever auto-starts there).
/// 2. home B — isolated server home, `instance init` through the real CLI.
/// 3. `server serve --data-root <home_b>` subprocess ([`serve_remote`]).
/// 4. Home A's `credentials.json` gets home B's `local.token` under
///    `rt-cred-<tag>`, and `target write` records the profile (kind=https,
///    endpoint = home B's `wsUrl`, expectedInstanceId pinned) — the same
///    product path the t005 target scenarios drive.
///
/// The profile is written but NOT selected: journeys select it (or pass
/// `--target`) explicitly, as J8 does.
pub fn remote_pair(tag: &str) -> RemoteTargetPair {
    let home_a = TestHome::new(&format!("{tag}-a"));
    let home_b = TestHome::new(&format!("{tag}-b"));
    let init = init_home(home_b.path());
    let instance_id = init["identity"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();
    let serve = serve_remote(home_b.path());

    // Home A's credential store references home B's local token (the t005
    // `connection.open` precedent: profiles reference, never inline,
    // credential material).
    let credential_ref = format!("rt-cred-{tag}");
    let token = std::fs::read_to_string(home_b.path().join("local.token"))
        .expect("read home B local.token")
        .trim()
        .to_owned();
    let mut tokens = serde_json::Map::new();
    tokens.insert(credential_ref.clone(), json!(token));
    std::fs::write(
        home_a.path().join("credentials.json"),
        serde_json::to_vec(&json!({"tokens": tokens})).expect("serialize credentials"),
    )
    .expect("write home A credentials.json");

    let target_id = format!("rt-{tag}");
    let endpoint = serve.ws_url();
    let wrote = await_cli(
        home_a.path(),
        &[
            "target",
            "write",
            "--output",
            "json",
            "--target-id",
            &target_id,
            "--name",
            "Local remote pair",
            "--kind",
            "https",
            "--endpoint",
            &endpoint,
            "--expected-instance-id",
            &instance_id,
            "--credential-ref",
            &credential_ref,
        ],
    );
    expect_ok("target.write", &wrote);

    RemoteTargetPair {
        serve,
        home_a,
        home_b,
        target_id,
        credential_ref,
    }
}

/// Remote variant of [`seed_project_session`]: registers the project and
/// session on the target's server (home B) through home A's CLI. The root
/// carries the SERVER's instanceId — remote commands bind the remote
/// identity, and `bind_instance_id` validates nested values too.
pub async fn seed_remote_project_session(
    home_a: &Path,
    target_id: &str,
    server_instance_id: &str,
    tag: &str,
) -> (String, String) {
    let (project_id, session_id) = (format!("p-{tag}"), format!("s-{tag}"));
    let root = format!(
        "{{\"instanceId\":\"{server_instance_id}\",\"rootId\":\"root-{tag}\",\"relativePath\":\"ws\"}}"
    );
    let out = await_cli(
        home_a,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--target",
            target_id,
            "--project-id",
            &project_id,
            "--name",
            "WP5 E2E",
            "--environment-id",
            "local",
            "--root",
            &root,
        ],
    );
    expect_ok("project.register", &out);
    let out = await_cli(
        home_a,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--target",
            target_id,
            "--session-id",
            &session_id,
            "--project-id",
            &project_id,
        ],
    );
    expect_ok("session.create", &out);
    (project_id, session_id)
}
