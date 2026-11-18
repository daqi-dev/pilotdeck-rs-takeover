//! End-to-end suite for the `context.*` domain (`cli.task.context`, S3):
//! the five operations driven through the real `agentcpm-pd` argv (`context
//! inspect / styles / configure / reload-styles / compact`) against an
//! isolated `AGENTCPM_PD_HOME` and the real `/v1/ws` transport.
//!
//! Acceptance anchors covered here (the unit suite in the gateway covers
//! the same anchors at the store level):
//! * the style catalog is scope-visible with project-over-user precedence,
//!   a load failure differs from empty, and a bad style keeps the valid
//!   catalog while the reload advances the revision (`cli.case.operation
//!   .context.styles` / `.context.reload_styles`);
//! * thinking and style overrides round-trip with `null` clear / omission
//!   preserve, unsupported thinking is rejected against the effective
//!   model, and an in-flight run keeps its submit-time config snapshot
//!   (`cli.case.operation.context.configure`);
//! * compaction refuses a busy session with `SESSION_BUSY`, and an idle
//!   empty session compacts as an explicit no-op that `context inspect`
//!   can verify (`cli.case.operation.context.compact`);
//! * `envelope.target` vs `params.instanceId` mismatch answers
//!   `INSTANCE_MISMATCH` with zero side effects.
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
    assert_eq!(value["outcome"], "failed", "{value}");
    value["error"].clone()
}

/// 播种一个带真实授权根的项目和工作区（t013 的 `directories create`
/// default-root bootstrap），外加一个会话。
async fn seed_workspace(tag: &str) -> (TestHome, std::path::PathBuf, String, String, String) {
    let home = TestHome::new(tag);
    init_home(home.path());
    let instance_id = init_home_instance(home.path());

    let workspace_rel = format!("{tag}-ws");
    let out = await_cli(
        home.path(),
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--path",
            &json!({
                "authorizedRootId": "default",
                "relativePath": {"encoding": "utf8", "value": workspace_rel},
            })
            .to_string(),
            "--request-key",
            &format!("dirs-{tag}"),
        ],
    );
    expect_ok("directories.create", &out);
    let workspace = home.path().join("workspaces").join(&workspace_rel);
    assert!(workspace.is_dir(), "workspace seeded");

    let project_id = format!("p-{tag}");
    let out = await_cli(
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "S3 Context E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"{workspace_rel}\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    let session_id = format!("s-{tag}");
    let out = await_cli(
        home.path(),
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
    (home, workspace, project_id, session_id, instance_id)
}

async fn context_inspect(root: &std::path::Path, session_id: &str) -> Value {
    let out = await_cli(
        root,
        &[
            "context",
            "inspect",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    expect_ok("context.inspect", &out)
}

fn source_value<'a>(result: &'a Value, key: &str) -> &'a Value {
    result["sources"]
        .as_array()
        .expect("inspect sources")
        .iter()
        .find(|pair| pair["key"].as_str() == Some(key))
        .map(|pair| &pair["value"])
        .expect("source present")
}

fn setting<'a>(result: &'a Value, key: &str) -> &'a Value {
    result["values"]
        .as_array()
        .expect("configure values")
        .iter()
        .find(|entry| entry["key"].as_str() == Some(key))
        .expect("setting present")
}

fn write_style(dir: &std::path::Path, relative: &str, body: &str) {
    let path = dir.join(format!("{relative}.md"));
    std::fs::create_dir_all(path.parent().expect("style parent")).unwrap();
    std::fs::write(path, body).unwrap();
}

