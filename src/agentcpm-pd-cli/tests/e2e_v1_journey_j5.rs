//! S5 业务旅程 J5（记忆管理）端到端：`_tmp/20261119-s5/journeys-execution-plan.md`
//! §3 J5 的命令序列，经真实 `agentcpm-pd` 二进制与 `/v1/ws` 传输、隔离
//! `AGENTCPM_PD_HOME`、`common_v1` 挂具的 envelope schema 校验。
//!
//! 旅程判据（计划文档原文）：bundle artifact 落盘 + 前后 export 计数对照；
//! clear 后记忆计数=0 而项目/会话仍在。
//!
//! - `j5_memory_lifecycle_journey`：settings（CAS + 故障负例）→ run 正常
//!   完成（OfflineRunDriver 无指令路径）→ 记忆基线 → bundle 导入播种
//!   （uploads 产物化 → `memory plan-import` → `import`，含错
//!   planRevision 负例）→ list/get/edit(+CAS 负例)/change-state/traces/
//!   overview → `memory maintain`（index）→ `memory export`（bundle
//!   artifact 落盘断言）。记忆的播种走生产可达路径：v1 合同没有 run→记忆
//!   采集入口（`domains/memory.rs` 模块注释），与既有
//!   `e2e_v1_memory_snapshots.rs` 同一结论。
//! - `j5_isolated_root_reimport_and_clear_journey`：隔离根（第二个
//!   TestHome）export→import→export 规范比较（计数 + 逐条业务字段）；未
//!   映射 project 的 plan-import 负例（SCOPE_MISMATCH）；`memory
//!   plan-clear`→`clear` 两段式后记忆=0 而项目/会话/工作区仍在，consumed
//!   plan 复用负例（NOT_FOUND）。

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common_v1::{
    CliOutcome, OP_VERSION, TestHome, await_cli, expect_ok, init_home, init_home_instance,
    submit_run, wait_run,
};

/// 测试 crate 无 base64 依赖：与 t013/t018 e2e 相同语义的最小编解码（标准
/// 字母表 + 填充），仅供构造上传分块与产物读取帧。
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
    assert_eq!(value["version"], OP_VERSION, "`{method}`: {value}");
    assert_eq!(value["outcome"], "failed", "`{method}`: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// 播种一个带真实授权根的项目 + 工作区 + 会话（t013/t018 e2e 同法）。
async fn seed_workspace(tag: &str) -> (TestHome, std::path::PathBuf, String, String) {
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
            "S5 Journey J5",
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
    (home, workspace, project_id, session_id)
}

/// 上传一段 JSON 字节为不可变产物，返回 artifactId（t018 e2e 同法）。
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

/// 读回一个产物并解析为 JSON（artifacts.read 全文）。
async fn read_artifact_json(home: &std::path::Path, artifact_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "artifacts",
            "read",
            "--output",
            "json",
            "--artifact-id",
            artifact_id,
            "--max-bytes",
            "262144",
        ],
    );
    let chunk = expect_ok("artifacts.read", &out);
    assert_eq!(chunk["chunk"]["eof"], json!(true));
    let decoded = base64_compat::b64_decode(chunk["chunk"]["bytesBase64"].as_str().unwrap());
    serde_json::from_slice(&decoded).expect("artifact bytes are canonical JSON")
}

/// 一个 run 的全部事件类型（run.events 分页补读至 caughtUp）。
async fn run_event_types(home: &std::path::Path, run_id: &str) -> Vec<String> {
    let mut types = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..64 {
        let mut args = vec![
            "run",
            "events",
            "--output",
            "json",
            "--run-id",
            run_id,
            "--page",
            "{\"limit\":500}",
        ];
        if let Some(cursor) = &after {
            args.push("--after-cursor");
            args.push(cursor);
        }
        let page = await_cli(home, &args);
        let page = expect_ok("run.events", &page);
        let frame = &page["frame"];
        let events = frame["events"].as_array().cloned().unwrap_or_default();
        let count = events.len();
        for event in events {
            types.push(event["type"].as_str().unwrap_or_default().to_owned());
        }
        if frame["caughtUp"] == true {
            return types;
        }
        after = frame["nextCursor"].as_str().map(str::to_owned);
        assert!(count > 0, "pagination stalled: {frame}");
    }
    panic!("run.events pagination did not converge for {run_id}");
}

