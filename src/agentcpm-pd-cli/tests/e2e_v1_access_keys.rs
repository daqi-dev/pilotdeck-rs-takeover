//! `cli.task.keys` end-to-end acceptance (`access_key.*`, `007 §10`).
//!
//! Every scenario drives the real `agentcpm-pd` binary against an isolated
//! temporary `AGENTCPM_PD_HOME` with the embedded gateway answering over the
//! real `/v1/ws` transport. The scenarios script the acceptance anchors from
//! the task's related cases (`cli.case.operation.access_key.*`):
//!
//! * the secret is output only by `access_key.create` and never appears in
//!   list results, duplicate-create errors, or any other output;
//! * a duplicate create with the same keyId generates no second key and
//!   does not re-expose the secret;
//! * disabled and revoked are separate, reported flags;
//! * conditional writes enforce `expectedRevision` (REVISION_CONFLICT);
//! * a revoked key can never be enabled (KEY_REVOKED) and repeated revoke
//!   has an explicit result;
//! * `envelope.target` vs `params.instanceId` mismatches answer
//!   INSTANCE_MISMATCH with zero side effects;
//! * keys are isolated across instances.

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

async fn create_key(home: &std::path::Path, key_id: &str, name: &str) -> (Value, String) {
    let out = await_cli(
        home,
        &[
            "access-keys",
            "create",
            "--output",
            "json",
            "--key-id",
            key_id,
            "--name",
            name,
        ],
    );
    let result = expect_ok("access_key.create", &out);
    let secret = result["secret"]
        .as_str()
        .expect("create returns secret")
        .to_owned();
    (result, secret)
}

async fn list_keys(home: &std::path::Path) -> Value {
    let out = await_cli(home, &["access-keys", "list", "--output", "json"]);
    expect_ok("access_key.list", &out)
}

/// Create → list round trip: the secret exists exactly once (the create
/// response) and the list carries only sanitized metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_then_list_outputs_the_secret_exactly_once() {
    let home = TestHome::new("ak-e2e-create");
    let root = home.path();
    init_home(root);

    let (created, secret) = create_key(root, "k-ci", "CI key").await;
    let key = &created["key"];
    assert_eq!(key["keyId"], "k-ci");
    assert_eq!(key["name"], "CI key");
    assert_eq!(key["enabled"], true);
    assert_eq!(key["revoked"], false);
    assert!(key["revision"].as_str().is_some_and(|r| !r.is_empty()));
    assert!(
        key["fingerprint"]
            .as_str()
            .is_some_and(|f| f.starts_with("sha256:")),
        "fingerprint must be hash-derived: {key}"
    );
    assert!(secret.starts_with("ck_") && secret.len() == 3 + 64);

    let listed = list_keys(root).await;
    let items = listed["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["keyId"], "k-ci");
    assert_ne!(items[0]["fingerprint"], secret);
    let list_text = serde_json::to_string(&listed).unwrap();
    assert!(
        !list_text.contains(&secret),
        "the list output must never contain the secret: {list_text}"
    );
    assert!(
        listed["pageInfo"]["snapshotRevision"]
            .as_str()
            .is_some_and(|r| !r.is_empty())
    );
    assert_eq!(listed["pageInfo"]["complete"], true);
}

/// The nonReplayable guard: re-creating the same keyId fails with
/// ALREADY_EXISTS, produces no second key, and never re-exposes the secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_create_rejects_and_keeps_the_secret_hidden() {
    let home = TestHome::new("ak-e2e-dup");
    let root = home.path();
    init_home(root);

    let (_, secret) = create_key(root, "k-dup", "first").await;
    let out = await_cli(
        root,
        &[
            "access-keys",
            "create",
            "--output",
            "json",
            "--key-id",
            "k-dup",
            "--name",
            "second",
        ],
    );
    let error = expect_failed("access_key.create", &out);
    assert_eq!(error["code"], "ALREADY_EXISTS");
    assert_eq!(error["effect"], "not_applied");
    assert_output_never_contains(&out, &secret, "duplicate create");

    let listed = list_keys(root).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
}

/// Blank names are refused like the TS baseline (INVALID_ARGUMENT).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blank_key_name_is_rejected() {
    let home = TestHome::new("ak-e2e-blank");
    let root = home.path();
    init_home(root);

    let out = await_cli(
        root,
        &[
            "access-keys",
            "create",
            "--output",
            "json",
            "--key-id",
            "k-blank",
            "--name",
            "   ",
        ],
    );
    let error = expect_failed("access_key.create", &out);
    assert_eq!(error["code"], "INVALID_ARGUMENT");
    assert_eq!(error["effect"], "not_applied");

    let listed = list_keys(root).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
}