async fn wait_operation(root: &std::path::Path, operation_id: &str) -> Value {
    for _ in 0..400 {
        let out = await_cli(
            root,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                operation_id,
            ],
        );
        if let Ok(page) =
            serde_json::from_str::<Value>(out.stdout.lines().next().unwrap_or_default())
        {
            let operation = &page["result"]["operation"];
            if matches!(
                operation["state"].as_str(),
                Some("succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown")
            ) {
                return operation.clone();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("operation `{operation_id}` never became terminal");
}

/// The style catalog: scope visibility, project-over-user precedence, bad
/// styles skipped, pagination, and the reload's revision + diagnostics with
/// idempotent-submit receipt semantics (`cli.case.operation.context.styles`
/// / `.context.reload_styles`).
#[tokio::test]
async fn styles_catalog_reload_and_idempotent_receipt() {
    let (home, workspace, _project_id, session_id, instance_id) = seed_workspace("ctx-sty").await;
    let root = home.path();

    // user namespace: two valid styles plus one bad (empty) file; project
    // namespace: a "concise" override and a nested one.
    write_style(&root.join("styles"), "concise.md", "user concise body");
    write_style(&root.join("styles"), "nested/deep.md", "user nested body");
    write_style(&root.join("styles"), "bad.md", "   \n");
    let project_styles = workspace.join(".pilotdeck").join("styles");
    write_style(&project_styles, "concise.md", "project concise body");
    write_style(&project_styles, "team/detailed.md", "project detailed body");

    let session_scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"p-ctx-sty\",\"sessionId\":\"{session_id}\"}}"
    );
    let out = await_cli(
        root,
        &[
            "context",
            "styles",
            "--output",
            "json",
            "--scope",
            &session_scope,
        ],
    );
    let listed = expect_ok("context.styles", &out);
    let ids: Vec<String> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["styleId"].as_str().unwrap().to_owned())
        .collect();
    // Project wins the "concise" name; the shadowed user entry is gone;
    // the bad file never lists (「坏风格保留旧有效目录」).
    assert_eq!(
        ids,
        vec![
            "project:concise".to_owned(),
            "project:team/detailed".to_owned(),
            "user:nested/deep".to_owned(),
        ]
    );
    assert_eq!(listed["items"][0]["available"], json!(true));
    assert_eq!(listed["pageInfo"]["complete"], json!(true));
    let snapshot_revision = listed["pageInfo"]["snapshotRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    // The instance scope sees only the user namespace; pagination works.
    let out = await_cli(
        root,
        &[
            "context",
            "styles",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"instance\"}",
        ],
    );
    let listed = expect_ok("context.styles", &out);
    let ids: Vec<String> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["styleId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        vec!["user:concise".to_owned(), "user:nested/deep".to_owned()]
    );
    let out = await_cli(
        root,
        &[
            "context",
            "styles",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"instance\"}",
            "--page",
            "{\"limit\":1}",
        ],
    );
    let page = expect_ok("context.styles", &out);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["pageInfo"]["complete"], json!(false));
    let cursor = page["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "context",
            "styles",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"instance\"}",
            "--page",
            &format!("{{\"limit\":1,\"cursor\":\"{cursor}\"}}"),
        ],
    );
    let page = expect_ok("context.styles", &out);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["pageInfo"]["complete"], json!(true));

    // A load failure differs from an empty catalog: replace the user styles
    // dir with a regular file and the listing fails (「加载失败不同于空」).
    let styles_dir = root.join("styles");
    let backup = root.join("styles-backup");
    std::fs::rename(&styles_dir, &backup).unwrap();
    std::fs::write(&styles_dir, "not a directory").unwrap();
    let out = await_cli(
        root,
        &[
            "context",
            "styles",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"instance\"}",
        ],
    );
    let error = expect_failed("context.styles", &out);
    assert_eq!(error["code"], "UNAVAILABLE", "{error}");
    std::fs::remove_file(&styles_dir).unwrap();
    std::fs::rename(&backup, &styles_dir).unwrap();

    // Reload: receipt, then the terminal operation's completion carries the
    // advanced revision plus a diagnostic for the bad style.
    let reload_args = |key: &str, expected: &str| {
        vec![
            "context".to_owned(),
            "reload-styles".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--scope".to_owned(),
            session_scope.clone(),
            "--expected-revision".to_owned(),
            expected.to_owned(),
            "--request-key".to_owned(),
            key.to_owned(),
        ]
    };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        await_cli(root, &refs)
    };
    let out = run(reload_args("e2e-ctx-reload-1", &snapshot_revision));
    let receipt = expect_envelope("context.reload_styles", &out);
    assert_eq!(receipt["kind"], json!("operation"));
    assert_eq!(receipt["durable"], json!(true));
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = wait_operation(root, &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let completion = &operation["result"];
    let new_revision = completion["revision"].as_str().unwrap().to_owned();
    assert_ne!(new_revision, snapshot_revision, "「成功revision改变」");
    let diagnostics = completion["diagnostics"].as_array().unwrap().clone();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], json!("STYLE_EMPTY"));
    assert_eq!(diagnostics[0]["resourceId"], json!("user:bad"));

    // The styles view now reports the reloaded revision.
    let out = await_cli(
        root,
        &[
            "context",
            "styles",
            "--output",
            "json",
            "--scope",
            &session_scope,
        ],
    );
    let listed = expect_ok("context.styles", &out);
    assert_eq!(listed["pageInfo"]["snapshotRevision"], json!(new_revision));

    // The same key + input replays the receipt (deduplicated); the same key
    // with different input conflicts; the lookup resolves the resource id.
    let out = run(reload_args("e2e-ctx-reload-1", &snapshot_revision));
    let again = expect_envelope("context.reload_styles", &out);
    assert_eq!(again["operationId"], operation_id.as_str());
    assert_eq!(again["deduplicated"], json!(true));
    let out = run(reload_args("e2e-ctx-reload-1", "rev-deadbeef"));
    let error = expect_failed("context.reload_styles", &out);
    assert_eq!(error["code"], "IDEMPOTENCY_CONFLICT", "{error}");

    let out = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "context.reload_styles",
            "--idempotency-key",
            "e2e-ctx-reload-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(lookup["resource"]["operationId"], operation_id.as_str());
    // The instance argument keeps the harness honest (no hidden state).
    assert!(!instance_id.is_empty());
}

