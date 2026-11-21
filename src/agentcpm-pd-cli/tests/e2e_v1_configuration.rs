//! End-to-end suite for the `configuration.*` domain (`cli.task.config`,
//! S3): the five operations driven through the real `agentcpm-pd` argv
//! (`settings get/set/validate/reload/layer`) against an isolated
//! `AGENTCPM_PD_HOME` and the real `/v1/ws` transport.
//!
//! Acceptance anchors covered here (the unit suite in the gateway covers
//! the same anchors at the store level):
//! * invalid configuration never replaces the old generation;
//! * secret omission / masked round-trip never clears the stored value;
//! * the configuration generation an active run holds is never hot-swapped.
//!
//! Local execution note: this machine is under a build freeze (disk
//! incident, 2026-11-18), so the suite was written but not executed here;
//! it runs in the integrator's cb8 verification pass.

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use serde_json::{Value, json};

use common_v1::*;

/// Parses a failed argv invocation: exactly one envelope line, `outcome:
/// "failed"`, and a non-zero exit. Returns the `error` object.
fn expect_error(method: &str, out: &CliOutcome) -> Value {
    assert_ne!(
        out.code, 0,
        "`{method}` must exit non-zero; stdout:\n{}\nstderr:\n{}",
        out.stdout, out.stderr
    );
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
    assert_eq!(value["version"], OP_VERSION, "{value}");
    assert_eq!(value["outcome"], "failed", "{value}");
    value["error"].clone()
}

async fn settings_get(home: &std::path::Path, scope: &str, keys: Option<&str>) -> Value {
    let mut args = vec!["settings", "get", "--output", "json", "--scope", scope];
    if let Some(keys) = keys {
        args.extend(["--keys", keys]);
    }
    let out = await_cli(home, &args);
    expect_ok("configuration.resolve", &out)
}

async fn settings_set(
    home: &std::path::Path,
    scope: &str,
    expected_revision: &str,
    changes: &str,
) -> Value {
    let out = await_cli(
        home,
        &[
            "settings",
            "set",
            "--output",
            "json",
            "--scope",
            scope,
            "--expected-revision",
            expected_revision,
            "--changes",
            changes,
        ],
    );
    expect_ok("configuration.update", &out)
}

async fn settings_set_err(
    home: &std::path::Path,
    scope: &str,
    expected_revision: &str,
    changes: &str,
) -> Value {
    let out = await_cli(
        home,
        &[
            "settings",
            "set",
            "--output",
            "json",
            "--scope",
            scope,
            "--expected-revision",
            expected_revision,
            "--changes",
            changes,
        ],
    );
    expect_error("configuration.update", &out)
}

fn value_of<'a>(result: &'a Value, key: &str) -> Option<&'a Value> {
    result["values"]
        .as_array()?
        .iter()
        .find(|v| v["key"] == key)
        .map(|v| &v["value"])
}

const INSTANCE: &str = "{\"kind\":\"instance\"}";

