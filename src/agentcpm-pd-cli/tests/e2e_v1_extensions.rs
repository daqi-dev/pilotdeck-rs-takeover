//! Extensions (MCP + skills) end-to-end suite (extensions-management，S3；
//! capability a030/a031；cases c027/c028)。
//!
//! Scripts the acceptance anchors of the 16 `mcp.*` / `skills.*` operations:
//! 配置与实际连接一致（schema 校验 + 继承 + 不假报在线）、过期写冲突
//! （configure/put 的 expectedRevision CAS + 会话配置 revision）、凭据不
//! 泄漏（脱敏读回 + credentialRef 诊断）、失败不假报可用（registry 源、
//! connect 握手、损坏源 hash）、两段式导入（plan → apply + PLAN_STALE +
//! INTEGRITY_ERROR + 幂等受理）。
//!
//! Mock 边界（无真实 MCP server 依赖）：全部走配置面与本地目录——stdio
//! 命令只是配置字符串（从不 spawn）；导入源是本地临时目录。transition 的
//! connect/reconnect 按 v1 裁决诚实失败（无运行时装配）。
//!
//! 本地未跑（构建冻结令）：按简报 §4，本机不起 cargo test 构建；本文件由
//! 集成者在 cb8 跑（`cargo nextest run -p agentcpm-pd-cli --test
//! e2e_v1_extensions --features embedded-gateway`）。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

const MCP_SCHEMA_ID: &str = "agentcpm-pd.mcp.server/1";

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

fn global_scope() -> String {
    json!({"kind": "global"}).to_string()
}

fn stdio_server(name: &str) -> Value {
    json!({
        "name": name,
        "transport": "stdio",
        "command": "/usr/bin/env",
        "args": ["true"],
    })
}

