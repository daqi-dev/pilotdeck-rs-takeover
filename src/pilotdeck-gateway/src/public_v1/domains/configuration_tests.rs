//! Unit and integration tests for the `configuration.*` domain
//! (`cli.task.config`). The three acceptance anchors get dedicated tests:
//! invalid configuration never replaces the old generation, secret omission
//! never clears, and the configuration generation an active run holds stays
//! explainable (and is never hot-swapped).

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::registry::OperationRegistry;
use serde_json::{Value, json};

use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{InstanceMeta, V1Store};
use crate::public_v1::test_support::TempDir;

use super::configuration_store::ConfigDb;

fn open_runtime(root_dir: &std::path::Path) -> (Arc<PublicV1Runtime>, String) {
    let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-test-1".to_owned(),
        server_revision: "srv-test-1".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };
    let store = V1Store::open_and_recover(root_dir, meta).unwrap();
    let rt = PublicV1Runtime::new(store);
    (rt, file.instance_id.clone())
}

fn setup(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, String) {
    let tmp = TempDir::new(tag);
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (rt, iid) = open_runtime(&root_dir);
    (tmp, rt, iid)
}

/// Registers a project + session through the dispatched operations (the
/// scopes the configuration layers hang off).
async fn seed_project_session(rt: &Arc<PublicV1Runtime>, iid: &str) {
    rt.dispatch(
        "project.register",
        json!({
            "instanceId": iid,
            "projectId": "proj-cfg",
            "name": "cfg",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"}
        }),
        None,
        None,
    )
    .await
    .unwrap();
    rt.dispatch(
        "session.create",
        json!({"instanceId": iid, "sessionId": "sess-cfg", "projectId": "proj-cfg"}),
        None,
        None,
    )
    .await
    .unwrap();
}

async fn resolve(rt: &Arc<PublicV1Runtime>, iid: &str, scope: Value) -> Value {
    let (outcome, result) = rt
        .dispatch(
            "configuration.resolve",
            json!({"instanceId": iid, "scope": scope}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Succeeded);
    OperationRegistry::global()
        .validate_result("configuration.resolve", &result)
        .expect("resolve result passes the contract schema");
    result
}

async fn update(
    rt: &Arc<PublicV1Runtime>,
    iid: &str,
    scope: Value,
    expected_revision: &str,
    changes: Value,
) -> Result<Value, DispatchError> {
    let (outcome, result) = rt
        .dispatch(
            "configuration.update",
            json!({
                "instanceId": iid,
                "scope": scope,
                "expectedRevision": expected_revision,
                "changes": changes,
            }),
            None,
            None,
        )
        .await?;
    assert_eq!(outcome, ResponseOutcome::Succeeded);
    OperationRegistry::global()
        .validate_result("configuration.update", &result)
        .expect("update result passes the contract schema");
    Ok(result)
}

fn contract_code(error: &DispatchError) -> String {
    match error {
        DispatchError::Application(err) => err.code.clone(),
        DispatchError::Rpc { data, .. } => data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

fn instance_scope() -> Value {
    json!({"kind": "instance"})
}

fn project_scope() -> Value {
    json!({"kind": "project", "projectId": "proj-cfg"})
}

fn session_scope() -> Value {
    json!({"kind": "session", "projectId": "proj-cfg", "sessionId": "sess-cfg"})
}

fn value_of<'a>(result: &'a Value, key: &str) -> Option<&'a Value> {
    result["values"]
        .as_array()?
        .iter()
        .find(|v| v["key"] == key)
        .map(|v| &v["value"])
}

// ---------------------------------------------------------------------------
// Layering, masking, source attribution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_folds_layers_and_reports_sources_and_masks_secrets() {
    let (_tmp, rt, iid) = setup("cfg-layers");
    seed_project_session(&rt, &iid).await;

    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([
            {"key": "theme", "action": "set", "value": "dark"},
            {"key": "provider.apiKey", "action": "set", "value": "sk-live-123"},
        ]),
    )
    .await
    .unwrap();
    update(
        &rt,
        &iid,
        project_scope(),
        "cfg-0",
        json!([{"key": "theme", "action": "set", "value": "light"}]),
    )
    .await
    .unwrap();

    // instance scope: only the instance layer.
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(value_of(&resolved, "theme"), Some(&json!("dark")));
    assert_eq!(
        resolved["values"].as_array().unwrap().len(),
        2,
        "instance resolve sees exactly the instance layer"
    );

    // project scope: the project overlay shadows `theme`, the instance
    // value stays attributed to its own layer.
    let resolved = resolve(&rt, &iid, project_scope()).await;
    assert_eq!(value_of(&resolved, "theme"), Some(&json!("light")));
    let theme = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "theme")
        .unwrap()
        .clone();
    assert_eq!(theme["sourceScope"], project_scope());
    assert!(
        theme["sourceRevision"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert_eq!(theme["sensitive"], false);

    // secrets are masked, never unset, and flagged sensitive.
    let key_entry = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "provider.apiKey")
        .unwrap()
        .clone();
    assert_eq!(key_entry["value"], "<redacted>");
    assert_eq!(key_entry["sensitive"], true);
    assert_eq!(key_entry["sourceScope"], instance_scope());

    // keys filter narrows the result.
    let _resolved = resolve(&rt, &iid, project_scope()).await;
    let filtered = rt
        .dispatch(
            "configuration.resolve",
            json!({"instanceId": iid, "scope": project_scope(), "keys": ["theme"]}),
            None,
            None,
        )
        .await
        .unwrap()
        .1;
    assert_eq!(filtered["values"].as_array().unwrap().len(), 1);
    assert_eq!(value_of(&filtered, "theme"), Some(&json!("light")));
    assert!(value_of(&filtered, "provider.apiKey").is_none());
}

