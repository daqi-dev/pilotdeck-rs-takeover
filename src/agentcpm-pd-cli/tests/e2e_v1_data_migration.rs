//! t020 旧数据备份迁移与验证端到端套件（008 §10：c032 数据迁移主验收）。
//!
//! 脚本化 008 §3 的六步闭环的完整正向路径与关键拒绝路径：
//! inspect（零写检查）→ create_backup（受理→完成）→ list/read（备份可
//! 读性抽查，g010）→ plan（preview；INVALID_DESTINATION 拒绝）→ stage（隔
//! 离暂存）→ verify（六项核对）→ commit（发布；VERIFICATION_REQUIRED /
//! REVISION_CONFLICT 拒绝）→ recover(restorePreviousTarget)（回滚）。
//!
//! 红线遵守：源根是**显式给出的隔离临时目录**（test-local 旧数据形态，
//! 从不触碰真实 `~/.pilotdeck`）；目标根是 harness 的 `AGENTCPM_PD_HOME`。
//!
//! 集成者注：本套件依赖 `domains/mod.rs` / `local_ops/mod.rs` 的 t020 注册
//! 行（data_backup / migration 域 + data_migration local ops）；行未加时本
//! 文件编译不受影响，但场景会以 `-32004 CAPABILITY_UNAVAILABLE` 失败。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

const TARGET_FORMAT_VERSION: &str = "agentcpm-pd.v1/1";

