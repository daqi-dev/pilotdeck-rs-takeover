//! B-layer contract tests: spawn the real `pilotdeck` binary and assert the
//! process-level script contract — exit code, stdout, stderr, and the
//! invocation sequence seen by the target.
//!
//! - Local paths (help / --version / usage errors / the five rejections /
//!   compat manifest / reserved words / no-home errors) never need a target:
//!   the child's PATH points at an empty controlled directory, so an
//!   accidental business call would fail with target-not-found instead of
//!   reaching a real installation.
//! - Business paths run against `fixtures/fake_agentcpm_pd.py` installed as
//!   `agentcpm-pd` in a temp directory prepended to the child's PATH (the
//!   test process's own environment is never modified). python3 unavailable
//!   => the whole group is skipped (`#[ignore]` + runtime probe; the real
//!   gates cb8/mbp2 have python3).
//! - The SIGINT scenario kills the shim mid-run and asserts the
//!   signal-immune guardian cancels the run (unix only).

#![cfg(unix)]

mod common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    HOME_ENV, LEGACY_ENV_VARS, TARGET_BIN_ENV, TARGET_BIN_NAME, TempDir, all_scenarios,
    json_subset, scenario_argv, scenario_bin_name, scenario_blackbox, scenario_home_env, seed_home,
};
use serde_json::Value;

fn make_executable(path: &Path) {
    let mut perms = std::fs::metadata(path)
        .expect("stat fake target")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod fake target");
}

/// Copies the checked-in python fake as `agentcpm-pd` into `dir`.
fn install_fake_target(dir: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("fake_agentcpm_pd.py");
    let dst = dir.join(TARGET_BIN_NAME);
    std::fs::copy(&src, &dst).expect("copy fake agentcpm-pd");
    make_executable(&dst);
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct Outcome {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    log: Vec<Value>,
    home: PathBuf,
}

/// Runs one scenario as a real child process. `fake` installs the scripted
/// target on the child's PATH; otherwise the PATH is an empty controlled
/// directory.
fn run_blackbox(sc: &Value, fake: bool) -> Result<Outcome, String> {
    let work = TempDir::new("bb");
    let home = TempDir::new("bb-home");
    seed_home(sc, home.path());

    let bin_name = scenario_bin_name(sc);
    let bin_path = if bin_name == "pilotdeck" {
        PathBuf::from(env!("CARGO_BIN_EXE_pilotdeck"))
    } else {
        let path = work.path().join(bin_name);
        std::fs::copy(env!("CARGO_BIN_EXE_pilotdeck"), &path)
            .map_err(|e| format!("copy binary as `{bin_name}`: {e}"))?;
        make_executable(&path);
        path
    };

    let scratch = work.path().join("fake");
    let path_value = if fake {
        std::fs::create_dir_all(&scratch).map_err(|e| format!("scratch: {e}"))?;
        install_fake_target(&scratch);
        format!(
            "{}:{}",
            scratch.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    } else {
        let empty = work.path().join("no-target");
        std::fs::create_dir_all(&empty).map_err(|e| format!("empty PATH dir: {e}"))?;
        empty.display().to_string()
    };

    let mut cmd = Command::new(&bin_path);
    cmd.env_remove(HOME_ENV);
    if scenario_home_env(sc) {
        cmd.env(HOME_ENV, home.path());
    }
    for var in LEGACY_ENV_VARS {
        cmd.env_remove(var);
    }
    if let Some(env) = sc.get("legacyEnv").and_then(Value::as_object) {
        for (key, value) in env {
            cmd.env(
                key,
                value
                    .as_str()
                    .ok_or_else(|| format!("legacyEnv `{key}` not a string"))?,
            );
        }
    }
    cmd.env_remove(TARGET_BIN_ENV);
    cmd.env("PATH", &path_value);
    cmd.args(scenario_argv(sc));
    if fake {
        cmd.env("AGENTCPM_PD_FAKE_DIR", &scratch);
        let script =
            serde_json::to_string(sc.get("fakeScript").unwrap_or(&Value::Array(Vec::new())))
                .map_err(|e| format!("serialize fakeScript: {e}"))?;
        cmd.env("AGENTCPM_PD_FAKE_SCRIPT", script);
        match sc.get("fakeDefault").filter(|v| !v.is_null()) {
            Some(default) => {
                let raw = serde_json::to_string(default).map_err(|e| e.to_string())?;
                cmd.env("AGENTCPM_PD_FAKE_DEFAULT", raw);
            }
            None => {
                cmd.env_remove("AGENTCPM_PD_FAKE_DEFAULT");
            }
        }
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = match sc.get("stdin").and_then(Value::as_str) {
        Some(text) => {
            cmd.stdin(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
            let mut stdin = child.stdin.take().expect("stdin piped");
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| format!("write stdin: {e}"))?;
            drop(stdin);
            child.wait_with_output().map_err(|e| format!("wait: {e}"))?
        }
        None => cmd.output().map_err(|e| format!("run: {e}"))?,
    };

    Ok(Outcome {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        log: read_fake_log(&scratch),
        home: home.path().to_path_buf(),
    })
}

fn read_fake_log(scratch: &Path) -> Vec<Value> {
    let Ok(raw) = std::fs::read_to_string(scratch.join("calls.jsonl")) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("fake log line parses"))
        .collect()
}

fn str_array(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Parses stdout as a sequence of JSON documents: one whole-buffer document
/// when it parses, otherwise one per non-empty line (JSONL).
fn parse_stdout_docs(stdout: &str) -> Option<Vec<Value>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    if let Ok(doc) = serde_json::from_str::<Value>(trimmed) {
        return Some(vec![doc]);
    }
    let mut docs = Vec::new();
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(doc) => docs.push(doc),
            Err(_) => return None,
        }
    }
    Some(docs)
}

