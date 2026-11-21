//! End-to-end scenarios for the `t005` domain: `policy.*` (Phase A, gateway
//! side) — later extended by the `target.*` / `connection.*` /
//! `instance.inspect_initialization` CLI-local scenarios once the
//! `local_ops` substrate lands (Phase B).
//!
//! 驱动真实二进制:temp `AGENTCPM_PD_HOME` + 真 `agentcpm-pd` 子进程经
//! `/v1/ws` 自动启动本机 gateway,argv 输入、`--output json` 信封与嵌入
//! schema 校验全部走产品路径(合同正本核对)。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use common_v1::{CliOutcome, TestHome, await_cli, expect_ok, init_home, init_home_instance};
use serde_json::{Value, json};

/// Parses a failed (non-zero exit) CLI envelope and returns its contract
/// error code.
fn failure_code(method: &str, out: &CliOutcome) -> String {
    assert_ne!(
        out.code, 0,
        "`{method}` was expected to fail, got: {}",
        out.stdout
    );
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` must print one envelope line");
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}"));
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn policy_get(home: &std::path::Path, scope: &Value) -> CliOutcome {
    await_cli(
        home,
        &[
            "policy",
            "get",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    )
}

fn policy_update(
    home: &std::path::Path,
    scope: &Value,
    expected_revision: &str,
    mode: &str,
    rules: &Value,
) -> CliOutcome {
    await_cli(
        home,
        &[
            "policy",
            "update",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--expected-revision",
            expected_revision,
            "--mode",
            mode,
            "--rules",
            &rules.to_string(),
        ],
    )
}

/// c001 (初始化与自动启动) on the policy domain: a fresh isolated home,
/// `policy get` auto-starts the local gateway through the real CLI and the
/// contract default policy comes back.
#[tokio::test]
async fn policy_get_on_fresh_home_auto_starts_gateway_and_returns_defaults() {
    let home = TestHome::new("t005-policy-default");
    init_home(home.path());

    for scope in [
        json!({"kind": "instance"}),
        json!({"kind": "project", "projectId": "proj-x"}),
        json!({"kind": "session", "projectId": "proj-x", "sessionId": "sess-x"}),
    ] {
        let out = policy_get(home.path(), &scope);
        let result = expect_ok("policy.get", &out);
        assert_eq!(result["policy"]["scope"], scope, "scope echoes back");
        assert_eq!(result["policy"]["mode"], "ask", "undefined chain is ask");
        assert_eq!(result["policy"]["rules"], json!([]));
        assert_eq!(result["inheritedFrom"], json!([]));
        assert!(
            result["policy"]["revision"]
                .as_str()
                .is_some_and(|r| !r.is_empty()),
            "default revision is present for the first CAS"
        );
    }
    // The gateway really started (fresh home auto-start through the client).
    assert!(home.gateway_pid().is_some());
}

/// The full policy roundtrip over the real CLI: default get -> conditional
/// update (CAS against the default revision) -> get reflects -> stale CAS
/// rejected -> invalid rules rejected as a whole.
#[tokio::test]
async fn policy_update_roundtrip_with_cas_and_rejection_paths() {
    let home = TestHome::new("t005-policy-roundtrip");
    init_home(home.path());
    let instance_scope = json!({"kind": "instance"});

    // 1. Read the default (its revision is the first update's CAS value).
    let default = expect_ok("policy.get", &policy_get(home.path(), &instance_scope));
    let default_rev = default["policy"]["revision"].as_str().unwrap().to_owned();

    // 2. Update the instance-scope policy.
    let rules = json!([
        {"ruleId": "allow-npm", "effect": "allow", "tool": "bash", "resourcePattern": "npm *"},
        {"ruleId": "deny-web", "effect": "deny", "tool": "web_fetch", "resourcePattern": "*.internal"},
    ]);
    let updated = expect_ok(
        "policy.update",
        &policy_update(home.path(), &instance_scope, &default_rev, "deny", &rules),
    );
    let updated_rev = updated["policy"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(updated_rev, default_rev);

    // 3. get reflects the stored policy.
    let got = expect_ok("policy.get", &policy_get(home.path(), &instance_scope));
    assert_eq!(got["policy"]["mode"], "deny");
    assert_eq!(got["policy"]["rules"], rules);
    assert_eq!(got["policy"]["revision"], updated_rev);
    assert_eq!(got["inheritedFrom"], json!([]));

    // 4. Stale CAS (the pre-write default revision) is rejected with no
    //    change applied.
    let stale = policy_update(
        home.path(),
        &instance_scope,
        &default_rev,
        "ask",
        &json!([]),
    );
    assert_eq!(failure_code("policy.update", &stale), "REVISION_CONFLICT");

    // 5. Duplicate ruleIds are rejected as a whole (schema passes, the
    //    semantic gate rejects).
    let dup = policy_update(
        home.path(),
        &instance_scope,
        &updated_rev,
        "ask",
        &json!([
            {"ruleId": "dup", "effect": "allow", "tool": "bash", "resourcePattern": "x"},
            {"ruleId": "dup", "effect": "deny", "tool": "bash", "resourcePattern": "y"},
        ]),
    );
    assert_eq!(failure_code("policy.update", &dup), "INVALID_ARGUMENT");

    // 6. Out-of-enum mode is rejected by the input schema gate.
    let bad_mode = policy_update(
        home.path(),
        &instance_scope,
        &updated_rev,
        "bypassPermissions",
        &json!([]),
    );
    assert_eq!(failure_code("policy.update", &bad_mode), "INVALID_ARGUMENT");

    // 7. Nothing from the rejected attempts changed the stored policy.
    let after = expect_ok("policy.get", &policy_get(home.path(), &instance_scope));
    assert_eq!(after["policy"]["mode"], "deny");
    assert_eq!(after["policy"]["rules"], rules);
    assert_eq!(after["policy"]["revision"], updated_rev);
}

/// Scope inheritance visible through the real CLI: an instance-level policy
/// serves undefined project/session scopes (labeled with the requested
/// scope, naming the contributing ancestor); a project-level definition then
/// shadows it for its own subtree.
#[tokio::test]
async fn policy_scope_inheritance_and_shadowing_over_cli() {
    let home = TestHome::new("t005-policy-inherit");
    init_home(home.path());
    let instance_scope = json!({"kind": "instance"});
    let project_scope = json!({"kind": "project", "projectId": "proj-1"});
    let session_scope = json!({"kind": "session", "projectId": "proj-1", "sessionId": "sess-1"});

    let default = expect_ok("policy.get", &policy_get(home.path(), &instance_scope));
    let default_rev = default["policy"]["revision"].as_str().unwrap().to_owned();
    let inst_rules = json!([
        {"ruleId": "r-1", "effect": "ask", "tool": "bash", "resourcePattern": "*"},
    ]);
    let inst = expect_ok(
        "policy.update",
        &policy_update(
            home.path(),
            &instance_scope,
            &default_rev,
            "allowConfigured",
            &inst_rules,
        ),
    );
    let inst_rev = inst["policy"]["revision"].as_str().unwrap().to_owned();

    // project scope inherits the instance policy, labeled as requested.
    let p = expect_ok("policy.get", &policy_get(home.path(), &project_scope));
    assert_eq!(p["policy"]["scope"], project_scope);
    assert_eq!(p["policy"]["mode"], "allowConfigured");
    assert_eq!(p["policy"]["rules"], inst_rules);
    assert_eq!(p["policy"]["revision"], inst_rev);
    assert_eq!(p["inheritedFrom"], json!([{"kind": "instance"}]));

    // session scope inherits through the project level.
    let s = expect_ok("policy.get", &policy_get(home.path(), &session_scope));
    assert_eq!(s["policy"]["mode"], "allowConfigured");
    assert_eq!(s["inheritedFrom"], json!([{"kind": "instance"}]));

    // The inherited revision works as the project scope's first CAS value.
    let pj = expect_ok(
        "policy.update",
        &policy_update(home.path(), &project_scope, &inst_rev, "ask", &json!([])),
    );
    let pj_rev = pj["policy"]["revision"].as_str().unwrap().to_owned();

    // The session scope now names the project as contributor.
    let s2 = expect_ok("policy.get", &policy_get(home.path(), &session_scope));
    assert_eq!(s2["policy"]["mode"], "ask");
    assert_eq!(s2["policy"]["rules"], json!([]));
    assert_eq!(s2["policy"]["revision"], pj_rev);
    assert_eq!(
        s2["inheritedFrom"],
        json!([{"kind": "project", "projectId": "proj-1"}])
    );

    // A stale inherited revision no longer writes the session scope.
    let stale = policy_update(home.path(), &session_scope, &inst_rev, "deny", &json!([]));
    assert_eq!(failure_code("policy.update", &stale), "REVISION_CONFLICT");
}

/// A schema-invalid scope is refused at the CLI input gate (usage error,
/// exit 2) before anything reaches the gateway.
#[tokio::test]
async fn policy_get_rejects_malformed_scope_with_usage_error() {
    let home = TestHome::new("t005-policy-bad-scope");
    init_home(home.path());
    let out = await_cli(
        home.path(),
        &[
            "policy",
            "get",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"workspace\"}",
        ],
    );
    assert_eq!(out.code, 2, "usage gate: {}", out.stderr);

    // The gateway pid exists only for the valid-call path above; nothing to
    // assert beyond the exit code here (no business side effects happened).
}

/// Sanity: the CLI binds the instanceId envelope field automatically — the
/// user does not pass it (instanceId comes from the initialized home).
#[tokio::test]
async fn policy_commands_bind_instance_implicitly_from_home() {
    let home = TestHome::new("t005-policy-implicit-iid");
    let init = init_home(home.path());
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();
    assert_eq!(instance_id, init_home_instance(home.path()));

    let result = expect_ok(
        "policy.get",
        &policy_get(home.path(), &json!({"kind": "instance"})),
    );
    assert!(
        result["policy"]["revision"].as_str().is_some(),
        "the implicitly-bound instance served the policy"
    );
}

// ---------------------------------------------------------------------------
// Phase B: target.* / connection.* / instance.inspect_initialization
// ---------------------------------------------------------------------------

fn target_cmd(home: &std::path::Path, args: &[&str]) -> CliOutcome {
    let mut argv = vec!["target"];
    argv.extend_from_slice(args);
    argv.push("--output");
    argv.push("json");
    await_cli(home, &argv)
}
#[allow(clippy::too_many_arguments)] // test helper
fn write_target(
    home: &std::path::Path,
    target_id: &str,
    name: &str,
    kind: &str,
    endpoint: &str,
    expected_instance_id: &str,
    credential_ref: Option<&str>,
    expected_revision: Option<&str>,
) -> CliOutcome {
    let mut args = vec![
        "write",
        "--target-id",
        target_id,
        "--name",
        name,
        "--kind",
        kind,
        "--endpoint",
        endpoint,
        "--expected-instance-id",
        expected_instance_id,
    ];
    if let Some(cred) = credential_ref {
        args.extend(["--credential-ref", cred]);
    }
    if let Some(rev) = expected_revision {
        args.extend(["--expected-revision", rev]);
    }
    target_cmd(home, &args)
}

/// The full target CRUD + select roundtrip over the real CLI, including the
/// CAS paths, the cleared-default-on-remove rule, and credential redaction
/// (list/get never print credential material, only the `credentialRef`).
#[tokio::test]
async fn target_crud_select_roundtrip_and_credential_redaction() {
    let home = TestHome::new("t005-target-crud");
    let init = init_home(home.path());
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();

    // Empty store.
    let empty = expect_ok("target.list", &target_cmd(home.path(), &["list"]));
    assert_eq!(empty["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty["selectedTargetId"], Value::Null);
    assert_eq!(empty["pageInfo"]["complete"], true);

    // A credential exists in the credentials store; the profile only
    // references it.
    std::fs::write(
        home.path().join("credentials.json"),
        serde_json::to_vec(&serde_json::json!({
            "tokens": {"e2e-remote-cred": "e2e-SECRET-token-value-8842"}
        }))
        .unwrap(),
    )
    .unwrap();

    // Local profile (endpoint = the home data root).
    let local = expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "dev-local",
            "Local dev",
            "local",
            &home.path().display().to_string(),
            &instance_id,
            None,
            None,
        ),
    );
    assert_eq!(local["target"]["kind"], "local");
    let local_rev = local["target"]["revision"].as_str().unwrap().to_owned();

    // Remote ws:// URL profile referencing the credential.
    let remote = expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "staging",
            "Staging",
            "https",
            "ws://127.0.0.1:48765/v1/ws",
            "inst-staging-1",
            Some("e2e-remote-cred"),
            None,
        ),
    );
    assert_eq!(remote["target"]["credentialRef"], "e2e-remote-cred");
    let staging_rev = remote["target"]["revision"].as_str().unwrap().to_owned();

    // get / list reflect both, and never leak the token value.
    let got = expect_ok(
        "target.get",
        &target_cmd(home.path(), &["get", "--target-id", "staging"]),
    );
    assert_eq!(got["target"]["endpoint"], "ws://127.0.0.1:48765/v1/ws");
    let listed = expect_ok("target.list", &target_cmd(home.path(), &["list"]));
    assert_eq!(listed["items"].as_array().unwrap().len(), 2);
    for out in [&got, &listed] {
        let rendered = out.to_string();
        assert!(
            !rendered.contains("e2e-SECRET-token-value-8842"),
            "credential material must never appear in target output"
        );
    }

    // Create-only write on an existing profile is refused (REVISION_CONFLICT).
    let dup = write_target(
        home.path(),
        "dev-local",
        "Local dev",
        "local",
        &home.path().display().to_string(),
        &instance_id,
        None,
        None,
    );
    assert_eq!(failure_code("target.write", &dup), "REVISION_CONFLICT");

    // CAS update with the current revision succeeds; a stale one does not.
    let updated = expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "dev-local",
            "Local dev renamed",
            "local",
            &home.path().display().to_string(),
            &instance_id,
            None,
            Some(&local_rev),
        ),
    );
    let renamed_rev = updated["target"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(renamed_rev, local_rev);
    let stale = write_target(
        home.path(),
        "dev-local",
        "Local dev renamed",
        "local",
        &home.path().display().to_string(),
        &instance_id,
        None,
        Some(&local_rev),
    );
    assert_eq!(failure_code("target.write", &stale), "REVISION_CONFLICT");

    // get of a missing profile is NOT_FOUND.
    let missing = target_cmd(home.path(), &["get", "--target-id", "nope"]);
    assert_eq!(failure_code("target.get", &missing), "NOT_FOUND");

    // select requires the current revision; then later commands use it.
    let bad_select = target_cmd(
        home.path(),
        &[
            "select",
            "--target-id",
            "dev-local",
            "--expected-revision",
            &local_rev,
        ],
    );
    assert_eq!(
        failure_code("target.select", &bad_select),
        "REVISION_CONFLICT"
    );
    let selected = expect_ok(
        "target.select",
        &target_cmd(
            home.path(),
            &[
                "select",
                "--target-id",
                "dev-local",
                "--expected-revision",
                &renamed_rev,
            ],
        ),
    );
    assert_eq!(selected["selectedTargetId"], "dev-local");
    assert_eq!(selected["revision"], renamed_rev);
    let after_select = expect_ok("target.list", &target_cmd(home.path(), &["list"]));
    assert_eq!(after_select["selectedTargetId"], "dev-local");

    // A one-shot explicit --target does not change the default.
    let _ = expect_ok(
        "target.get",
        &target_cmd(
            home.path(),
            &["get", "--target-id", "staging", "--target", "staging"],
        ),
    );
    let after_explicit = expect_ok("target.list", &target_cmd(home.path(), &["list"]));
    assert_eq!(
        after_explicit["selectedTargetId"], "dev-local",
        "one-shot --target must not rewrite the default"
    );

    // remove with a stale revision is refused; with the current one the
    // non-default remote disappears and the default survives.
    let bad_remove = target_cmd(
        home.path(),
        &[
            "remove",
            "--target-id",
            "staging",
            "--expected-revision",
            "rev-000000",
        ],
    );
    assert_eq!(
        failure_code("target.remove", &bad_remove),
        "REVISION_CONFLICT"
    );
    let removed = expect_ok(
        "target.remove",
        &target_cmd(
            home.path(),
            &[
                "remove",
                "--target-id",
                "staging",
                "--expected-revision",
                &staging_rev,
            ],
        ),
    );
    assert_eq!(removed["removedTargetId"], "staging");
    let after_remove = expect_ok("target.list", &target_cmd(home.path(), &["list"]));
    assert_eq!(after_remove["items"].as_array().unwrap().len(), 1);
    assert_eq!(after_remove["selectedTargetId"], "dev-local");

    // Removing the default clears the selection rather than picking another.
    let removed_default = expect_ok(
        "target.remove",
        &target_cmd(
            home.path(),
            &[
                "remove",
                "--target-id",
                "dev-local",
                "--expected-revision",
                &renamed_rev,
            ],
        ),
    );
    assert_eq!(removed_default["removedTargetId"], "dev-local");
    let final_list = expect_ok("target.list", &target_cmd(home.path(), &["list"]));
    assert!(final_list["items"].as_array().unwrap().is_empty());
    assert_eq!(
        final_list["selectedTargetId"],
        Value::Null,
        "removing the default clears the selection, never falls back"
    );
}

/// Selecting a target steers subsequent commands (config.rs profile-store
/// fallback), and a selected unreachable remote fails loudly without
/// starting the local service (c002: 远程失败不回落).
#[tokio::test]
async fn selected_target_steers_subsequent_commands_and_remote_failure_never_falls_back() {
    let home = TestHome::new("t005-target-steer");
    let init = init_home(home.path());
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();

    // Local profile selected -> policy.get uses it (and auto-starts the
    // local gateway through the selected profile, not through a fallback).
    let local = expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "dev-local",
            "Local dev",
            "local",
            &home.path().display().to_string(),
            &instance_id,
            None,
            None,
        ),
    );
    expect_ok(
        "target.select",
        &target_cmd(
            home.path(),
            &[
                "select",
                "--target-id",
                "dev-local",
                "--expected-revision",
                local["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );
    let steered = expect_ok(
        "policy.get",
        &policy_get(home.path(), &json!({"kind": "instance"})),
    );
    assert!(
        steered["policy"]["revision"].as_str().is_some(),
        "the selected profile served the follow-up command"
    );
    assert!(home.gateway_pid().is_some(), "local gateway started");

    // c002 on a fresh home: an unreachable selected remote fails explicitly
    // and the local service is never started.
    let home2 = TestHome::new("t005-target-c002");
    init_home(home2.path());
    let remote = expect_ok(
        "target.write",
        &write_target(
            home2.path(),
            "dead-remote",
            "Dead remote",
            "https",
            "ws://127.0.0.1:1/v1/ws",
            "inst-nowhere",
            None,
            None,
        ),
    );
    expect_ok(
        "target.select",
        &target_cmd(
            home2.path(),
            &[
                "select",
                "--target-id",
                "dead-remote",
                "--expected-revision",
                remote["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );
    let failed = policy_get(home2.path(), &json!({"kind": "instance"}));
    assert_eq!(failure_code("policy.get", &failed), "CONNECTION_FAILED");
    assert!(
        home2.gateway_pid().is_none(),
        "c002: the local service must not start to back a failed remote"
    );
}

/// `connection.open` authenticates and verifies the target identity over the
/// real transport: the local profile (auto-start) and a remote ws:// profile
/// pointing at the live local gateway both report the gateway identity;
/// wrong expectedInstanceId answers INSTANCE_MISMATCH with zero business
/// requests; a dangling credential reference answers AUTH_REQUIRED.
#[tokio::test]
async fn connection_open_verifies_identity_local_and_remote() {
    let home = TestHome::new("t005-conn-open");
    let init = init_home(home.path());
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();

    // Local profile: connection open auto-starts the gateway and reports
    // its identity.
    let _local = expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "dev-local",
            "Local dev",
            "local",
            &home.path().display().to_string(),
            &instance_id,
            None,
            None,
        ),
    );
    let opened_local = expect_ok(
        "connection.open",
        &await_cli(
            home.path(),
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                "dev-local",
            ],
        ),
    );
    assert_eq!(
        opened_local["identity"]["instanceId"], instance_id,
        "local profile reports the gateway identity"
    );
    assert_eq!(opened_local["identity"]["product"], "agentcpm-pd");
    assert!(
        opened_local["connectionId"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "server-assigned connectionId present"
    );
    assert!(
        opened_local["protocolVersion"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "negotiated protocolVersion present"
    );
    assert!(home.gateway_pid().is_some());

    // Remote profile against the live local gateway, authenticated by a
    // credential reference holding the local token.
    let ready_raw = std::fs::read_to_string(home.path().join("run").join("ready.json")).unwrap();
    let ready: Value = serde_json::from_str(&ready_raw).unwrap();
    let ws_url = ready["wsUrl"].as_str().unwrap().to_owned();
    let token = std::fs::read_to_string(home.path().join("local.token"))
        .unwrap()
        .trim()
        .to_owned();
    std::fs::write(
        home.path().join("credentials.json"),
        serde_json::to_vec(&serde_json::json!({"tokens": {"conn-cred": token}})).unwrap(),
    )
    .unwrap();
    expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "via-remote",
            "Remote profile",
            "https",
            &ws_url,
            &instance_id,
            Some("conn-cred"),
            None,
        ),
    );
    let opened_remote = expect_ok(
        "connection.open",
        &await_cli(
            home.path(),
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                "via-remote",
            ],
        ),
    );
    assert_eq!(opened_remote["identity"]["instanceId"], instance_id);
    let rendered = opened_remote.to_string();
    assert!(
        !rendered.contains(&token),
        "the bearer token must never appear in connection.open output"
    );

    // Wrong expectedInstanceId: INSTANCE_MISMATCH, zero business requests.
    expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "wrong-iid",
            "Wrong identity",
            "https",
            &ws_url,
            "inst-not-this-one",
            Some("conn-cred"),
            None,
        ),
    );
    let mismatched = await_cli(
        home.path(),
        &[
            "connection",
            "open",
            "--output",
            "json",
            "--target-id",
            "wrong-iid",
        ],
    );
    assert_eq!(
        failure_code("connection.open", &mismatched),
        "INSTANCE_MISMATCH"
    );

    // A credential reference that resolves to nothing: AUTH_REQUIRED.
    expect_ok(
        "target.write",
        &write_target(
            home.path(),
            "no-cred",
            "Missing credential",
            "https",
            &ws_url,
            &instance_id,
            Some("no-such-credential"),
            None,
        ),
    );
    let unauthenticated = await_cli(
        home.path(),
        &[
            "connection",
            "open",
            "--output",
            "json",
            "--target-id",
            "no-cred",
        ],
    );
    assert_eq!(
        failure_code("connection.open", &unauthenticated),
        "AUTH_REQUIRED"
    );

    // Opening an unconfigured target is a usage-level INVALID_ARGUMENT.
    let unknown = await_cli(
        home.path(),
        &[
            "connection",
            "open",
            "--output",
            "json",
            "--target-id",
            "never-written",
        ],
    );
    assert_eq!(
        failure_code("connection.open", &unknown),
        "INVALID_ARGUMENT"
    );

    // connection.close: the argv process that opened the connection has
    // exited, so its server-assigned connectionId is honestly reported as
    // already closed (repeat closes and unknown ids never fail).
    let connection_id = opened_remote["connectionId"].as_str().unwrap().to_owned();
    let closed = expect_ok(
        "connection.close",
        &await_cli(
            home.path(),
            &[
                "connection",
                "close",
                "--output",
                "json",
                "--connection-id",
                &connection_id,
            ],
        ),
    );
    assert_eq!(
        closed["closed"], false,
        "cross-process ids are already closed"
    );
    let closed_again = expect_ok(
        "connection.close",
        &await_cli(
            home.path(),
            &[
                "connection",
                "close",
                "--output",
                "json",
                "--connection-id",
                &connection_id,
            ],
        ),
    );
    assert_eq!(
        closed_again["closed"], false,
        "repeat close stays a non-error"
    );
}

/// `instance.inspect_initialization` covers the four states: absent,
/// initialized, corrupt (bad JSON), and incompatible (wrong product /
/// protocol major).
#[tokio::test]
async fn instance_inspect_initialization_reports_four_states() {
    let home = TestHome::new("t005-inspect");

    // absent: nothing was created by the inspection itself.
    let absent_dir = home.path().join("absent-root");
    let absent = expect_ok(
        "instance.inspect_initialization",
        &await_cli(
            home.path(),
            &[
                "instance",
                "inspect",
                "--output",
                "json",
                "--data-root",
                &absent_dir.display().to_string(),
            ],
        ),
    );
    assert_eq!(absent["initializationState"], "absent");
    assert_eq!(absent["identity"], Value::Null);
    assert_eq!(absent["issues"].as_array().unwrap().len(), 0);
    assert!(
        !absent_dir.exists(),
        "inspection must not create the data root"
    );

    // initialized: the real initialized home.
    let init = init_home(home.path());
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();
    let initialized = expect_ok(
        "instance.inspect_initialization",
        &await_cli(
            home.path(),
            &[
                "instance",
                "inspect",
                "--output",
                "json",
                "--data-root",
                &home.path().display().to_string(),
            ],
        ),
    );
    assert_eq!(initialized["initializationState"], "initialized");
    assert_eq!(initialized["identity"]["instanceId"], instance_id);
    assert_eq!(initialized["identity"]["product"], "agentcpm-pd");

    // corrupt: unreadable local.json.
    let corrupt_dir = home.path().join("corrupt-root");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    std::fs::write(corrupt_dir.join("local.json"), "{not json").unwrap();
    let corrupt = expect_ok(
        "instance.inspect_initialization",
        &await_cli(
            home.path(),
            &[
                "instance",
                "inspect",
                "--output",
                "json",
                "--data-root",
                &corrupt_dir.display().to_string(),
            ],
        ),
    );
    assert_eq!(corrupt["initializationState"], "corrupt");
    assert_eq!(corrupt["identity"], Value::Null);
    assert!(
        !corrupt["issues"]
            .as_array()
            .expect("issues present")
            .is_empty(),
        "corrupt reports at least one issue"
    );

    // incompatible: foreign product / protocol major.
    let meta = std::fs::read_to_string(home.path().join("local.json")).unwrap();
    let incompatible_dir = home.path().join("incompat-root");
    for (tag, product, protocol) in [
        ("product", "some-other-product", "agentcpm-pd.operation/1"),
        ("protocol", "agentcpm-pd", "agentcpm-pd.operation/99"),
    ] {
        std::fs::create_dir_all(&incompatible_dir).unwrap();
        let mut value: Value = serde_json::from_str(&meta).unwrap();
        value["product"] = json!(product);
        value["protocolVersion"] = json!(protocol);
        std::fs::write(
            incompatible_dir.join("local.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let incompatible = expect_ok(
            "instance.inspect_initialization",
            &await_cli(
                home.path(),
                &[
                    "instance",
                    "inspect",
                    "--output",
                    "json",
                    "--data-root",
                    &incompatible_dir.display().to_string(),
                ],
            ),
        );
        assert_eq!(
            incompatible["initializationState"], "incompatible",
            "{tag} mismatch must report incompatible"
        );
        assert_eq!(incompatible["identity"], Value::Null);
        assert!(
            !incompatible["issues"]
                .as_array()
                .expect("issues present")
                .is_empty()
        );
        let _ = std::fs::remove_file(incompatible_dir.join("local.json"));
    }
}

#[tokio::test]
async fn policy_rejects_blank_tool_rule_and_configuration_update_cannot_bypass_policy_domain() {
    let home = TestHome::new("t005-policy-boundary");
    let instance_scope = json!({"kind": "instance"});
    let default = expect_ok("policy.get", &policy_get(home.path(), &instance_scope));
    let rev = default["policy"]["revision"].as_str().unwrap().to_owned();

    let bad_tool = policy_update(
        home.path(),
        &instance_scope,
        &rev,
        "ask",
        &json!([{"ruleId": "r-blank", "effect": "allow", "tool": "  "}]),
    );
    assert_eq!(failure_code("policy.update", &bad_tool), "INVALID_ARGUMENT");

    let cfg_bypass = await_cli(
        home.path(),
        &[
            "settings",
            "set",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"instance"}"#,
            "--expected-revision",
            "cfg-0",
            "--changes",
            r#"[{"key":"policy.default","action":"set","value":{"mode":"allowConfigured"}}]"#,
        ],
    );
    assert_eq!(
        failure_code("configuration.update", &cfg_bypass),
        "INVALID_ARGUMENT"
    );
}