fn seed_legacy_root(base: &Path) -> PathBuf {
    let root = base.join("legacy-src");
    let project = root.join("projects").join("demo-proj");
    std::fs::create_dir_all(project.join("chats")).unwrap();
    std::fs::create_dir_all(root.join("runs")).unwrap();
    std::fs::create_dir_all(root.join("cron").join("projects").join("demo-proj")).unwrap();
    std::fs::create_dir_all(root.join("memory")).unwrap();
    std::fs::create_dir_all(root.join("skills").join("my-skill")).unwrap();
    std::fs::create_dir_all(root.join("always-on").join("plans")).unwrap();
    std::fs::write(
        root.join("pilotdeck.yaml"),
        "adapters:\n  feishu:\n    enabled: false\n    appId: cli-x\n    appSecret: secret-should-not-leave\n",
    )
    .unwrap();
    std::fs::write(
        root.join("weixin-credentials.json"),
        r#"{"baseUrl":"https://example","botToken":"tok","accountId":"a1"}"#,
    )
    .unwrap();
    std::fs::write(project.join(".cwd"), "/tmp/demo").unwrap();
    let transcript = concat!(
        r#"{"type":"session_metadata","sessionId":"tui:project=/tmp/demo:default","turnId":"t0","sequence":1,"createdAt":"2026-11-01T00:00:00Z","title":"Demo 会话","tag":"old"}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"tui:project=/tmp/demo:default","turnId":"t1","sequence":2,"createdAt":"2026-11-01T00:00:01Z","messages":[{"role":"user","content":[{"type":"text","text":"你好"}]}]}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"tui:project=/tmp/demo:default","turnId":"t1","sequence":3,"createdAt":"2026-11-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"你好，有什么可以帮你？"}]}}"#,
        "\n",
        r#"{"type":"model_stream_event","sessionId":"tui:project=/tmp/demo:default","turnId":"t1","sequence":4,"createdAt":"2026-11-01T00:00:03Z","delta":"…"}"#,
        "\n",
    );
    std::fs::write(
        project.join("chats").join("tui-project-demo-default.jsonl"),
        transcript,
    )
    .unwrap();
    std::fs::write(
        root.join("runs").join("registry.json"),
        r#"{"schemaVersion":1,"runs":{"11111111-1111-1111-1111-111111111111":{"sessionKey":"tui:project=/tmp/demo:default","state":"completed","revision":"r1","acceptedAt":"2026-11-01T00:00:00Z","updatedAt":"2026-11-01T00:00:05Z"}}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("cron").join("projects").join("demo-proj").join("tasks.json"),
        r#"{"tasks":[{"taskId":"22222222-2222-2222-2222-222222222222","message":"daily report","schedule":{"type":"cron","expression":"0 9 * * *","timezone":"UTC"},"status":"active","sessionKey":"tui:project=/tmp/demo:default","channelKey":"tui"}]}"#,
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

/// 解析失败 envelope（outcome: "failed"）的 error.code；同时断言非零退出。
fn expect_failed_code(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must exit non-zero on failure");
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` envelope: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn cli_json(method_hint: &str, home: &Path, args: &[&str]) -> Value {
    let out = await_cli(home, args);
    expect_ok(method_hint, &out)
}

/// 轮询 operations.get 至终态。
async fn await_operation(home: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = await_cli(
            home,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                operation_id,
            ],
        );
        let got = expect_ok("operations.get", &out);
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

#[tokio::test]
async fn backup_stage_verify_commit_and_restore_loop() {
    let home = TestHome::new("t020-loop");
    init_home(home.path());
    let source = seed_legacy_root(&std::env::temp_dir());

    // ---- (1) inspect：零写检查；活跃写者空；摘要与 issues ----
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
    assert!(
        inspect["activeWriter"].is_null(),
        "no active writer: {inspect}"
    );
    assert!(
        inspect["identity"]
            .as_str()
            .is_some_and(|id| id.starts_with("dident-"))
    );
    assert_eq!(inspect["summary"]["sessions"], json!(1));
    assert!(
        inspect["issues"]
            .as_array()
            .is_some_and(|issues| issues.is_empty()),
        "seeded source is clean: {}",
        inspect["issues"]
    );

    // ---- (2) create_backup：受理 → 完成；凭据默认不导出 ----
    let backup_destination =
        std::env::temp_dir().join(format!("t020-loop-backup-{}", std::process::id()));
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
            backup_destination.to_string_lossy().as_ref(),
        ],
    )
    .await;
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(home.path(), &operation_id).await;
    assert_eq!(
        operation["state"],
        json!("succeeded"),
        "backup failed: {operation}"
    );
    let completion = &operation["result"];
    assert_eq!(completion["includesSecrets"], json!(false));
    assert!(completion["entryCount"].as_u64().unwrap() >= 5);
    assert!(
        completion["checksum"]
            .as_str()
            .is_some_and(|c| c.len() == 64)
    );
    let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();
    let backup_source_revision = completion["sourceRevision"].as_str().unwrap().to_owned();

    // ---- (3) 备份可读性抽查（g010）----
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
    assert_eq!(
        page["manifest"]["sourceRevision"],
        json!(backup_source_revision)
    );
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
        "manifest entryCount matches the completion"
    );
    // 凭据默认不导出；配置副本的凭据键置空。
    let all_items = page2["items"].as_array().unwrap();
    assert!(
        !all_items
            .iter()
            .any(|item| item["kind"] == json!("credential")),
        "credential never backed up: {all_items:?}"
    );
    // 条目导出：hash 校验 + 不覆盖。
    let extract = std::env::temp_dir().join(format!("t020-loop-extract-{}", std::process::id()));
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
            "projects/demo-proj/.cwd",
            "--destination",
            extract.to_string_lossy().as_ref(),
        ],
    )
    .await;
    assert!(extracted["bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        std::fs::read_to_string(&extract).unwrap().trim(),
        "/tmp/demo"
    );
    // 配置条目：明文凭据值不进备份（键置 null）。
    let config_extract =
        std::env::temp_dir().join(format!("t020-loop-config-{}", std::process::id()));
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
            "projects/demo-proj/.cwd",
            "--destination",
            extract.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(
        expect_failed_code("data.read_backup_artifact", &dup),
        "DESTINATION_EXISTS"
    );

    // ---- (4) plan：preview；源/目标嵌套拒绝 ----
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
    assert_eq!(find("project")["action"], json!("convert"));
    assert_eq!(
        find("skill")["action"],
        json!("manual-via-skills.plan_import")
    );
    assert_eq!(
        find("memory")["action"],
        json!("manual-via-memory.plan_import")
    );
    assert_eq!(
        find("credential")["action"],
        json!("manual-via-channels.configure")
    );
    assert_eq!(
        find("config-key")["action"],
        json!("manual-via-configuration.update")
    );
    assert_eq!(find("always-on")["action"], json!("unsupported_projection"));
    assert!(
        plan["unsupportedEntries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.as_str().unwrap().contains("model_stream_event")),
        "unsupported lists the unconvertible transcript type"
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

    // ---- (5) stage：隔离暂存（不写目标业务表）----
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
            &backup_source_revision,
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

    // ---- (6) verify：verified=true；失败核验挡提交 ----
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

    // 未知核验 → VERIFICATION_REQUIRED；错 revision → REVISION_CONFLICT。
    let before = target_revision(home.path()).await;
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
            "verif-bogus",
            "--plan-hash",
            &plan_hash,
            "--expected-target-revision",
            &before,
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

    // ---- (7) commit：发布；目标数据可查 ----
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
            &before,
        ],
    )
    .await;
    let committed_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(committed_op["state"], json!("succeeded"), "{committed_op}");
    let commit = &committed_op["result"];
    assert_eq!(commit["published"], json!(true));
    let post_revision = commit["targetRevision"].as_str().unwrap().to_owned();
    assert_ne!(post_revision, before);

    // 目标侧：项目/会话可查；compat 导出可读。
    let listed = cli_json(
        "project.list",
        home.path(),
        &["projects", "list", "--output", "json"],
    )
    .await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1, "{listed}");
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
    let new_project_id = items
        .iter()
        .find(|item| item["legacyKind"] == json!("project"))
        .expect("project mapping")["newId"]
        .as_str()
        .unwrap()
        .to_owned();
    let session_map = items
        .iter()
        .find(|item| item["legacyKind"] == json!("session"))
        .expect("session mapping");
    let new_session_id = session_map["newId"].as_str().unwrap().to_owned();
    let sessions = cli_json(
        "session.list",
        home.path(),
        &[
            "sessions",
            "list",
            "--output",
            "json",
            "--project-id",
            &new_project_id,
        ],
    )
    .await;
    assert!(
        sessions["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["sessionId"] == json!(new_session_id)),
        "migrated session visible: {sessions}"
    );

    // ---- (8) recover(restorePreviousTarget)：无新写入 → 回滚 ----
    let receipt = cli_json(
        "migration.recover",
        home.path(),
        &[
            "migration",
            "recover",
            "--output",
            "json",
            "--staging-id",
            &staging_id,
            "--action",
            "restorePreviousTarget",
            "--expected-target-revision",
            &post_revision,
        ],
    )
    .await;
    let restored_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(restored_op["state"], json!("succeeded"), "{restored_op}");
    let restore = &restored_op["result"];
    assert_eq!(restore["recovered"], json!(true), "{restore}");
    assert_eq!(restore["action"], json!("restorePreviousTarget"));
    let listed = cli_json(
        "project.list",
        home.path(),
        &["projects", "list", "--output", "json"],
    )
    .await;
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        0,
        "rolled back: {listed}"
    );
    assert!(
        !home
            .path()
            .join("compat")
            .join("legacy-id-map.json")
            .exists(),
        "compat export withdrawn after restore"
    );

    // 源根全称未被迁移触碰。
    assert!(source.join("runs").join("registry.json").exists());
    assert!(source.join("weixin-credentials.json").exists());
    let _ = cleanup(&[backup_destination.clone(), extract.clone(), source.clone()]);
}

fn cleanup(paths: &[PathBuf]) -> std::io::Result<()> {
    for path in paths {
        let _ = std::fs::remove_dir_all(path);
    }
    Ok(())
}

#[tokio::test]
async fn plan_stale_and_discard_unpublished() {
    let home = TestHome::new("t020-stale");
    init_home(home.path());
    let source = seed_legacy_root(&std::env::temp_dir());

    // 备份。
    let backup_destination =
        std::env::temp_dir().join(format!("t020-stale-backup-{}", std::process::id()));
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
            backup_destination.to_string_lossy().as_ref(),
        ],
    )
    .await;
    let operation = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let completion = &operation["result"];
    let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();
    let backup_source_revision = completion["sourceRevision"].as_str().unwrap().to_owned();

    // 源在备份后变化 → PLAN_STALE。
    std::fs::write(source.join("runs").join("registry.json"), "{}").unwrap();
    let bad = await_cli(
        home.path(),
        &[
            "migration",
            "stage",
            "--output",
            "json",
            "--plan-id",
            "plan-stale",
            "--plan-hash",
            "phash-stale",
            "--backup-artifact-id",
            &artifact_id,
            "--source-revision",
            &backup_source_revision,
        ],
    );
    assert_eq!(expect_failed_code("migration.stage", &bad), "PLAN_STALE");

    // 未知 staging 的 recover → RESOURCE_NOT_OWNED。
    let revision = target_revision(home.path()).await;
    let bad = await_cli(
        home.path(),
        &[
            "migration",
            "recover",
            "--output",
            "json",
            "--staging-id",
            "stg-unknown",
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

    // 恢复源内容重新备份 → stage → discard。
    std::fs::write(
        source.join("runs").join("registry.json"),
        r#"{"schemaVersion":1,"runs":{"11111111-1111-1111-1111-111111111111":{"sessionKey":"tui:project=/tmp/demo:default","state":"completed","revision":"r1","acceptedAt":"2026-11-01T00:00:00Z","updatedAt":"2026-11-01T00:00:05Z"}}}"#,
    )
    .unwrap();
    let backup2 = std::env::temp_dir().join(format!("t020-stale-backup2-{}", std::process::id()));
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
            backup2.to_string_lossy().as_ref(),
        ],
    )
    .await;
    let operation = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let completion = &operation["result"];
    let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();
    let source_revision = completion["sourceRevision"].as_str().unwrap().to_owned();

    let receipt = cli_json(
        "migration.stage",
        home.path(),
        &[
            "migration",
            "stage",
            "--output",
            "json",
            "--plan-id",
            "plan-2",
            "--plan-hash",
            "phash-2",
            "--backup-artifact-id",
            &artifact_id,
            "--source-revision",
            &source_revision,
        ],
    )
    .await;
    let staged_op = await_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(staged_op["state"], json!("succeeded"), "{staged_op}");
    let staging_id = staged_op["result"]["stagingId"]
        .as_str()
        .unwrap()
        .to_owned();

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
            &staging_id,
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
            .join(&staging_id)
            .exists()
    );
    assert!(
        source.join("runs").join("registry.json").exists(),
        "source untouched"
    );
    let _ = cleanup(&[backup_destination, backup2, source]);
}
