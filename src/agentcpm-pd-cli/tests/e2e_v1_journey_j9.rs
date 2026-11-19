//! S5 业务旅程 J9（迁移与兼容）端到端：`_tmp/20261119-s5/
//! journeys-execution-plan.md` §3 J9 的命令序列（S4 t020 已合入，旅程不再
//! 受阻）。真实 `agentcpm-pd` 二进制、隔离 `AGENTCPM_PD_HOME`、`common_v1`
//! 的 envelope schema 校验；旧形状样本根在临时目录内**手工构造**，绝不触
//! 碰本机真实 `~/.pilotdeck`。
//!
//! 旅程判据（计划文档原文）：迁移前后数据对照（条目数/关键文件 hash）；
//! shim stdout 与新 CLI 等价；失败分支落盘终态。
//!
//! - `j9_migration_journey_and_post_migration_recheck`：`data inspect`（零写
//!   检查）→ `data create-backup`（受理→完成；凭据默认不导出）→
//!   `data list-backup-entries`（分页抽查，g010）+ `data read-backup-artifact`
//!   （条目导出 + 配置脱敏 + 不覆盖负例 DESTINATION_EXISTS）→ `migration
//!   plan`（preview + 源/目标嵌套负例 INVALID_DESTINATION）→ `stage`（隔离
//!   暂存目录落盘）→ `verify`（verified=true）→ `commit` 拒绝路径
//!   （VERIFICATION_REQUIRED / REVISION_CONFLICT）→ `commit`（发布 +
//!   compat/legacy-id-map.json 落盘）→ **迁移后抽 J1 关键步骤复核**
//!   （register/create/submit/get，迁移后的库仍承载完整业务环）→ 迁移前后
//!   对照（源根关键文件 sha256 不变；条目数与 manifest 一致）→ `migration
//!   recover` 异常分支（第二来源 stage 后放弃：discardUnpublished 清暂存而
//!   已发布数据不动；未知 staging → RESOURCE_NOT_OWNED）。
//! - `j9_compat_shim_legacy_entry_sample`（`#[ignore]`）：`pilotdeck` shim 的
//!   旧入口抽样（计划「27 项兼容矩阵抽 5–8 项」的旅程半程；shim 二进制属
//!   `pilotdeck-cli` crate，本 crate 测试拿不到 `CARGO_BIN_EXE_pilotdeck`，
//!   故按跨主机轮同一模式 `#[ignore]` + 独立 shell 驱动，整仓构建存在时也
//!   可手工 `--ignored` 触发；27 项全矩阵的逐字节对照由 pilotdeck-cli 的
//!   c1 测试族持有）。抽样：`--help`/`help`、`resume` 缺参会（usage 错
//!   误）、`settings` 未知子命令（usage 错误）、`tui` 非终端拒绝、
//!   `gateway` 缺渠道（usage 错误）、`skills migrate --from cc --json`
//!   （dry-run 报告 + 零写入）。

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common_v1::*;

/// The migration target format (migration.rs `TARGET_FORMAT_VERSION`).
const TARGET_FORMAT_VERSION: &str = "agentcpm-pd.v1/1";