/// Conditional toggling: expectedRevision guards the write, `disabled` and
/// `revoked` stay separate flags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_enabled_requires_the_current_revision() {
    let home = TestHome::new("ak-e2e-toggle");
    let root = home.path();
    init_home(root);

    let (created, secret) = create_key(root, "k-toggle", "toggle").await;
    let rev1 = created["key"]["revision"].as_str().unwrap().to_owned();

    // Stale revision first.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "set-enabled",
            "--output",
            "json",
            "--key-id",
            "k-toggle",
            "--expected-revision",
            "rev-00000000",
            "--enabled",
            "false",
        ],
    );
    let error = expect_failed("access_key.set_enabled", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");
    assert_eq!(error["effect"], "not_applied");
    assert_output_never_contains(&out, &secret, "stale set_enabled");

    // Disable with the current revision.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "set-enabled",
            "--output",
            "json",
            "--key-id",
            "k-toggle",
            "--expected-revision",
            &rev1,
            "--enabled",
            "false",
        ],
    );
    let disabled = expect_ok("access_key.set_enabled", &out);
    assert_eq!(disabled["key"]["enabled"], false);
    assert_eq!(disabled["key"]["revoked"], false);
    let rev2 = disabled["key"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(rev1, rev2);

    // The list reports the disabled key as enabled=false / revoked=false.
    let listed = list_keys(root).await;
    assert_eq!(listed["items"][0]["enabled"], false);
    assert_eq!(listed["items"][0]["revoked"], false);

    // Same-value write is a clean no-op.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "set-enabled",
            "--output",
            "json",
            "--key-id",
            "k-toggle",
            "--expected-revision",
            &rev2,
            "--enabled",
            "false",
        ],
    );
    let noop = expect_ok("access_key.set_enabled", &out);
    assert_eq!(noop["key"]["revision"], rev2.as_str());
}

/// Revocation is permanent: the key stays listed with `revoked=true`,
/// re-revoke with the observed state is an explicit success, a revoked key
/// can never be enabled, and the old revision stops working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_is_permanent_and_repeat_revoke_is_explicit() {
    let home = TestHome::new("ak-e2e-revoke");
    let root = home.path();
    init_home(root);

    let (created, secret) = create_key(root, "k-rev", "doomed").await;
    let rev1 = created["key"]["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "access-keys",
            "revoke",
            "--output",
            "json",
            "--key-id",
            "k-rev",
            "--expected-revision",
            &rev1,
        ],
    );
    let revoked = expect_ok("access_key.revoke", &out);
    assert_eq!(revoked["key"]["revoked"], true);
    assert_eq!(
        revoked["key"]["enabled"], true,
        "revoke leaves enabled untouched"
    );
    let rev2 = revoked["key"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(rev1, rev2);

    // Re-revoke with the observed state: explicit success, unchanged.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "revoke",
            "--output",
            "json",
            "--key-id",
            "k-rev",
            "--expected-revision",
            &rev2,
        ],
    );
    let again = expect_ok("access_key.revoke", &out);
    assert_eq!(again["key"]["revoked"], true);
    assert_eq!(again["key"]["revision"], rev2.as_str());

    // Re-revoke with the stale revision is a conflict.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "revoke",
            "--output",
            "json",
            "--key-id",
            "k-rev",
            "--expected-revision",
            &rev1,
        ],
    );
    let error = expect_failed("access_key.revoke", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");
    assert_output_never_contains(&out, &secret, "stale re-revoke");

    // The revoked key can never be enabled, even with the current revision.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "set-enabled",
            "--output",
            "json",
            "--key-id",
            "k-rev",
            "--expected-revision",
            &rev2,
            "--enabled",
            "true",
        ],
    );
    let error = expect_failed("access_key.set_enabled", &out);
    assert_eq!(error["code"], "KEY_REVOKED");
    assert_eq!(error["effect"], "not_applied");
    assert_output_never_contains(&out, &secret, "set_enabled on revoked");

    // Still listed, with revoked=true.
    let listed = list_keys(root).await;
    assert_eq!(listed["items"][0]["revoked"], true);

    // Unknown key.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "revoke",
            "--output",
            "json",
            "--key-id",
            "k-missing",
            "--expected-revision",
            &rev2,
        ],
    );
    let error = expect_failed("access_key.revoke", &out);
    assert_eq!(error["code"], "NOT_FOUND");
}

