//! `cli.task.t016` end-to-end acceptance (models / providers / credentials /
//! usage, `007 §10`).
//!
//! Every scenario drives the real `agentcpm-pd` binary against an isolated
//! temporary `AGENTCPM_PD_HOME` with the embedded gateway answering over the
//! real `/v1/ws` transport. The scenarios script the task's related cases:
//!
//! * `cli.case.c025` (模型选择): provider + model registration, session-level
//!   `model_selection.set` with `expectedRevision` CAS, `get` explaining the
//!   override/fallback, and `null` clearing.
//! * `cli.case.c026` (凭据保护): the secret enters only through
//!   `--input-json` (argv secrets are refused), never appears in stdout,
//!   stderr, lists, or errors; referenced credentials refuse removal with
//!   `RESOURCE_IN_USE` until the provider reference is cleared.
//! * operation acceptance anchors: unsupported protocols
//!   (`PROTOCOL_UNSUPPORTED`), stale revisions (`REVISION_CONFLICT`),
//!   `envelope.target` vs `params.instanceId` mismatch
//!   (`INSTANCE_MISMATCH`) with zero side effects, and schema-valid usage
//!   snapshot/summary results.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use serde_json::{Value, json};

use common_v1::*;

/// Parses a failure envelope: exactly one stdout line, `outcome=failed`,
/// and the contract error payload returned.
fn expect_failed(method: &str, out: &CliOutcome) -> Value {
    assert_ne!(
        out.code, 0,
        "`{method}` unexpectedly succeeded: {}",
        out.stdout
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
    assert_eq!(
        value["outcome"], "failed",
        "`{method}` outcome: {value}; stderr:\n{}",
        out.stderr
    );
    let error = value["error"].clone();
    assert!(
        error["code"].as_str().is_some_and(|c| !c.is_empty()),
        "`{method}` envelope has no error code: {value}"
    );
    error
}

fn assert_output_never_contains(out: &CliOutcome, secret: &str, what: &str) {
    assert!(
        !out.stdout.contains(secret),
        "{what}: the secret leaked into stdout:\n{}",
        out.stdout
    );
    assert!(
        !out.stderr.contains(secret),
        "{what}: the secret leaked into stderr:\n{}",
        out.stderr
    );
}

/// The registration + catalog + selection lifecycle (`cli.case.c025`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_model_catalog_and_selection_lifecycle() {
    let home = TestHome::new("t016-e2e-catalog");
    let root = home.path();
    init_home(root);

    // Provider registration. `保存不自动出网`: the write is pure storage.
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-openai",
            "--name",
            "OpenAI compatible",
            "--protocol",
            "openai",
            "--endpoint",
            "https://api.openai.com/v1/",
        ],
    );
    let created = expect_ok("provider.write", &out);
    let provider = &created["provider"];
    assert_eq!(provider["providerId"], "prov-openai");
    assert_eq!(provider["endpoint"], "https://api.openai.com/v1");
    assert_eq!(provider["credentialRef"], Value::Null);
    let prov_rev1 = provider["revision"].as_str().unwrap().to_owned();

    // Unsupported protocol is rejected (`PROTOCOL_UNSUPPORTED`).
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-bad",
            "--name",
            "bad",
            "--protocol",
            "carrier-pigeon",
            "--endpoint",
            "https://api.example.com",
        ],
    );
    let error = expect_failed("provider.write", &out);
    assert_eq!(error["code"], "PROTOCOL_UNSUPPORTED");
    assert_eq!(error["effect"], "not_applied");

    // Stale revision on the existing provider (`REVISION_CONFLICT`).
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-openai",
            "--expected-revision",
            "rev-00000001",
            "--name",
            "renamed",
            "--protocol",
            "openai",
            "--endpoint",
            "https://api.openai.com/v1",
        ],
    );
    let error = expect_failed("provider.write", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");

    // Model registration under the provider.
    let out = await_cli(
        root,
        &[
            "models",
            "write",
            "--output",
            "json",
            "--model",
            "{\"providerId\":\"prov-openai\",\"modelId\":\"gpt-test\"}",
            "--name",
            "GPT Test",
            "--declared-capabilities",
            "[{\"key\":\"text\",\"value\":true}]",
        ],
    );
    let created_model = expect_ok("model.write", &out);
    assert_eq!(created_model["model"]["name"], "GPT Test");
    assert_eq!(created_model["model"]["capabilities"][0]["key"], "text");
    assert_eq!(created_model["model"]["capabilityEvidence"], json!([]));
    let model_rev1 = created_model["model"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // model.get / model.list round trips.
    let out = await_cli(
        root,
        &[
            "models",
            "get",
            "--output",
            "json",
            "--model",
            "{\"providerId\":\"prov-openai\",\"modelId\":\"gpt-test\"}",
        ],
    );
    let got = expect_ok("model.get", &out);
    assert_eq!(got["model"]["revision"], model_rev1.as_str());
    let out = await_cli(root, &["models", "list", "--output", "json"]);
    let listed = expect_ok("model.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));

    // Unknown provider for a model write (`PROVIDER_NOT_FOUND`).
    let out = await_cli(
        root,
        &[
            "models",
            "write",
            "--output",
            "json",
            "--model",
            "{\"providerId\":\"prov-missing\",\"modelId\":\"m\"}",
            "--name",
            "orphan",
        ],
    );
    let error = expect_failed("model.write", &out);
    assert_eq!(error["code"], "PROVIDER_NOT_FOUND");

    // Session-level selection (c025): set, get (explains the override),
    // then clear with an explicit null model.
    let (project_id, session_id) = seed_project_session(root, "t016").await;
    let scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
    );

    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "get",
            "--output",
            "json",
            "--scope",
            &scope,
        ],
    );
    let unset = expect_ok("model_selection.get", &out);
    assert_eq!(unset["effectiveModel"], Value::Null);
    assert_eq!(unset["overrideModel"], Value::Null);
    let base_rev = unset["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "set",
            "--output",
            "json",
            "--scope",
            &scope,
            "--expected-revision",
            &base_rev,
            "--model",
            "{\"providerId\":\"prov-openai\",\"modelId\":\"gpt-test\"}",
        ],
    );
    let set = expect_ok("model_selection.set", &out);
    assert_eq!(set["effectiveModel"]["modelId"], "gpt-test");
    assert_eq!(set["overrideModel"]["modelId"], "gpt-test");

    // The provider revision changed since the model was written? No — the
    // selection does not care; but a stale selection CAS must fail after
    // the selection itself moved on.
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "set",
            "--output",
            "json",
            "--scope",
            &scope,
            "--expected-revision",
            &base_rev,
            "--model",
            "{\"providerId\":\"prov-openai\",\"modelId\":\"gpt-test\"}",
        ],
    );
    let error = expect_failed("model_selection.set", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");

    // Invalid model selection (`MODEL_UNAVAILABLE`).
    let current_rev = set["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "set",
            "--output",
            "json",
            "--scope",
            &scope,
            "--expected-revision",
            &current_rev,
            "--model",
            "{\"providerId\":\"prov-openai\",\"modelId\":\"nope\"}",
        ],
    );
    let error = expect_failed("model_selection.set", &out);
    assert_eq!(error["code"], "MODEL_UNAVAILABLE");

    // Clear with an explicit null: the override disappears.
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "set",
            "--output",
            "json",
            "--scope",
            &scope,
            "--expected-revision",
            &current_rev,
            "--model",
            "null",
        ],
    );
    let cleared = expect_ok("model_selection.set", &out);
    assert_eq!(cleared["overrideModel"], Value::Null);
    assert_eq!(cleared["effectiveModel"], Value::Null);

    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "get",
            "--output",
            "json",
            "--scope",
            &scope,
        ],
    );
    let after = expect_ok("model_selection.get", &out);
    assert_eq!(after["overrideModel"], Value::Null);
    assert_eq!(after["effectiveModel"], Value::Null);

    // The provider's own revision only moved once (the successful rename
    // attempts above were all rejected).
    let out = await_cli(
        root,
        &[
            "providers",
            "get",
            "--output",
            "json",
            "--provider-id",
            "prov-openai",
        ],
    );
    let got = expect_ok("provider.get", &out);
    assert_eq!(got["provider"]["revision"], prov_rev1.as_str());
}