/// Thinking and style overrides: supported/unsupported thinking against the
/// effective model, `null` clear / omission preserve, `STYLE_NOT_FOUND`,
/// and the stale-revision CAS (`cli.case.operation.context.configure`).
#[tokio::test]
async fn configure_thinking_and_style_roundtrip() {
    let (home, _workspace, project_id, session_id, instance_id) = seed_workspace("ctx-cfg").await;
    let root = home.path();

    // Seed a provider + model declaring thinking levels, selected at the
    // session scope (the chain `context.inspect` must agree with).
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-e2e",
            "--name",
            "context e2e provider",
            "--protocol",
            "openai",
            "--endpoint",
            "https://api.example.com/v1",
        ],
    );
    expect_ok("provider.write", &out);
    let out = await_cli(
        root,
        &[
            "models",
            "write",
            "--output",
            "json",
            "--model",
            "{\"providerId\":\"prov-e2e\",\"modelId\":\"m-e2e\"}",
            "--name",
            "context e2e model",
            "--declared-capabilities",
            "[{\"key\":\"thinking\",\"value\":[\"low\",\"high\"]}]",
        ],
    );
    expect_ok("model.write", &out);
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
    let base = expect_ok("model_selection.get", &out);
    let selection_rev = base["revision"].as_str().unwrap().to_owned();
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
            &selection_rev,
            "--model",
            "{\"providerId\":\"prov-e2e\",\"modelId\":\"m-e2e\"}",
        ],
    );
    expect_ok("model_selection.set", &out);

    // The inspect side agrees with the selection chain
    // (「实际模型配置可核对」).
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(
        source_value(&inspect, "effectiveModel"),
        &json!({"providerId": "prov-e2e", "modelId": "m-e2e"})
    );
    assert_eq!(source_value(&inspect, "thinking"), &json!(Value::Null));
    let base_revision = inspect["revision"].as_str().unwrap().to_owned();
    let generation = inspect["effectiveGeneration"].as_str().unwrap().to_owned();

    // A supported effort level configures.
    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &base_revision,
            "--thinking",
            "high",
        ],
    );
    let configured = expect_ok("context.configure", &out);
    assert_eq!(setting(&configured, "thinking")["value"], json!("high"));
    let revision = configured["revision"].as_str().unwrap().to_owned();
    assert_ne!(revision, base_revision);

    // An unsupported level is rejected without fake success
    // (「不支持thinking拒绝」).
    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &revision,
            "--thinking",
            "xhigh",
        ],
    );
    let error = expect_failed("context.configure", &out);
    assert_eq!(error["code"], "MODEL_CAPABILITY_UNSUPPORTED", "{error}");

    // A syntactically unknown level is malformed input.
    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &revision,
            "--thinking",
            "ultra",
        ],
    );
    let error = expect_failed("context.configure", &out);
    assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");

    // Stale expectedRevision conflicts.
    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &base_revision,
            "--thinking",
            "low",
        ],
    );
    let error = expect_failed("context.configure", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT", "{error}");

    // `null` clears through the exclusive JSON input
    // (「null清除/省略保留」; typed options cannot express JSON null).
    let input = json!({
        "instanceId": instance_id,
        "sessionId": session_id,
        "expectedRevision": revision,
        "thinking": Value::Null,
    })
    .to_string();
    let out = cli_with_stdin(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--input-json",
            "-",
        ],
        Some(&input),
    )
    .await;
    let cleared = expect_ok("context.configure", &out);
    assert_eq!(setting(&cleared, "thinking")["value"], json!(Value::Null));
    let cleared_revision = cleared["revision"].as_str().unwrap().to_owned();
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(source_value(&inspect, "thinking"), &json!(Value::Null));

    // An unknown style is STYLE_NOT_FOUND; a seeded style configures.
    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &cleared_revision,
            "--output-style-id",
            "user:none",
        ],
    );
    let error = expect_failed("context.configure", &out);
    assert_eq!(error["code"], "STYLE_NOT_FOUND", "{error}");

    write_style(&root.join("styles"), "concise.md", "user concise body");
    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &cleared_revision,
            "--output-style-id",
            "user:concise",
        ],
    );
    let styled = expect_ok("context.configure", &out);
    assert_eq!(
        setting(&styled, "outputStyleId")["value"],
        json!("user:concise")
    );
    let style_revision = styled["revision"].as_str().unwrap().to_owned();
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(
        source_value(&inspect, "outputStyleId"),
        &json!("user:concise")
    );
    // The effective generation is stable across context-only writes (it is
    // the run-snapshot generation, not the context revision).
    assert_eq!(inspect["effectiveGeneration"], json!(generation));
}