/// Offset-cursor pagination through the real CLI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_paginates_with_offset_cursors() {
    let home = TestHome::new("ak-e2e-page");
    let root = home.path();
    init_home(root);

    for n in 0..3 {
        create_key(root, &format!("k-{n}"), &format!("key {n}")).await;
    }

    let out = await_cli(
        root,
        &[
            "access-keys",
            "list",
            "--output",
            "json",
            "--page",
            "{\"limit\":2}",
        ],
    );
    let first = expect_ok("access_key.list", &out);
    assert_eq!(first["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(first["pageInfo"]["complete"], false);
    let cursor = first["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "access-keys",
            "list",
            "--output",
            "json",
            "--page",
            &format!("{{\"limit\":2,\"cursor\":\"{cursor}\"}}"),
        ],
    );
    let second = expect_ok("access_key.list", &out);
    assert_eq!(second["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(second["pageInfo"]["complete"], true);
    assert_eq!(second["pageInfo"]["nextCursor"], Value::Null);

    // A malformed cursor is rejected.
    let out = await_cli(
        root,
        &[
            "access-keys",
            "list",
            "--output",
            "json",
            "--page",
            "{\"cursor\":\"not-a-number\"}",
        ],
    );
    let error = expect_failed("access_key.list", &out);
    assert_eq!(error["code"], "INVALID_ARGUMENT");
}

/// envelope.target vs params.instanceId mismatch answers INSTANCE_MISMATCH
/// with zero side effects on both ends (T4-style probe over `api connect`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_mismatch_is_refused_before_side_effects() {
    let home = TestHome::new("ak-e2e-mismatch");
    let root = home.path();
    init_home(root);

    let mut conn = ConnectSession::start(root).await;
    let _init = conn.initialize().await;

    let mismatch = conn
        .call(
            "access_key.create",
            json!({
                "instanceId": "local-00000000-0000-7000-8000-00000000dead",
                "keyId": "k-mismatch",
                "name": "never created"
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

    // Zero side effects: the instance list stays empty.
    conn.close_stdin().await;
    conn.wait_exit(0, "api connect exit").await;
    let listed = list_keys(root).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
    assert_eq!(listed["pageInfo"]["complete"], true);
}

/// Keys are isolated per instance: instance B neither lists nor mutates
/// instance A's keys, and a secret created in A carries no meaning in B.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_are_isolated_across_instances() {
    let home_a = TestHome::new("ak-e2e-iso-a");
    let root_a = home_a.path();
    init_home(root_a);
    let home_b = TestHome::new("ak-e2e-iso-b");
    let root_b = home_b.path();
    init_home(root_b);

    let (created_a, secret_a) = create_key(root_a, "k-a", "instance A").await;
    let rev_a = created_a["key"]["revision"].as_str().unwrap().to_owned();
    let (_, secret_b) = create_key(root_b, "k-b", "instance B").await;
    assert_ne!(secret_a, secret_b);

    // B's list shows only B's key.
    let listed_b = list_keys(root_b).await;
    let ids: Vec<&str> = listed_b["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["keyId"].as_str())
        .collect();
    assert_eq!(ids, vec!["k-b"]);
    assert!(
        !serde_json::to_string(&listed_b)
            .unwrap()
            .contains(&secret_a),
        "instance B's list must not carry instance A's secret"
    );

    // A's key cannot be revoked or disabled through instance B.
    let out = await_cli(
        root_b,
        &[
            "access-keys",
            "revoke",
            "--output",
            "json",
            "--key-id",
            "k-a",
            "--expected-revision",
            &rev_a,
        ],
    );
    let error = expect_failed("access_key.revoke", &out);
    assert_eq!(error["code"], "NOT_FOUND");

    let out = await_cli(
        root_b,
        &[
            "access-keys",
            "set-enabled",
            "--output",
            "json",
            "--key-id",
            "k-a",
            "--expected-revision",
            &rev_a,
            "--enabled",
            "false",
        ],
    );
    let error = expect_failed("access_key.set_enabled", &out);
    assert_eq!(error["code"], "NOT_FOUND");

    // A's key is untouched in A.
    let listed_a = list_keys(root_a).await;
    assert_eq!(listed_a["items"][0]["keyId"], "k-a");
    assert_eq!(listed_a["items"][0]["revoked"], false);
    assert_eq!(listed_a["items"][0]["enabled"], true);
}
