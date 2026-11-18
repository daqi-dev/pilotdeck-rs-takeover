//! Shared harness for the fixture-driven contract tests.
//!
//! The fixture documents (`fixtures/cases/<slug>.json`, schema in
//! `fixtures/README.md`) are the machine-readable source of truth; this
//! module holds the plumbing both test layers use: an isolated temp home
//! (the real `~/.pilotdeck` / `~/.agentcpm-pd` are never touched), fixture
//! loading validated against the builtin manifest, subset JSON matching,
//! and the shim-state seed writers.
//!
//! Mirrors of crate-internal behavior stay out: this module only reads and
//! writes files and compares JSON.

#![allow(dead_code)] // each test binary uses a different subset

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};

/// Environment variable selecting the (isolated) agentcpm-pd home.
pub const HOME_ENV: &str = "AGENTCPM_PD_HOME";
/// Environment hook pinning the target binary (cleared for blackbox children).
pub const TARGET_BIN_ENV: &str = "AGENTCPM_PD_COMPAT_TARGET_BIN";
/// The public CLI the shim targets.
pub const TARGET_BIN_NAME: &str = "agentcpm-pd";

/// Legacy env vars cleared for every blackbox child before the scenario's
/// own `legacyEnv` entries are applied (ambient values must not leak).
pub const LEGACY_ENV_VARS: &[&str] = &[
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

/// An isolated temporary directory removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .join("agentcpm-pd-compat-fixture")
            .join(format!("{tag}-{}-{unique}", std::process::id()));
        for attempt in 0..8 {
            let candidate = if attempt == 0 {
                base.clone()
            } else {
                base.with_file_name(format!(
                    "{}-{attempt}",
                    base.file_name().unwrap().to_string_lossy()
                ))
            };
            match std::fs::create_dir_all(&candidate) {
                Ok(()) => return Self { path: candidate },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("tempdir create failed: {e}"),
            }
        }
        panic!("tempdir create: name collision after retries");
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Directory holding the per-slug fixture documents.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("cases")
}

/// The 27 slugs from the builtin manifest (the authoritative slug list).
pub fn expected_slugs() -> Vec<String> {
    agentcpm_pd_compat::manifest::Manifest::builtin()
        .entries()
        .iter()
        .map(|e| e.slug.to_owned())
        .collect()
}

/// Loads one fixture document (slug, caseId, scenarios).
pub fn load_fixture(slug: &str) -> Value {
    let path = fixtures_dir().join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture `{slug}` unreadable: {e}"));
    let doc: Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture `{slug}` invalid: {e}"));
    assert_eq!(doc["slug"].as_str(), Some(slug), "fixture slug mismatch");
    assert!(
        doc["scenarios"].as_array().is_some_and(|s| !s.is_empty()),
        "fixture `{slug}` has no scenarios"
    );
    doc
}

/// Subset match: every key of `expected` must exist in `actual` with a
/// recursively subset-equal value; arrays require equal length and
/// element-wise subset; leaves require equality.
pub fn json_subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(exp), Value::Object(act)) => exp
            .iter()
            .all(|(k, v)| act.get(k).is_some_and(|a| json_subset(v, a))),
        (Value::Array(exp), Value::Array(act)) => {
            exp.len() == act.len() && exp.iter().zip(act).all(|(e, a)| json_subset(e, a))
        }
        _ => expected == actual,
    }
}

/// Writes the scenario's `shimConfig` document to `<home>/compat/config.json`.
pub fn write_shim_config(home: &Path, config: &Value) {
    let dir = home.join("compat");
    std::fs::create_dir_all(&dir).expect("create compat dir");
    let raw = serde_json::to_string(config).expect("serialize config");
    std::fs::write(dir.join("config.json"), raw).expect("write config");
}

/// Writes the scenario's `shimSessions` map (`{"<legacy>": {"sessionId",
/// "projectId"}}`) as the shim's `compat/sessions.json` table.
pub fn write_shim_sessions(home: &Path, sessions: &Value) {
    let map = sessions
        .as_object()
        .expect("shimSessions must be an object of legacy-id -> row");
    let rows: Vec<Value> = map
        .iter()
        .map(|(legacy, row)| {
            json!({
                "legacySessionId": legacy,
                "sessionId": row.get("sessionId").and_then(Value::as_str).unwrap_or_default(),
                "projectId": row.get("projectId").and_then(Value::as_str).unwrap_or(""),
                "createdAt": "1",
            })
        })
        .collect();
    let doc = json!({"schemaVersion": 1, "sessions": rows});
    let dir = home.join("compat");
    std::fs::create_dir_all(&dir).expect("create compat dir");
    let raw = serde_json::to_string(&doc).expect("serialize sessions");
    std::fs::write(dir.join("sessions.json"), raw).expect("write sessions");
}

/// Seeds shim state for a scenario (config and/or sessions when present).
pub fn seed_home(scenario: &Value, home: &Path) {
    if let Some(config) = scenario.get("shimConfig").filter(|v| !v.is_null()) {
        write_shim_config(home, config);
    }
    if let Some(sessions) = scenario.get("shimSessions").filter(|v| !v.is_null()) {
        write_shim_sessions(home, sessions);
    }
}

/// The scenario's argv as owned strings.
pub fn scenario_argv(scenario: &Value) -> Vec<String> {
    scenario["argv"]
        .as_array()
        .unwrap_or_else(|| panic!("scenario without argv"))
        .iter()
        .map(|v| v.as_str().expect("argv entries are strings").to_owned())
        .collect()
}

/// Scenario field accessors with schema defaults.
pub fn scenario_bin_name(scenario: &Value) -> &str {
    scenario
        .get("binName")
        .and_then(Value::as_str)
        .unwrap_or("pilotdeck")
}

/// The scenario's blackbox mode: "local" (default), "fake", "signal", "none".
pub fn scenario_blackbox(scenario: &Value) -> String {
    scenario
        .get("blackbox")
        .and_then(Value::as_str)
        .unwrap_or("local")
        .to_owned()
}

/// Whether the scenario sets `AGENTCPM_PD_HOME` for the child (default true).
pub fn scenario_home_env(scenario: &Value) -> bool {
    scenario
        .get("homeEnv")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Builds the crate-side `LegacyEnv` (rejected-entry routing) from the
/// scenario's `legacyEnv` map.
pub fn legacy_env_of(scenario: &Value) -> agentcpm_pd_compat::entry::reject::LegacyEnv {
    let var = |key: &str| {
        scenario
            .get("legacyEnv")
            .and_then(|e| e.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    agentcpm_pd_compat::entry::reject::LegacyEnv {
        gateway_mode: var("PILOTDECK_GATEWAY_MODE"),
        agent_loop_factory: var("PILOTDECK_AGENT_LOOP_FACTORY"),
        agent_loop_tcp_host: var("PILOTDECK_AGENT_LOOP_TCP_HOST"),
        agent_loop_tcp_port: var("PILOTDECK_AGENT_LOOP_TCP_PORT"),
    }
}

/// One fixture scenario, flattened with its provenance for failure messages.
pub struct Scenario<'a> {
    pub slug: String,
    pub name: String,
    pub doc: &'a Value,
}

/// Iterates every scenario of every fixture, manifest-slug-ordered.
pub fn all_scenarios() -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for slug in expected_slugs() {
        let doc = load_fixture(&slug);
        for scenario in doc["scenarios"].as_array().expect("scenarios array") {
            out.push((slug.clone(), scenario.clone()));
        }
    }
    out
}