#[tokio::test]
async fn read_layer_returns_one_raw_layer_with_secrets_masked() {
    let (_tmp, rt, iid) = setup("cfg-read-layer");
    seed_project_session(&rt, &iid).await;
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([
            {"key": "theme", "action": "set", "value": "dark"},
            {"key": "provider.apiKey", "action": "set", "value": "sk-live-123"},
        ]),
    )
    .await
    .unwrap();
    update(
        &rt,
        &iid,
        project_scope(),
        "cfg-0",
        json!([{"key": "feature", "action": "set", "value": true}]),
    )
    .await
    .unwrap();

    // The instance layer only (no effective mixing, no project keys).
    let (outcome, result) = rt
        .dispatch(
            "configuration.read_layer",
            json!({"instanceId": iid, "scope": instance_scope()}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Succeeded);
    OperationRegistry::global()
        .validate_result("configuration.read_layer", &result)
        .expect("read_layer result passes the contract schema");
    let values = result["values"].as_array().unwrap().clone();
    assert_eq!(values.len(), 2);
    assert!(
        values
            .iter()
            .any(|v| v["key"] == "theme" && v["value"] == "dark")
    );
    assert!(
        values
            .iter()
            .any(|v| v["key"] == "provider.apiKey" && v["value"] == "<redacted>")
    );
    assert!(result["revision"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn unset_in_a_layer_re_exposes_the_lower_layer_value() {
    let (_tmp, rt, iid) = setup("cfg-unset");
    seed_project_session(&rt, &iid).await;
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "k", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let project_update = update(
        &rt,
        &iid,
        project_scope(),
        "cfg-0",
        json!([{"key": "k", "action": "set", "value": 2}]),
    )
    .await
    .unwrap();
    let project_rev = project_update["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    let resolved = resolve(&rt, &iid, project_scope()).await;
    assert_eq!(value_of(&resolved, "k"), Some(&json!(2)));

    update(
        &rt,
        &iid,
        project_scope(),
        &project_rev,
        json!([{"key": "k", "action": "unset"}]),
    )
    .await
    .unwrap();
    let resolved = resolve(&rt, &iid, project_scope()).await;
    assert_eq!(
        value_of(&resolved, "k"),
        Some(&json!(1)),
        "unset removes the overlay entry; the instance value shows through"
    );
    let k_entry = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "k")
        .unwrap()
        .clone();
    assert_eq!(k_entry["sourceScope"], instance_scope());
}

// ---------------------------------------------------------------------------
// Anchor 1: invalid configuration never replaces the old generation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anchor_invalid_update_keeps_the_old_generation() {
    let (_tmp, rt, iid) = setup("cfg-invalid");

    let first = update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "a", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let stored = first["storedRevision"].as_str().unwrap().to_owned();
    let generation = first["effectiveGeneration"].as_str().unwrap().to_owned();
    assert_eq!(first["reloadState"], "current");

    // An empty key fails validation; the whole change list is rejected.
    let error = update(
        &rt,
        &iid,
        instance_scope(),
        &stored,
        json!([
            {"key": "", "action": "set", "value": "x"},
            {"key": "b", "action": "set", "value": 2},
        ]),
    )
    .await
    .unwrap_err();
    assert_eq!(contract_code(&error), "INVALID_ARGUMENT");
    if let DispatchError::Application(err) = &error {
        let issues = err
            .details
            .as_ref()
            .and_then(|d| d.get("issues"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(issues.iter().any(|i| i["code"] == "EMPTY_KEY"));
    } else {
        panic!("expected an application error, got {error:?}");
    }

    // Nothing was written: same revision, same values, same generation.
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(resolved["storedRevision"], stored);
    assert_eq!(resolved["effectiveGeneration"], generation);
    assert_eq!(value_of(&resolved, "a"), Some(&json!(1)));
    assert!(value_of(&resolved, "b").is_none());
    assert_eq!(resolved["reloadState"], "current");

    // A valueless set of a key that does not exist yet is also rejected.
    let error = update(
        &rt,
        &iid,
        instance_scope(),
        &stored,
        json!([{"key": "fresh", "action": "set"}]),
    )
    .await
    .unwrap_err();
    assert_eq!(contract_code(&error), "INVALID_ARGUMENT");

    // Stale expectedRevision: REVISION_CONFLICT, nothing written.
    let error = update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "a", "action": "set", "value": 9}]),
    )
    .await
    .unwrap_err();
    assert_eq!(contract_code(&error), "REVISION_CONFLICT");
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(value_of(&resolved, "a"), Some(&json!(1)));
    assert_eq!(resolved["storedRevision"], stored);
}