/// The full layered round trip: instance < project < session with source
/// attribution, secret masking, per-layer reads, and revisions/generations
/// that advance with each write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_layered_round_trip_sources_and_masks() {
    let home = TestHome::new("cfg-e2e-layers");
    let root = home.path();
    init_home(root);

    // A fresh instance resolves an empty instance layer with a current,
    // explainable state (no empty success pretending a configured runtime).
    let resolved = settings_get(root, INSTANCE, None).await;
    assert_eq!(resolved["values"], json!([]));
    assert_eq!(resolved["storedRevision"], "cfg-0");
    assert_eq!(resolved["effectiveGeneration"], "cfggen-0");
    assert_eq!(resolved["reloadState"], "current");

    let first = settings_set(
        root,
        INSTANCE,
        "cfg-0",
        r#"[
            {"key": "theme", "action": "set", "value": "dark"},
            {"key": "provider.apiKey", "action": "set", "value": "sk-e2e-abc"}
        ]"#,
    )
    .await;
    assert_eq!(first["reloadState"], "current");
    assert_eq!(first["storedRevision"], "cfg-1");
    assert_eq!(first["effectiveGeneration"], "cfggen-1");

    // Secrets are masked (not unset) and flagged sensitive; plain values
    // round-trip and attribute to their source layer.
    let resolved = settings_get(root, INSTANCE, None).await;
    assert_eq!(value_of(&resolved, "theme"), Some(&json!("dark")));
    assert_eq!(
        value_of(&resolved, "provider.apiKey"),
        Some(&json!("<redacted>"))
    );
    let key_entry = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "provider.apiKey")
        .unwrap()
        .clone();
    assert_eq!(key_entry["sensitive"], true);
    assert_eq!(key_entry["sourceScope"], json!({"kind": "instance"}));
    assert_eq!(key_entry["sourceRevision"], "cfg-1");

    // Project and session overlays shadow the instance layer; the effective
    // value reports the layer it came from.
    let (project_id, session_id) = seed_project_session(root, "cfg").await;
    let project_scope = format!("{{\"kind\":\"project\",\"projectId\":\"{project_id}\"}}");
    let session_scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
    );

    let project_update = settings_set(
        root,
        &project_scope,
        "cfg-0",
        r#"[{"key": "theme", "action": "set", "value": "light"}]"#,
    )
    .await;
    assert_eq!(project_update["storedRevision"], "cfg-2");

    let resolved = settings_get(root, &project_scope, None).await;
    assert_eq!(value_of(&resolved, "theme"), Some(&json!("light")));
    let theme = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "theme")
        .unwrap()
        .clone();
    assert_eq!(theme["sourceScope"]["kind"], "project");
    assert_eq!(theme["sourceScope"]["projectId"], project_id.as_str());
    // The instance secret still shows through, attributed to its own layer.
    let key_entry = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "provider.apiKey")
        .unwrap()
        .clone();
    assert_eq!(key_entry["sourceScope"]["kind"], "instance");
    assert_eq!(key_entry["value"], "<redacted>");

    // read_layer returns exactly one raw layer (no effective mixing).
    let out = await_cli(
        root,
        &[
            "settings",
            "layer",
            "--output",
            "json",
            "--scope",
            &project_scope,
        ],
    );
    let layer = expect_ok("configuration.read_layer", &out);
    let values = layer["values"].as_array().unwrap().clone();
    assert_eq!(
        values.len(),
        1,
        "project layer holds only its own entry: {values:?}"
    );
    assert_eq!(values[0]["key"], "theme");
    assert_eq!(values[0]["value"], "light");

    // The session scope resolves over the whole stack and honors `keys`.
    settings_set(
        root,
        &session_scope,
        "cfg-0",
        r#"[{"key": "featureX", "action": "set", "value": true}]"#,
    )
    .await;
    let resolved = settings_get(root, &session_scope, None).await;
    assert_eq!(value_of(&resolved, "featureX"), Some(&json!(true)));
    let feature = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "featureX")
        .unwrap()
        .clone();
    assert_eq!(feature["sourceScope"]["kind"], "session");

    let filtered = settings_get(root, &session_scope, Some(r#"["theme"]"#)).await;
    assert_eq!(filtered["values"].as_array().unwrap().len(), 1);
    assert_eq!(value_of(&filtered, "theme"), Some(&json!("light")));
}

/// Anchor: an invalid change list (or a stale revision) never replaces the
/// stored configuration or the effective generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_update_and_stale_revision_keep_the_old_generation() {
    let home = TestHome::new("cfg-e2e-invalid");
    let root = home.path();
    init_home(root);

    let first = settings_set(
        root,
        INSTANCE,
        "cfg-0",
        r#"[{"key": "a", "action": "set", "value": 1}]"#,
    )
    .await;
    let stored = first["storedRevision"].as_str().unwrap().to_owned();
    let generation = first["effectiveGeneration"].as_str().unwrap().to_owned();

    // An empty key rejects the whole change list.
    let error = settings_set_err(
        root,
        INSTANCE,
        &stored,
        r#"[{"key": "", "action": "set", "value": "x"}]"#,
    )
    .await;
    assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");
    assert!(
        error["details"]["issues"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "issues ride along: {error}"
    );

    // A stale revision is a conflict, not an overwrite.
    let error = settings_set_err(
        root,
        INSTANCE,
        "cfg-0",
        r#"[{"key": "a", "action": "set", "value": 9}]"#,
    )
    .await;
    assert_eq!(error["code"], "REVISION_CONFLICT", "{error}");

    // Nothing changed: same revision, values, and generation.
    let resolved = settings_get(root, INSTANCE, None).await;
    assert_eq!(resolved["storedRevision"], stored.as_str());
    assert_eq!(resolved["effectiveGeneration"], generation.as_str());
    assert_eq!(value_of(&resolved, "a"), Some(&json!(1)));
    assert_eq!(resolved["reloadState"], "current");
}

/// Anchor: omitting a secret's value (or round-tripping the read-path mask)
/// keeps the stored value — the key never disappears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_omission_and_mask_roundtrip_do_not_clear() {
    let home = TestHome::new("cfg-e2e-secret");
    let root = home.path();
    init_home(root);

    settings_set(
        root,
        INSTANCE,
        "cfg-0",
        r#"[
            {"key": "provider.apiKey", "action": "set", "value": "sk-e2e-xyz"},
            {"key": "theme", "action": "set", "value": "dark"}
        ]"#,
    )
    .await;

    // Omitted value: preserved, and the resolve still reports the key.
    let rev = settings_get(root, INSTANCE, None).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = settings_set(
        root,
        INSTANCE,
        &rev,
        r#"[
            {"key": "provider.apiKey", "action": "set"},
            {"key": "theme", "action": "set", "value": "light"}
        ]"#,
    )
    .await;
    assert!(
        result["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "VALUE_OMITTED_PRESERVED")
    );
    let resolved = settings_get(root, INSTANCE, None).await;
    assert_eq!(
        value_of(&resolved, "provider.apiKey"),
        Some(&json!("<redacted>")),
        "an omitted secret must stay present (masked), never unset"
    );
    assert_eq!(
        resolved["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["key"] == "provider.apiKey")
            .unwrap()["sensitive"],
        true
    );

    // Round-tripping the mask back does not overwrite the secret with the
    // mask literal.
    let rev = resolved["storedRevision"].as_str().unwrap().to_owned();
    let result = settings_set(
        root,
        INSTANCE,
        &rev,
        r#"[{"key": "provider.apiKey", "action": "set", "value": "<redacted>"}]"#,
    )
    .await;
    assert!(
        result["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "MASKED_SECRET_PRESERVED")
    );
    let resolved = settings_get(root, INSTANCE, None).await;
    assert_eq!(
        value_of(&resolved, "provider.apiKey"),
        Some(&json!("<redacted>"))
    );
}

/// `settings validate` is a pure dry run: diagnostics without writes, stable
/// candidate digest, and a hard rejection on a stale revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_is_a_dry_run_with_stable_digest() {
    let home = TestHome::new("cfg-e2e-validate");
    let root = home.path();
    init_home(root);
    settings_set(
        root,
        INSTANCE,
        "cfg-0",
        r#"[{"key": "a", "action": "set", "value": 1}]"#,
    )
    .await;
    let rev = settings_get(root, INSTANCE, None).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    // Invalid candidate: diagnostics, valid=false, nothing written.
    let out = await_cli(
        root,
        &[
            "settings",
            "validate",
            "--output",
            "json",
            "--scope",
            INSTANCE,
            "--expected-revision",
            &rev,
            "--changes",
            r#"[{"key": "", "action": "set", "value": "x"}]"#,
        ],
    );
    let result = expect_ok("configuration.validate", &out);
    assert_eq!(result["valid"], false);
    assert!(
        result["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "EMPTY_KEY")
    );
    let digest = result["candidateDigest"].as_str().unwrap().to_owned();
    assert_eq!(digest.len(), 64);
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    let resolved = settings_get(root, INSTANCE, None).await;
    assert_eq!(
        resolved["storedRevision"],
        rev.as_str(),
        "validate must not write"
    );

    // Valid candidate: stable digest across identical runs.
    let mut args = vec![
        "settings",
        "validate",
        "--output",
        "json",
        "--scope",
        INSTANCE,
        "--expected-revision",
        rev.as_str(),
        "--changes",
        r#"[{"key": "b", "action": "set", "value": 2}]"#,
    ];
    let first = expect_ok("configuration.validate", &await_cli(root, &args));
    args.pop();
    args.push(r#"[{"key": "b", "action": "set", "value": 2}]"#);
    let second = expect_ok("configuration.validate", &await_cli(root, &args));
    assert_eq!(first["valid"], true);
    assert_eq!(first["candidateDigest"], second["candidateDigest"]);

    // Stale revision: a conflict error, not a valid=false result.
    args[7] = "cfg-0";
    let out = await_cli(root, &args);
    let error = expect_error("configuration.validate", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT", "{error}");
}

/// `settings reload` submits an idempotent async operation: the receipt is
/// queryable by key, replays deduplicate, and conflicts are explicit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_receipt_dedup_and_lookup() {
    let home = TestHome::new("cfg-e2e-reload");
    let root = home.path();
    init_home(root);
    settings_set(
        root,
        INSTANCE,
        "cfg-0",
        r#"[{"key": "a", "action": "set", "value": 1}]"#,
    )
    .await;
    let rev = settings_get(root, INSTANCE, None).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    let reload_args = |key: &str| {
        vec![
            "settings".to_string(),
            "reload".to_string(),
            "--output".to_string(),
            "json".to_string(),
            "--scope".to_string(),
            INSTANCE.to_string(),
            "--expected-stored-revision".to_string(),
            rev.clone(),
            "--request-key".to_string(),
            key.to_string(),
        ]
    };
    let refs_vec = reload_args("e2e-cfg-reload-1");
    let refs: Vec<&str> = refs_vec.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let receipt = expect_envelope("configuration.reload", &out);
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["state"], "succeeded");
    assert_eq!(receipt["deduplicated"], false);
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    assert!(!operation_id.is_empty());

    // The same key + input returns the original receipt, deduplicated.
    let refs_vec = reload_args("e2e-cfg-reload-1");
    let refs: Vec<&str> = refs_vec.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let again = expect_envelope("configuration.reload", &out);
    assert_eq!(again["operationId"], operation_id.as_str());
    assert_eq!(again["deduplicated"], true);

    // The same key with different input is a content conflict.
    let mut conflicting = reload_args("e2e-cfg-reload-1");
    conflicting[7] = "cfg-0".to_string();
    let refs: Vec<&str> = conflicting.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let error = expect_error("configuration.reload", &out);
    assert_eq!(error["code"], "IDEMPOTENCY_CONFLICT", "{error}");

    // A fresh key with a stale revision is rejected definitively, and the
    // rejection is observable through requests.lookup.
    let mut stale = reload_args("e2e-cfg-reload-2");
    stale[7] = "cfg-0".to_string();
    let refs: Vec<&str> = stale.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let error = expect_error("configuration.reload", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT", "{error}");

    let lookup = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "configuration.reload",
            "--idempotency-key",
            "e2e-cfg-reload-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &lookup);
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(lookup["resource"]["operationId"], operation_id.as_str());

    let lookup = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "configuration.reload",
            "--idempotency-key",
            "e2e-cfg-reload-2",
        ],
    );
    let lookup = expect_ok("requests.lookup", &lookup);
    assert_eq!(lookup["requestState"], "notAccepted");
    assert_eq!(lookup["rejectionCode"], "REVISION_CONFLICT");
}

/// Anchor: the configuration generation an active run holds is exposed by
/// `run get` and is never hot-swapped by later configuration changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_generation_is_explainable_and_not_hot_swapped() {
    let home = TestHome::new("cfg-e2e-run");
    let root = home.path();
    init_home(root);
    let (_project_id, session_id) = seed_project_session(root, "run").await;

    // Submit and settle one run before any configuration change.
    let submitted = submit_run(root, &session_id, "before config change", "e2e-cfg-run-1").await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(root, &run_id);
    let _ = expect_envelope("run.wait", &waited);

    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    let run = expect_ok("run.get", &out);
    let config = run["run"]["config"].clone();
    assert!(
        config["configRevision"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "run.get must expose the config revision the run holds: {config}"
    );
    assert!(
        config["effectiveGeneration"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "run.get must expose the effective generation the run holds: {config}"
    );

    // Store two configuration generations.
    settings_set(
        root,
        INSTANCE,
        "cfg-0",
        r#"[{"key": "a", "action": "set", "value": 1}]"#,
    )
    .await;
    let rev = settings_get(root, INSTANCE, None).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = settings_set(
        root,
        INSTANCE,
        &rev,
        r#"[{"key": "a", "action": "set", "value": 2}]"#,
    )
    .await;
    assert_eq!(second["reloadState"], "current");

    // The settled run still reports the configuration it ran with.
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    let run = expect_ok("run.get", &out);
    assert_eq!(
        run["run"]["config"], config,
        "a run must keep the configuration generation it holds across config changes"
    );

    // A new run submitted after the configuration update captures the new
    // applied configuration generation and revision.
    let out = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--prompt",
            "anchor3-after-update",
        ],
    );
    let submitted2 = expect_ok("run.submit", &out);
    let run2_id = submitted2["runId"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run2_id],
    );
    let run2 = expect_ok("run.get", &out);
    assert_eq!(
        run2["run"]["config"]["effectiveGeneration"],
        second["effectiveGeneration"]
    );
    assert_eq!(
        run2["run"]["config"]["configRevision"],
        second["storedRevision"]
    );
    assert_ne!(run2["run"]["config"], config);
}