/// 播种一个带真实授权根的项目（skills.scan/plan_import 的 project 锚），
/// 返回 (home, workspace, project_id, session_id)。
async fn seed_workspace(tag: &str) -> (TestHome, PathBuf, String, String) {
    let home = TestHome::new(tag);
    // `init_home` 是同步挂具（tools 套件同款）。
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
            "Extensions E2E",
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

/// 轮询 `operations get` 直到终态。
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

/// 配置一个服务器（helper：configure + expect_ok，返回结果）。
async fn mcp_configure(
    home: &Path,
    scope: &str,
    servers: &Value,
    expected_revision: &str,
) -> Value {
    let out = await_cli(
        home,
        &[
            "mcp",
            "configure",
            "--output",
            "json",
            "--scope",
            scope,
            "--schema-id",
            MCP_SCHEMA_ID,
            "--servers",
            &servers.to_string(),
            "--expected-revision",
            expected_revision,
        ],
    );
    expect_ok("mcp.configure", &out)
}

/// c027/a030 合同验收：schema 校验、CAS、继承视图、会话覆盖、transition
/// 状态机与诚实失败、脱敏与凭据引用诊断。
#[tokio::test]
async fn mcp_configuration_surface_and_cas() {
    let (home, _workspace, project_id, session_id) = seed_workspace("ext-mcp").await;
    let project_scope = json!({"kind": "project", "id": project_id}).to_string();
    let session_scope = json!({"kind": "session", "id": session_id}).to_string();

    // --- mcp.validate：非法字段（任意 shell）→ 诊断 ---
    let out = await_cli(
        home.path(),
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
    let result = expect_ok("mcp.validate", &out);
    assert_eq!(result["valid"], json!(false));

    // 未知 schemaId → INVALID_ARGUMENT。
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "validate",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--schema-id",
            "agentcpm-pd.mcp.server/2",
            "--configuration",
            &stdio_server("x").to_string(),
        ],
    );
    assert_eq!(expect_fail("mcp.validate", &out), "INVALID_ARGUMENT");

    // 凭据引用不解析 → CREDENTIAL_NOT_FOUND 诊断（秘密只接受 credentialRef）。
    let out = await_cli(
        home.path(),
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
                "name": "with-cred",
                "transport": "http",
                "url": "https://mcp.example.test/rpc",
                "credentialRef": "cred-missing",
            })
            .to_string(),
        ],
    );
    let result = expect_ok("mcp.validate", &out);
    assert_eq!(result["valid"], json!(false));
    assert!(
        result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == json!("CREDENTIAL_NOT_FOUND"))
    );

    // --- mcp.configure：创建（absent）→ CAS → 整组替换 ---
    let result = mcp_configure(
        home.path(),
        &global_scope(),
        &json!([stdio_server("shared")]),
        "absent",
    )
    .await;
    assert_eq!(result["applied"], json!(false), "保存与运行生效分离");
    assert_eq!(result["generation"], Value::Null);

    // 过期 revision → REVISION_CONFLICT（过期写冲突锚）。
    let out = await_cli(
        home.path(),
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
            &json!([stdio_server("other")]).to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(expect_fail("mcp.configure", &out), "REVISION_CONFLICT");

    // 项目层覆盖同名 + 追加。
    let result = mcp_configure(
        home.path(),
        &project_scope,
        &json!([stdio_server("shared"), {
            "name": "proj-only",
            "transport": "http",
            "url": "https://mcp.example.test/rpc",
            "headers": {"Authorization": "Bearer secret-value-do-not-leak"},
        }]),
        "absent",
    )
    .await;
    let project_rev = result["revision"].as_str().unwrap().to_owned();

    // --- mcp.list：继承 + 覆盖 + 不假报在线 ---
    let out = await_cli(
        home.path(),
        &["mcp", "list", "--output", "json", "--scope", &project_scope],
    );
    let listed = expect_ok("mcp.list", &out);
    let items = listed["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2, "project view covers shared + proj-only");
    for item in &items {
        assert_eq!(
            item["runtime"]["available"],
            json!(false),
            "配置存在不宣称在线: {item}"
        );
        assert!(item["runtime"]["generation"].as_str().is_some());
    }

    // --- mcp.get：脱敏读回（凭据不泄漏）+ effectiveSource ---
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "get",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--server-id",
            "proj-only",
        ],
    );
    let got = expect_ok("mcp.get", &out);
    assert_eq!(got["schemaId"], json!(MCP_SCHEMA_ID));
    assert_eq!(
        got["effectiveSource"],
        json!(format!("project:{project_id}"))
    );
    assert_eq!(
        got["configuration"]["headers"]["Authorization"],
        json!("<redacted>"),
        "敏感 header 值脱敏"
    );
    assert!(
        !serde_json::to_string(&got)
            .unwrap()
            .contains("secret-value")
    );
    assert_eq!(got["toolIds"], json!([]));

    // --- mcp.remove：只删本层 ---
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "remove",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--server-id",
            "proj-only",
            "--expected-revision",
            &project_rev,
        ],
    );
    let removed = expect_ok("mcp.remove", &out);
    assert_eq!(removed["retainedBindings"], json!([]));
    let out = await_cli(
        home.path(),
        &["mcp", "list", "--output", "json", "--scope", &project_scope],
    );
    let listed = expect_ok("mcp.list", &out);
    // shared 仍在（项目层自有配置，不受 remove 影响）。
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["serverId"], json!("shared"));

    // --- mcp.set_session_servers：会话集合 + CAS ---
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "set-session-servers",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--server-ids",
            &json!(["shared"]).to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    let result = expect_ok("mcp.set_session_servers", &out);
    let session_rev = result["revision"].as_str().unwrap().to_owned();

    // 会话视图（继承）：shared 可见。
    let out = await_cli(
        home.path(),
        &["mcp", "list", "--output", "json", "--scope", &session_scope],
    );
    let listed = expect_ok("mcp.list", &out);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["serverId"], json!("shared"));

    // 未知服务器 → NOT_FOUND。
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "set-session-servers",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--server-ids",
            &json!(["nope"]).to_string(),
            "--expected-revision",
            &session_rev,
        ],
    );
    assert_eq!(expect_fail("mcp.set_session_servers", &out), "NOT_FOUND");

    // --- mcp.set_permission_mode：mode 校验 + 共用 revision CAS ---
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "set-permission-mode",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--mode",
            "bogus",
            "--expected-revision",
            &session_rev,
        ],
    );
    assert_eq!(
        expect_fail("mcp.set_permission_mode", &out),
        "INVALID_ARGUMENT"
    );

    let out = await_cli(
        home.path(),
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
            &session_rev,
        ],
    );
    expect_ok("mcp.set_permission_mode", &out);
    // 旧 revision（被 permission mode 写占用）→ REVISION_CONFLICT。
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "set-session-servers",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--server-ids",
            &json!(["shared"]).to_string(),
            "--expected-revision",
            &session_rev,
        ],
    );
    assert_eq!(
        expect_fail("mcp.set_session_servers", &out),
        "REVISION_CONFLICT"
    );

    // --- mcp.transition：状态机 + generation CAS + 诚实 connect 失败 ---
    // transition 操作目标 scope 自有配置的服务器：shared 配在 global。
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
        ],
    );
    let listed = expect_ok("mcp.list", &out);
    let generation = listed["items"][0]["runtime"]["generation"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "transition",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--server-id",
            "shared",
            "--action",
            "disable",
            "--expected-generation",
            &generation,
            "--request-key",
            "ext-mcp-disable",
        ],
    );
    let receipt = expect_ok("mcp.transition", &out);
    let operation = wait_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    assert_eq!(operation["result"]["state"], json!("disabled"));
    let generation2 = operation["result"]["generation"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(generation2, generation);

    // 状态可见：list 里 shared 已禁用（运行资源排空状态可见）。
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
        ],
    );
    let listed = expect_ok("mcp.list", &out);
    assert_eq!(listed["items"][0]["enabled"], json!(false));
    assert_eq!(listed["items"][0]["runtime"]["state"], json!("disabled"));

    // 过期 generation → GENERATION_CONFLICT。
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "transition",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--server-id",
            "shared",
            "--action",
            "enable",
            "--expected-generation",
            &generation,
            "--request-key",
            "ext-mcp-stale-gen",
        ],
    );
    assert_eq!(expect_fail("mcp.transition", &out), "GENERATION_CONFLICT");

    // connect：无运行时 → 诚实失败（不假报可用）。
    let out = await_cli(
        home.path(),
        &[
            "mcp",
            "transition",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--server-id",
            "shared",
            "--action",
            "connect",
            "--expected-generation",
            &generation2,
            "--request-key",
            "ext-mcp-connect",
        ],
    );
    let receipt = expect_ok("mcp.transition", &out);
    let operation = wait_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("failed"));
    assert_eq!(operation["result"]["code"], json!("UNAVAILABLE"));
}

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
}