// ---------------------------------------------------------------------------
// Anchor 2: secret omission never clears the stored value
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anchor_secret_omission_and_mask_roundtrip_preserve_the_value() {
    let (_tmp, rt, iid) = setup("cfg-secret");
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([
            {"key": "provider.apiKey", "action": "set", "value": "sk-live-abc"},
            {"key": "theme", "action": "set", "value": "dark"},
        ]),
    )
    .await
    .unwrap();

    let home = rt.store().home().to_path_buf();
    let stored_secret = || -> String {
        let db = ConfigDb::open(&home).unwrap();
        let entry = db
            .read_layer("instance")
            .unwrap()
            .get("provider.apiKey")
            .cloned()
            .expect("secret entry exists");
        entry.value.as_str().unwrap().to_owned()
    };
    assert_eq!(stored_secret(), "sk-live-abc");

    // Omitting the value on a set keeps the current value ("秘密省略不清空").
    let rev = resolve(&rt, &iid, instance_scope()).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([
            {"key": "provider.apiKey", "action": "set"},
            {"key": "theme", "action": "set", "value": "light"},
        ]),
    )
    .await
    .unwrap();
    assert!(
        result["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "VALUE_OMITTED_PRESERVED")
    );
    assert_eq!(stored_secret(), "sk-live-abc");
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(
        value_of(&resolved, "provider.apiKey"),
        Some(&json!("<redacted>"))
    );

    // Round-tripping the read-path mask back does not overwrite the secret
    // with the mask literal (the baseline's `preserveMaskedSecrets` rule).
    let rev = resolved["storedRevision"].as_str().unwrap().to_owned();
    update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([{"key": "provider.apiKey", "action": "set", "value": "<redacted>"}]),
    )
    .await
    .unwrap();
    assert_eq!(stored_secret(), "sk-live-abc");

    // An unresolved mask (no current value to preserve) is a hard error.
    let rev = resolve(&rt, &iid, instance_scope()).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let error = update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([{"key": "provider.token", "action": "set", "value": "<redacted>"}]),
    )
    .await
    .unwrap_err();
    assert_eq!(contract_code(&error), "INVALID_ARGUMENT");
}

// ---------------------------------------------------------------------------
// Anchor 3: the generation an active run holds stays explainable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anchor_run_generation_is_explainable_and_not_hot_swapped() {
    let (_tmp, rt, iid) = setup("cfg-run-gen");
    seed_project_session(&rt, &iid).await;

    // A run submitted before any config change.
    let run_id = match rt
        .store()
        .submit_run(
            &json!({
                "instanceId": iid,
                "sessionId": "sess-cfg",
                "input": [{"kind": "text", "text": "queued"}],
            }),
            Some("key-cfg-run-1"),
        )
        .unwrap()
    {
        crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
        other => panic!("expected a new run, got {other:?}"),
    };
    let run_before = rt.store().get_run(&run_id).unwrap();
    let config_before = run_before.to_contract_json()["config"].clone();
    assert!(
        config_before["configRevision"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "run.get must expose the config revision the run holds: {config_before}"
    );
    assert!(
        config_before["effectiveGeneration"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "run.get must expose the effective generation the run holds"
    );

    // Store two configuration generations after the run was submitted.
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "a", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let rev = resolve(&rt, &iid, instance_scope()).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([{"key": "a", "action": "set", "value": 2}]),
    )
    .await
    .unwrap();

    // The in-flight run is not hot-swapped: it keeps the config it was
    // submitted with, and both observation paths agree.
    let run_after = rt.store().get_run(&run_id).unwrap();
    assert_eq!(
        run_after.to_contract_json()["config"],
        config_before,
        "an active run must keep the configuration generation it started with"
    );
    let (_, snapshot) = rt
        .dispatch(
            "run.snapshot",
            json!({"instanceId": iid, "runId": run_id}),
            None,
            None,
        )
        .await
        .unwrap();
    let snapshot_run = snapshot["run"].clone();
    assert_eq!(snapshot_run["config"], config_before);

    // The configuration's own generation advanced and a newly submitted run
    // picks up the new applied generation and revision without affecting the
    // earlier in-flight run.
    assert_ne!(
        second["effectiveGeneration"], config_before["effectiveGeneration"],
        "a stored config change must advance the configuration generation"
    );

    let new_run_id = match rt
        .store()
        .submit_run(
            &json!({
                "instanceId": iid,
                "sessionId": "sess-cfg",
                "input": [{"kind": "text", "text": "after-config"}],
            }),
            Some("key-cfg-run-2"),
        )
        .unwrap()
    {
        crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
        other => panic!("expected a new run, got {other:?}"),
    };
    let run_after_cfg = rt.store().get_run(&new_run_id).unwrap();
    let config_new_run = run_after_cfg.to_contract_json()["config"].clone();
    assert_eq!(
        config_new_run["effectiveGeneration"], second["effectiveGeneration"],
        "new runs submitted after configuration.update must bind the new effectiveGeneration"
    );
    assert_eq!(
        config_new_run["configRevision"], second["storedRevision"],
        "new runs submitted after configuration.update must bind the applied configRevision"
    );
    assert_ne!(config_new_run, config_before);
}