/// bundle 的规范索引：name -> (kind, description, fields)。跨根比较用
/// （memoryId/revision/createdAt/scope.projectId 在导入侧必然重签）。
fn bundle_index(bundle: &Value) -> HashMap<String, (String, String, Value)> {
    bundle["entries"]
        .as_array()
        .expect("bundle entries")
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap_or_default().to_owned(),
                (
                    e["kind"].as_str().unwrap_or_default().to_owned(),
                    e["description"].as_str().unwrap_or_default().to_owned(),
                    e["fields"].clone(),
                ),
            )
        })
        .collect()
}

/// J5 主旅程：settings CAS → run 正常完成 → 记忆基线 → bundle 导入播种 →
/// 读写面（list/get/edit/change-state/traces/overview）→ maintain → export。
#[tokio::test]
async fn j5_memory_lifecycle_journey() {
    let (home, _workspace, project_id, session_id) = seed_workspace("j5-life").await;
    let root = home.path();
    let scope = json!({"kind": "project", "projectId": project_id});

    // 1) settings 读 → 写（代际/revision 变化）→ 旧 revision 故障负例。
    let out = await_cli(root, &["memory", "get-settings", "--output", "json"]);
    let settings = expect_ok("memory.get_settings", &out);
    assert_eq!(settings["settings"]["reasoningMode"], json!("answerFirst"));
    let settings_rev = settings["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
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
    assert_ne!(
        set["revision"].as_str().unwrap_or_default(),
        settings_rev,
        "a settings write must advance the revision"
    );
    let out = await_cli(
        root,
        &[
            "memory",
            "set-settings",
            "--output",
            "json",
            "--settings",
            &json!({
                "reasoningMode": "answerFirst",
                "autoIndexIntervalMinutes": 15,
                "autoDreamIntervalMinutes": 45,
            })
            .to_string(),
            "--expected-revision",
            &settings_rev,
        ],
    );
    assert_eq!(
        expect_failure("memory.set_settings", &out),
        "REVISION_CONFLICT",
        "stale settings revision must be a definitive conflict"
    );

    // 2) run 正常完成（OfflineRunDriver 无指令路径）：终态 + 终态事件。
    let submitted = submit_run(root, &session_id, "j5 ordinary turn", "j5-life-run-1").await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(root, &run_id);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");
    assert_eq!(waited["run"]["runId"].as_str(), Some(run_id.as_str()));
    let event_types = run_event_types(root, &run_id).await;
    assert!(
        event_types.iter().any(|t| t == "run.completed"),
        "the ordinary run must reach its terminal transition event; got {event_types:?}"
    );

    // 记忆基线：v1 没有 run→记忆采集入口（`domains/memory.rs` 模块注释），
    // 所以一个成功 run 之后该 scope 的记忆面从 0 开始——这也是后面
    // 0 → 2 → 0 计数轨迹的起点。
    let out = await_cli(
        root,
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

    // 3) bundle 播种（uploads 产物化 → plan-import → import 两段式）。
    let bundle = json!({
        "bundleFormatVersion": "agentcpm-pd.memory-bundle.v1",
        "createdAt": "2026-11-19T00:00:00Z",
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
        root,
        &project_id,
        "j5-memory-bundle.json",
        &bundle_bytes,
        "j5-life-bundle",
    )
    .await;
    assert!(!artifact_id.is_empty());
    // bundle artifact 落盘：产物在 `<home>/v1/artifacts/<artifactId>`。
    let artifact_path = root.join("v1").join("artifacts").join(&artifact_id);
    assert!(
        artifact_path.is_file(),
        "bundle artifact must land on disk: {}",
        artifact_path.display()
    );

    let out = await_cli(
        root,
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

    // 故障负例：错 planRevision 的第二跳必须 REVISION_CONFLICT 且 plan 未消费。
    let out = await_cli(
        root,
        &[
            "memory",
            "import",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--expected-plan-revision",
            "j5-wrong-plan-revision",
            "--request-key",
            "j5-life-import-wrong",
        ],
    );
    assert_eq!(expect_failure("memory.import", &out), "REVISION_CONFLICT");

    let out = await_cli(
        root,
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
            "j5-life-import",
        ],
    );
    let receipt = expect_ok("memory.import", &out);
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(root, &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let imported: Vec<&str> = operation["result"]["importedIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(imported.len(), 2);

    // 4) 读面：list（计数 0→2）+ get（missingIds）。
    let out = await_cli(
        root,
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
    let entries = listed["entries"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 2);
    let alpha = entries
        .iter()
        .find(|e| e["name"] == json!("alpha"))
        .expect("alpha entry");
    let alpha_id = alpha["memoryId"].as_str().unwrap().to_owned();
    let alpha_rev = alpha["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
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
            &json!([alpha_id, "j5-missing-id"]).to_string(),
        ],
    );
    let got = expect_ok("memory.get", &out);
    assert_eq!(got["missingIds"], json!(["j5-missing-id"]));
    assert_eq!(got["entries"].as_array().unwrap().len(), 1);

    // 5) 条件写：edit 成功 → 旧 revision 故障负例。
    let out = await_cli(
        root,
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
            "edited by j5 journey",
            "--fields",
            &json!({"stage": "building"}).to_string(),
            "--expected-revision",
            &alpha_rev,
        ],
    );
    let edited = expect_ok("memory.edit", &out);
    assert_eq!(
        edited["entry"]["description"],
        json!("edited by j5 journey")
    );
    let out = await_cli(
        root,
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
            "stale write",
            "--expected-revision",
            &alpha_rev,
        ],
    );
    assert_eq!(expect_failure("memory.edit", &out), "REVISION_CONFLICT");

    // 6) change_state：deprecate → 默认隐藏 → include-deprecated 可见 → restore。
    let out = await_cli(
        root,
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
    let items = json!(
        listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                json!({
                    "memoryId": e["memoryId"],
                    "expectedRevision": e["revision"],
                })
            })
            .collect::<Vec<Value>>()
    );
    let out = await_cli(
        root,
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
            &items.to_string(),
        ],
    );
    let dep = expect_ok("memory.change_state", &out);
    assert_eq!(dep["changedIds"].as_array().unwrap().len(), 2);
    let out = await_cli(
        root,
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
        root,
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
    let deprecated: Vec<Value> = listed["entries"].as_array().unwrap().clone();
    assert_eq!(deprecated.len(), 2);
    let restore_items = json!(
        deprecated
            .into_iter()
            .map(|e| {
                json!({
                    "memoryId": e["memoryId"],
                    "expectedRevision": e["revision"],
                })
            })
            .collect::<Vec<Value>>()
    );
    let out = await_cli(
        root,
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
            &restore_items.to_string(),
        ],
    );
    let res = expect_ok("memory.change_state", &out);
    assert_eq!(res["changedIds"].as_array().unwrap().len(), 2);

    // 7) overview（snapshot 视图）+ traces（index 此刻为空）。
    let out = await_cli(
        root,
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
        root,
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

    // 8) maintain（index）：受理 + 完成（本构建无采集路径 → skipped 带原因）。
    let out = await_cli(
        root,
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
        root,
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
            "j5-life-maintain",
        ],
    );
    let receipt = expect_ok("memory.maintain", &out);
    let maintain_op = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(root, &maintain_op).await;
    assert_eq!(operation["state"], json!("succeeded"));
    assert_eq!(operation["result"]["status"], json!("skipped"));
    assert!(
        operation["result"]["reason"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the skipped maintenance must carry a reason"
    );
    let out = await_cli(
        root,
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

    // 9) export：bundle artifact 落盘 + 读回可解（前后计数对照的第一侧）。
    let out = await_cli(
        root,
        &[
            "memory",
            "export",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--request-key",
            "j5-life-export",
        ],
    );
    let receipt = expect_ok("memory.export", &out);
    let export_op = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(root, &export_op).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let export_artifact = operation["result"]["artifact"]["artifactId"]
        .as_str()
        .unwrap()
        .to_owned();
    let exported = read_artifact_json(root, &export_artifact).await;
    assert_eq!(
        exported["bundleFormatVersion"],
        json!("agentcpm-pd.memory-bundle.v1")
    );
    assert_eq!(exported["entries"].as_array().unwrap().len(), 2);
    assert_eq!(
        exported["entries"].as_array().unwrap().len(),
        entries.len(),
        "export count must match the listed entries"
    );
    // 业务落盘：state.sqlite 与产物树都在隔离 home 内。
    assert!(root.join("v1").join("state.sqlite").is_file());
    assert!(
        root.join("v1")
            .join("artifacts")
            .join(export_artifact)
            .is_file()
    );
}

/// J5 隔离根 + clear 旅程：home A export → home B（隔离根）import → home B
/// export 规范比较；home A 两段式 clear 后记忆=0 而项目/会话/工作区仍在。
#[tokio::test]
async fn j5_isolated_root_reimport_and_clear_journey() {
    let (home, workspace, project_id, session_id) = seed_workspace("j5-iso").await;
    let root = home.path();
    let scope = json!({"kind": "project", "projectId": project_id});

    // 播种两个条目（生产可达路径）。
    let bundle = json!({
        "bundleFormatVersion": "agentcpm-pd.memory-bundle.v1",
        "createdAt": "2026-11-19T00:00:00Z",
        "scope": scope,
        "entries": [
            {"kind": "project", "name": "iso-alpha", "description": "iso first",
             "scope": scope, "fields": {"stage": "planning"}},
            {"kind": "feedback", "name": "iso-beta", "description": "iso second",
             "scope": scope, "fields": {"rule": "keep scope small"}},
        ],
        "summary": {"entryCount": 2, "projectIds": [project_id]},
    });
    let bundle_bytes = bundle.to_string().into_bytes();
    let artifact_id = upload_bytes(
        root,
        &project_id,
        "j5-iso-bundle.json",
        &bundle_bytes,
        "j5-iso-seed",
    )
    .await;
    let out = await_cli(
        root,
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
    let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "memory",
            "import",
            "--output",
            "json",
            "--plan-id",
            plan["plan"]["planId"].as_str().unwrap(),
            "--expected-plan-revision",
            &plan_rev,
            "--request-key",
            "j5-iso-seed-import",
        ],
    );
    let receipt = expect_ok("memory.import", &out);
    let operation = await_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));

    // home A export：计数对照的源侧。
    let out = await_cli(
        root,
        &[
            "memory",
            "export",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
            "--request-key",
            "j5-iso-export-a",
        ],
    );
    let receipt = expect_ok("memory.export", &out);
    let operation = await_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let export_artifact = operation["result"]["artifact"]["artifactId"]
        .as_str()
        .unwrap()
        .to_owned();
    let exported_a = read_artifact_json(root, &export_artifact).await;
    assert_eq!(exported_a["entries"].as_array().unwrap().len(), 2);

    // 隔离根 home B：instance init + 注册项目 + 上传同一 bundle 字节。
    let home_b = TestHome::new("j5-iso-b");
    init_home(home_b.path());
    let instance_b = init_home_instance(home_b.path());
    let workspace_b_rel = "j5-iso-b-ws";
    let out = await_cli(
        home_b.path(),
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--path",
            &json!({
                "authorizedRootId": "default",
                "relativePath": {"encoding": "utf8", "value": workspace_b_rel},
            })
            .to_string(),
            "--request-key",
            "j5-iso-b-dirs",
        ],
    );
    expect_ok("directories.create", &out);
    let project_b = "p-j5-iso-b".to_owned();
    let out = await_cli(
        home_b.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_b,
            "--name",
            "S5 Journey J5 isolated root",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_b}\",\"rootId\":\"default\",\"relativePath\":\"{workspace_b_rel}\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    let artifact_b = upload_bytes(
        home_b.path(),
        &project_b,
        "j5-iso-bundle.json",
        &bundle_bytes,
        "j5-iso-b-bundle",
    )
    .await;
    let scope_b = json!({"kind": "project", "projectId": project_b});

    // 故障负例：不携带 projectMappings 的导入必须 SCOPE_MISMATCH（bundle 的
    // 条目属于 home A 的项目，映射目标未注册/未映射时不落不可见项目）。
    let out = await_cli(
        home_b.path(),
        &[
            "memory",
            "plan-import",
            "--output",
            "json",
            "--artifact-id",
            &artifact_b,
            "--target-scope",
            &scope_b.to_string(),
            "--mode",
            "merge",
        ],
    );
    assert_eq!(expect_failure("memory.plan_import", &out), "SCOPE_MISMATCH");

    // 正确导入：source home A project -> target home B project。
    let out = await_cli(
        home_b.path(),
        &[
            "memory",
            "plan-import",
            "--output",
            "json",
            "--artifact-id",
            &artifact_b,
            "--target-scope",
            &scope_b.to_string(),
            "--mode",
            "merge",
            "--project-mappings",
            &json!([{"sourceProjectId": project_id, "targetProjectId": project_b}]).to_string(),
        ],
    );
    let plan = expect_ok("memory.plan_import", &out);
    assert_eq!(plan["entryCount"], json!(2));
    let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
    let out = await_cli(
        home_b.path(),
        &[
            "memory",
            "import",
            "--output",
            "json",
            "--plan-id",
            plan["plan"]["planId"].as_str().unwrap(),
            "--expected-plan-revision",
            &plan_rev,
            "--request-key",
            "j5-iso-b-import",
        ],
    );
    let receipt = expect_ok("memory.import", &out);
    let operation = await_operation(home_b.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    assert_eq!(
        operation["result"]["importedIds"].as_array().unwrap().len(),
        2
    );

    // home B list + export：规范比较（计数 + 逐条业务字段；memoryId/revision/
    // createdAt/scope.projectId 导入侧必然重签，不参与比较）。
    let out = await_cli(
        home_b.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope_b.to_string(),
        ],
    );
    let listed = expect_ok("memory.list", &out);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 2);
    let out = await_cli(
        home_b.path(),
        &[
            "memory",
            "export",
            "--output",
            "json",
            "--scope",
            &scope_b.to_string(),
            "--request-key",
            "j5-iso-b-export",
        ],
    );
    let receipt = expect_ok("memory.export", &out);
    let operation = await_operation(home_b.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let export_b_artifact = operation["result"]["artifact"]["artifactId"]
        .as_str()
        .unwrap()
        .to_owned();
    let exported_b = read_artifact_json(home_b.path(), &export_b_artifact).await;
    assert_eq!(
        exported_b["bundleFormatVersion"],
        json!("agentcpm-pd.memory-bundle.v1")
    );
    assert_eq!(
        exported_b["entries"].as_array().unwrap().len(),
        exported_a["entries"].as_array().unwrap().len(),
        "export counts must match across the isolated roots"
    );
    assert_eq!(
        bundle_index(&exported_a),
        bundle_index(&exported_b),
        "exported bundles must be canonically equal (name -> kind/description/fields)"
    );

    // home A 两段式 clear：预览 → 清除。
    let out = await_cli(
        root,
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
        root,
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
            "j5-iso-clear",
        ],
    );
    let receipt = expect_ok("memory.clear", &out);
    let operation = await_operation(root, receipt["operationId"].as_str().unwrap()).await;
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
        "recovery material must be preserved"
    );

    // clear 后：记忆计数=0，而项目/会话/工作区仍在。
    let out = await_cli(
        root,
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

    let out = await_cli(
        root,
        &[
            "projects",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let project = expect_ok("project.get", &out);
    assert_eq!(
        project["project"]["projectId"].as_str(),
        Some(project_id.as_str())
    );
    let out = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    let session = expect_ok("session.get", &out);
    assert_eq!(
        session["session"]["sessionId"].as_str(),
        Some(session_id.as_str())
    );
    let out = await_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let history = expect_ok("history.read", &out);
    assert!(
        history["items"].as_array().is_some(),
        "history stays readable"
    );
    assert!(
        workspace.is_dir(),
        "clearing memory must not delete workspace source files"
    );

    // 跨根隔离：home B 的记忆不受 home A clear 影响。
    let out = await_cli(
        home_b.path(),
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope_b.to_string(),
        ],
    );
    let listed = expect_ok("memory.list", &out);
    assert_eq!(
        listed["entries"].as_array().unwrap().len(),
        2,
        "the isolated root's memory must survive the source root's clear"
    );

    // 故障负例：consumed plan 的一次性（同 plan 新 key → NOT_FOUND）。
    let out = await_cli(
        root,
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
            "j5-iso-clear-again",
        ],
    );
    assert_eq!(expect_failure("memory.clear", &out), "NOT_FOUND");
}
