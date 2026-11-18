//! t018（记忆与快照）端到端场景：`memory.*`（16 操作）与 `snapshots.*`
//! （4 操作）经真实 CLI 二进制与 `/v1/ws` 传输。
//!
//! 记忆条目的播种走生产可达路径：bundle JSON → `uploads.*` 产物化 →
//! `memory.plan_import` → `memory.import`（两段式）。快照捕获 seam 本构建
//! 无人接线（见任务报告 §6），`snapshots.*` 的 e2e 验收读面可用性、
//! NOT_FOUND 门与 20 操作全量注册断言；plan_restore/restore 的深度行为
//! （diff 协议、恢复快照、事件、revision 冲突）由 gateway 域内测试覆盖。
//!
//! 场景对应 design.json 关联用例：c029（记忆）、a021/a032/memory-
//! maintenance 能力合同、consumer.req.028。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common_v1::{CliOutcome, TestHome, await_cli, expect_ok, init_home, init_home_instance};

/// 测试 crate 无 base64 依赖：与 t013 e2e 相同语义的最小编解码（标准字
/// 母表 + 填充），仅供构造/校验上传分块与产物读取帧。
mod base64_compat {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn b64_encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[(triple >> 18) as usize & 0x3F] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 0x3F] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(triple >> 6) as usize & 0x3F] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[triple as usize & 0x3F] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn b64_decode(text: &str) -> Vec<u8> {
        fn value_of(ch: char) -> u32 {
            match ch {
                'A'..='Z' => (ch as u32) - b'A' as u32,
                'a'..='z' => (ch as u32) - b'a' as u32 + 26,
                '0'..='9' => (ch as u32) - b'0' as u32 + 52,
                '+' => 62,
                '/' => 63,
                _ => 0,
            }
        }
        let chars: Vec<char> = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        let mut out = Vec::with_capacity(chars.len() / 4 * 3);
        for chunk in chars.chunks(4) {
            let pad = chunk.iter().filter(|ch| **ch == '=').count();
            let mut triple: u32 = 0;
            for (index, ch) in chunk.iter().enumerate() {
                let value = if *ch == '=' { 0 } else { value_of(*ch) };
                triple |= value << (18 - 6 * index);
            }
            out.push((triple >> 16) as u8);
            if pad < 2 {
                out.push((triple >> 8) as u8);
            }
            if pad < 1 {
                out.push(triple as u8);
            }
        }
        out
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 播种一个带真实授权根的项目（t012 声明面；与 t013 e2e 同法）。
async fn seed_workspace(tag: &str) -> (TestHome, std::path::PathBuf, String, String) {
    let home = TestHome::new(tag);
    // `common_v1::init_home` 是同步函数（collections/configuration e2e 的
    // 调用形态）；t013 的 e2e 里有一个 `.await` 残留，属潜在编译断点，见
    // 任务报告 §6。
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
    assert!(
        workspace.is_dir(),
        "workspace seeded: {}",
        workspace.display()
    );

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
            "T018 E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"{workspace_rel}\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    (home, workspace, project_id, instance_id)
}

/// 失败断言：非零退出码 + `outcome=failed` + `error.code`。
fn expect_failure(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must not exit 0: {}", out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout not JSON: {e}"));
    assert_eq!(value["outcome"], "failed", "`{method}`: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// t018 的 20 个操作必须全部注册（防「合并丢 arm」的静默 -32004 回归）。
#[tokio::test]
async fn all_t018_operations_are_implemented() {
    // The implemented set is only populated when the gateway's domain
    // registry initializes; a CLI subprocess has its own set. Access the
    // registry directly (embedded-gateway feature links the gateway).
    let _ = pilotdeck_gateway::public_v1::domains::DomainRegistry::global();
    for id in [
        "memory.list",
        "memory.get",
        "memory.edit",
        "memory.change_state",
        "memory.overview",
        "memory.get_settings",
        "memory.set_settings",
        "memory.get_project_meta",
        "memory.set_project_meta",
        "memory.traces",
        "memory.maintain",
        "memory.export",
        "memory.plan_import",
        "memory.import",
        "memory.plan_clear",
        "memory.clear",
        "snapshots.list",
        "snapshots.get",
        "snapshots.plan_restore",
        "snapshots.restore",
    ] {
        assert!(
            agentcpm_pd_protocol::registry::is_implemented(id),
            "`{id}` must be registered by the t018 domains"
        );
    }
}

/// 上传一段 JSON 字节为不可变产物，返回 artifactId。
async fn upload_bytes(
    home: &std::path::Path,
    project_id: &str,
    display: &str,
    bytes: &[u8],
    key: &str,
) -> String {
    let out = await_cli(
        home,
        &[
            "uploads",
            "create",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--display-name",
            display,
            "--media-type",
            "application/json",
            "--byte-length",
            &bytes.len().to_string(),
            "--sha256",
            &sha256_hex(bytes),
            "--request-key",
            key,
        ],
    );
    let created = expect_ok("uploads.create", &out);
    let upload_id = created["upload"]["uploadId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let out = await_cli(
        home,
        &[
            "uploads",
            "part",
            "--output",
            "json",
            "--upload-id",
            &upload_id,
            "--part-number",
            "0",
            "--bytes-base64",
            &base64_compat::b64_encode(bytes),
            "--sha256",
            &sha256_hex(bytes),
        ],
    );
    expect_ok("uploads.part", &out);

    let out = await_cli(
        home,
        &[
            "uploads",
            "complete",
            "--output",
            "json",
            "--upload-id",
            &upload_id,
            "--expected-sha256",
            &sha256_hex(bytes),
            "--request-key",
            &format!("{key}-complete"),
        ],
    );
    expect_ok("uploads.complete", &out);

    // 轮询 uploads.get 至 complete。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = await_cli(
            home,
            &[
                "uploads",
                "get",
                "--output",
                "json",
                "--upload-id",
                &upload_id,
            ],
        );
        let got = expect_ok("uploads.get", &out);
        if got["upload"]["state"] == json!("complete") {
            return got["upload"]["artifactId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
        assert!(Instant::now() < deadline, "upload never completed: {got}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 轮询 operations.get 至终态，返回 operation JSON（含 result）。
async fn await_operation(home: &std::path::Path, operation_id: &str) -> Value {
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

/// c029 + memory 能力：bundle 导入播种 → 读写面 → 条件写 → 设置/meta →
/// overview/traces → export → maintain → plan_clear/clear 两段式。
#[tokio::test]
async fn memory_full_lifecycle_over_argv() {
    let (home, _workspace, project_id, _iid) = seed_workspace("mem-e2e").await;
    let scope = json!({"kind": "project", "projectId": project_id});

    // 1) bundle 播种（uploads → plan_import → import 两段式）。
    let bundle = json!({
        "bundleFormatVersion": "agentcpm-pd.memory-bundle.v1",
        "createdAt": "2026-11-18T00:00:00Z",
        "scope": scope,
        "entries": [
            {"kind": "project", "name": "alpha", "description": "first entry",
             "scope": scope, "fields": {"stage": "planning", "decisions": ["use rust"]}},
            {"kind": "feedback", "name": "beta", "description": "second entry",
             "scope": scope, "fields": {"rule": "prefer explicit", "why": "clarity"}},
        ],
        "summary": {"entryCount": 2, "projectIds": [project_id]},
    });
    let bundle_bytes = bundle.to_string().into_bytes();
    let artifact_id = upload_bytes(
        home.path(),
        &project_id,
        "memory-bundle.json",
        &bundle_bytes,
        "mem-e2e-bundle",
    )
    .await;
    assert!(!artifact_id.is_empty());

    let out = await_cli(
        home.path(),
        &[
            "memory",
            "plan-import",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--target-scope",
            &scope.to_string(),
            "--mode",
            "merge",
        ],
    );
    let plan = expect_ok("memory.plan_import", &out);
    assert_eq!(plan["entryCount"], json!(2));
    assert_eq!(plan["plan"]["destructive"], json!(false));
    let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
    let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

    // 错 planRevision → REVISION_CONFLICT（plan 未消费）。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "import",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--expected-plan-revision",
            "wrong-rev",
            "--request-key",
            "mem-e2e-import-wrong",
        ],
    );
    assert_eq!(expect_failure("memory.import", &out), "REVISION_CONFLICT");

    let out = await_cli(
        home.path(),
        &[
            "memory",
            "import",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--expected-plan-revision",
            &plan_rev,
            "--request-key",
            "mem-e2e-import",
        ],
    );
    let receipt = expect_ok("memory.import", &out);
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(home.path(), &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let imported: Vec<&str> = operation["result"]["importedIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(imported.len(), 2);

    // 2) list / get（含 missingIds 与内容产物读取）。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    );
    let listed = expect_ok("memory.list", &out);
    assert_eq!(listed["configured"], json!(true));
    let entries = listed["entries"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 2);
    let memory_rev = listed["revision"].as_str().unwrap().to_owned();
    let alpha = entries
        .iter()
        .find(|e| e["name"] == json!("alpha"))
        .unwrap();
    let alpha_id = alpha["memoryId"].as_str().unwrap().to_owned();
    let alpha_rev = alpha["revision"].as_str().unwrap().to_owned();
    let content_artifact = alpha["contentArtifactId"].as_str().unwrap().to_owned();

    let out = await_cli(
        home.path(),
        &[
            "memory",
            "get",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--view",
            "entries",
            "--memory-ids",
            &json!([alpha_id, "missing-id"]).to_string(),
        ],
    );
    let got = expect_ok("memory.get", &out);
    assert_eq!(got["missingIds"], json!(["missing-id"]));
    assert_eq!(got["entries"].as_array().unwrap().len(), 1);

    // 内容产物：artifacts.read 全文（不截断）→ 规范 JSON 可解。
    let out = await_cli(
        home.path(),
        &[
            "artifacts",
            "read",
            "--output",
            "json",
            "--artifact-id",
            &content_artifact,
            "--max-bytes",
            "65536",
        ],
    );
    let chunk = expect_ok("artifacts.read", &out);
    assert_eq!(chunk["chunk"]["eof"], json!(true));
    let decoded = base64_compat::b64_decode(chunk["chunk"]["bytesBase64"].as_str().unwrap());
    let document: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(document["kind"], json!("project"));
    assert_eq!(document["fields"]["stage"], json!("planning"));

    // 3) 条件写：edit 成功 / 旧 revision 冲突。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "edit",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--memory-id",
            &alpha_id,
            "--name",
            "alpha",
            "--description",
            "edited entry",
            "--fields",
            &json!({"stage": "building"}).to_string(),
            "--expected-revision",
            &alpha_rev,
        ],
    );
    let edited = expect_ok("memory.edit", &out);
    assert_eq!(edited["entry"]["description"], json!("edited entry"));
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "edit",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--memory-id",
            &alpha_id,
            "--name",
            "alpha",
            "--description",
            "stale",
            "--expected-revision",
            &alpha_rev,
        ],
    );
    assert_eq!(expect_failure("memory.edit", &out), "REVISION_CONFLICT");

    // 4) change_state：deprecate → 默认隐藏 → restore。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    );
    let listed = expect_ok("memory.list", &out);
    let revs: Vec<String> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["revision"].as_str().unwrap().to_owned())
        .collect();
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "change-state",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--action",
            "deprecate",
            "--items",
            &json!([
                {"memoryId": imported[0], "expectedRevision": revs[0]},
                {"memoryId": imported[1], "expectedRevision": revs[1]},
            ])
            .to_string(),
        ],
    );
    let dep = expect_ok("memory.change_state", &out);
    assert_eq!(dep["changedIds"].as_array().unwrap().len(), 2);
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    );
    let listed = expect_ok("memory.list", &out);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 0);
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--include-deprecated",
            "true",
        ],
    );
    let listed = expect_ok("memory.list", &out);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 2);
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "change-state",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--action",
            "restore",
            "--items",
            &json!([
                {"memoryId": imported[0], "expectedRevision": revs[0]},
                {"memoryId": imported[1], "expectedRevision": revs[1]},
            ])
            .to_string(),
        ],
    );
    let res = expect_ok("memory.change_state", &out);
    assert_eq!(res["changedIds"].as_array().unwrap().len(), 2);

    // 5) settings / project meta。
    let out = await_cli(home.path(), &["memory", "get-settings", "--output", "json"]);
    let settings = expect_ok("memory.get_settings", &out);
    assert_eq!(settings["settings"]["reasoningMode"], json!("answerFirst"));
    let settings_rev = settings["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "set-settings",
            "--output",
            "json",
            "--settings",
            &json!({
                "reasoningMode": "accuracyFirst",
                "autoIndexIntervalMinutes": 20,
                "autoDreamIntervalMinutes": 90,
            })
            .to_string(),
            "--expected-revision",
            &settings_rev,
        ],
    );
    let set = expect_ok("memory.set_settings", &out);
    assert_eq!(set["settings"]["reasoningMode"], json!("accuracyFirst"));

    let out = await_cli(
        home.path(),
        &[
            "memory",
            "get-project-meta",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let missing = expect_ok("memory.get_project_meta", &out);
    assert_eq!(missing["present"], json!(false));
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "set-project-meta",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--project-name",
            "T018 E2E",
            "--description",
            "meta via e2e",
            "--status",
            "active",
            "--expected-revision",
            "",
        ],
    );
    expect_ok("memory.set_project_meta", &out);
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "get-project-meta",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let meta = expect_ok("memory.get_project_meta", &out);
    assert_eq!(meta["present"], json!(true));
    assert_eq!(meta["meta"]["status"], json!("active"));

    // 6) overview（snapshot 视图）+ traces（index 空 → maintain 后一条）。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "overview",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--view",
            "snapshot",
        ],
    );
    let overview = expect_ok("memory.overview", &out);
    assert_eq!(overview["memoryCount"], json!(2));
    assert_eq!(overview["snapshotSchemaId"], json!("memory.snapshot.v1"));
    assert!(
        overview["snapshotArtifactId"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "traces",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--kind",
            "index",
        ],
    );
    let traces = expect_ok("memory.traces", &out);
    assert_eq!(traces["traces"].as_array().unwrap().len(), 0);

    // 7) maintain：受理 + 完成（本构建无采集路径 → skipped 带原因）。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    );
    let current_rev = expect_ok("memory.list", &out)["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "maintain",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--action",
            "index",
            "--expected-revision",
            &current_rev,
            "--request-key",
            "mem-e2e-maintain",
        ],
    );
    let receipt = expect_ok("memory.maintain", &out);
    let maintain_op = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(home.path(), &maintain_op).await;
    assert_eq!(operation["state"], json!("succeeded"));
    assert_eq!(operation["result"]["status"], json!("skipped"));
    assert!(
        operation["result"]["reason"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    let out = await_cli(
        home.path(),
        &[
            "memory",
            "traces",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--kind",
            "index",
        ],
    );
    let traces = expect_ok("memory.traces", &out);
    assert_eq!(traces["traces"].as_array().unwrap().len(), 1);
    assert_eq!(traces["traces"][0]["state"], json!("skipped"));
    assert_eq!(
        traces["traces"][0]["detailSchemaId"],
        json!("memory.indexTrace.v1")
    );

    // rollbackDream 指向不存在轨迹 → NOT_FOUND。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "maintain",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--action",
            "rollbackDream",
            "--dream-trace-id",
            "no-such-trace",
            "--expected-revision",
            &current_rev,
            "--request-key",
            "mem-e2e-rollback",
        ],
    );
    assert_eq!(expect_failure("memory.maintain", &out), "NOT_FOUND");

    // 8) export：完成产物可读回并再导入（export→import 幂等闭环）。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "export",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--request-key",
            "mem-e2e-export",
        ],
    );
    let receipt = expect_ok("memory.export", &out);
    let export_op = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(home.path(), &export_op).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let export_artifact = operation["result"]["artifact"]["artifactId"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = await_cli(
        home.path(),
        &[
            "artifacts",
            "read",
            "--output",
            "json",
            "--artifact-id",
            &export_artifact,
            "--max-bytes",
            "262144",
        ],
    );
    let chunk = expect_ok("artifacts.read", &out);
    let decoded = base64_compat::b64_decode(chunk["chunk"]["bytesBase64"].as_str().unwrap());
    let exported: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(
        exported["bundleFormatVersion"],
        json!("agentcpm-pd.memory-bundle.v1")
    );
    assert_eq!(exported["entries"].as_array().unwrap().len(), 2);

    // 9) plan_clear → clear 两段式。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "plan-clear",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--include-traces",
            "true",
        ],
    );
    let plan = expect_ok("memory.plan_clear", &out);
    assert_eq!(plan["memoryCount"], json!(2));
    assert_eq!(plan["plan"]["destructive"], json!(true));
    let clear_plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
    let clear_plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "clear",
            "--output",
            "json",
            "--plan-id",
            &clear_plan_id,
            "--expected-plan-revision",
            &clear_plan_rev,
            "--request-key",
            "mem-e2e-clear",
        ],
    );
    let receipt = expect_ok("memory.clear", &out);
    let clear_op = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(home.path(), &clear_op).await;
    assert_eq!(operation["state"], json!("succeeded"));
    assert_eq!(operation["result"]["outcome"], json!("complete"));
    assert_eq!(
        operation["result"]["deletedIds"].as_array().unwrap().len(),
        2
    );
    assert!(
        operation["result"]["recoveryArtifactId"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "recovery material preserved"
    );

    let out = await_cli(
        home.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--include-deprecated",
            "true",
        ],
    );
    let listed = expect_ok("memory.list", &out);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 0);

    // consumed plan 的一次性：同 plan 新 key → NOT_FOUND。
    let out = await_cli(
        home.path(),
        &[
            "memory",
            "clear",
            "--output",
            "json",
            "--plan-id",
            &clear_plan_id,
            "--expected-plan-revision",
            &clear_plan_rev,
            "--request-key",
            "mem-e2e-clear-again",
        ],
    );
    assert_eq!(expect_failure("memory.clear", &out), "NOT_FOUND");

    let _ = memory_rev;
}