// ---------------------------------------------------------------------------
// validate: dry-run diagnostics and digest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validate_reports_issues_without_writing() {
    let (_tmp, rt, iid) = setup("cfg-validate");
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "a", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let rev = resolve(&rt, &iid, instance_scope()).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    let (outcome, result) = rt
        .dispatch(
            "configuration.validate",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedRevision": rev,
                "changes": [{"key": "", "action": "set", "value": "x"}],
            }),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Succeeded);
    OperationRegistry::global()
        .validate_result("configuration.validate", &result)
        .expect("validate result passes the contract schema");
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

    // A valid candidate digests stably and writes nothing.
    let valid_input = json!({
        "instanceId": iid,
        "scope": instance_scope(),
        "expectedRevision": rev,
        "changes": [{"key": "b", "action": "set", "value": 2}],
    });
    let (_, first) = rt
        .dispatch("configuration.validate", valid_input.clone(), None, None)
        .await
        .unwrap();
    let (_, second) = rt
        .dispatch("configuration.validate", valid_input, None, None)
        .await
        .unwrap();
    assert_eq!(first["valid"], true);
    assert_eq!(first["candidateDigest"], second["candidateDigest"]);
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert!(
        value_of(&resolved, "b").is_none(),
        "validate must not write"
    );
    assert_eq!(resolved["storedRevision"], rev);

    // A stale revision is rejected outright.
    let error = rt
        .dispatch(
            "configuration.validate",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedRevision": "cfg-0",
                "changes": [{"key": "b", "action": "set", "value": 2}],
            }),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(contract_code(&error), "REVISION_CONFLICT");
}