/// c028/a031 合同验收：put/read/remove CAS、结构校验、扫描发现、两段式
/// 导入（计划 → 原子应用 → 过期拒绝 → 幂等重放）。
#[tokio::test]
async fn skills_crud_validation_scan_and_two_phase_import() {
    let (home, workspace, project_id, _session_id) = seed_workspace("ext-skills").await;
    let project_scope = json!({"kind": "project", "id": project_id}).to_string();

    // --- skills.validate：files 形态的正反例 ---
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "validate",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--files",
            &json!([
                {"relativePath": "README.md", "content": "no skill md here"}
            ])
            .to_string(),
        ],
    );
    let result = expect_ok("skills.validate", &out);
    assert_eq!(result["valid"], json!(false));
    assert!(
        result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == json!("NO_SKILL_MD"))
    );

    // skillId 与 files 恰选其一。
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "validate",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--skill-id",
            "x",
            "--files",
            &json!([
                {"relativePath": "SKILL.md", "content": skill_md("x", "A skill for the e2e suite.")}
            ])
            .to_string(),
        ],
    );
    assert_eq!(expect_fail("skills.validate", &out), "INVALID_ARGUMENT");

    // --- skills.put：创建（absent）→ CAS → 读回一致 ---
    let put_files = json!([
        {"relativePath": "SKILL.md", "content": skill_md("demo", "A skill for the extensions e2e suite.")},
        {"relativePath": "notes/usage.md", "content": "usage notes"},
    ]);
    let out = await_cli(
        home.path(),
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
    let result = expect_ok("skills.put", &out);
    let _skill_rev = result["revision"].as_str().unwrap().to_owned();
    assert_eq!(result["skillId"], json!("demo"));
    assert_eq!(result["applied"], json!(false), "保存不宣称已装载");
    assert_eq!(result["generation"], Value::Null);

    // 重复创建 → REVISION_CONFLICT。
    let out = await_cli(
        home.path(),
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
    assert_eq!(expect_fail("skills.put", &out), "REVISION_CONFLICT");

    // --- skills.read：内容 + hash + 路径安全 ---
    let out = await_cli(
        home.path(),
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
    assert_eq!(read["skill"]["installed"], json!(true));
    assert_eq!(read["skill"]["available"], json!(true));
    assert_eq!(read["skill"]["origin"], json!("global"));

    let out = await_cli(
        home.path(),
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
            "../escape",
        ],
    );
    assert_eq!(expect_fail("skills.read", &out), "PATH_OUTSIDE_SCOPE");

    // --- skills.list：scope 视图 + registry 诚实失败 ---
    let out = await_cli(
        home.path(),
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
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);

    let out = await_cli(
        home.path(),
        &[
            "skills",
            "list",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--source",
            "registry",
        ],
    );
    assert_eq!(expect_fail("skills.list", &out), "UPSTREAM_ERROR");

    // --- skills.scan：授权根内发现 ---
    let source = workspace.join("import-src");
    std::fs::create_dir_all(source.join("alpha")).unwrap();
    std::fs::create_dir_all(source.join("beta")).unwrap();
    std::fs::write(
        source.join("alpha").join("SKILL.md"),
        skill_md("alpha", "The alpha skill imported from a directory."),
    )
    .unwrap();
    std::fs::write(
        source.join("beta").join("SKILL.md"),
        skill_md("beta", "The beta skill imported from a directory."),
    )
    .unwrap();
    std::fs::write(source.join("beta").join("extra.md"), "beta extra").unwrap();

    let out = await_cli(
        home.path(),
        &[
            "skills",
            "scan",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--source-root",
            &source.display().to_string(),
        ],
    );
    let result = expect_ok("skills.scan", &out);
    let candidates = result["candidates"].as_array().unwrap().clone();
    let alpha = candidates
        .iter()
        .find(|c| c["relativePath"] == json!("alpha"))
        .expect("alpha discovered");
    assert_eq!(alpha["valid"], json!(true));

    // 授权根外 → PATH_OUTSIDE_SCOPE。
    let outside = home.path().join("outside-src");
    std::fs::create_dir_all(&outside).unwrap();
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "scan",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--source-root",
            &outside.display().to_string(),
        ],
    );
    assert_eq!(expect_fail("skills.scan", &out), "PATH_OUTSIDE_SCOPE");

    // --- plan_import：skip 策略（demo 无冲突，alpha/beta 新入）---
    // 注意：目标 scope 是 project（源在工作区内），global 里已有 demo。
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "plan-import",
            "--output",
            "json",
            "--scope",
            &project_scope,
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
    let changes = plan["changes"].as_array().unwrap().clone();
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().all(|c| c["action"] == json!("import")));

    // --- apply_import：受理 → 原子发布 → 幂等重放 ---
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--expected-revision",
            &expected_revision,
            "--request-key",
            "ext-skills-apply-1",
        ],
    );
    let receipt = expect_ok("skills.apply_import", &out);
    let operation = wait_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let result = operation["result"].clone();
    assert_eq!(
        result["importedSkillIds"],
        json!(["alpha", "beta"]),
        "both skills imported"
    );
    assert_eq!(result["skipped"], json!([]));

    // 同 key 重放（同 input）→ deduplicated。
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--expected-revision",
            &expected_revision,
            "--request-key",
            "ext-skills-apply-1",
        ],
    );
    let replay = expect_ok("skills.apply_import", &out);
    assert_eq!(replay["deduplicated"], json!(true));

    // 计划一次性：另一 key → PLAN_STALE。
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--expected-revision",
            result["revision"].as_str().unwrap(),
            "--request-key",
            "ext-skills-apply-2",
        ],
    );
    assert_eq!(expect_fail("skills.apply_import", &out), "PLAN_STALE");

    // 伪 planHash → PLAN_STALE。
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--plan-id",
            &plan_id,
            "--plan-hash",
            "deadbeef",
            "--expected-revision",
            result["revision"].as_str().unwrap(),
            "--request-key",
            "ext-skills-apply-3",
        ],
    );
    assert_eq!(expect_fail("skills.apply_import", &out), "PLAN_STALE");

    // --- 导入结果与源规范可比（导出→导入锚）：read 回 alpha ---
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "read",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--skill-id",
            "alpha",
        ],
    );
    let read = expect_ok("skills.read", &out);
    assert_eq!(
        read["content"].as_str().unwrap(),
        skill_md("alpha", "The alpha skill imported from a directory.")
    );

    // --- 源变化 → INTEGRITY_ERROR（损坏下载不标 installed）---
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "plan-import",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--origin",
            "directory",
            "--source-ref",
            &source.display().to_string(),
            "--conflict-policy",
            "overwrite",
        ],
    );
    let plan = expect_ok("skills.plan_import", &out);
    std::fs::write(
        source.join("beta").join("SKILL.md"),
        skill_md("beta", "Mutated after the plan was created."),
    )
    .unwrap();
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "apply-import",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--plan-id",
            plan["planId"].as_str().unwrap(),
            "--plan-hash",
            plan["planHash"].as_str().unwrap(),
            "--expected-revision",
            plan["expectedRevision"].as_str().unwrap(),
            "--request-key",
            "ext-skills-apply-4",
        ],
    );
    let receipt = expect_ok("skills.apply_import", &out);
    let operation = wait_operation(home.path(), receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], json!("failed"));
    assert_eq!(operation["result"]["code"], json!("INTEGRITY_ERROR"));

    // --- skills.remove：CAS + 删除 ---
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "read",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--skill-id",
            "beta",
        ],
    );
    let read = expect_ok("skills.read", &out);
    let beta_rev = read["skill"]["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "remove",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--skill-id",
            "beta",
            "--expected-revision",
            &beta_rev,
        ],
    );
    expect_ok("skills.remove", &out);
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "read",
            "--output",
            "json",
            "--scope",
            &project_scope,
            "--skill-id",
            "beta",
        ],
    );
    assert_eq!(expect_fail("skills.read", &out), "NOT_FOUND");

    // --- 不支持的 origin 诚实失败 ---
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "plan-import",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--origin",
            "registry",
            "--source-ref",
            "https://registry.example/skills",
        ],
    );
    assert_eq!(expect_fail("skills.plan_import", &out), "UPSTREAM_ERROR");
    let out = await_cli(
        home.path(),
        &[
            "skills",
            "plan-import",
            "--output",
            "json",
            "--scope",
            &global_scope(),
            "--origin",
            "archive",
            "--source-ref",
            "/tmp/skills.zip",
        ],
    );
    assert_eq!(expect_fail("skills.plan_import", &out), "UNAVAILABLE");
}