/// snapshots 读面可用性 + NOT_FOUND 门（深度行为由域内测试覆盖；捕获
/// seam 接线见任务报告 §6）。
#[tokio::test]
async fn snapshots_read_surface_and_not_found_gates() {
    let (home, _workspace, project_id, _iid) = seed_workspace("snap-e2e").await;

    // 空（但结构完整）列表。
    let out = await_cli(
        home.path(),
        &[
            "snapshots",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let listed = expect_ok("snapshots.list", &out);
    assert_eq!(listed["snapshots"].as_array().unwrap().len(), 0);
    assert!(listed["nextCursor"].is_null());

    // 未注册项目 → NOT_FOUND。
    let out = await_cli(
        home.path(),
        &[
            "snapshots",
            "list",
            "--output",
            "json",
            "--project-id",
            "p-not-registered",
        ],
    );
    assert_eq!(expect_failure("snapshots.list", &out), "NOT_FOUND");

    // 不存在的快照 → NOT_FOUND（不展示为可恢复成功）。
    let out = await_cli(
        home.path(),
        &[
            "snapshots",
            "get",
            "--output",
            "json",
            "--snapshot-id",
            "missing-snapshot",
        ],
    );
    assert_eq!(expect_failure("snapshots.get", &out), "NOT_FOUND");

    // plan_restore 指向不存在快照 → NOT_FOUND。
    let out = await_cli(
        home.path(),
        &[
            "snapshots",
            "plan-restore",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--snapshot-ids",
            &json!(["missing-snapshot"]).to_string(),
            "--direction",
            "after",
        ],
    );
    assert_eq!(expect_failure("snapshots.plan_restore", &out), "NOT_FOUND");

    // restore 指向不存在计划 → NOT_FOUND。
    let out = await_cli(
        home.path(),
        &[
            "snapshots",
            "restore",
            "--output",
            "json",
            "--plan-id",
            "missing-plan",
            "--expected-plan-revision",
            "whatever",
            "--request-key",
            "snap-e2e-restore",
        ],
    );
    assert_eq!(expect_failure("snapshots.restore", &out), "NOT_FOUND");
}