/// Anchor: an in-flight run keeps its submit-time config snapshot across a
/// context configure, and `context inspect` reads the run's own view
/// (「忙碌维护不损坏正在执行轮」/「在途run不热换」).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_snapshot_is_not_hot_swapped_and_inspect_reads_it() {
    let (home, _workspace, _project_id, session_id, _instance_id) =
        seed_workspace("ctx-snap").await;
    let root = home.path();

    // Submit and settle one run before any context change.
    let submitted = submit_run(root, &session_id, "before context change", "e2e-ctx-snap-1").await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(root, &run_id);
    let _ = expect_envelope("run.wait", &waited);
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    let run = expect_ok("run.get", &out);
    let config = run["run"]["config"].clone();

    let inspect = context_inspect(root, &session_id).await;
    let base_revision = inspect["revision"].as_str().unwrap().to_owned();
    // inspect's effectiveGeneration is exactly the generation the run
    // holds at submit time (the run execution-time snapshot semantics).
    assert_eq!(
        inspect["effectiveGeneration"], config["effectiveGeneration"],
        "inspect reports the generation runs snapshot at submit"
    );

    // Configure while the next run is in flight: the running turn is not
    // hot-swapped — its stored config stays byte-identical.
    let submitted = submit_run(root, &session_id, "[pd-driver:sleep=600]", "e2e-ctx-snap-2").await;
    let busy_run = submitted["runId"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &base_revision,
            "--thinking",
            "off",
        ],
    );
    expect_ok("context.configure", &out);

    // Busy maintenance does not damage the in-flight turn: inspect (with
    // the run pinned) works mid-run and reports the run's own view.
    let out = await_cli(
        root,
        &[
            "context",
            "inspect",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--run-id",
            &busy_run,
        ],
    );
    let mid = expect_ok("context.inspect", &out);
    assert_eq!(source_value(&mid, "runState"), &json!("running"));
    assert_eq!(
        source_value(&mid, "runEffectiveGeneration"),
        &config["effectiveGeneration"]
    );

    let waited = wait_run(root, &busy_run);
    let _ = expect_envelope("run.wait", &waited);
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &busy_run],
    );
    let after = expect_ok("run.get", &out);
    assert_eq!(
        after["run"]["config"], config,
        "a run must keep the config snapshot it holds across context changes"
    );

    // The settled inspect reads the stored override; the run observation is
    // gone (no runId in the input).
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(source_value(&inspect, "thinking"), &json!("off"));
}