/// The invocation log must start with the exact expected sequence; trailing
/// entries are allowed only for the SIGINT guardian (which wakes after the
/// shim dies) — anything else is an orchestration bug.
fn assert_log_methods(expected: &[&str], log: &[Value]) -> Result<(), String> {
    if log.len() < expected.len() {
        return Err(format!(
            "invocation log has {} entries, expected at least {}: {:?}",
            log.len(),
            expected.len(),
            log
        ));
    }
    for (index, want) in expected.iter().enumerate() {
        let got = log[index]["method"].as_str().unwrap_or_default();
        if got != *want {
            return Err(format!(
                "invocation {index} is `{got}`, expected `{want}` (log: {log:?})"
            ));
        }
    }
    for extra in &log[expected.len()..] {
        match extra["method"].as_str() {
            Some("requests.lookup") | Some("run.cancel") | Some("run.get") => {}
            other => return Err(format!("unexpected trailing invocation {other:?}")),
        }
    }
    Ok(())
}

fn check_log_calls(calls: &[Value], log: &[Value]) -> Result<(), String> {
    // Selected-call assertions over the invocation log: each entry matches
    // the first call of its method at or after the running cursor.
    let mut cursor = 0usize;
    for expected in calls {
        let method = expected["method"].as_str().unwrap_or_default();
        let Some(offset) = log[cursor..]
            .iter()
            .position(|e| e["method"].as_str() == Some(method))
        else {
            return Err(format!(
                "no `{method}` invocation after log index {cursor}: {:?}",
                log
            ));
        };
        let index = cursor + offset;
        cursor = index + 1;
        let entry = &log[index];
        if let Some(input) = expected.get("input")
            && !json_subset(input, &entry["input"])
        {
            return Err(format!(
                "invocation {index} ({}) input {} is not a superset of {input}",
                entry["method"].as_str().unwrap_or_default(),
                entry["input"]
            ));
        }
        match expected["requestKey"].as_bool() {
            Some(true) if entry["requestKey"].is_null() => {
                return Err(format!("invocation {index} carries no request key"));
            }
            Some(false) if !entry["requestKey"].is_null() => {
                return Err(format!(
                    "invocation {index} unexpectedly carries request key {}",
                    entry["requestKey"]
                ));
            }
            _ => {}
        }
        for needle in str_array(expected.get("inputNotContains").unwrap_or(&Value::Null)) {
            let raw = serde_json::to_string(&entry["input"]).unwrap_or_default();
            if raw.contains(needle) {
                return Err(format!("invocation {index} input contains `{needle}`"));
            }
        }
    }
    Ok(())
}

