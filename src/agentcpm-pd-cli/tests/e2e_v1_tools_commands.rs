//! Tools + commands end-to-end suite (t017, capability a029).
//!
//! Scripts the acceptance anchors of `cli.case.contract.capability.a029` and
//! the per-operation cases (`cli.case.operation.tools.{list,describe,test}`,
//! `cli.case.operation.commands.{list,read,execute}`): catalog discovery with
//! per-scope policy reflection, schema-pinned describe, the controlled test
//! execution loop (pre-accept rejections + idempotent submit), command
//! discovery/loading with path-shaped rejection, and the session execution
//! that submits a real run with the prepared content.
//!
//! 本地未跑（构建冻结令）：按简报 §4，本机不起 cargo test 构建；本文件由
//! 集成者在 cb8 跑（`cargo nextest run -p agentcpm-pd-cli --test
//! e2e_v1_tools_commands --features embedded-gateway`）。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

/// 失败断言：非零退出码 + `outcome=failed` + `error.code`。
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must not exit 0: {}", out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0]).unwrap_or_else(|e| panic!("`{method}`: {e}"));
    assert_eq!(value["outcome"], "failed", "`{method}`: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// 播种一个带真实授权根的项目（t012 的 directories.create 声明面），
/// 返回 (home, workspace, project_id, instance_id)。
async fn seed_workspace(tag: &str) -> (TestHome, PathBuf, String, String) {
    let home = TestHome::new(tag);
    // `init_home` is sync in the current harness (the S2-proven form in
    // `e2e_v1_roundtrip.rs`; several sibling suites written under the build
    // freeze call it with `.await`, which does not compile against the sync
    // helper — flagged in the task report for the integrator).
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
            "T017 E2E",
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

async fn seed_session(home: &Path, project_id: &str, session_id: &str) {
    let out = await_cli(
        home,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--project-id",
            project_id,
        ],
    );
    expect_ok("session.create", &out);
}

fn global_scope() -> String {
    json!({"kind": "global"}).to_string()
}

/// Polls `operations get` until the operation is terminal.
async fn wait_operation(home: &Path, operation_id: &str) -> Value {
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
        let page = expect_ok("operations.get", &out);
        let state = page["operation"]["state"].as_str().unwrap_or_default();
        if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
        ) {
            return page["operation"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never became terminal (state `{state}`)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// c027/a029 合同验收：分页/搜索正确，目录按合同形态投影，不可用工具附
/// 原因；tools.test 的受控执行闭环（前置拒绝 + 幂等受理）。
#[tokio::test]
async fn tools_catalog_and_controlled_test_execution() {
    let (home, _workspace, _project_id, _instance_id) = seed_workspace("t017-tools").await;

    // --- tools.list: the catalog with schema-validated results ---
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
        ],
    );
    let listed = expect_ok("tools.list", &out);
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert!(items.len() >= 10, "the builtin families are listed");
    let find = |tool_id: &str| {
        items
            .iter()
            .find(|item| item["toolId"] == json!(tool_id))
            .unwrap_or_else(|| panic!("tool `{tool_id}` missing"))
            .clone()
    };
    let read_file = find("read_file");
    assert_eq!(read_file["ownerId"], json!("builtin"));
    assert_eq!(read_file["availability"]["state"], json!("ready"));
    let web_search = find("web_search");
    assert_eq!(web_search["availability"]["state"], json!("unavailable"));
    assert_eq!(
        web_search["availability"]["reasonCode"],
        json!("SEARCH_PROVIDER_NOT_CONFIGURED")
    );

    // 分页不漏重（a029 验收锚）。
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args = vec![
            "tools".to_owned(),
            "list".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--scope".to_owned(),
            global_scope(),
            "--limit".to_owned(),
            "4".to_owned(),
        ];
        if let Some(cursor) = &cursor {
            args.push("--cursor".to_owned());
            args.push(cursor.clone());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = await_cli(home.path(), &refs);
        let page = expect_ok("tools.list", &out);
        let page_ids: Vec<String> = page["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["toolId"].as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        assert!(page_ids.len() <= 4);
        seen.extend(page_ids);
        match page["nextCursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    let full_ids: Vec<String> = items
        .iter()
        .filter_map(|item| item["toolId"].as_str().map(ToOwned::to_owned))
        .collect();
    assert_eq!(
        seen, full_ids,
        "pagination loses nothing and repeats nothing"
    );

    // 搜索过滤：query 命中 id 与描述（多条目允许，见 tools 测试的精确单命中）。
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--query",
            "GET_CURRENT_TIME",
        ],
    );
    let queried = expect_ok("tools.list", &out);
    let queried_ids: Vec<String> = queried["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["toolId"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        queried_ids,
        vec!["get_current_time"],
        "only get_current_time mentions the query in id or description"
    );

    // --- tools.describe: schema 与版本声明 ---
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "describe",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--tool-id",
            "read_file",
        ],
    );
    let described = expect_ok("tools.describe", &out);
    assert_eq!(
        described["inputSchema"]["properties"]["file_path"]["type"],
        json!("string")
    );
    assert_eq!(described["retryPolicy"], json!("readOnly"));
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "describe",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--tool-id",
            "no-such-tool",
        ],
    );
    assert_eq!(expect_fail("tools.describe", &out), "NOT_FOUND");

    // --- tools.test: 前置拒绝（a029 验收锚） ---
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "test",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--tool-id",
            "get_current_time",
            "--schema-id",
            "agentcpm-pd.tool.get_current_time/2",
            "--arguments",
            "{}",
            "--request-key",
            "t017-stale-schema",
        ],
    );
    assert_eq!(expect_fail("tools.test", &out), "SCHEMA_MISMATCH");

    let out = await_cli(
        home.path(),
        &[
            "tools",
            "test",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--tool-id",
            "bash",
            "--schema-id",
            "agentcpm-pd.tool.bash/1",
            "--arguments",
            r#"{"command":"true"}"#,
            "--request-key",
            "t017-bash-effects",
        ],
    );
    assert_eq!(expect_fail("tools.test", &out), "EFFECT_APPROVAL_REQUIRED");

    // --- tools.test: 受控执行（固定工具闭环） ---
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "test",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--tool-id",
            "get_current_time",
            "--schema-id",
            "agentcpm-pd.tool.get_current_time/1",
            "--arguments",
            "{}",
            "--request-key",
            "t017-time-1",
        ],
    );
    let receipt = expect_ok("tools.test", &out);
    assert_eq!(receipt["kind"], json!("operation"));
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = wait_operation(home.path(), &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let text = serde_json::to_string(&operation["result"]).unwrap_or_default();
    assert!(
        text.contains("agentcpm-pd.tool.testresult/1"),
        "the completion carries the fixed result schema: {text}"
    );

    // 同 request-key 重放（受理恰好一次）；同 key 不同参数冲突。
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "test",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--tool-id",
            "get_current_time",
            "--schema-id",
            "agentcpm-pd.tool.get_current_time/1",
            "--arguments",
            "{}",
            "--request-key",
            "t017-time-1",
        ],
    );
    let replay = expect_ok("tools.test", &out);
    assert_eq!(replay["deduplicated"], json!(true));
    let out = await_cli(
        home.path(),
        &[
            "tools",
            "test",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--tool-id",
            "get_current_time",
            "--schema-id",
            "agentcpm-pd.tool.get_current_time/1",
            "--arguments",
            r#"{"timezone":"UTC"}"#,
            "--request-key",
            "t017-time-1",
        ],
    );
    assert_eq!(expect_fail("tools.test", &out), "IDEMPOTENCY_CONFLICT");
}