#[tokio::test]
async fn candidate_digest_hashes_the_masked_layer() {
    // Two homes with the same logical layer but different secret plaintexts
    // digest identically: the digest is computed over the masked form.
    let cases = [("sk-one", "digest-a"), ("sk-two", "digest-b")];
    let mut digests = Vec::new();
    for (secret, tag) in cases {
        let (_tmp, rt, iid) = setup(tag);
        update(
            &rt,
            &iid,
            instance_scope(),
            "cfg-0",
            json!([
                {"key": "provider.apiKey", "action": "set", "value": secret},
                {"key": "n", "action": "set", "value": 7},
            ]),
        )
        .await
        .unwrap();
        let rev = resolve(&rt, &iid, instance_scope()).await["storedRevision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, result) = rt
            .dispatch(
                "configuration.validate",
                json!({
                    "instanceId": iid,
                    "scope": instance_scope(),
                    "expectedRevision": rev,
                    "changes": [{"key": "n", "action": "set", "value": 8}],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        digests.push(result["candidateDigest"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        digests[0], digests[1],
        "the candidate digest must not encode secret plaintexts"
    );
}

// ---------------------------------------------------------------------------
// reload: idempotent receipt, dedup, conflict, lookup, failure retention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reload_returns_an_operation_receipt_and_deduplicates() {
    let (_tmp, rt, iid) = setup("cfg-reload");
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "a", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let rev = resolve(&rt, &iid, instance_scope()).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    let input = json!({
        "instanceId": iid,
        "scope": instance_scope(),
        "expectedStoredRevision": rev,
    });
    let (outcome, receipt) = rt
        .dispatch(
            "configuration.reload",
            input.clone(),
            Some("key-reload-1"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Accepted);
    OperationRegistry::global()
        .validate_result("configuration.reload", &receipt)
        .expect("reload receipt passes the contract schema");
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["deduplicated"], false);
    assert_eq!(receipt["state"], "succeeded");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

    // Same key + same semantic input: the original receipt, deduplicated.
    let (outcome, again) = rt
        .dispatch(
            "configuration.reload",
            input.clone(),
            Some("key-reload-1"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Accepted);
    assert_eq!(again["operationId"], operation_id.as_str());
    assert_eq!(again["deduplicated"], true);

    // Same key + different input: a content conflict, nothing executed.
    let mut different = input.clone();
    different["expectedStoredRevision"] = json!("cfg-0");
    let error = rt
        .dispatch(
            "configuration.reload",
            different,
            Some("key-reload-1"),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(contract_code(&error), "IDEMPOTENCY_CONFLICT");

    // A stale revision with a fresh key: rejected and durably recorded.
    let error = rt
        .dispatch(
            "configuration.reload",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedStoredRevision": "cfg-0",
            }),
            Some("key-reload-2"),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(contract_code(&error), "REVISION_CONFLICT");
    let lookup = rt
        .store()
        .lookup_request("configuration.reload", "key-reload-2")
        .unwrap();
    assert_eq!(lookup["requestState"], "notAccepted");
    assert_eq!(lookup["rejectionCode"], "REVISION_CONFLICT");

    // The accepted submission is observable through requests.lookup.
    let lookup = rt
        .store()
        .lookup_request("configuration.reload", "key-reload-1")
        .unwrap();
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(lookup["resource"]["operationId"], operation_id.as_str());

    // A no-change reload keeps the current generation (the baseline's
    // `unchanged`): the stored revision is already incorporated.
    let (_, result) = rt
        .dispatch(
            "configuration.reload",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedStoredRevision": rev,
            }),
            Some("key-reload-3"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "succeeded");
    let db = ConfigDb::open(rt.store().home()).unwrap();
    let record = db
        .reload_operation(operation_id.as_str())
        .expect("operation record");
    assert_eq!(record.state, "succeeded");
    assert_eq!(
        record.result.as_ref().unwrap()["storedRevision"],
        rev.as_str()
    );
}

#[tokio::test]
async fn anchor_reload_failure_keeps_the_old_generation_and_reports_the_reason() {
    let (_tmp, rt, iid) = setup("cfg-reload-fail");
    let first = update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "a", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let generation = first["effectiveGeneration"].as_str().unwrap().to_owned();

    // Corrupt the stored layer the way an external editor would (a row the
    // write path would never produce): the apply gate must reject it.
    {
        let db = ConfigDb::open(rt.store().home()).unwrap();
        db.conn()
            .execute(
                "INSERT INTO v1_config_layer_rows (layer, cfg_key, value_json, write_revision, updated_at)
                 VALUES ('instance', '', '1', 'cfg-1', '2026-11-18T00:00:00Z')",
                [],
            )
            .unwrap();
    }

    // A later update stores fine but its apply fails: new stored revision,
    // OLD effective generation, reloadState failed.
    let second = update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-1",
        json!([{"key": "b", "action": "set", "value": 2}]),
    )
    .await
    .unwrap();
    assert_eq!(second["reloadState"], "failed");
    assert_eq!(second["effectiveGeneration"], generation.as_str());
    let stored = second["storedRevision"].as_str().unwrap().to_owned();

    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(resolved["reloadState"], "failed");
    assert_eq!(resolved["effectiveGeneration"], generation.as_str());
    assert_eq!(resolved["storedRevision"], stored.as_str());
    assert_eq!(
        value_of(&resolved, "b"),
        Some(&json!(2)),
        "stored, not effective"
    );

    // The explicit reload reports the failure and keeps the old generation.
    let (outcome, receipt) = rt
        .dispatch(
            "configuration.reload",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedStoredRevision": stored,
            }),
            Some("key-reload-fail-1"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Accepted);
    assert_eq!(receipt["state"], "failed");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let db = ConfigDb::open(rt.store().home()).unwrap();
    let record = db
        .reload_operation(operation_id.as_str())
        .expect("recorded");
    assert_eq!(record.state, "failed");
    let error = record.error.as_ref().expect("failure reason recorded");
    assert!(error["code"].as_str().is_some_and(|c| !c.is_empty()));
    assert!(error["message"].as_str().is_some_and(|m| !m.is_empty()));
    // "报告目标revision": the failed apply records what it tried to apply.
    let progress: Vec<Value> = record.progress.clone();
    assert!(
        progress
            .iter()
            .any(|p| p["key"] == "targetStoredRevision" && p["value"] == stored.as_str())
    );
    assert!(
        progress
            .iter()
            .any(|p| p["key"] == "effectiveGeneration" && p["value"] == generation.as_str())
    );

    // Repairing the stored state and reloading recovers to `current`.
    {
        let db = ConfigDb::open(rt.store().home()).unwrap();
        db.conn()
            .execute(
                "DELETE FROM v1_config_layer_rows WHERE layer = 'instance' AND cfg_key = ''",
                [],
            )
            .unwrap();
    }
    let (outcome, receipt) = rt
        .dispatch(
            "configuration.reload",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedStoredRevision": stored,
            }),
            Some("key-reload-fail-2"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Accepted);
    assert_eq!(receipt["state"], "succeeded");
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(resolved["reloadState"], "current");
    assert_ne!(
        resolved["effectiveGeneration"],
        generation.as_str(),
        "a successful apply advances the effective generation"
    );
}

// ---------------------------------------------------------------------------
// Registered per-key validators (the hook the model/extension/policy
// domains reuse — update preconditions route each key through its owner)
// ---------------------------------------------------------------------------

fn test_validator(key: &str, value: &Value) -> Vec<super::VIssue> {
    let mut issues = Vec::new();
    if key == "test.cfg-agent.restricted" && value != &json!("allowed") {
        issues.push(super::VIssue {
            code: "RESTRICTED_VALUE".to_owned(),
            field: Some(key.to_owned()),
            message: "restricted key only accepts \"allowed\"".to_owned(),
            blocking: true,
        });
    }
    if key == "test.cfg-agent.soft" {
        issues.push(super::VIssue {
            code: "SOFT_WARNING".to_owned(),
            field: Some(key.to_owned()),
            message: "soft warning".to_owned(),
            blocking: false,
        });
    }
    issues
}

#[tokio::test]
async fn registered_key_validators_block_and_warn() {
    super::register_key_validator(test_validator);
    let (_tmp, rt, iid) = setup("cfg-validators");
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "base", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let rev = resolve(&rt, &iid, instance_scope()).await["storedRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    // A blocking validator issue rejects the change list wholesale.
    let error = update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([{"key": "test.cfg-agent.restricted", "action": "set", "value": "nope"}]),
    )
    .await
    .unwrap_err();
    assert_eq!(contract_code(&error), "INVALID_ARGUMENT");
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(resolved["storedRevision"], rev);
    assert!(value_of(&resolved, "test.cfg-agent.restricted").is_none());

    // The accepted value stores and applies.
    let ok = update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([{"key": "test.cfg-agent.restricted", "action": "set", "value": "allowed"}]),
    )
    .await
    .unwrap();
    assert_eq!(ok["reloadState"], "current");

    // Non-blocking issues ride along in the result's issues list.
    let rev = ok["storedRevision"].as_str().unwrap().to_owned();
    let ok = update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([{"key": "test.cfg-agent.soft", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    assert!(
        ok["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "SOFT_WARNING")
    );
    assert_eq!(ok["reloadState"], "current");
}

// ---------------------------------------------------------------------------
// Scope existence and event emission
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_scopes_are_rejected_with_invalid_argument() {
    let (_tmp, rt, iid) = setup("cfg-scope");
    for scope in [
        json!({"kind": "project", "projectId": "missing"}),
        json!({"kind": "session", "projectId": "missing", "sessionId": "nope"}),
    ] {
        let error = rt
            .dispatch(
                "configuration.resolve",
                json!({"instanceId": iid, "scope": scope}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "INVALID_ARGUMENT");
    }
}

#[tokio::test]
async fn session_scope_must_match_the_session_project() {
    let (_tmp, rt, iid) = setup("cfg-scope-mismatch");
    seed_project_session(&rt, &iid).await;
    // The seeded session matches its project.
    let (outcome, _) = rt
        .dispatch(
            "configuration.resolve",
            json!({"instanceId": iid, "scope": session_scope()}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ResponseOutcome::Succeeded);
    // The same session id under another project id is rejected.
    let error = rt
        .dispatch(
            "configuration.resolve",
            json!({"instanceId": iid, "scope": {
                "kind": "session", "projectId": "other", "sessionId": "sess-cfg"
            }}),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(contract_code(&error), "INVALID_ARGUMENT");
}

#[tokio::test]
async fn update_emits_one_configuration_changed_event_without_values() {
    let (_tmp, rt, iid) = setup("cfg-event");
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([
            {"key": "a", "action": "set", "value": 1},
            {"key": "provider.apiKey", "action": "set", "value": "sk-live-x"},
        ]),
    )
    .await
    .unwrap();

    let events: Vec<Value> = {
        let db = ConfigDb::open(rt.store().home()).unwrap();
        let conn = db.conn();
        let mut stmt = conn
            .prepare("SELECT payload_json FROM events WHERE event_type = 'configuration.changed'")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.iter()
            .map(|r| serde_json::from_str(r).unwrap_or(Value::Null))
            .collect()
    };
    assert_eq!(events.len(), 1, "exactly one configuration.changed event");
    let payload = &events[0];
    assert_eq!(payload["scope"], instance_scope());
    assert_eq!(payload["changedKeys"], json!(["a", "provider.apiKey"]));
    assert!(payload["storedRevision"].as_str().is_some());
    assert!(payload["effectiveGeneration"].as_str().is_some());
    // The payload carries revisions and key names only — never values, so
    // no secret can leak through the event stream.
    let text = serde_json::to_string(payload).unwrap();
    assert!(
        !text.contains("sk-live-x"),
        "event payload leaked a secret: {text}"
    );
}

#[tokio::test]
async fn nested_secrets_are_masked_everywhere_and_redacted_roundtrip_preserves() {
    let (_tmp, rt, iid) = setup("cfg-nested-secret");
    let first = update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([
            {
                "key": "providers.custom",
                "action": "set",
                "value": {
                    "endpoint": "https://api.example.com/v1",
                    "apiKey": "sk-nested-live-1",
                    "nested": [{"token": "tok-nested-array"}]
                }
            },
            {
                "key": "adapters.feishu",
                "action": "set",
                "value": {"appSecret": "feishu-secret-99"}
            }
        ]),
    )
    .await
    .unwrap();
    let rev = first["storedRevision"].as_str().unwrap().to_owned();

    // Neither resolve nor read_layer may expose any nested secret literal.
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    let resolved_text = serde_json::to_string(&resolved).unwrap();
    assert!(!resolved_text.contains("sk-nested-live-1"));
    assert!(!resolved_text.contains("tok-nested-array"));
    assert!(!resolved_text.contains("feishu-secret-99"));

    let p_entry = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "providers.custom")
        .unwrap();
    assert_eq!(p_entry["sensitive"], true);
    assert_eq!(p_entry["value"]["apiKey"], super::REDACTED);
    assert_eq!(p_entry["value"]["endpoint"], "https://api.example.com/v1");
    assert_eq!(p_entry["value"]["nested"][0]["token"], super::REDACTED);

    let (_, layer) = rt
        .dispatch(
            "configuration.read_layer",
            json!({"instanceId": iid, "scope": instance_scope()}),
            None,
            None,
        )
        .await
        .unwrap();
    let layer_text = serde_json::to_string(&layer).unwrap();
    assert!(!layer_text.contains("sk-nested-live-1"));
    assert!(!layer_text.contains("tok-nested-array"));
    assert!(!layer_text.contains("feishu-secret-99"));

    // Updating another field while passing "<redacted>" back preserves stored nested secrets.
    let second = update(
        &rt,
        &iid,
        instance_scope(),
        &rev,
        json!([
            {
                "key": "providers.custom",
                "action": "set",
                "value": {
                    "endpoint": "https://api2.example.com/v1",
                    "apiKey": "<redacted>",
                    "nested": [{"token": "<redacted>"}]
                }
            }
        ]),
    )
    .await
    .unwrap();
    assert!(
        second["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "MASKED_SECRET_PRESERVED")
    );
    let db = ConfigDb::open(rt.store().home()).unwrap();
    let stored = db
        .read_layer("instance")
        .unwrap()
        .get("providers.custom")
        .cloned()
        .unwrap();
    assert_eq!(stored.value["endpoint"], "https://api2.example.com/v1");
    assert_eq!(stored.value["apiKey"], "sk-nested-live-1");
    assert_eq!(stored.value["nested"][0]["token"], "tok-nested-array");

    // Passing "<redacted>" for a nested field without an existing stored value rejects.
    let rev2 = second["storedRevision"].as_str().unwrap().to_owned();
    let err = update(
        &rt,
        &iid,
        instance_scope(),
        &rev2,
        json!([
            {
                "key": "adapters.slack",
                "action": "set",
                "value": {"botToken": "<redacted>"}
            }
        ]),
    )
    .await
    .unwrap_err();
    assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
}

#[tokio::test]
async fn validate_rejected_digest_masks_secrets_and_does_not_hold_write_lock() {
    let (_tmp, rt, iid) = setup("cfg-validate-digest-mask");
    let (_, res_a) = rt
        .dispatch(
            "configuration.validate",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedRevision": "cfg-0",
                "changes": [
                    {"key": "", "action": "set", "value": "bad"},
                    {"key": "provider.apiKey", "action": "set", "value": "sk-secret-alpha"},
                    {"key": "providers.custom", "action": "set", "value": {"apiKey": "sk-nested-alpha"}}
                ]
            }),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(res_a["valid"], false);

    let (_, res_b) = rt
        .dispatch(
            "configuration.validate",
            json!({
                "instanceId": iid,
                "scope": instance_scope(),
                "expectedRevision": "cfg-0",
                "changes": [
                    {"key": "", "action": "set", "value": "bad"},
                    {"key": "provider.apiKey", "action": "set", "value": "sk-secret-beta"},
                    {"key": "providers.custom", "action": "set", "value": {"apiKey": "sk-nested-beta"}}
                ]
            }),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(res_b["valid"], false);
    assert_eq!(
        res_a["candidateDigest"], res_b["candidateDigest"],
        "rejected validation candidateDigest must redact secrets before hashing"
    );
}

#[tokio::test]
async fn snapshot_run_config_in_tx_tracks_applied_generation_and_skips_failed_apply() {
    let (_tmp, rt, iid) = setup("cfg-snapshot-fn");
    seed_project_session(&rt, &iid).await;
    {
        let conn = rt.store().conn.lock().unwrap();
        let (rev, applied_gen) =
            super::snapshot_run_config_in_tx(&conn, "proj-cfg", "sess-cfg", "init-rev", "gen-init")
                .unwrap();
        assert_eq!(rev, "init-rev");
        assert_eq!(applied_gen, "gen-init");
    }

    let ok = update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "a", "action": "set", "value": 1}]),
    )
    .await
    .unwrap();
    let rev1 = ok["storedRevision"].as_str().unwrap().to_owned();
    let gen1 = ok["effectiveGeneration"].as_str().unwrap().to_owned();
    {
        let conn = rt.store().conn.lock().unwrap();
        let (rev, applied_gen) =
            super::snapshot_run_config_in_tx(&conn, "proj-cfg", "sess-cfg", "init-rev", "gen-init")
                .unwrap();
        assert_eq!(rev, rev1);
        assert_eq!(applied_gen, gen1);
    }

    // Corrupt a stored row so next apply fails: snapshot must remain at applied rev1/gen1.
    {
        let conn = rt.store().conn.lock().unwrap();
        conn.execute(
            "INSERT INTO v1_config_layer_rows (layer, cfg_key, value_json, write_revision, updated_at)
             VALUES ('instance', '', '1', 'cfg-1', '2026-11-18T00:00:00Z')",
            [],
        )
        .unwrap();
    }
    let failed = update(
        &rt,
        &iid,
        instance_scope(),
        &rev1,
        json!([{"key": "b", "action": "set", "value": 2}]),
    )
    .await
    .unwrap();
    assert_eq!(failed["reloadState"], "failed");
    {
        let conn = rt.store().conn.lock().unwrap();
        let (rev, applied_gen) =
            super::snapshot_run_config_in_tx(&conn, "proj-cfg", "sess-cfg", "init-rev", "gen-init")
                .unwrap();
        assert_eq!(rev, rev1);
        assert_eq!(applied_gen, gen1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_permissions_0700_0600_enforced_after_writes() {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, rt, iid) = setup("cfg-perms");
    update(
        &rt,
        &iid,
        instance_scope(),
        "cfg-0",
        json!([{"key": "provider.apiKey", "action": "set", "value": "sk-perm"}]),
    )
    .await
    .unwrap();
    let v1_dir = rt.store().home().join("v1");
    let dir_mode = std::fs::metadata(&v1_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
    for suffix in ["", "-wal", "-shm"] {
        let mut os = v1_dir.join("state.sqlite").as_os_str().to_os_string();
        os.push(suffix);
        let p = std::path::PathBuf::from(os);
        if p.exists() {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "unexpected mode for {}", p.display());
        }
    }
}

#[tokio::test]
async fn domain_key_validators_block_policy_bypass_and_invalid_domain_keys() {
    let (_tmp, rt, iid) = setup("cfg-domain-validators");
    for bad in [
        json!([{"key": "policy.default", "action": "set", "value": {"mode": "allow"}}]),
        json!([{"key": "permissions.tools", "action": "set", "value": []}]),
        json!([{"key": "defaultModel", "action": "set", "value": "missing-provider/missing-model"}]),
        json!([{"key": "providers.bad", "action": "set", "value": {"endpoint": "ftp://example.com"}}]),
        json!([{"key": "context.thinking", "action": "set", "value": "not-a-bool"}]),
    ] {
        let err = update(&rt, &iid, instance_scope(), "cfg-0", bad)
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
    }
    let resolved = resolve(&rt, &iid, instance_scope()).await;
    assert_eq!(resolved["storedRevision"], "cfg-0");
}
