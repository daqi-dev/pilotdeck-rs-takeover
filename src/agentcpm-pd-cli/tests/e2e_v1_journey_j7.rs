//! S5 业务旅程 J7（扩展生态）端到端验收。
//!
//! 旅程正本是 `_tmp/20261119-s5/journeys-execution-plan.md` §3 J7：单一用户
//! 在一个实例上完成「MCP 服务器配置与状态面 → 技能登记/校验/扫描/两段式
//! 导入 → 钩子配置与真实触发回执 → 插件安装/配置/启用/调用/卸载 → 工具
//! 目录与受控测试 → 自定义命令发现/加载/执行」的扩展生态闭环。命令名按
//! 计划 §2 映射表改写为实际 CLI 形态（`mcp set-permission-mode`、
//! `skills plan-import/apply-import`、`hooks get-invocation/submit-result`
//! 等连字符形态）。
//!
//! 两条场景（每条一个 `#[tokio::test]`，全部驱动真实 `agentcpm-pd` 二进制、
//! 隔离 `AGENTCPM_PD_HOME`、`--output json` envelope 经嵌入 registry 的
//! schema 校验）：
//!
//! * 正向旅程（`journey_j7_extension_ecosystem`）：
//!   1. mcp：validate（正反例）→ configure（global + project 覆盖）→ list
//!      （继承视图、不假报在线）→ get（敏感 header 脱敏）→ transition
//!      enable/disable（generation CAS）→ set-session-servers +
//!      set-permission-mode → remove（只删本层）；
//!   2. skills：validate → put/read/list → scan → plan-import → apply-import
//!      （原子发布 + 幂等重放）→ 导入内容与源一致；
//!   3. hooks：configure → list → `[pd-driver:fire-hook=...]` 真实触发 →
//!      get-invocation → submit-result → run 复活 + 调用记录与回执对账；
//!   4. plugins：install（本地源，安装落盘断言 `v1/plugins/<layer>/<id>`）
//!      → configure（秘密只回掩码）→ transition enable → invoke（受理后
//!      诚实失败）→ remove（清理边界、重装配置恢复）；
//!   5. tools：list（分页不漏重 + 可用性原因）→ describe（schema 钉版）→
//!      test（固定工具闭环 + 幂等重放）；
//!   6. commands：list（project 胜出）→ read（正文加载 + 越界拒绝）→
//!      execute（结果可追到实际 run）。
//!
//! * 故障负例（`journey_j7_fault_forged_hook_token_corrupt_import_dead_url`）：
//!   伪造 hook 回执令牌 → PERMISSION_DENIED 且调用记录与 run 不受扰（仍可
//!   用真令牌恢复）；导入源在计划后损坏 → INTEGRITY_ERROR 且已导入技能不
//!   受串扰；URL 插件源（无抓取桥）→ UPSTREAM_ERROR 且零行零落盘。
//!
//! 与计划的已核差异（详见 `_tmp/20261119-s5/w2a-report.md` 覆盖缺口登记）：
//! * `mcp transition connect` 在本构建无 MCP 运行时装配，诚实失败
//!   `UNAVAILABLE`（mcp.rs:1372）——「mcp.list 连接状态真连过桩」不可达，
//!   改按合同断言不假报在线（`MCP_RUNTIME_NOT_ASSEMBLED`）+ connect 的
//!   明确失败；
//! * 钩子触发走离线驱动的确定性触发桥 `[pd-driver:fire-hook=<id>]`
//!   （hooks 域的联调点），不是任意外部可执行脚本；
//! * `plugins invoke` 受理后同样因无执行桥诚实失败
//!   `PLUGIN_EXECUTION_UNAVAILABLE`。
//!
//! 本地未跑（2026-11-18 起本机构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

const MCP_SCHEMA_ID: &str = "agentcpm-pd.mcp.server/1";
const PLUGIN_ID: &str = "j7-plugin";

// ---------------------------------------------------------------------------
// 挂具：失败断言、api invoke、异步操作/run 轮询、工作区播种
// ---------------------------------------------------------------------------