/// c027/a029 合同验收：命令发现/加载（越界拒绝）与执行（结果可追到
/// 实际 run；参数不经 shell；会话忙/版本过期的明确错误）。
#[tokio::test]
async fn commands_discovery_loading_and_execution() {
    let (home, workspace, project_id, _instance_id) = seed_workspace("t017-cmds").await;
    let session_id = "t017-sess";
    seed_session(home.path(), &project_id, session_id).await;

    // 项目命令与用户命令；同名时 project 胜出。
    let project_commands = workspace.join(".pilotdeck").join("commands");
    std::fs::create_dir_all(&project_commands).unwrap();
    std::fs::write(
        project_commands.join("greet.md"),
        "---\ndescription: Greet the target\n---\nHello $1! Args: [$ARGUMENTS]",
    )
    .unwrap();
    std::fs::write(project_commands.join("dup.md"), "project dup").unwrap();
    let user_commands = home.path().join("commands");
    std::fs::create_dir_all(&user_commands).unwrap();
    std::fs::write(
        user_commands.join("notes.md"),
        "---\ndescription: scratch notes\n---\nnote: $ARGUMENTS",
    )
    .unwrap();
    std::fs::write(user_commands.join("dup.md"), "user dup").unwrap();

    let session_scope = json!({"kind": "session", "id": session_id}).to_string();
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "list",
            "--output",
            "json",
            "--scope",
            &session_scope,
        ],
    );
    let listed = expect_ok("commands.list", &out);
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| item["name"].as_str().map(ToOwned::to_owned))
        .collect();
    assert_eq!(names, vec!["/dup", "/greet", "/notes"]);
    let dup = items
        .iter()
        .find(|item| item["name"] == json!("/dup"))
        .unwrap();
    assert_eq!(dup["source"], json!("project"), "project wins the dedupe");

    // 全局范围只看到用户命令。
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
        ],
    );
    let global = expect_ok("commands.list", &out);
    let global_names: Vec<String> = global["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["name"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(global_names, vec!["/dup", "/notes"]);

    // --- commands.read: 正文加载 + 越界拒绝（无执行副作用） ---
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "read",
            "--output",
            "json",
            "--scope",
            &session_scope,
            "--command-id",
            "project:greet",
        ],
    );
    let read = expect_ok("commands.read", &out);
    assert_eq!(read["content"], json!("Hello $1! Args: [$ARGUMENTS]"));
    assert_eq!(
        read["inputSchema"]["properties"]["args"]["type"],
        json!("array")
    );
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "read",
            "--output",
            "json",
            "--scope",
            &session_scope,
            "--command-id",
            "/etc/passwd",
        ],
    );
    assert_eq!(expect_fail("commands.read", &out), "PATH_OUTSIDE_SCOPE");

    // --- commands.execute: 结果可追到实际 run；参数原样落正文 ---
    let greet_revision = items
        .iter()
        .find(|item| item["name"] == json!("/greet"))
        .and_then(|item| item["revision"].as_str())
        .unwrap_or_default()
        .to_owned();
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "execute",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--command-id",
            "project:greet",
            "--expected-revision",
            &greet_revision,
            "--schema-id",
            "agentcpm-pd.command.input/1",
            "--arguments",
            r#"{"args":["world","extra"]}"#,
            "--request-key",
            "t017-exec-1",
        ],
    );
    let receipt = expect_ok("commands.execute", &out);
    assert_eq!(receipt["kind"], json!("operation"));
    let operation = wait_operation(
        home.path(),
        receipt["operationId"].as_str().unwrap_or_default(),
    )
    .await;
    assert_eq!(operation["state"], json!("succeeded"));
    let run_id = operation["result"]["runId"].as_str().unwrap().to_owned();

    // 执行结果追到实际 run：run 终态可见。
    let out = await_cli(
        home.path(),
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    let run = expect_ok("run.get", &out);
    assert_eq!(run["run"]["sessionId"], json!(session_id));

    // --- 前置明确错误：版本过期 ---
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "execute",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--command-id",
            "project:greet",
            "--expected-revision",
            "cmdrev-deadbeefdeadbeefdeadbeef",
            "--schema-id",
            "agentcpm-pd.command.input/1",
            "--arguments",
            r#"{"args":[]}"#,
            "--request-key",
            "t017-exec-stale",
        ],
    );
    assert_eq!(expect_fail("commands.execute", &out), "REVISION_CONFLICT");

    // --- 幂等：同 key 重放受理 ---
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "execute",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--command-id",
            "project:greet",
            "--expected-revision",
            &greet_revision,
            "--schema-id",
            "agentcpm-pd.command.input/1",
            "--arguments",
            r#"{"args":["world","extra"]}"#,
            "--request-key",
            "t017-exec-1",
        ],
    );
    let replay = expect_ok("commands.execute", &out);
    assert_eq!(replay["deduplicated"], json!(true));

    // --- 会话忙：在途 run 时拒绝，释放后同 key 可重试 ---
    let out = await_cli(
        home.path(),
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--request-key",
            "t017-hold-run",
            "--session-id",
            session_id,
            "--text",
            "[pd-driver:sleep=1200]",
        ],
    );
    expect_ok("run.submit", &out);
    let out = await_cli(
        home.path(),
        &[
            "commands",
            "execute",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--command-id",
            "project:greet",
            "--expected-revision",
            &greet_revision,
            "--schema-id",
            "agentcpm-pd.command.input/1",
            "--arguments",
            r#"{"args":["again"]}"#,
            "--request-key",
            "t017-exec-busy",
        ],
    );
    assert_eq!(expect_fail("commands.execute", &out), "SESSION_BUSY");
}