fn check_blackbox(sc: &Value, out: &Outcome, fake: bool) -> Result<(), String> {
    let expect = &sc["expect"];

    if let Some(want) = expect["exitCode"].as_i64() {
        match out.status.code() {
            Some(code) if code as i64 == want => {}
            other => return Err(format!("exit {other:?} != expected {want}")),
        }
    }

    for needle in str_array(&expect["stdoutContains"]) {
        if !out.stdout.contains(needle) {
            return Err(format!("stdout missing `{needle}`:\n{}", out.stdout));
        }
    }
    for needle in str_array(&expect["stdoutExcludes"]) {
        if out.stdout.contains(needle) {
            return Err(format!(
                "stdout must not contain `{needle}`:\n{}",
                out.stdout
            ));
        }
    }
    if expect["stdoutEmpty"].as_bool() == Some(true) && !out.stdout.trim().is_empty() {
        return Err(format!("stdout not empty:\n{}", out.stdout));
    }
    if let Some(docs) = expect["stdoutJson"].as_array() {
        let Some(parsed) = parse_stdout_docs(&out.stdout) else {
            return Err(format!("stdout is not JSON:\n{}", out.stdout));
        };
        if parsed.len() != docs.len() {
            return Err(format!(
                "stdout has {} JSON docs, expected {}: {parsed:?}",
                parsed.len(),
                docs.len()
            ));
        }
        for (want, got) in docs.iter().zip(&parsed) {
            if !json_subset(want, got) {
                return Err(format!("stdout doc {got} is not a superset of {want}"));
            }
        }
    }

    for needle in str_array(&expect["stderrContains"]) {
        if !out.stderr.contains(needle) {
            return Err(format!("stderr missing `{needle}`:\n{}", out.stderr));
        }
    }
    for needle in str_array(&expect["stderrExcludes"]) {
        if out.stderr.contains(needle) {
            return Err(format!(
                "stderr must not contain `{needle}`:\n{}",
                out.stderr
            ));
        }
    }

    if let Some(expected) = expect["invokeMethods"].as_array() {
        let expected: Vec<&str> = expected.iter().filter_map(Value::as_str).collect();
        if fake {
            assert_log_methods(&expected, &out.log)?;
        } else if !expected.is_empty() {
            return Err("local scenarios can only assert an empty invokeMethods".to_owned());
        }
    }
    if let Some(calls) = expect["calls"].as_array() {
        if !fake {
            return Err("calls assertions need the fake target (blackbox: \"fake\")".to_owned());
        }
        check_log_calls(calls, &out.log)?;
    }

    for rel in str_array(&expect["homeAbsent"]) {
        if out.home.join(rel).exists() {
            return Err(format!("home state `{rel}` must not exist"));
        }
    }
    Ok(())
}

fn run_group(mode: &str, fake: bool) -> (usize, Vec<String>) {
    let mut ran = 0usize;
    let mut failures = Vec::new();
    for (slug, sc) in all_scenarios() {
        if scenario_blackbox(&sc) != mode {
            continue;
        }
        let name = sc["name"].as_str().expect("scenario name");
        ran += 1;
        if let Err(e) = run_blackbox(&sc, fake).and_then(|out| check_blackbox(&sc, &out, fake)) {
            failures.push(format!("{slug}/{name}: {e}"));
        }
    }
    (ran, failures)
}

