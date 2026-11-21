//! Live integration tests: the real `pilotdeck` shim binary driving a real
//! `agentcpm-pd` binary (workspace build) over an isolated
//! `AGENTCPM_PD_HOME` — the first three "fake → real" chains from the S4
//! work order §5 (batches E/A/A):
//!
//! - sdk-run: `run.submit` → `run.events` paged wait for the real terminal
//!   state (the offline deterministic driver settles a plain-text prompt
//!   after its bounded sleep, so no model provider is needed) → the legacy
//!   result projection and exit code;
//! - sessions-list: the `session.list` paging projection over the real
//!   first-page envelope (`pageInfo.nextCursor: null` ends the loop);
//! - cron-list/create/delete: the full `schedules.*` chain.
//!
//! The real binary is located through, in order: the `AGENTCPM_PD_BIN`
//! override, `$CARGO_TARGET_DIR/debug/agentcpm-pd`, or the default
//! workspace build `<manifest>/../agentcpm-pd-cli/../target/debug/
//! agentcpm-pd` (= `src/target/debug/agentcpm-pd`). When none exists the
//! tests FAIL: a machine that has not built the CLI must not report the
//! live suites green (build `agentcpm-pd` first, e.g.
//! `cargo build -p agentcpm-pd-cli`, or point `AGENTCPM_PD_BIN` at a
//! built binary).
//!
//! Every home is a temp dir whose Drop kills the auto-started gateway
//! (pid from `run/ready.json`; the gateway binds an ephemeral loopback
//! port, so parallel homes never collide) before removing the tree — the
//! real `~/.agentcpm-pd` / `~/.pilotdeck` are never touched.

#![cfg(unix)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Environment variable selecting the (isolated) agentcpm-pd home.
const HOME_ENV: &str = "AGENTCPM_PD_HOME";
/// Executor hook pinning the target binary the shim drives.
const TARGET_BIN_ENV: &str = "AGENTCPM_PD_COMPAT_TARGET_BIN";
/// Test-side override for the real `agentcpm-pd` binary location.
const REAL_BIN_ENV: &str = "AGENTCPM_PD_BIN";
/// One child invocation bound; the first gateway auto-start is included.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
/// Envelope version printed by every unary CLI invocation.
const OP_VERSION: &str = "agentcpm-pd.operation/1";

/// Legacy env vars cleared for every shim child (ambient values must not
/// leak into the compat routing) — the same list as the blackbox layer.
const LEGACY_ENV_VARS: &[&str] = &[
    "PILOTDECK_GATEWAY_MODE",
    "PILOTDECK_AGENT_LOOP_FACTORY",
    "PILOTDECK_AGENT_LOOP_TCP_HOST",
    "PILOTDECK_AGENT_LOOP_TCP_PORT",
    "PILOTDECK_SDK_GATEWAY_URL",
    "PILOTDECK_GATEWAY_URL",
    "PILOTDECK_GATEWAY_PORT",
    "PILOTDECK_SDK_AUTH_TOKEN",
    "PILOTDECK_TOKEN",
    "PILOT_HOME",
    "PILOTDECK_PROXY_LISTEN",
    "PILOTDECK_PROXY_UPSTREAM",
];

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Pure resolution helper for [`real_agentcpm_pd_bin`].
fn resolve_real_agentcpm_pd_bin(
    explicit: Option<&std::ffi::OsStr>,
    target_dir: Option<&std::ffi::OsStr>,
    manifest_dir: &Path,
) -> PathBuf {
    if let Some(explicit) = explicit.filter(|v| !v.is_empty()) {
        return PathBuf::from(explicit);
    }
    if let Some(dir) = target_dir.filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        let base = if dir.is_absolute() {
            dir
        } else {
            // A relative CARGO_TARGET_DIR is workspace-root relative.
            manifest_dir.join("..").join(dir)
        };
        return base.join("debug").join("agentcpm-pd");
    }
    manifest_dir.join("../agentcpm-pd-cli/../target/debug/agentcpm-pd")
}

/// Locates the real `agentcpm-pd` binary: explicit `AGENTCPM_PD_BIN`
/// override, then `$CARGO_TARGET_DIR/debug`, then the default workspace
/// build output next to the cli crate. `None` when the candidate is not a
/// file (the caller fails via [`require_real_bin`]).
fn real_agentcpm_pd_bin() -> Option<PathBuf> {
    let candidate = resolve_real_agentcpm_pd_bin(
        std::env::var_os(REAL_BIN_ENV).as_deref(),
        std::env::var_os("CARGO_TARGET_DIR").as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );
    candidate.is_file().then_some(candidate)
}