/// 失败断言：非零退出码 + `outcome=failed` + `error.code`。
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must not exit 0: {}", out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|error| panic!("`{method}`: {error}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` envelope: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// `api invoke <methodId> --input-json -`（stdin 通道；expectedInstalledVersion
/// 等需要显式 JSON null 的输入走这里）。
async fn invoke(root: &Path, method: &str, input: Value, key: &str) -> Value {
    let out = cli_with_stdin(
        root,
        &[
            "api",
            "invoke",
            method,
            "--input-json",
            "-",
            "--request-key",
            key,
            "--output",
            "json",
        ],
        Some(&input.to_string()),
    )
    .await;
    expect_ok(method, &out)
}

/// 播种一个带真实授权根的项目与会话（skills scan / plugins / hooks /
/// commands 的锚），返回 (home, workspace, project_id, session_id)。
async fn seed_workspace(tag: &str) -> (TestHome, PathBuf, String, String) {
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
            "J7 E2E",
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

fn project_scope(project_id: &str) -> String {
    json!({"kind": "project", "id": project_id}).to_string()
}

fn session_scope(session_id: &str) -> String {
    json!({"kind": "session", "id": session_id}).to_string()
}

fn global_scope() -> String {
    json!({"kind": "global"}).to_string()
}

/// 轮询 `operations get` 至终态并返回 operation 投影。
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

/// 轮询 run.get 直到终态，返回 run JSON（failed run 也走 succeeded 信封）。
async fn wait_run_terminal(home: &Path, run_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = await_cli(
            home,
            &["run", "get", "--output", "json", "--run-id", run_id],
        );
        let page = expect_ok("run.get", &out);
        if page["run"]["terminal"] == true {
            return page["run"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "run `{run_id}` never became terminal"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 轮询 run.events 直到出现 `hook.invoked` 事件，返回其 payload（含
/// invocationId 与 completionTokenRef——执行者的令牌发现路径）。
async fn wait_hook_invoked(home: &Path, run_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = await_cli(
            home,
            &[
                "run",
                "events",
                "--output",
                "json",
                "--run-id",
                run_id,
                "--page",
                "{\"limit\":500}",
            ],
        );
        let page = expect_ok("run.events", &out);
        let events = page["frame"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(event) = events.iter().find(|event| event["type"] == "hook.invoked") {
            return event["payload"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "run `{run_id}` never emitted hook.invoked"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
}

/// 在工作区内播种一个插件源目录（清单声明 analyze 操作、结果 Schema、
/// 配置 Schema 与密钥字段）。
fn seed_plugin_source(workspace: &Path, name: &str) -> PathBuf {
    let dir = workspace.join("sources").join(name);
    std::fs::create_dir_all(&dir).expect("create plugin source");
    let manifest = json!({
        "name": name,
        "displayName": "J7 Plugin",
        "entry": "index.js",
        "version": "1.0.0",
        "configurationSchema": {
            "type": "object",
            "properties": {
                "endpoint": {"type": "string"},
                "apiKey": {"type": "string"}
            },
            "required": ["endpoint"],
            "additionalProperties": false
        },
        "operations": [{
            "operationId": "analyze",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            },
            "resultSchema": {
                "type": "object",
                "properties": {"verdict": {"type": "string"}},
                "required": ["verdict"],
                "additionalProperties": false
            },
            "permission": "plugins.invoke:analyze"
        }]
    });
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("write manifest");
    std::fs::write(dir.join("index.js"), "// j7 plugin entry\n").expect("write entry");
    dir
}

/// 安装一棵技能源目录（alpha/beta，授权根内）。
fn seed_skill_source(workspace: &Path) -> PathBuf {
    let source = workspace.join("skill-src");
    std::fs::create_dir_all(source.join("alpha")).unwrap();
    std::fs::create_dir_all(source.join("beta")).unwrap();
    std::fs::write(
        source.join("alpha").join("SKILL.md"),
        skill_md("alpha", "The alpha skill for the J7 journey."),
    )
    .unwrap();
    std::fs::write(
        source.join("beta").join("SKILL.md"),
        skill_md("beta", "The beta skill for the J7 journey."),
    )
    .unwrap();
    std::fs::write(source.join("beta").join("extra.md"), "beta extra").unwrap();
    source
}

/// 旅程 J7 正向主干：见文件头步骤清单。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j7_extension_ecosystem() {
    let (home, workspace, project_id, session_id) = seed_workspace("j7-main").await;
    let root = home.path();

    // ------------------------------------------------------------------
    // 1. mcp：配置面 + 状态机 + 会话集合 + 诚实连接语义。
    // ------------------------------------------------------------------
    // validate：非法字段（任意 shell）→ 诊断；合法 stdio → valid。
    let out = await_cli(
        root,
        &[
            "mcp",
            "validate",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--schema-id",
            MCP_SCHEMA_ID,
            "--configuration",
            &json!({
                "name": "evil",
                "transport": "stdio",
                "command": "sh",
                "shell": "/bin/bash -c",
            })
            .to_string(),
        ],
    );
    let invalid = expect_ok("mcp.validate", &out);
    assert_eq!(invalid["valid"], json!(false));

    let stdio_server = json!({
        "name": "j7-mcp",
        "transport": "stdio",
        "command": "/usr/bin/env",
        "args": ["true"],
    });
    let out = await_cli(
        root,
        &[
            "mcp",
            "validate",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--schema-id",
            MCP_SCHEMA_ID,
            "--configuration",
            &stdio_server.to_string(),
        ],
    );
    let valid = expect_ok("mcp.validate", &out);
    assert_eq!(valid["valid"], json!(true));

    // configure：global 一台 + project 覆盖同名并追加一台（带敏感 header）。
    let out = await_cli(
        root,
        &[
            "mcp",
            "configure",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--schema-id",
            MCP_SCHEMA_ID,
            "--servers",
            &json!([stdio_server]).to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    let configured = expect_ok("mcp.configure", &out);
    assert_eq!(configured["applied"], json!(false), "保存与运行生效分离");

    let out = await_cli(
        root,
        &[
            "mcp",
            "configure",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--schema-id",
            MCP_SCHEMA_ID,
            "--servers",
            &json!([stdio_server, {
                "name": "j7-proj",
                "transport": "http",
                "url": "https://mcp.example.test/rpc",
                "headers": {"Authorization": "Bearer j7-secret-do-not-leak"},
            }])
            .to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    let project_configured = expect_ok("mcp.configure", &out);
    let project_mcp_revision = project_configured["revision"].as_str().unwrap().to_owned();

    // list：项目视图 = 覆盖 + 追加；运行面不假报在线。
    let out = await_cli(
        root,
        &[
            "mcp",
            "list",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
        ],
    );
    let listed = expect_ok("mcp.list", &out);
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2, "项目视图覆盖 j7-mcp + 追加 j7-proj");
    for item in &items {
        assert_eq!(
            item["runtime"]["available"],
            json!(false),
            "配置存在不宣称在线: {item}"
        );
    }

    // get：敏感 header 脱敏（凭据不泄漏）。
    let out = await_cli(
        root,
        &[
            "mcp",
            "get",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--server-id",
            "j7-proj",
        ],
    );
    let got = expect_ok("mcp.get", &out);
    assert_eq!(got["schemaId"], json!(MCP_SCHEMA_ID));
    assert_eq!(
        got["configuration"]["headers"]["Authorization"],
        json!("<redacted>"),
        "敏感 header 值脱敏"
    );
    assert!(!got.to_string().contains("j7-secret-do-not-leak"));

    // transition enable（generation CAS）→ 状态可见；connect 诚实失败。
    let out = await_cli(
        root,
        &[
            "mcp",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
        ],
    );
    let global_listed = expect_ok("mcp.list", &out);
    let generation = global_listed["items"][0]["runtime"]["generation"]
        .as_str()
        .expect("generation")
        .to_owned();
    let out = await_cli(
        root,
        &[
            "mcp",
            "transition",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--server-id",
            "j7-mcp",
            "--action",
            "enable",
            "--expected-generation",
            &generation,
            "--request-key",
            "j7-mcp-enable",
        ],
    );
    let receipt = expect_ok("mcp.transition", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(operation["result"]["state"], json!("idle"));
    let generation_after_enable = operation["result"]["generation"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(generation_after_enable, generation);

    // 启用后的诚实在线语义：enabled 但未装配 → 明确原因，绝不假报连接。
    let out = await_cli(
        root,
        &[
            "mcp",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
        ],
    );
    let global_listed = expect_ok("mcp.list", &out);
    assert_eq!(global_listed["items"][0]["enabled"], json!(true));
    assert_eq!(
        global_listed["items"][0]["runtime"]["reasonCode"],
        json!("MCP_RUNTIME_NOT_ASSEMBLED"),
        "未装配的运行时必须给出明确原因: {global_listed}"
    );

    // connect：本构建无运行时装配 → 受理后诚实失败（不伪装握手）。
    let out = await_cli(
        root,
        &[
            "mcp",
            "transition",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--server-id",
            "j7-mcp",
            "--action",
            "connect",
            "--expected-generation",
            &generation_after_enable,
            "--request-key",
            "j7-mcp-connect",
        ],
    );
    let receipt = expect_ok("mcp.transition", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("failed"), "{operation}");
    assert_eq!(operation["result"]["code"], json!("UNAVAILABLE"));

    // 会话集合 + 权限模式（共用 revision CAS）。
    let out = await_cli(
        root,
        &[
            "mcp",
            "set-session-servers",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--server-ids",
            &json!(["j7-mcp"]).to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    let set_servers = expect_ok("mcp.set_session_servers", &out);
    let session_mcp_revision = set_servers["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "mcp",
            "list",
            "--output",
            "json",
            "--scope",
            &session_scope(&session_id),
        ],
    );
    let session_listed = expect_ok("mcp.list", &out);
    // 会话视图（includeInherited 默认 true）继承整条配置链：global 的
    // j7-mcp 被项目层同名覆盖、j7-proj 追加，两台都可见。会话启用集合
    // （set-session-servers）是运行面选择，不过滤 mcp.list 的配置枚举
    // （合同 acceptance「明确全局继承和项目覆盖」；e2e_v1_extensions.rs
    // 的会话视图断言同口径）。
    let session_server_ids: Vec<Value> = session_listed["items"]
        .as_array()
        .map(|items| items.iter().map(|item| item["serverId"].clone()).collect())
        .unwrap_or_default();
    assert_eq!(
        session_server_ids,
        vec![json!("j7-mcp"), json!("j7-proj")],
        "会话视图继承项目层覆盖与追加: {session_listed}"
    );
    let out = await_cli(
        root,
        &[
            "mcp",
            "set-permission-mode",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--mode",
            "default",
            "--expected-revision",
            &session_mcp_revision,
        ],
    );
    expect_ok("mcp.set_permission_mode", &out);

    // remove：只删本层（j7-proj），覆盖的 j7-mcp 仍可见。
    let out = await_cli(
        root,
        &[
            "mcp",
            "remove",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--server-id",
            "j7-proj",
            "--expected-revision",
            &project_mcp_revision,
        ],
    );
    let removed = expect_ok("mcp.remove", &out);
    assert_eq!(removed["retainedBindings"], json!([]));
    let out = await_cli(
        root,
        &[
            "mcp",
            "list",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
        ],
    );
    let listed = expect_ok("mcp.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["items"][0]["serverId"], json!("j7-mcp"));

    // ------------------------------------------------------------------
    // 2. skills：登记/校验/扫描/两段式导入。
    // ------------------------------------------------------------------
    let put_files = json!([
        {"relativePath": "SKILL.md", "content": skill_md("demo", "A skill for the J7 journey.")},
        {"relativePath": "notes/usage.md", "content": "usage notes"},
    ]);
    let out = await_cli(
        root,
        &[
            "skills",
            "put",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--skill-id",
            "demo",
            "--files",
            &put_files.to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    let put = expect_ok("skills.put", &out);
    assert_eq!(put["skillId"], json!("demo"));
    assert_eq!(put["applied"], json!(false), "保存不宣称已装载");
    let out = await_cli(
        root,
        &[
            "skills",
            "read",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--skill-id",
            "demo",
            "--relative-path",
            "notes/usage.md",
        ],
    );
    let read = expect_ok("skills.read", &out);
    assert_eq!(read["content"], json!("usage notes"));
    let out = await_cli(
        root,
        &[
            "skills",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
        ],
    );
    let listed = expect_ok("skills.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));

    // scan：授权根内发现候选。
    let source = seed_skill_source(&workspace);
    let out = await_cli(
        root,
        &[
            "skills",
            "scan",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--source-root",
            &source.display().to_string(),
        ],
    );
    let scanned = expect_ok("skills.scan", &out);
    let candidates = scanned["candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // 扫描面按实现语义：扫描根本身也作为一个候选上报（无 SKILL.md →
    // NO_SKILL_MD invalid），子技能 alpha/beta 合法——三个候选、两个 valid。
    assert_eq!(candidates.len(), 3, "根 + alpha + beta 候选: {scanned}");
    let valid: Vec<&str> = candidates
        .iter()
        .filter(|c| c["valid"] == json!(true))
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(valid, ["alpha", "beta"], "alpha + beta 被发现: {scanned}");
    assert!(
        candidates
            .iter()
            .any(|c| c["name"] == json!("skill-src") && c["valid"] == json!(false)),
        "the scan root reports itself as an invalid candidate: {scanned}"
    );

    // plan-import → apply-import：原子发布 + 幂等重放。
    let out = await_cli(
        root,
        &[
            "skills",
            "plan-import",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--origin",
            "directory",
            "--source-ref",
            &source.display().to_string(),
        ],
    );
    let plan = expect_ok("skills.plan_import", &out);
    let plan_id = plan["planId"].as_str().unwrap().to_owned();
    let plan_hash = plan["planHash"].as_str().unwrap().to_owned();
    let expected_revision = plan["expectedRevision"].as_str().unwrap().to_owned();
    assert_eq!(
        plan["changes"].as_array().map(Vec::len),
        Some(2),
        "两条导入变更: {plan}"
    );
    let out = await_cli(
        root,
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--expected-revision",
            &expected_revision,
            "--request-key",
            "j7-skills-apply",
        ],
    );
    let receipt = expect_ok("skills.apply_import", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    let import_result = operation["result"].clone();
    assert_eq!(
        import_result["importedSkillIds"],
        json!(["alpha", "beta"]),
        "两技能原子入位: {import_result}"
    );
    // 同 key 同输入重放 → deduplicated。
    let out = await_cli(
        root,
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--expected-revision",
            &expected_revision,
            "--request-key",
            "j7-skills-apply",
        ],
    );
    let replay = expect_ok("skills.apply_import", &out);
    assert_eq!(replay["deduplicated"], json!(true));
    // 导入内容与源一致。
    let out = await_cli(
        root,
        &[
            "skills",
            "read",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--skill-id",
            "alpha",
        ],
    );
    let read = expect_ok("skills.read", &out);
    assert_eq!(
        read["content"].as_str().unwrap(),
        skill_md("alpha", "The alpha skill for the J7 journey.")
    );

    // ------------------------------------------------------------------
    // 3. hooks：配置 → 真实触发 → 轮询 → 提交 → run 复活 → 对账。
    // ------------------------------------------------------------------
    let plugin_source = seed_plugin_source(&workspace, PLUGIN_ID);
    let plugin_source_ref = plugin_source.display().to_string();
    let receipt = invoke(
        root,
        "plugins.install",
        json!({
            "scope": {"kind": "project", "id": project_id},
            "pluginId": PLUGIN_ID,
            "sourceRef": plugin_source_ref,
            "version": "1.0.0",
            "expectedInstalledVersion": Value::Null,
        }),
        "j7-plugin-install",
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(operation["result"]["pluginId"], json!(PLUGIN_ID));
    assert_eq!(operation["result"]["installed"], json!(true));
    assert_eq!(
        operation["result"]["enabled"],
        json!(false),
        "安装不自动启用"
    );

    // 安装落盘断言：v1/plugins/<layer-slug>/<plugin>/ 复制了源树。
    let plugins_root = root.join("v1").join("plugins");
    let layers: Vec<_> = std::fs::read_dir(&plugins_root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .collect();
    assert_eq!(layers.len(), 1, "恰好一个层目录: {layers:?}");
    let installed_dir = layers[0].path().join(PLUGIN_ID);
    assert!(installed_dir.join("manifest.json").is_file(), "清单落盘");
    assert!(installed_dir.join("index.js").is_file(), "入口落盘");
    assert_eq!(
        std::fs::read(installed_dir.join("manifest.json")).unwrap(),
        std::fs::read(plugin_source.join("manifest.json")).unwrap(),
        "安装副本与源清单逐字节一致"
    );

    // hooks configure（ownerId 必须是本层已安装插件）→ list 投影。
    let out = await_cli(
        root,
        &[
            "hooks",
            "configure",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--bindings",
            &json!([{
                "hookId": "on-stop",
                "trigger": "Stop",
                "ownerId": PLUGIN_ID,
                "operationId": "analyze",
                "enabled": true,
                "deadlineMs": 30000,
            }])
            .to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    let configured = expect_ok("hooks.configure", &out);
    let hooks_revision = configured["revision"].as_str().unwrap().to_owned();
    assert!(hooks_revision.starts_with("hrev-"));
    let out = await_cli(
        root,
        &[
            "hooks",
            "list",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
        ],
    );
    let listed = expect_ok("hooks.list", &out);
    assert_eq!(listed["revision"], json!(hooks_revision));
    let hooks = listed["hooks"].as_array().cloned().unwrap_or_default();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["hookId"], "on-stop");
    assert_eq!(hooks[0]["trigger"], "Stop");

    // 真实触发（离线驱动确定性触发桥）→ invocation 轮询。
    let hook_run = submit_run(
        root,
        &session_id,
        "j7 finish [pd-driver:fire-hook=on-stop]",
        "j7-hook-run",
    )
    .await;
    let hook_run_id = hook_run["runId"].as_str().unwrap().to_owned();
    let invoked = wait_hook_invoked(root, &hook_run_id).await;
    let invocation_id = invoked["invocationId"].as_str().unwrap().to_owned();
    let token = invoked["completionTokenRef"].as_str().unwrap().to_owned();
    assert!(token.starts_with("hkct-"));
    let schema_id = invoked["resultSchemaId"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "hooks",
            "get-invocation",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--invocation-id",
            &invocation_id,
        ],
    );
    let invocation = expect_ok("hooks.get_invocation", &out);
    assert_eq!(invocation["state"], "pending");
    assert_eq!(invocation["hookId"], "on-stop");
    assert_eq!(invocation["runId"], json!(hook_run_id));
    assert_eq!(invocation["resultSchemaId"], json!(schema_id));

    // 合法提交 → accepted；run 复活并携带 hook 结果投影。
    let out = await_cli(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\"}",
            "--request-key",
            "j7-hook-submit",
        ],
    );
    let submitted = expect_ok("hooks.submit_result", &out);
    assert_eq!(submitted["disposition"], "accepted");
    let receipt_id = submitted["receiptId"].as_str().unwrap().to_owned();
    let run = wait_run_terminal(root, &hook_run_id).await;
    assert_eq!(run["state"], "succeeded", "hook 结果复活 run");
    // 对账：invocation 反映 delivered + 回执；事件流有 run.progress 投影。
    let out = await_cli(
        root,
        &[
            "hooks",
            "get-invocation",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--invocation-id",
            &invocation_id,
        ],
    );
    let invocation = expect_ok("hooks.get_invocation", &out);
    assert_eq!(invocation["state"], "delivered");
    assert_eq!(invocation["receiptId"], json!(receipt_id));
    let out = await_cli(
        root,
        &[
            "run",
            "events",
            "--output",
            "json",
            "--run-id",
            &hook_run_id,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let page = expect_ok("run.events", &out);
    let hook_progress = page["frame"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|event| event["type"] == "run.progress" && event["payload"]["hookResult"].is_object())
        .expect("run.progress projection of the hook result");
    assert_eq!(
        hook_progress["payload"]["hookResult"]["result"]["verdict"],
        "allow"
    );

    // ------------------------------------------------------------------
    // 4. plugins：配置面 + transition + invoke + remove。
    // ------------------------------------------------------------------
    let configuration_schema_id = format!("agentcpm-pd.plugin.{PLUGIN_ID}.configuration/1");
    let out = await_cli(
        root,
        &[
            "plugins",
            "configure",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plugin-id",
            PLUGIN_ID,
            "--schema-id",
            &configuration_schema_id,
            "--configuration",
            "{\"endpoint\":\"https://api.example.test\",\"apiKey\":\"sk-j7-live\"}",
            "--expected-revision",
            "absent",
        ],
    );
    let configured = expect_ok("plugins.configure", &out);
    let plugin_config_revision = configured["revision"].as_str().unwrap().to_owned();
    assert!(plugin_config_revision.starts_with("pcfg-"));
    // 读取面脱敏。
    let out = await_cli(
        root,
        &[
            "plugins",
            "get",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plugin-id",
            PLUGIN_ID,
        ],
    );
    let got = expect_ok("plugins.get", &out);
    assert_eq!(got["configuration"]["apiKey"], json!("<redacted>"));
    assert_eq!(
        got["plugin"]["configurationRevision"],
        json!(plugin_config_revision)
    );

    // transition enable → generation；invoke 受理后诚实失败（无执行桥）。
    let out = await_cli(
        root,
        &[
            "plugins",
            "transition",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plugin-id",
            PLUGIN_ID,
            "--action",
            "enable",
            "--expected-generation",
            "absent",
            "--request-key",
            "j7-plugin-enable",
        ],
    );
    let receipt = expect_ok("plugins.transition", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(operation["result"]["enabled"], json!(true));

    let out = await_cli(
        root,
        &[
            "plugins",
            "invoke",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plugin-id",
            PLUGIN_ID,
            "--operation-id",
            "analyze",
            "--schema-id",
            &format!("agentcpm-pd.plugin.{PLUGIN_ID}.op.analyze.input/1"),
            "--arguments",
            "{\"text\":\"hello\"}",
            "--request-key",
            "j7-plugin-invoke",
        ],
    );
    let receipt = expect_ok("plugins.invoke", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("failed"), "{operation}");
    assert_eq!(
        operation["result"]["code"],
        json!("PLUGIN_EXECUTION_UNAVAILABLE"),
        "受理后诚实失败，不伪装执行"
    );

    // remove：清理边界（保留配置）；重装恢复配置（含秘密掩码回读）。
    let out = await_cli(
        root,
        &[
            "plugins",
            "get",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plugin-id",
            PLUGIN_ID,
        ],
    );
    let got = expect_ok("plugins.get", &out);
    let installed_version = got["plugin"]["version"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "plugins",
            "remove",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plugin-id",
            PLUGIN_ID,
            "--expected-version",
            &installed_version,
            "--request-key",
            "j7-plugin-remove",
        ],
    );
    let receipt = expect_ok("plugins.remove", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(operation["result"]["removed"], json!(true));
    assert_eq!(operation["result"]["configurationRetained"], json!(true));
    // 卸载清理落盘：安装目录消失，层目录仍在（配置行保留）。
    assert!(
        !layers[0].path().join(PLUGIN_ID).exists(),
        "卸载必须清掉安装目录"
    );
    let out = await_cli(
        root,
        &[
            "plugins",
            "list",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
        ],
    );
    let listed = expect_ok("plugins.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));

    let receipt = invoke(
        root,
        "plugins.install",
        json!({
            "scope": {"kind": "project", "id": project_id},
            "pluginId": PLUGIN_ID,
            "sourceRef": plugin_source_ref,
            "version": "1.0.0",
            "expectedInstalledVersion": Value::Null,
        }),
        "j7-plugin-reinstall",
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    let out = await_cli(
        root,
        &[
            "plugins",
            "get",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plugin-id",
            PLUGIN_ID,
        ],
    );
    let got = expect_ok("plugins.get", &out);
    assert_eq!(got["configuration"]["apiKey"], json!("<redacted>"));
    assert_eq!(
        got["plugin"]["configurationRevision"],
        json!(plugin_config_revision),
        "保留的配置跨卸载/重装存活"
    );

    // ------------------------------------------------------------------
    // 5. tools：目录 + describe + 受控测试。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
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
    let tool_ids: Vec<String> = listed["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["toolId"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert!(tool_ids.contains(&"read_file".to_owned()), "内建工具在册");
    let read_file = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["toolId"] == json!("read_file"))
        .unwrap();
    assert_eq!(read_file["availability"]["state"], json!("ready"));
    let web_search = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["toolId"] == json!("web_search"))
        .expect("web_search in catalog");
    assert_eq!(web_search["availability"]["state"], json!("unavailable"));
    assert_eq!(
        web_search["availability"]["reasonCode"],
        json!("SEARCH_PROVIDER_NOT_CONFIGURED"),
        "不可用工具必须带原因"
    );

    // 分页不漏重。
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
        let out = await_cli(root, &refs);
        let page = expect_ok("tools.list", &out);
        let page_ids: Vec<String> = page["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["toolId"].as_str().map(str::to_owned))
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
    assert_eq!(seen, tool_ids, "分页不漏不重");

    // describe：schema 钉版。
    let out = await_cli(
        root,
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

    // test：固定工具闭环 + 幂等重放。
    let out = await_cli(
        root,
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
            "j7-tools-time",
        ],
    );
    let receipt = expect_ok("tools.test", &out);
    assert_eq!(receipt["kind"], json!("operation"));
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    let text = serde_json::to_string(&operation["result"]).unwrap_or_default();
    assert!(
        text.contains("agentcpm-pd.tool.testresult/1"),
        "完成结果携带固定结果 schema: {text}"
    );
    let out = await_cli(
        root,
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
            "j7-tools-time",
        ],
    );
    let replay = expect_ok("tools.test", &out);
    assert_eq!(replay["deduplicated"], json!(true), "同 key 同参重放去重");

    // ------------------------------------------------------------------
    // 6. commands：发现（project 胜出）→ 加载 → 执行（追到实际 run）。
    // ------------------------------------------------------------------
    let project_commands = workspace.join(".pilotdeck").join("commands");
    std::fs::create_dir_all(&project_commands).unwrap();
    std::fs::write(
        project_commands.join("greet.md"),
        "---\ndescription: Greet the J7 target\n---\nHello $1! Args: [$ARGUMENTS]",
    )
    .unwrap();
    let user_commands = root.join("commands");
    std::fs::create_dir_all(&user_commands).unwrap();
    std::fs::write(
        user_commands.join("notes.md"),
        "---\ndescription: j7 scratch notes\n---\nnote: $ARGUMENTS",
    )
    .unwrap();

    let out = await_cli(
        root,
        &[
            "commands",
            "list",
            "--output",
            "json",
            "--scope",
            &session_scope(&session_id),
        ],
    );
    let listed = expect_ok("commands.list", &out);
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| item["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(names, vec!["/greet".to_owned(), "/notes".to_owned()]);
    let greet = items
        .iter()
        .find(|item| item["name"] == json!("/greet"))
        .unwrap();
    assert_eq!(greet["source"], json!("project"), "项目命令胜出去重");
    let greet_revision = greet["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "commands",
            "read",
            "--output",
            "json",
            "--scope",
            &session_scope(&session_id),
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
        root,
        &[
            "commands",
            "read",
            "--output",
            "json",
            "--scope",
            &session_scope(&session_id),
            "--command-id",
            "/etc/passwd",
        ],
    );
    assert_eq!(
        expect_fail("commands.read", &out),
        "PATH_OUTSIDE_SCOPE",
        "路径形命令 id 必须被拒"
    );

    let out = await_cli(
        root,
        &[
            "commands",
            "execute",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--command-id",
            "project:greet",
            "--expected-revision",
            &greet_revision,
            "--schema-id",
            "agentcpm-pd.command.input/1",
            "--arguments",
            r#"{"args":["world"]}"#,
            "--request-key",
            "j7-cmd-exec",
        ],
    );
    let receipt = expect_ok("commands.execute", &out);
    assert_eq!(receipt["kind"], json!("operation"));
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    let exec_run_id = operation["result"]["runId"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &exec_run_id],
    );
    let run = expect_ok("run.get", &out);
    assert_eq!(
        run["run"]["sessionId"],
        json!(session_id),
        "结果可追到实际 run"
    );

    // 落盘总断言。
    assert!(root.join("v1").join("state.sqlite").is_file());
}

/// 旅程 J7 故障负例：伪造 hook 回执令牌（PERMISSION_DENIED、零扰动、真令牌
/// 恢复）；导入源计划后损坏（INTEGRITY_ERROR、已导入技能不受串扰）；URL
/// 插件源受理后失败（UPSTREAM_ERROR、零行零落盘）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j7_fault_forged_hook_token_corrupt_import_dead_url() {
    let (home, workspace, project_id, session_id) = seed_workspace("j7-fault").await;
    let root = home.path();

    // 装置：插件 + 钩子（触发路径的前置）。
    let plugin_source = seed_plugin_source(&workspace, "j7f-plugin");
    let receipt = invoke(
        root,
        "plugins.install",
        json!({
            "scope": {"kind": "project", "id": project_id},
            "pluginId": "j7f-plugin",
            "sourceRef": plugin_source.display().to_string(),
            "version": "1.0.0",
            "expectedInstalledVersion": Value::Null,
        }),
        "j7f-plugin-install",
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");

    let out = await_cli(
        root,
        &[
            "hooks",
            "configure",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--bindings",
            &json!([{
                "hookId": "on-stop",
                "trigger": "Stop",
                "ownerId": "j7f-plugin",
                "operationId": "analyze",
                "enabled": true,
                "deadlineMs": 30000,
            }])
            .to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    expect_ok("hooks.configure", &out);

    // ------------------------------------------------------------------
    // 故障 A：伪造回执令牌 → PERMISSION_DENIED，调用记录与 run 零扰动，
    // 真令牌恢复。
    // ------------------------------------------------------------------
    let hook_run = submit_run(
        root,
        &session_id,
        "[pd-driver:fire-hook=on-stop]",
        "j7f-hook-run",
    )
    .await;
    let hook_run_id = hook_run["runId"].as_str().unwrap().to_owned();
    let invoked = wait_hook_invoked(root, &hook_run_id).await;
    let invocation_id = invoked["invocationId"].as_str().unwrap().to_owned();
    let real_token = invoked["completionTokenRef"].as_str().unwrap().to_owned();
    let schema_id = invoked["resultSchemaId"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            "hkct-forged",
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\"}",
            "--request-key",
            "j7f-forged",
        ],
    );
    assert_eq!(
        expect_fail("hooks.submit_result", &out),
        "PERMISSION_DENIED",
        "伪造令牌必须被拒"
    );
    // 零扰动：invocation 仍 pending、无回执；run 仍在等待（未被伪造回执复活）。
    let out = await_cli(
        root,
        &[
            "hooks",
            "get-invocation",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--invocation-id",
            &invocation_id,
        ],
    );
    let invocation = expect_ok("hooks.get_invocation", &out);
    assert_eq!(invocation["state"], "pending");
    assert_eq!(invocation["receiptId"], Value::Null);
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &hook_run_id],
    );
    let run = expect_ok("run.get", &out);
    assert_eq!(
        run["run"]["state"], "waiting",
        "伪造回执不得复活 run: {run}"
    );

    // 恢复：真令牌提交 → run 复活。
    let out = await_cli(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &real_token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\"}",
            "--request-key",
            "j7f-real",
        ],
    );
    let submitted = expect_ok("hooks.submit_result", &out);
    assert_eq!(submitted["disposition"], "accepted");
    let run = wait_run_terminal(root, &hook_run_id).await;
    assert_eq!(run["state"], "succeeded");

    // ------------------------------------------------------------------
    // 故障 B：导入源在计划后损坏 → INTEGRITY_ERROR，已登记技能不受串扰。
    // ------------------------------------------------------------------
    // 先登记一个既有技能（后续验证它不受失败导入的串扰）。
    let out = await_cli(
        root,
        &[
            "skills",
            "put",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--skill-id",
            "demo-fault",
            "--files",
            &json!([
                {"relativePath": "SKILL.md", "content": skill_md("demo-fault", "Survives the corrupt import.")},
            ])
            .to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    expect_ok("skills.put", &out);

    let source = seed_skill_source(&workspace);
    let out = await_cli(
        root,
        &[
            "skills",
            "plan-import",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--origin",
            "directory",
            "--source-ref",
            &source.display().to_string(),
        ],
    );
    let plan = expect_ok("skills.plan_import", &out);
    // 计划后破坏源（模拟损坏下载）。
    std::fs::write(
        source.join("beta").join("SKILL.md"),
        skill_md("beta", "Mutated after the plan was created."),
    )
    .unwrap();
    let out = await_cli(
        root,
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
            "--plan-id",
            plan["planId"].as_str().unwrap(),
            "--plan-hash",
            plan["planHash"].as_str().unwrap(),
            "--expected-revision",
            plan["expectedRevision"].as_str().unwrap(),
            "--request-key",
            "j7f-skills-apply",
        ],
    );
    let receipt = expect_ok("skills.apply_import", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("failed"), "{operation}");
    assert_eq!(
        operation["result"]["code"],
        json!("INTEGRITY_ERROR"),
        "损坏源不得标装完成"
    );
    // 既有技能不受串扰（内容原样可读）。
    let out = await_cli(
        root,
        &[
            "skills",
            "read",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--skill-id",
            "demo-fault",
        ],
    );
    let read = expect_ok("skills.read", &out);
    assert_eq!(
        read["content"].as_str().unwrap(),
        skill_md("demo-fault", "Survives the corrupt import.")
    );

    // ------------------------------------------------------------------
    // 故障 C：URL 插件源（无抓取桥）→ 受理后 UPSTREAM_ERROR，零行零落盘。
    // ------------------------------------------------------------------
    let receipt = invoke(
        root,
        "plugins.install",
        json!({
            "scope": {"kind": "project", "id": project_id},
            "pluginId": "url-plugin",
            "sourceRef": "https://example.com/no-bridge.git",
            "version": "0.1.0",
            "expectedInstalledVersion": Value::Null,
        }),
        "j7f-url-install",
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("failed"), "{operation}");
    assert_eq!(operation["result"]["code"], json!("UPSTREAM_ERROR"));

    // 半安装不公告：清单无行、安装树无该插件目录。
    let out = await_cli(
        root,
        &[
            "plugins",
            "list",
            "--output",
            "json",
            "--scope",
            &project_scope(&project_id),
        ],
    );
    let listed = expect_ok("plugins.list", &out);
    let ids: Vec<&str> = listed["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i["pluginId"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !ids.contains(&"url-plugin"),
        "失败的 URL 安装不得留下行: {ids:?}"
    );
    let plugins_root = root.join("v1").join("plugins");
    let installed: Vec<_> = std::fs::read_dir(&plugins_root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .flat_map(|layer| {
            std::fs::read_dir(layer.path())
                .unwrap()
                .flatten()
                .filter(|p| p.path().is_dir())
                .collect::<Vec<_>>()
        })
        .collect();
    for entry in &installed {
        assert_ne!(
            entry.file_name().to_string_lossy(),
            "url-plugin",
            "失败的 URL 安装不得落盘: {}",
            entry.path().display()
        );
    }

    assert!(root.join("v1").join("state.sqlite").is_file());
}