/// Constructs one test-local legacy `~/.pilotdeck`-shape source root (never a
/// copy of the real one): one project with a transcript (including one
/// unconvertible `model_stream_event`), the run registry, one cron task,
/// memory, a skill, an always-on plan, and config carrying plaintext
/// credentials. `slug`/`tag` keep the two journey sources disjoint.
fn seed_legacy_root(
    base: &Path,
    tag: &str,
    slug: &str,
    run_uuid: &str,
    task_uuid: &str,
) -> PathBuf {
    let root = base.join(format!("j9-legacy-{tag}-{}", std::process::id()));
    let cwd = format!("/tmp/{slug}");
    let session_key = format!("tui:project={cwd}:default");
    let project = root.join("projects").join(format!("{slug}-proj"));
    std::fs::create_dir_all(project.join("chats")).unwrap();
    std::fs::create_dir_all(root.join("runs")).unwrap();
    std::fs::create_dir_all(
        root.join("cron")
            .join("projects")
            .join(format!("{slug}-proj")),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("memory")).unwrap();
    std::fs::create_dir_all(root.join("skills").join("my-skill")).unwrap();
    std::fs::create_dir_all(root.join("always-on").join("plans")).unwrap();
    std::fs::write(
        root.join("pilotdeck.yaml"),
        "adapters:\n  feishu:\n    enabled: false\n    appId: cli-j9\n    appSecret: secret-should-not-leave\n",
    )
    .unwrap();
    std::fs::write(
        root.join("weixin-credentials.json"),
        r#"{"baseUrl":"https://example","botToken":"tok","accountId":"a1"}"#,
    )
    .unwrap();
    std::fs::write(project.join(".cwd"), &cwd).unwrap();
    let transcript = concat!(
        r#"{"type":"session_metadata","sessionId":"tui:project=SESSION_KEY_PLACEHOLDER:default","turnId":"t0","sequence":1,"createdAt":"2026-11-01T00:00:00Z","title":"Demo 会话","tag":"old"}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"tui:project=SESSION_KEY_PLACEHOLDER:default","turnId":"t1","sequence":2,"createdAt":"2026-11-01T00:00:01Z","messages":[{"role":"user","content":[{"type":"text","text":"你好"}]}]}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"tui:project=SESSION_KEY_PLACEHOLDER:default","turnId":"t1","sequence":3,"createdAt":"2026-11-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"你好，有什么可以帮你？"}]}}"#,
        "\n",
        r#"{"type":"model_stream_event","sessionId":"tui:project=SESSION_KEY_PLACEHOLDER:default","turnId":"t1","sequence":4,"createdAt":"2026-11-01T00:00:03Z","delta":"…"}"#,
        "\n",
    )
    .replace("SESSION_KEY_PLACEHOLDER", &session_key);
    std::fs::write(project.join("chats").join("session.jsonl"), transcript).unwrap();
    std::fs::write(
        root.join("runs").join("registry.json"),
        format!(
            r#"{{"schemaVersion":1,"runs":{{"{run_uuid}":{{"sessionKey":"{session_key}","state":"completed","revision":"r1","acceptedAt":"2026-11-01T00:00:00Z","updatedAt":"2026-11-01T00:00:05Z"}}}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("cron")
            .join("projects")
            .join(format!("{slug}-proj"))
            .join("tasks.json"),
        format!(
            r#"{{"tasks":[{{"taskId":"{task_uuid}","message":"daily report","schedule":{{"type":"cron","expression":"0 9 * * *","timezone":"UTC"}},"status":"active","sessionKey":"{session_key}","channelKey":"tui"}}]}}"#
        ),
    )
    .unwrap();
    std::fs::write(root.join("memory").join("note.md"), "# memory\n").unwrap();
    std::fs::write(
        root.join("skills").join("my-skill").join("SKILL.md"),
        "# my-skill\n",
    )
    .unwrap();
    std::fs::write(root.join("always-on").join("plans").join("plan.json"), "{}").unwrap();
    root
}

/// The sha256 of one file, hex-encoded (the source-untouched comparison base).
fn sha256_file(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    format!("{:x}", Sha256::digest(&bytes))
}

/// The migration-relevant source files whose hashes the journey compares
/// before and after the whole migration loop.
fn source_fingerprints(root: &Path, slug: &str) -> Vec<(PathBuf, String)> {
    [
        root.join("pilotdeck.yaml"),
        root.join("weixin-credentials.json"),
        root.join("runs").join("registry.json"),
        root.join("projects")
            .join(format!("{slug}-proj"))
            .join("chats")
            .join("session.jsonl"),
    ]
    .into_iter()
    .map(|path| {
        let hash = sha256_file(&path);
        (path, hash)
    })
    .collect()
}

/// Parses a failed envelope (outcome: "failed") and asserts the non-zero exit;
/// returns the contract error code (the t020 suite's shape).
fn expect_failed_code(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must exit non-zero on failure");
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON ({e}): {}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` envelope: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// `await_cli` + `expect_ok`: the method hint is the operation id the argv
/// maps to — tree-mapped aliases (`projects list` -> `project.list`,
/// `sessions list` -> `session.list`) must name the real id, exactly like the
/// t020 suite's `cli_json`.
async fn cli_json(method: &str, home: &Path, args: &[&str]) -> Value {
    let out = await_cli(home, args);
    expect_ok(method, &out)
}

/// Polls `operations get` until the async operation reaches a terminal state.
async fn await_operation(home: &Path, operation_id: &str) -> Value {
    // 满载预算（判据不变）：等操作落终态，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let got = cli_json(
            "operations.get",
            home,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                operation_id,
            ],
        )
        .await;
        let state = got["operation"]["state"].as_str().unwrap_or_default();
        if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
        ) {
            return got["operation"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never reached a terminal state"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The target-side revision through the projects listing (the commit CAS
/// base; the t020 suite's shape).
async fn target_revision(home: &Path) -> String {
    let listed = cli_json(
        "project.list",
        home,
        &["projects", "list", "--output", "json"],
    )
    .await;
    listed["pageInfo"]["snapshotRevision"]
        .as_str()
        .expect("target revision from project.list pageInfo")
        .to_owned()
}

/// The J9 migration journey: the full closed loop over a test-local legacy
/// source, the honest rejection paths, the post-migration business recheck
/// (the J1 key steps), the before/after data comparison, and the abandoned
/// second migration's recover branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn j9_migration_journey_and_post_migration_recheck() {
    let home = TestHome::new("j9-journey");
    init_home(home.path());
    let source = seed_legacy_root(
        &std::env::temp_dir(),
        "a",
        "j9demo",
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
    );
    let before = source_fingerprints(&source, "j9demo");

    // ---- (1) inspect: the zero-write check on the legacy shape. ----
    let inspect = cli_json(
        "data.inspect",
        home.path(),
        &[
            "data",
            "inspect",
            "--output",
            "json",
            "--data-root",
            source.to_string_lossy().as_ref(),
            "--depth",
            "full",
        ],
    )
    .await;
    assert_eq!(
        inspect["formatVersion"],
        json!("agentcpm-pd.legacy-pilotdeck/1")
    );
    assert!(inspect["activeWriter"].is_null(), "{inspect}");
    assert_eq!(inspect["summary"]["sessions"], json!(1));
    assert!(
        inspect["issues"]
            .as_array()
            .is_some_and(|issues| issues.is_empty()),
        "the seeded source is clean: {}",
        inspect["issues"]
    );

    // ---- (2) create-backup: accepted -> completed; secrets stay out. ----
    let backup_dir = std::env::temp_dir().join(format!("j9-backup-a-{}", std::process::id()));
    let receipt = cli_json(
        "data.create_backup",
        home.path(),
        &[
            "data",
            "create-backup",
            "--output",
            "json",
            "--data-root",
            source.to_string_lossy().as_ref(),
            "--destination",
            backup_dir.to_string_lossy().as_ref(),
        ],
    )
    .await;
    let operation = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    let completion = &operation["result"];
    assert_eq!(completion["includesSecrets"], json!(false));
    assert!(
        completion["entryCount"].as_u64().unwrap() >= 5,
        "{completion}"
    );
    assert!(
        completion["checksum"]
            .as_str()
            .is_some_and(|c| c.len() == 64),
        "{completion}"
    );
    let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();
    let source_revision = completion["sourceRevision"].as_str().unwrap().to_owned();

    // ---- (3) backup readability spot-checks (g010) + honest negatives. ----
    let page = cli_json(
        "data.list_backup_entries",
        home.path(),
        &[
            "data",
            "list-backup-entries",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--limit",
            "2",
        ],
    )
    .await;
    assert_eq!(page["manifest"]["artifactId"], json!(artifact_id));
    assert_eq!(page["manifest"]["sourceRevision"], json!(source_revision));
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let cursor = page["nextCursor"]
        .as_str()
        .expect("more entries")
        .to_owned();
    let page2 = cli_json(
        "data.list_backup_entries",
        home.path(),
        &[
            "data",
            "list-backup-entries",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--cursor",
            &cursor,
            "--limit",
            "200",
        ],
    )
    .await;
    assert!(page2["nextCursor"].is_null());
    assert_eq!(
        page2["manifest"]["entryCount"], completion["entryCount"],
        "manifest entryCount matches the completion (the 条目数 comparison)"
    );
    let all_items = page2["items"].as_array().unwrap();
    assert!(
        !all_items
            .iter()
            .any(|item| item["kind"] == json!("credential")),
        "credential material never enters the backup: {all_items:?}"
    );
    let extract = std::env::temp_dir().join(format!("j9-extract-a-{}", std::process::id()));
    let extracted = cli_json(
        "data.read_backup_artifact",
        home.path(),
        &[
            "data",
            "read-backup-artifact",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--entry-id",
            "projects/j9demo-proj/.cwd",
            "--destination",
            extract.to_string_lossy().as_ref(),
        ],
    )
    .await;
    assert!(extracted["bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        std::fs::read_to_string(&extract).unwrap().trim(),
        "/tmp/j9demo"
    );
    let config_extract = std::env::temp_dir().join(format!("j9-config-a-{}", std::process::id()));
    cli_json(
        "data.read_backup_artifact",
        home.path(),
        &[
            "data",
            "read-backup-artifact",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--entry-id",
            "pilotdeck.yaml",
            "--destination",
            config_extract.to_string_lossy().as_ref(),
        ],
    )
    .await;
    let config_text = std::fs::read_to_string(&config_extract).unwrap();
    assert!(
        !config_text.contains("secret-should-not-leave"),
        "appSecret redacted in the backup: {config_text}"
    );
    assert!(config_text.contains("appSecret: null"));
    let dup = await_cli(
        home.path(),
        &[
            "data",
            "read-backup-artifact",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--entry-id",
            "projects/j9demo-proj/.cwd",
            "--destination",
            extract.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(
        expect_failed_code("data.read_backup_artifact", &dup),
        "DESTINATION_EXISTS"
    );

    // ---- (4) plan: preview + the nested-destination negative. ----
    let plan = cli_json(
        "migration.plan",
        home.path(),
        &[
            "migration",
            "plan",
            "--output",
            "json",
            "--source-root",
            source.to_string_lossy().as_ref(),
            "--target-root",
            home.path().to_string_lossy().as_ref(),
            "--target-format-version",
            TARGET_FORMAT_VERSION,
        ],
    )
    .await;
    let plan_id = plan["planId"].as_str().unwrap().to_owned();
    let plan_hash = plan["planHash"].as_str().unwrap().to_owned();
    assert!(!plan_id.is_empty() && !plan_hash.is_empty(), "{plan}");
    assert_eq!(plan["requiredBackup"], json!(true));
    let find = |kind: &str| -> Value {
        plan["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["kind"] == json!(kind))
            .cloned()
            .expect("change kind present")
    };
    assert_eq!(find("project")["count"], json!(1));
    assert_eq!(find("session")["count"], json!(1));
    assert_eq!(find("run-record")["count"], json!(1));
    assert_eq!(find("schedule")["count"], json!(1));
    assert_eq!(
        find("skill")["action"],
        json!("manual-via-skills.plan_import")
    );
    assert!(
        plan["unsupportedEntries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.as_str().unwrap().contains("model_stream_event")),
        "the unconvertible transcript type is listed: {}",
        plan["unsupportedEntries"]
    );
    let bad = await_cli(
        home.path(),
        &[
            "migration",
            "plan",
            "--output",
            "json",
            "--source-root",
            source.to_string_lossy().as_ref(),
            "--target-root",
            source.to_string_lossy().as_ref(),
            "--target-format-version",
            TARGET_FORMAT_VERSION,
        ],
    );
    assert_eq!(
        expect_failed_code("migration.plan", &bad),
        "INVALID_DESTINATION"
    );

    // ---- (5) stage: the isolated staging area lands on disk. ----
    let receipt = cli_json(
        "migration.stage",
        home.path(),
        &[
            "migration",
            "stage",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--backup-artifact-id",
            &artifact_id,
            "--source-revision",
            &source_revision,
        ],
    )
    .await;
    let staged_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(staged_op["state"], json!("succeeded"), "{staged_op}");
    let staged = &staged_op["result"];
    let staging_id = staged["stagingId"].as_str().unwrap().to_owned();
    assert!(staged["idMapArtifactId"].as_str().is_some());
    assert!(
        staged["convertedEntries"].as_u64().unwrap() >= 5,
        "{staged}"
    );
    assert!(
        home.path()
            .join("v1")
            .join("staging")
            .join(&staging_id)
            .exists()
    );

    // ---- (6) verify: verified=true. ----
    let receipt = cli_json(
        "migration.verify",
        home.path(),
        &[
            "migration",
            "verify",
            "--output",
            "json",
            "--staging-id",
            &staging_id,
            "--plan-hash",
            &plan_hash,
        ],
    )
    .await;
    let verified_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(verified_op["state"], json!("succeeded"), "{verified_op}");
    let verification = &verified_op["result"];
    assert_eq!(verification["verified"], json!(true), "{verification}");
    let verification_id = verification["verificationId"].as_str().unwrap().to_owned();
    assert!(verification["reportArtifactId"].as_str().is_some());

    // ---- (7) commit rejection paths before the real publish. ----
    let pre_commit_revision = target_revision(home.path()).await;
    let bad = await_cli(
        home.path(),
        &[
            "migration",
            "commit",
            "--output",
            "json",
            "--staging-id",
            &staging_id,
            "--verification-id",
            "verif-j9-bogus",
            "--plan-hash",
            &plan_hash,
            "--expected-target-revision",
            &pre_commit_revision,
        ],
    );
    assert_eq!(
        expect_failed_code("migration.commit", &bad),
        "VERIFICATION_REQUIRED"
    );
    let bad = await_cli(
        home.path(),
        &[
            "migration",
            "commit",
            "--output",
            "json",
            "--staging-id",
            &staging_id,
            "--verification-id",
            &verification_id,
            "--plan-hash",
            &plan_hash,
            "--expected-target-revision",
            "rev-00000000",
        ],
    );
    assert_eq!(
        expect_failed_code("migration.commit", &bad),
        "REVISION_CONFLICT"
    );

    // ---- (8) commit: published; compat export lands on disk. ----
    let receipt = cli_json(
        "migration.commit",
        home.path(),
        &[
            "migration",
            "commit",
            "--output",
            "json",
            "--staging-id",
            &staging_id,
            "--verification-id",
            &verification_id,
            "--plan-hash",
            &plan_hash,
            "--expected-target-revision",
            &pre_commit_revision,
        ],
    )
    .await;
    let committed_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(committed_op["state"], json!("succeeded"), "{committed_op}");
    let commit = &committed_op["result"];
    assert_eq!(commit["published"], json!(true));
    assert_ne!(commit["targetRevision"], pre_commit_revision);
    let compat = std::fs::read_to_string(home.path().join("compat").join("legacy-id-map.json"))
        .expect("compat export after commit");
    let compat_json: Value = serde_json::from_str(&compat).unwrap();
    let items = compat_json["items"].as_array().unwrap();
    assert!(items.len() >= 3, "id map exported: {items:?}");
    assert!(
        home.path()
            .join("compat")
            .join("legacy-id-map.sha256")
            .exists()
    );
    let migrated_project = items
        .iter()
        .find(|item| item["legacyKind"] == json!("project"))
        .expect("project mapping")["newId"]
        .as_str()
        .unwrap()
        .to_owned();
    let migrated_session = items
        .iter()
        .find(|item| item["legacyKind"] == json!("session"))
        .expect("session mapping")["newId"]
        .as_str()
        .unwrap()
        .to_owned();
    let listed = cli_json(
        "project.list",
        home.path(),
        &["projects", "list", "--output", "json"],
    )
    .await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1, "{listed}");
    let sessions = cli_json(
        "session.list",
        home.path(),
        &[
            "sessions",
            "list",
            "--output",
            "json",
            "--project-id",
            &migrated_project,
        ],
    )
    .await;
    assert!(
        sessions["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["sessionId"] == json!(migrated_session)),
        "the migrated session is visible: {sessions}"
    );

    // ---- (9) 迁移后抽 J1 关键步骤复核: the migrated store still carries
    // the full business loop (register/create/submit/get). ----
    let instance_id = init_home_instance(home.path());
    let post_project = "p-j9-post";
    let post_session = "s-j9-post";
    let root = format!(
        "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"root-j9-post\",\"relativePath\":\"ws\"}}"
    );
    cli_json(
        "project.register",
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            post_project,
            "--name",
            "J9 post-migration recheck",
            "--environment-id",
            "local",
            "--root",
            &root,
        ],
    )
    .await;
    cli_json(
        "session.create",
        home.path(),
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            post_session,
            "--project-id",
            post_project,
        ],
    )
    .await;
    let run = cli_json(
        "run.submit",
        home.path(),
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            post_session,
            "--request-key",
            "j9-post-run-1",
            "--text",
            "hello after migration",
        ],
    )
    .await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(home.path(), &run_id);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded", "{waited}");
    let got = cli_json(
        "run.get",
        home.path(),
        &["run", "get", "--output", "json", "--run-id", &run_id],
    )
    .await;
    assert_eq!(got["run"]["state"], "succeeded");
    let listed = cli_json(
        "project.list",
        home.path(),
        &["projects", "list", "--output", "json"],
    )
    .await;
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        2,
        "migrated + post-migration project: {listed}"
    );

    // ---- (10) 迁移前后对照: the source root is byte-identical. ----
    let after = source_fingerprints(&source, "j9demo");
    for ((path_before, hash_before), (path_after, hash_after)) in before.iter().zip(&after) {
        assert_eq!(path_before, path_after);
        assert_eq!(
            hash_before,
            hash_after,
            "the migration must never touch the source: {}",
            path_before.display()
        );
    }

    // ---- (11) recover 异常分支: a second legacy source staged then
    // abandoned — discardUnpublished clears its staging while the committed
    // data stays; an unknown staging is RESOURCE_NOT_OWNED. ----
    let source_b = seed_legacy_root(
        &std::env::temp_dir(),
        "b",
        "j9demo2",
        "33333333-3333-3333-3333-333333333333",
        "44444444-4444-4444-4444-444444444444",
    );
    let plan_b = cli_json(
        "migration.plan",
        home.path(),
        &[
            "migration",
            "plan",
            "--output",
            "json",
            "--source-root",
            source_b.to_string_lossy().as_ref(),
            "--target-root",
            home.path().to_string_lossy().as_ref(),
            "--target-format-version",
            TARGET_FORMAT_VERSION,
        ],
    )
    .await;
    let backup_b = std::env::temp_dir().join(format!("j9-backup-b-{}", std::process::id()));
    let receipt = cli_json(
        "data.create_backup",
        home.path(),
        &[
            "data",
            "create-backup",
            "--output",
            "json",
            "--data-root",
            source_b.to_string_lossy().as_ref(),
            "--destination",
            backup_b.to_string_lossy().as_ref(),
        ],
    )
    .await;
    let operation = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let completion_b = &operation["result"];
    let receipt = cli_json(
        "migration.stage",
        home.path(),
        &[
            "migration",
            "stage",
            "--output",
            "json",
            "--plan-id",
            plan_b["planId"].as_str().unwrap(),
            "--plan-hash",
            plan_b["planHash"].as_str().unwrap(),
            "--backup-artifact-id",
            completion_b["artifactId"].as_str().unwrap(),
            "--source-revision",
            completion_b["sourceRevision"].as_str().unwrap(),
        ],
    )
    .await;
    let staged_b_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(staged_b_op["state"], json!("succeeded"), "{staged_b_op}");
    let staging_b = staged_b_op["result"]["stagingId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        home.path()
            .join("v1")
            .join("staging")
            .join(&staging_b)
            .exists()
    );
    let revision = target_revision(home.path()).await;
    let receipt = cli_json(
        "migration.recover",
        home.path(),
        &[
            "migration",
            "recover",
            "--output",
            "json",
            "--staging-id",
            &staging_b,
            "--action",
            "discardUnpublished",
            "--expected-target-revision",
            &revision,
        ],
    )
    .await;
    let discard_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(discard_op["state"], json!("succeeded"), "{discard_op}");
    assert_eq!(discard_op["result"]["recovered"], json!(true));
    assert!(
        !home
            .path()
            .join("v1")
            .join("staging")
            .join(&staging_b)
            .exists(),
        "the abandoned staging is cleared"
    );
    let bad = await_cli(
        home.path(),
        &[
            "migration",
            "recover",
            "--output",
            "json",
            "--staging-id",
            "stg-j9-unknown",
            "--action",
            "discardUnpublished",
            "--expected-target-revision",
            &revision,
        ],
    );
    assert_eq!(
        expect_failed_code("migration.recover", &bad),
        "RESOURCE_NOT_OWNED"
    );
    let listed = cli_json(
        "project.list",
        home.path(),
        &["projects", "list", "--output", "json"],
    )
    .await;
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        2,
        "the committed migration survives the discard: {listed}"
    );
    assert!(
        home.path()
            .join("compat")
            .join("legacy-id-map.json")
            .exists(),
        "the compat export survives the discard"
    );

    let _ = std::fs::remove_dir_all(&backup_dir);
    let _ = std::fs::remove_dir_all(&backup_b);
    let _ = std::fs::remove_dir_all(&extract);
    let _ = std::fs::remove_dir_all(&config_extract);
    let _ = std::fs::remove_dir_all(&source);
    let _ = std::fs::remove_dir_all(&source_b);
}

/// Resolves the workspace-built `pilotdeck` shim for the compat sampling:
/// the explicit `J9_PILOTDECK_BIN` override, else `<profile>/pilotdeck`
/// beside this crate's own binary (a full-workspace build places both in the
/// same profile root; a single-crate build does not, which is exactly why
/// this test is `#[ignore]`d and driven by the shell round).
fn shim_binary() -> PathBuf {
    if let Ok(path) = std::env::var("J9_PILOTDECK_BIN") {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "J9_PILOTDECK_BIN does not point at a file");
        return path;
    }
    let beside = Path::new(BIN)
        .parent()
        .expect("the profile root beside the agentcpm-pd binary")
        .join("pilotdeck");
    assert!(
        beside.is_file(),
        "the `pilotdeck` shim was not built into {}; build the whole workspace or set J9_PILOTDECK_BIN",
        beside.display()
    );
    beside
}

/// One shim invocation with a fully isolated PILOT_HOME/HOME scratch.
fn shim_run(shim: &Path, scratch: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(shim)
        .env_clear()
        .env("HOME", scratch.join("home"))
        .env("PILOT_HOME", scratch.join("pilot"))
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .current_dir(scratch)
        .args(args)
        .output()
        .expect("spawn the pilotdeck shim")
}

/// J9's compat sampling (the journey half of the 27-entry matrix): five to
/// six legacy entries through the real `pilotdeck` shim, each with a
/// source-verified exit shape, zero network, and zero writes into the
/// isolated PILOT_HOME. The byte-exact matrix is the c1 family's
/// (`src/pilotdeck-cli/tests/`).
#[ignore = "cross-crate binary: the workspace-built `pilotdeck` shim is not this crate's CARGO_BIN_EXE; run via the W4/W5 shell driver (or manually with --ignored after a full workspace build, optionally with J9_PILOTDECK_BIN)"]
#[test]
fn j9_compat_shim_legacy_entry_sample() {
    let shim = shim_binary();
    let root = std::env::temp_dir().join(format!("j9-shim-{}", std::process::id()));
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("project")).unwrap();

    // (1) --help / help: the SDK entry help, exit 0.
    for args in [vec!["--help"], vec!["help"]] {
        let out = shim_run(&shim, &root, &args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with("PilotDeck SDK CLI"),
            "{args:?}: {stdout}"
        );
        assert!(stdout.contains("pilotdeck run <prompt>"), "{stdout}");
        assert!(stdout.contains("--gateway-url"), "{stdout}");
    }

    // (2) resume without a session id: the usage error fires before any
    // connection attempt (the auth token is provided inline).
    let out = shim_run(&shim, &root, &["resume", "--auth-token", "j9-token"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Usage: pilotdeck resume <session-id> <prompt>"
    );

    // (3) settings with an unknown subcommand: the subcommand usage error.
    let out = shim_run(
        &shim,
        &root,
        &["settings", "bogus", "--auth-token", "j9-token"],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Usage: pilotdeck settings <get|set> ..."
    );

    // (4) tui without an interactive terminal: refused, never a hang.
    let out = shim_run(&shim, &root, &["tui"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "pilotdeck tui requires an interactive terminal."
    );

    // (5) gateway without a setup subcommand: the channel usage error.
    let out = shim_run(&shim, &root, &["gateway"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Usage: pilotdeck gateway setup [feishu|weixin|wecom]"
    );

    // (6) skills migrate: the client-side dry-run report (exit 0, honest
    // statuses, and no write into the isolated PILOT_HOME).
    let source = root.join("source");
    std::fs::create_dir_all(source.join("z")).unwrap();
    std::fs::write(
        source.join("z").join("SKILL.md"),
        "---\nname: sample\ndescription: A sample skill for testing local migration.\n---\nBody.\n",
    )
    .unwrap();
    let out = shim_run(
        &shim,
        &root,
        &[
            "skills", "migrate", "--source", "source", "--from", "cc", "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let item = report["items"]
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .unwrap_or_else(|| panic!("the migrate report lists the skill: {report}"));
    assert_eq!(item["slug"], "z");
    assert_eq!(item["status"], "would_migrate");
    assert_eq!(item["sourceKind"], "custom");
    assert!(
        !root.join("pilot").exists(),
        "the dry-run never writes into PILOT_HOME"
    );

    let _ = std::fs::remove_dir_all(&root);
}