/// Compact: a busy session is refused with `SESSION_BUSY`, an idle empty
/// session compacts as an explicit no-op, and `context inspect` verifies it
/// (`cli.case.operation.context.compact`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_busy_refusal_then_idle_noop() {
    let (home, _workspace, _project_id, session_id, _instance_id) =
        seed_workspace("ctx-cmpt").await;
    let root = home.path();

    let inspect = context_inspect(root, &session_id).await;
    let base_revision = inspect["revision"].as_str().unwrap().to_owned();
    assert_eq!(inspect["compactionState"], json!("none"));

    // A busy session is refused before acceptance.
    let submitted = submit_run(
        root,
        &session_id,
        "[pd-driver:sleep=800]",
        "e2e-ctx-cmpt-run",
    )
    .await;
    let busy_run = submitted["runId"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "context",
            "compact",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &base_revision,
            "--request-key",
            "e2e-ctx-cmpt-1",
        ],
    );
    let error = expect_failed("context.compact", &out);
    assert_eq!(error["code"], "SESSION_BUSY", "{error}");

    // The busy refusal is retryable, not pinned: once idle, the same key is
    // accepted fresh.
    let waited = wait_run(root, &busy_run);
    let _ = expect_envelope("run.wait", &waited);
    let out = await_cli(
        root,
        &[
            "context",
            "compact",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &base_revision,
            "--request-key",
            "e2e-ctx-cmpt-1",
        ],
    );
    let receipt = expect_envelope("context.compact", &out);
    assert_eq!(receipt["kind"], json!("operation"));
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = wait_operation(root, &operation_id).await;
    // The session has no history entries in the e2e world (the turn-to-
    // history projection lands with the runtime wiring): the compaction is
    // an explicit no-op success.
    assert_eq!(operation["state"], json!("succeeded"));
    let completion = &operation["result"];
    assert_eq!(completion["beforeRevision"], json!(base_revision));
    assert_eq!(completion["afterRevision"], json!(base_revision));
    assert_eq!(completion["budget"]["status"], json!("estimated"));
    assert_eq!(completion["budget"]["value"], json!(0));

    // 「成功可inspect核验」: nothing was folded, the revision stands.
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(inspect["compactionState"], json!("none"));
    assert_eq!(inspect["revision"], json!(base_revision));
    assert_eq!(source_value(&inspect, "historyEntryCount"), &json!(0));
    assert_eq!(inspect["budget"]["status"], json!("estimated"));

    // The same key + input replays the receipt.
    let out = await_cli(
        root,
        &[
            "context",
            "compact",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &base_revision,
            "--request-key",
            "e2e-ctx-cmpt-1",
        ],
    );
    let replay = expect_envelope("context.compact", &out);
    assert_eq!(replay["operationId"], operation_id.as_str());
    assert_eq!(replay["deduplicated"], json!(true));

    // A stale revision is a definitive conflict.
    let out = await_cli(
        root,
        &[
            "context",
            "compact",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            "rev-deadbeef",
            "--request-key",
            "e2e-ctx-cmpt-2",
        ],
    );
    let error = expect_failed("context.compact", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT", "{error}");
}

/// `envelope.target` vs `params.instanceId` mismatch answers
/// `INSTANCE_MISMATCH` with zero side effects (the shared acceptance for
/// every operation in this domain).
#[tokio::test]
async fn instance_mismatch_is_rejected_with_zero_side_effects() {
    let (home, _workspace, _project_id, session_id, _instance_id) = seed_workspace("ctx-mm").await;
    let root = home.path();

    let inspect = context_inspect(root, &session_id).await;
    let base_revision = inspect["revision"].as_str().unwrap().to_owned();

    let mut session = ConnectSession::start(root).await;
    let _init = session.initialize().await;
    let mismatch = session
        .call(
            "context.configure",
            json!({
                "instanceId": "local-00000000-0000-7000-8000-00000000dead",
                "sessionId": session_id,
                "expectedRevision": base_revision,
                "thinking": "off",
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

    // Zero side effects: the session context is unchanged.
    session.close_stdin().await;
    session.wait_exit(0, "api connect exits cleanly").await;
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(inspect["revision"], json!(base_revision));
    assert_eq!(source_value(&inspect, "thinking"), &json!(Value::Null));
}