#[test]
fn blackbox_local_paths() {
    let (ran, failures) = run_group("local", false);
    eprintln!(
        "blackbox local: {ran} scenarios, {} failures",
        failures.len()
    );
    assert!(ran > 25, "expected local scenarios to run, got {ran}");
    assert!(
        failures.is_empty(),
        "{} blackbox failure(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
#[ignore = "needs python3 on PATH to run the fake agentcpm-pd target; the real gates (cb8/mbp2) have python3 — run with `cargo test -- --ignored`"]
fn blackbox_fake_target() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (ran, failures) = run_group("fake", true);
    eprintln!(
        "blackbox fake: {ran} scenarios, {} failures",
        failures.len()
    );
    assert!(ran > 25, "expected fake-target scenarios to run, got {ran}");
    assert!(
        failures.is_empty(),
        "{} blackbox failure(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// SIGINT mid-run: the shim dies from the signal (the invoking shell reports
/// the legacy exit 130), while the signal-immune guardian wakes, cancels the
/// unsettled run and waits for its real terminal state.
#[test]
#[ignore = "needs python3 on PATH to run the fake agentcpm-pd target; the real gates (cb8/mbp2) have python3 — run with `cargo test -- --ignored`"]
fn blackbox_sigint_guardian() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let scenarios: Vec<(String, Value)> = all_scenarios()
        .into_iter()
        .filter(|(_, sc)| scenario_blackbox(sc) == "signal")
        .collect();
    assert!(
        !scenarios.is_empty(),
        "expected at least one signal scenario"
    );

    let mut failures = Vec::new();
    for (slug, sc) in scenarios {
        let name = sc["name"].as_str().expect("scenario name");
        if let Err(e) = run_signal_scenario(&sc) {
            failures.push(format!("{slug}/{name}: {e}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} signal failure(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

fn run_signal_scenario(sc: &Value) -> Result<(), String> {
    let work = TempDir::new("sig");
    let home = TempDir::new("sig-home");
    seed_home(sc, home.path());
    let scratch = work.path().join("fake");
    std::fs::create_dir_all(&scratch).map_err(|e| format!("scratch: {e}"))?;
    install_fake_target(&scratch);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pilotdeck"));
    cmd.env(HOME_ENV, home.path());
    cmd.env_remove(TARGET_BIN_ENV);
    for var in LEGACY_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env(
        "PATH",
        format!(
            "{}:{}",
            scratch.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    cmd.env("AGENTCPM_PD_FAKE_DIR", &scratch);
    let script = serde_json::to_string(sc.get("fakeScript").unwrap_or(&Value::Array(Vec::new())))
        .map_err(|e| format!("serialize fakeScript: {e}"))?;
    cmd.env("AGENTCPM_PD_FAKE_SCRIPT", script);
    if let Some(default) = sc.get("fakeDefault").filter(|v| !v.is_null()) {
        let raw = serde_json::to_string(default).map_err(|e| e.to_string())?;
        cmd.env("AGENTCPM_PD_FAKE_DEFAULT", raw);
    }
    cmd.args(scenario_argv(sc));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;

    // Wait until the submission landed before signaling.
    let log_path = scratch.join("calls.jsonl");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(raw) = std::fs::read_to_string(&log_path)
            && raw.contains("\"run.submit\"")
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("run.submit never appeared in the fake log".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let pid = child.id().to_string();
    let killed = Command::new("/bin/kill")
        .args(["-s", "INT", &pid])
        .status()
        .map_err(|e| format!("kill -INT: {e}"))?;
    if !killed.success() {
        return Err("kill -INT failed".to_owned());
    }

    // The guardian holds the pipes open until it settles the cancellation.
    let output = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    if output.status.signal() != Some(2) {
        return Err(format!("expected death by SIGINT, got {:?}", output.status));
    }

    let log = read_fake_log(&scratch);
    for method in str_array(&sc["expect"]["invokeMethodsContains"]) {
        if !log.iter().any(|e| e["method"].as_str() == Some(method)) {
            return Err(format!(
                "invocation log never called `{method}`: {:?}",
                log.iter().map(|e| e["method"].clone()).collect::<Vec<_>>()
            ));
        }
    }

    if let Some(expected) = sc["expect"]["runRecord"].as_object() {
        let runs_dir = home.path().join("compat").join("runs");
        let mut records: Vec<PathBuf> = std::fs::read_dir(&runs_dir)
            .map_err(|e| format!("read runs dir: {e}"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        if records.len() != 1 {
            return Err(format!("expected exactly 1 run record, found {records:?}"));
        }
        let raw = std::fs::read_to_string(records.remove(0))
            .map_err(|e| format!("read run record: {e}"))?;
        let record: Value = serde_json::from_str(&raw).map_err(|e| format!("parse record: {e}"))?;
        if let Some(state) = expected["state"].as_str()
            && record["state"].as_str() != Some(state)
        {
            return Err(format!("run record state {} != {state}", record["state"]));
        }
        if let Some(terminal) = expected["terminalState"].as_str()
            && record["terminalState"].as_str() != Some(terminal)
        {
            return Err(format!(
                "run record terminalState {} != {terminal}",
                record["terminalState"]
            ));
        }
    }
    Ok(())
}

// Scenario hygiene: every scenario carries an argv array and an expect
// object; empty-argv (default-stdin) scenarios must need the fake target,
// since their stdin flow cannot run without one.
#[test]
fn blackbox_fixture_scenarios_parse() {
    let mut count = 0usize;
    for (slug, sc) in all_scenarios() {
        let name = sc["name"].as_str().unwrap_or("?");
        assert!(
            sc.get("argv").is_some_and(Value::is_array),
            "{slug}/{name}: missing argv array"
        );
        if scenario_argv(&sc).is_empty() && scenario_bin_name(&sc) == "pilotdeck" {
            assert!(
                matches!(scenario_blackbox(&sc).as_str(), "fake" | "signal"),
                "{slug}/{name}: empty argv under the pilotdeck name needs the fake target (default-stdin)"
            );
        }
        assert!(
            sc["expect"].is_object(),
            "{slug}/{name}: missing expect object"
        );
        count += 1;
    }
    assert!(count >= 27 * 2, "expected a rich scenario set, got {count}");
}