/// The real-binary guard: a missing binary FAILS the test with build
/// instructions instead of silently passing (a machine that has not built
/// the CLI must not report the live suites green).
fn require_real_bin() -> PathBuf {
    match real_agentcpm_pd_bin() {
        Some(bin) => bin,
        None => panic!(
            "live integration requires the real agentcpm-pd binary, none was found: set \
             {REAL_BIN_ENV} or build src/target/debug/agentcpm-pd first \
             (cargo build -p agentcpm-pd-cli)"
        ),
    }
}

/// An isolated temporary home; Drop kills the auto-started gateway before
/// removing the tree (an orphaned gateway would hold the sqlite files).
struct TestHome(PathBuf);

impl TestHome {
    fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agentcpm-pd-compat-live-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create live test home");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The PID of the gateway auto-started for this home, if any.
    fn gateway_pid(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(self.0.join("run").join("ready.json")).ok()?;
        let value: Value = serde_json::from_str(&raw).ok()?;
        value.get("pid").and_then(Value::as_u64).map(|p| p as u32)
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        if let Some(pid) = self.gateway_pid() {
            let _ = Command::new("/bin/kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
        std::thread::sleep(Duration::from_millis(60));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One completed child invocation.
struct CliOutcome {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Reads one child pipe to EOF on its helper thread.
fn read_pipe_to_end(mut pipe: impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf);
    buf
}

/// Runs one child to completion under a bounded wait. Both pipes drain on
/// helper threads (a full pipe would deadlock the wait); a timeout kills
/// the child and fails loudly.
fn run_child(mut cmd: Command) -> Result<CliOutcome, String> {
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_handle = std::thread::spawn(move || read_pipe_to_end(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe_to_end(stderr_pipe));
    let deadline = Instant::now() + CHILD_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("wait: {e}"))? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err("child timed out".to_owned());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = String::from_utf8_lossy(&stdout_handle.join().expect("read stdout")).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_handle.join().expect("read stderr")).into_owned();
    Ok(CliOutcome {
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

/// Runs the real `agentcpm-pd` CLI against the isolated home.
fn run_cli(bin: &Path, home: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = Command::new(bin);
    cmd.env(HOME_ENV, home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    run_child(cmd).unwrap_or_else(|e| panic!("cli {args:?} failed to run: {e}"))
}

/// Runs the real `pilotdeck` shim against the isolated home with the real
/// CLI pinned as its target (no PATH dependency). The shim's own children
/// inherit `AGENTCPM_PD_HOME` and `AGENTCPM_PD_COMPAT_TARGET_BIN`.
fn run_shim(home: &Path, real_bin: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pilotdeck-compat"));
    cmd.env(HOME_ENV, home)
        .env(TARGET_BIN_ENV, real_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    for var in LEGACY_ENV_VARS {
        cmd.env_remove(var);
    }
    run_child(cmd).unwrap_or_else(|e| panic!("shim {args:?} failed to run: {e}"))
}

/// Parses one `--output json` CLI envelope line (exit 0, exactly one line,
/// version/requestId present, succeeded/accepted outcome) and returns the
/// operation result — the same shape the e2e suites assert.
fn expect_cli_result(name: &str, out: &CliOutcome) -> Value {
    assert_eq!(out.code, 0, "`{name}` exited {}: {}", out.code, out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "`{name}` must print exactly one envelope line; stdout:\n{}",
        out.stdout
    );
    let envelope: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{name}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(envelope["version"], OP_VERSION, "{envelope}");
    assert!(
        envelope["requestId"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "`{name}` envelope has no requestId: {envelope}"
    );
    let outcome = envelope["outcome"].as_str().unwrap_or_default();
    assert!(
        outcome == "succeeded" || outcome == "accepted",
        "`{name}` outcome `{outcome}`: {}",
        out.stderr
    );
    envelope["result"].clone()
}

/// Parses the shim's whole stdout as one JSON document (the legacy
/// projections print one pretty JSON doc).
fn shim_json(name: &str, out: &CliOutcome) -> Value {
    serde_json::from_str(out.stdout.trim())
        .unwrap_or_else(|e| panic!("`{name}` stdout is not one JSON doc ({e}):\n{}", out.stdout))
}

/// `instance init` → the instanceId (needed by project registration).
fn init_instance(bin: &Path, home: &Path) -> String {
    let out = run_cli(bin, home, &["instance", "init", "--output", "json"]);
    let result = expect_cli_result("instance.initialize", &out);
    result
        .pointer("/identity/instanceId")
        .and_then(Value::as_str)
        .expect("instanceId in the initialize result")
        .to_owned()
}

/// Registers `p-live` with a real authorized workspace root.
fn register_project(bin: &Path, home: &Path, instance_id: &str) {
    let root = json!({
        "instanceId": instance_id,
        "rootId": "root-live",
        "relativePath": "ws",
    })
    .to_string();
    let out = run_cli(
        bin,
        home,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-live",
            "--name",
            "compat live",
            "--environment-id",
            "local",
            "--root",
            &root,
        ],
    );
    expect_cli_result("project.register", &out);
}

/// Creates `s-live` under `p-live`.
fn create_session(bin: &Path, home: &Path) {
    let out = run_cli(
        bin,
        home,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            "s-live",
            "--project-id",
            "p-live",
        ],
    );
    expect_cli_result("session.create", &out);
}

/// Writes the shim config document (`compat/config.json`, schema v2).
fn write_shim_config(home: &Path, config: &Value) {
    let dir = home.join("compat");
    std::fs::create_dir_all(&dir).expect("create compat dir");
    let raw = serde_json::to_string(config).expect("serialize config");
    std::fs::write(dir.join("config.json"), raw).expect("write shim config");
}

/// Writes one row of the shim legacy→new session table.
fn write_shim_session(home: &Path, legacy: &str, session_id: &str, project_id: &str) {
    let doc = json!({
        "schemaVersion": 1,
        "sessions": [{
            "legacySessionId": legacy,
            "sessionId": session_id,
            "projectId": project_id,
            "createdAt": "1",
        }],
    });
    let dir = home.join("compat");
    std::fs::create_dir_all(&dir).expect("create compat dir");
    let raw = serde_json::to_string(&doc).expect("serialize sessions");
    std::fs::write(dir.join("sessions.json"), raw).expect("write shim sessions");
}

/// sdk-run (work order §5 batch E): `run.submit` → `run.events` paged wait
/// → legacy projection. The offline deterministic driver settles the
/// plain-text prompt after its bounded sleep (`run.completed`), so the
/// shim must report the old `completed` result with exit 0.
#[test]
fn live_sdk_run_submits_waits_and_projects_the_legacy_result() {
    let bin = require_real_bin();
    let home = TestHome::new("sdk-run");
    let instance_id = init_instance(&bin, home.path());
    register_project(&bin, home.path(), &instance_id);
    // `--project` must map through legacyTargets onto a target profile;
    // "local" resolves to the home's own instance (no profile store needed).
    write_shim_config(
        home.path(),
        &json!({
            "schemaVersion": 2,
            "legacyTargets": [{"project": "p-live", "target": "local"}],
        }),
    );

    // --json: one compact {"sessionId","result"} line, exit 0.
    let out = run_shim(
        home.path(),
        &bin,
        &["run", "hello world", "--project", "p-live", "--json"],
    );
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    let doc = shim_json("sdk-run --json", &out);
    let session_id = doc["sessionId"].as_str().expect("sessionId").to_owned();
    assert!(
        session_id.starts_with("shim-s"),
        "shim-generated session id: {session_id}"
    );
    assert_eq!(doc["result"]["status"], "completed", "doc: {doc}");
    assert_eq!(
        doc["result"]["output"], "completed: hello world",
        "doc: {doc}"
    );
    assert!(
        !out.stderr.contains("shim interrupted"),
        "guardian must not trigger on normal completion; stderr:\n{}",
        out.stderr
    );
    // The old→new session identity row was persisted.
    assert!(
        home.path().join("compat").join("sessions.json").exists(),
        "the shim must record the session mapping"
    );

    // Plain text: completed with run.output.delta text prints the stream text.
    let out = run_shim(
        home.path(),
        &bin,
        &["run", "hello world", "--project", "p-live"],
    );
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    assert_eq!(
        out.stdout.trim(),
        "completed: hello world",
        "stdout:\n{}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("shim interrupted"),
        "guardian must not trigger on normal completion; stderr:\n{}",
        out.stderr
    );
}

/// sessions-list (batch A): `session.list` paging projection over the
/// real first-page envelope (`pageInfo.nextCursor: null` ends the loop;
/// the paging itself is covered by the in-proc tests).
#[test]
fn live_sessions_list_projects_the_old_array() {
    let bin = require_real_bin();
    let home = TestHome::new("sessions-list");
    let instance_id = init_instance(&bin, home.path());
    register_project(&bin, home.path(), &instance_id);
    create_session(&bin, home.path());
    // No legacy flags: defaultTarget unset keeps the CLI's own default
    // (the local instance under the isolated home).
    write_shim_config(
        home.path(),
        &json!({"schemaVersion": 2, "defaultProjectId": "p-live"}),
    );

    let out = run_shim(home.path(), &bin, &["sessions"]);
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    let list = shim_json("sessions list", &out);
    let items = list.as_array().expect("the old shape is a JSON array");
    assert_eq!(items.len(), 1, "stdout:\n{}", out.stdout);
    assert_eq!(items[0]["sessionId"], "s-live", "stdout:\n{}", out.stdout);
    assert_eq!(items[0]["sessionKey"], "s-live", "stdout:\n{}", out.stdout);
}

/// cron-list/create/delete (batch A): the full `schedules.*` chain —
/// empty listing, create through the legacy session key (once schedule
/// verbatim), listing back, delete (revision fetched through
/// `schedules.list`), and the empty listing again.
#[test]
fn live_cron_list_create_delete_round_trip() {
    let bin = require_real_bin();
    let home = TestHome::new("cron");
    let instance_id = init_instance(&bin, home.path());
    register_project(&bin, home.path(), &instance_id);
    create_session(&bin, home.path());
    write_shim_config(
        home.path(),
        &json!({"schemaVersion": 2, "defaultProjectId": "p-live"}),
    );
    // The legacy session key resolves through the shim's identity table.
    write_shim_session(home.path(), "cli-live", "s-live", "p-live");

    // Empty listing.
    let out = run_shim(home.path(), &bin, &["cron", "list"]);
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    assert_eq!(
        shim_json("cron list", &out)["tasks"],
        json!([]),
        "stdout:\n{}",
        out.stdout
    );

    // Create: the legacy key maps, the once schedule stores verbatim.
    let out = run_shim(
        home.path(),
        &bin,
        &[
            "cron",
            "create",
            "--session",
            "cli-live",
            "--message",
            "live check",
            "--once",
            "2027-01-01T00:00:00Z",
        ],
    );
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    let task = shim_json("cron create", &out)["task"].clone();
    let task_id = task["taskId"].as_str().expect("taskId").to_owned();
    assert!(
        task_id.starts_with("shim-sched"),
        "shim-generated schedule id: {task_id}"
    );
    assert_eq!(task["schedule"]["type"], "once", "task: {task}");
    assert_eq!(
        task["schedule"]["runAt"], "2027-01-01T00:00:00Z",
        "task: {task}"
    );
    assert_eq!(task["sessionKey"], "s-live", "task: {task}");
    assert_eq!(task["projectKey"], "p-live", "task: {task}");

    // The definition is listed back.
    let out = run_shim(home.path(), &bin, &["cron", "list"]);
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    let doc = shim_json("cron list (after create)", &out);
    let listed: Vec<&str> = doc["tasks"]
        .as_array()
        .expect("tasks array")
        .iter()
        .filter_map(|t| t["taskId"].as_str())
        .collect();
    assert!(
        listed.contains(&task_id.as_str()),
        "created task {task_id} not in {listed:?}"
    );

    // Delete (the revision is fetched through schedules.list first).
    let out = run_shim(home.path(), &bin, &["cron", "delete", &task_id]);
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    assert_eq!(
        shim_json("cron delete", &out)["deleted"],
        true,
        "stdout:\n{}",
        out.stdout
    );

    // Gone.
    let out = run_shim(home.path(), &bin, &["cron", "list"]);
    assert_eq!(out.code, 0, "shim exited {}: {}", out.code, out.stderr);
    assert_eq!(
        shim_json("cron list (after delete)", &out)["tasks"],
        json!([]),
        "stdout:\n{}",
        out.stdout
    );
}

#[test]
fn real_agentcpm_pd_bin_resolution_avoids_double_debug_suffix() {
    use std::ffi::OsStr;

    let manifest = Path::new("/ws/src/agentcpm-pd-compat");
    assert_eq!(
        resolve_real_agentcpm_pd_bin(Some(OsStr::new("/custom/pd")), None, manifest),
        PathBuf::from("/custom/pd")
    );
    assert_eq!(
        resolve_real_agentcpm_pd_bin(None, Some(OsStr::new("/tmp/tgt")), manifest),
        PathBuf::from("/tmp/tgt/debug/agentcpm-pd")
    );
    assert_eq!(
        resolve_real_agentcpm_pd_bin(None, Some(OsStr::new("custom-target")), manifest),
        PathBuf::from("/ws/src/agentcpm-pd-compat/../custom-target/debug/agentcpm-pd")
    );
    assert_eq!(
        resolve_real_agentcpm_pd_bin(None, None, manifest),
        PathBuf::from("/ws/src/agentcpm-pd-compat/../agentcpm-pd-cli/../target/debug/agentcpm-pd")
    );
}