/// The credential protection lifecycle (`cli.case.c026`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_secret_stays_out_of_every_output() {
    let home = TestHome::new("t016-e2e-cred");
    let root = home.path();
    init_home(root);

    let secret = "sk-live-t016-e2e-probe-42";

    // argv-borne secrets are refused before any request leaves the CLI.
    let out = await_cli(
        root,
        &[
            "credentials",
            "write",
            "--output",
            "json",
            "--credential-id",
            "cred-ci",
            "--kind",
            "api_key",
            "--name",
            "CI",
            "--secret-file",
            secret,
        ],
    );
    assert_ne!(out.code, 0, "argv secret must be refused: {}", out.stdout);
    assert!(
        !out.stdout.contains(secret) && !out.stderr.contains(secret),
        "the refusal must echo the flag, never the value"
    );

    // The secret enters only through --input-json (here: stdin).
    let input = json!({
        "credentialId": "cred-ci",
        "kind": "api_key",
        "name": "CI key",
        "secret": secret,
    })
    .to_string();
    let out = await_cli_with_stdin(
        root,
        &[
            "credentials",
            "write",
            "--output",
            "json",
            "--input-json",
            "-",
        ],
        &input,
    );
    let created = expect_ok("credential.write", &out);
    let credential = &created["credential"];
    assert_eq!(credential["credentialId"], "cred-ci");
    assert_eq!(credential["kind"], "api_key");
    assert_eq!(credential["enabled"], true);
    assert!(
        credential["fingerprint"]
            .as_str()
            .is_some_and(|f| f.starts_with("sha256:")),
        "fingerprint must be hash-derived: {credential}"
    );
    assert!(!out.stdout.contains(secret));
    let cred_rev1 = credential["revision"].as_str().unwrap().to_owned();

    // The list carries metadata + fingerprint only.
    let out = await_cli(root, &["credentials", "list", "--output", "json"]);
    let listed = expect_ok("credential.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    assert_output_never_contains(&out, secret, "credential.list");

    // A provider referencing the credential blocks its removal
    // (`RESOURCE_IN_USE`), without the secret appearing anywhere.
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-ci",
            "--name",
            "CI provider",
            "--protocol",
            "anthropic",
            "--endpoint",
            "https://api.anthropic.com",
            "--credential-ref",
            "cred-ci",
        ],
    );
    let provider = expect_ok("provider.write", &out);
    assert_eq!(provider["provider"]["credentialRef"], "cred-ci");

    let out = await_cli(
        root,
        &[
            "credentials",
            "remove",
            "--output",
            "json",
            "--credential-id",
            "cred-ci",
            "--expected-revision",
            &cred_rev1,
        ],
    );
    let error = expect_failed("credential.remove", &out);
    assert_eq!(error["code"], "RESOURCE_IN_USE");
    assert_eq!(error["effect"], "not_applied");
    assert_output_never_contains(&out, secret, "blocked remove");

    // Conditional toggle: stale revision first, then the real flip.
    let out = await_cli(
        root,
        &[
            "credentials",
            "set-enabled",
            "--output",
            "json",
            "--credential-id",
            "cred-ci",
            "--expected-revision",
            "rev-00000001",
            "--enabled",
            "false",
        ],
    );
    let error = expect_failed("credential.set_enabled", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");
    assert_output_never_contains(&out, secret, "stale set-enabled");

    let out = await_cli(
        root,
        &[
            "credentials",
            "set-enabled",
            "--output",
            "json",
            "--credential-id",
            "cred-ci",
            "--expected-revision",
            &cred_rev1,
            "--enabled",
            "false",
        ],
    );
    let disabled = expect_ok("credential.set_enabled", &out);
    assert_eq!(disabled["credential"]["enabled"], false);
    let cred_rev2 = disabled["credential"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // Clearing the provider's reference (explicit null through --input-json)
    // unblocks removal; the removal succeeds and the id stops resolving.
    let provider_rev = provider["provider"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let input = json!({
        "providerId": "prov-ci",
        "expectedRevision": provider_rev,
        "name": "CI provider",
        "protocol": "anthropic",
        "endpoint": "https://api.anthropic.com",
        "credentialRef": null,
    })
    .to_string();
    let out = await_cli_with_stdin(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--input-json",
            "-",
        ],
        &input,
    );
    let detached = expect_ok("provider.write", &out);
    assert_eq!(detached["provider"]["credentialRef"], Value::Null);

    let out = await_cli(
        root,
        &[
            "credentials",
            "remove",
            "--output",
            "json",
            "--credential-id",
            "cred-ci",
            "--expected-revision",
            &cred_rev2,
        ],
    );
    let removed = expect_ok("credential.remove", &out);
    assert_eq!(removed["removedCredentialId"], "cred-ci");

    let out = await_cli(root, &["credentials", "list", "--output", "json"]);
    let listed = expect_ok("credential.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
}

/// `envelope.target` vs `params.instanceId` mismatch answers
/// `INSTANCE_MISMATCH` with zero side effects (representative write op).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_mismatch_is_refused_before_side_effects() {
    let home = TestHome::new("t016-e2e-mismatch");
    let root = home.path();
    init_home(root);

    let mut conn = ConnectSession::start(root).await;
    let _init = conn.initialize().await;

    let mismatch = conn
        .call(
            "provider.write",
            json!({
                "instanceId": "local-00000000-0000-7000-8000-00000000dead",
                "providerId": "prov-mismatch",
                "name": "never created",
                "protocol": "openai",
                "endpoint": "https://api.example.com",
            }),
            None,
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

    conn.close_stdin().await;
    conn.wait_exit(0, "api connect exit").await;
    let out = await_cli(root, &["providers", "list", "--output", "json"]);
    let listed = expect_ok("provider.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
}

/// usage.* answer schema-valid (possibly empty) results once the domain is
/// registered — no `-32004`, no invented data (S3 has no usage writer yet;
/// the aggregation semantics are unit-tested against the ingestion seam).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_operations_answer_valid_results() {
    let home = TestHome::new("t016-e2e-usage");
    let root = home.path();
    init_home(root);

    let out = await_cli(
        root,
        &[
            "usage",
            "snapshot",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"instance\"}",
        ],
    );
    let snapshot = expect_ok("usage.snapshot", &out);
    assert_eq!(snapshot["items"].as_array().map(Vec::len), Some(0));
    assert_eq!(snapshot["complete"], true);
    assert_eq!(snapshot["pageInfo"]["complete"], true);
    assert!(
        snapshot["pageInfo"]["snapshotRevision"]
            .as_str()
            .is_some_and(|r| !r.is_empty())
    );

    let out = await_cli(
        root,
        &[
            "usage",
            "summary",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"instance\"}",
            "--group-by",
            "model",
        ],
    );
    let summary = expect_ok("usage.summary", &out);
    assert_eq!(summary["rows"].as_array().map(Vec::len), Some(0));
    assert_eq!(summary["unknownRecordCount"], 0);
    assert!(summary["asOf"].as_str().is_some_and(|t| !t.is_empty()));

    // A scope naming a nonexistent project is rejected with a structured
    // error, not a silent empty answer.
    let out = await_cli(
        root,
        &[
            "usage",
            "snapshot",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"project\",\"projectId\":\"nope\"}",
        ],
    );
    let error = expect_failed("usage.snapshot", &out);
    assert_eq!(error["code"], "INVALID_ARGUMENT");
}

/// A thin wrapper so the credential scenario can pipe stdin into the CLI
/// (the `--input-json -` path secrets must use).
async fn await_cli_with_stdin(home: &std::path::Path, args: &[&str], stdin: &str) -> CliOutcome {
    cli_with_stdin(home, args, Some(stdin)).await
}
