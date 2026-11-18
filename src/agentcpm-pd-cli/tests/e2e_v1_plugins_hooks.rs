//! Plugins + hooks end-to-end suite (`cli.task.hooks`, S3).
//!
//! Scripts the task's acceptance anchors over the real `agentcpm-pd` binary
//! and the real `/v1/ws` transport (embedded gateway auto-start):
//!
//! * 安装/更新/卸载生命周期：install 的来源校验（本地路径锚定 + git URL
//!   形态）、`.tmp-` 暂存的「半安装不公告」、remove 的清理边界（默认保留
//!   配置、重装恢复）。
//! * 配置面：configure 的「改写后校验」（absent→revision、未知字段拒绝、
//!   掩码秘密回传保真、CAS 冲突）。
//! * 回执流：`[pd-driver:fire-hook=<hookId>]` 触发 → `hooks.get_invocation`
//!   轮询 → `hooks.submit_result` 提交 → run 复活；「过期回执不能复活
//!   run」与终态拒绝。
//!
//! 本地未跑（构建冻结令）：按简报 §4，本机不起 cargo test 构建；本文件由
//! 集成者在 cb8 跑（`cargo nextest run -p agentcpm-pd-cli --test
//! e2e_v1_plugins_hooks --features embedded-gateway`）。

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

async fn cli_json(root: &Path, args: &[&str]) -> Value {
    let out = await_cli(root, args);
    expect_ok(args[0], &out)
}

/// `api invoke`（null 友好路径：plugins.install 的 expectedInstalledVersion
/// 需要显式 JSON null，typed-options 表达不了）。
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

/// 同 [`invoke`]，但断言失败并返回 `error.code`。
async fn invoke_fail(root: &Path, method: &str, input: Value, key: &str) -> String {
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
    expect_fail(method, &out)
}

/// 播种一个带真实授权根的项目（t012 directories.create 声明面），返回
/// (home, workspace, project_id, instance_id)。
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
            "Hooks E2E",
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

/// 在工作区内播种一个插件源目录（清单声明 analyze 操作、结果 Schema、
/// 配置 Schema 与密钥字段）。
fn seed_plugin_source(workspace: &Path, name: &str) -> PathBuf {
    let dir = workspace.join("sources").join(name);
    std::fs::create_dir_all(&dir).expect("create plugin source");
    let manifest = json!({
        "name": name,
        "displayName": "E2E Plugin",
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
    std::fs::write(dir.join("index.js"), "// plugin entry\n").expect("write entry");
    dir
}

fn project_scope(project_id: &str) -> String {
    json!({"kind": "project", "id": project_id}).to_string()
}

async fn install(
    home: &Path,
    scope: &str,
    plugin_id: &str,
    source_ref: &str,
    version: &str,
    expected_installed: Value,
    key: &str,
) -> Value {
    invoke(
        home,
        "plugins.install",
        json!({
            "scope": serde_json::from_str::<Value>(scope).expect("scope json"),
            "pluginId": plugin_id,
            "sourceRef": source_ref,
            "version": version,
            "expectedInstalledVersion": expected_installed,
        }),
        key,
    )
    .await
}

async fn wait_operation(home: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let page = cli_json(
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
        let page = cli_json(
            home,
            &["run", "get", "--output", "json", "--run-id", run_id],
        )
        .await;
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
        let page = cli_json(
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
        )
        .await;
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

async fn plugins_list(home: &Path, scope: &str) -> Value {
    cli_json(
        home,
        &["plugins", "list", "--output", "json", "--scope", scope],
    )
    .await
}

async fn hooks_configure(
    home: &Path,
    scope: &str,
    hook_id: &str,
    trigger: &str,
    deadline_ms: i64,
    enabled: bool,
    expected_revision: &str,
) -> Value {
    cli_json(
        home,
        &[
            "hooks",
            "configure",
            "--output",
            "json",
            "--scope",
            scope,
            "--bindings",
            &json!([{
                "hookId": hook_id,
                "trigger": trigger,
                "ownerId": "e2e-plugin",
                "operationId": "analyze",
                "enabled": enabled,
                "deadlineMs": deadline_ms,
            }])
            .to_string(),
            "--expected-revision",
            expected_revision,
        ],
    )
    .await
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// 安装生命周期与「半安装不公告」：本地路径安装、URL 安装诚实失败不留
/// 行、来源校验拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_install_lifecycle_and_half_install_invisibility() {
    let (home, workspace, project_id, _instance_id) = seed_workspace("plug-e2e-install").await;
    let scope = project_scope(&project_id);
    let root = home.path();
    let source = seed_plugin_source(&workspace, "e2e-plugin");
    let source_ref = source.display().to_string();

    // 空目录起步。
    let listed = plugins_list(root, &scope).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
    assert!(listed["snapshotRevision"].as_str().is_some());

    // git URL 形态合法但本构建无抓取桥：受理后以 UPSTREAM_ERROR 失败。
    let receipt = install(
        root,
        &scope,
        "url-plugin",
        "https://example.com/no-bridge.git",
        "0.1.0",
        Value::Null,
        "key-url-install",
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "failed");
    assert_eq!(operation["result"]["code"], "UPSTREAM_ERROR");

    // 本地路径安装（expectedInstalledVersion=null 走 api invoke）。
    let receipt = install(
        root,
        &scope,
        "e2e-plugin",
        &source_ref,
        "1.0.0",
        Value::Null,
        "key-install-1",
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["durable"], true);
    assert_eq!(receipt["state"], "queued");
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "succeeded");
    assert_eq!(operation["result"]["pluginId"], "e2e-plugin");
    assert_eq!(operation["result"]["installed"], true);
    // 安装成功不自动宣称已启用。
    assert_eq!(operation["result"]["enabled"], false);
    assert_eq!(operation["result"]["available"], false);
    assert!(
        operation["result"]["manifestHash"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64),
        "manifestHash is the sha256 digest of the manifest"
    );

    // 失败的 URL 安装没有留下行（半安装不公告）。
    let listed = plugins_list(root, &scope).await;
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["pluginId"], "e2e-plugin");
    assert_eq!(items[0]["installed"], true);
    assert_eq!(items[0]["enabled"], false);
    assert_eq!(items[0]["runtime"]["state"], "disabled");
    assert_eq!(items[0]["runtime"]["available"], false);

    // get：清单投影 + 声明操作 + 配置 Schema id。
    let got = cli_json(
        root,
        &[
            "plugins",
            "get",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
        ],
    )
    .await;
    assert_eq!(
        got["configurationSchemaId"],
        "agentcpm-pd.plugin.e2e-plugin.configuration/1"
    );
    let operations = got["operations"].as_array().cloned().unwrap_or_default();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0]["operationId"], "analyze");
    assert_eq!(operations[0]["available"], false);

    // 来源校验拒绝：越界路径（工作区之外）与版本不匹配。
    let outside = workspace.parent().unwrap().join("outside-src");
    std::fs::create_dir_all(&outside).expect("create outside source");
    let code = invoke_fail(
        root,
        "plugins.install",
        json!({
            "scope": serde_json::from_str::<Value>(&scope).expect("scope json"),
            "pluginId": "outside-plugin",
            "sourceRef": outside.display().to_string(),
            "version": "1.0.0",
            "expectedInstalledVersion": Value::Null,
        }),
        "key-outside",
    )
    .await;
    assert_eq!(code, "PATH_OUTSIDE_SCOPE");

    // manifest 声明版本与请求不一致：预受理拒绝（无操作产生）。
    let code = invoke_fail(
        root,
        "plugins.install",
        json!({
            "scope": serde_json::from_str::<Value>(&scope).expect("scope json"),
            "pluginId": "e2e-plugin",
            "sourceRef": source_ref,
            "version": "2.0.0",
            "expectedInstalledVersion": Value::Null,
        }),
        "key-wrong-version",
    )
    .await;
    assert_eq!(code, "INVALID_ARGUMENT");

    // 更新路径：expectedInstalledVersion 指向当前版本。
    let manifest_path = source.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["version"] = json!("1.1.0");
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    let receipt = install(
        root,
        &scope,
        "e2e-plugin",
        &source_ref,
        "1.1.0",
        json!("1.0.0"),
        "key-update-1",
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "succeeded");
    assert_eq!(operation["result"]["version"], "1.1.0");
    let listed = plugins_list(root, &scope).await;
    assert_eq!(listed["items"][0]["version"], "1.1.0");
    drop(home);
}

/// 配置面 + transition 状态机 + remove 清理边界（默认保留配置、重装恢复）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_configure_transition_and_remove_boundaries() {
    let (home, workspace, project_id, _instance_id) = seed_workspace("plug-e2e-config").await;
    let scope = project_scope(&project_id);
    let root = home.path();
    let source = seed_plugin_source(&workspace, "e2e-plugin");
    let receipt = install(
        root,
        &scope,
        "e2e-plugin",
        &source.display().to_string(),
        "1.0.0",
        Value::Null,
        "key-cfg-install",
    )
    .await;
    wait_operation(root, receipt["operationId"].as_str().unwrap()).await;

    // 未知字段拒绝（改写后校验的预写一半）。
    let out = await_cli(
        root,
        &[
            "plugins",
            "configure",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--schema-id",
            "agentcpm-pd.plugin.e2e-plugin.configuration/1",
            "--configuration",
            "{\"endpoint\":\"https://api.example.test\",\"shell\":\"bash\"}",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(expect_fail("plugins.configure", &out), "SCHEMA_MISMATCH");

    // 首次配置：absent → revision；读取面脱敏。
    let configured = cli_json(
        root,
        &[
            "plugins",
            "configure",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--schema-id",
            "agentcpm-pd.plugin.e2e-plugin.configuration/1",
            "--configuration",
            "{\"endpoint\":\"https://api.example.test\",\"apiKey\":\"sk-e2e-live\"}",
            "--expected-revision",
            "absent",
        ],
    )
    .await;
    let revision = configured["revision"].as_str().unwrap().to_owned();
    assert!(revision.starts_with("pcfg-"));
    assert_eq!(configured["applied"], false);

    let got = cli_json(
        root,
        &[
            "plugins",
            "get",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
        ],
    )
    .await;
    assert_eq!(got["configuration"]["endpoint"], "https://api.example.test");
    assert_eq!(got["configuration"]["apiKey"], "<redacted>");

    // 旧 revision → REVISION_CONFLICT。
    let out = await_cli(
        root,
        &[
            "plugins",
            "configure",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--schema-id",
            "agentcpm-pd.plugin.e2e-plugin.configuration/1",
            "--configuration",
            "{\"endpoint\":\"https://v2.example.test\"}",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(expect_fail("plugins.configure", &out), "REVISION_CONFLICT");

    // transition：enable（absent CAS）→ 新代际；旧代际 → GENERATION_CONFLICT。
    let receipt = cli_json(
        root,
        &[
            "plugins",
            "transition",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--action",
            "enable",
            "--expected-generation",
            "absent",
            "--request-key",
            "key-enable-1",
        ],
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "succeeded");
    assert_eq!(operation["result"]["enabled"], true);
    let generation = operation["result"]["generation"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(generation.starts_with("pgen-"));

    let out = await_cli(
        root,
        &[
            "plugins",
            "transition",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--action",
            "reload",
            "--expected-generation",
            "absent",
            "--request-key",
            "key-reload-stale",
        ],
    );
    assert_eq!(
        expect_fail("plugins.transition", &out),
        "GENERATION_CONFLICT"
    );

    // remove：错误版本 → VERSION_CONFLICT；默认保留配置。
    let out = await_cli(
        root,
        &[
            "plugins",
            "remove",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--expected-version",
            "9.9.9",
            "--request-key",
            "key-remove-stale",
        ],
    );
    assert_eq!(expect_fail("plugins.remove", &out), "VERSION_CONFLICT");

    let receipt = cli_json(
        root,
        &[
            "plugins",
            "remove",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--expected-version",
            "1.0.0",
            "--request-key",
            "key-remove-1",
        ],
    )
    .await;
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "succeeded");
    assert_eq!(operation["result"]["removed"], true);
    assert_eq!(operation["result"]["configurationRetained"], true);
    assert_eq!(
        operation["result"]["retainedResources"],
        json!(["configuration"])
    );

    let listed = plugins_list(root, &scope).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));

    // 重装：保留的配置恢复（含秘密——掩码回读）。
    let receipt = install(
        root,
        &scope,
        "e2e-plugin",
        &source.display().to_string(),
        "1.0.0",
        Value::Null,
        "key-reinstall",
    )
    .await;
    wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    let got = cli_json(
        root,
        &[
            "plugins",
            "get",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
        ],
    )
    .await;
    assert_eq!(got["configuration"]["apiKey"], "<redacted>");
    assert_eq!(
        got["plugin"]["configurationRevision"],
        json!(revision),
        "the retained configuration revision survives re-install"
    );
    drop(home);
}

/// plugins.invoke 的执行前拒绝面 + 受理后诚实失败（无执行桥）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_invoke_rejects_before_execution() {
    let (home, workspace, project_id, _instance_id) = seed_workspace("plug-e2e-invoke").await;
    let scope = project_scope(&project_id);
    let root = home.path();
    let source = seed_plugin_source(&workspace, "e2e-plugin");
    let receipt = install(
        root,
        &scope,
        "e2e-plugin",
        &source.display().to_string(),
        "1.0.0",
        Value::Null,
        "key-invoke-install",
    )
    .await;
    wait_operation(root, receipt["operationId"].as_str().unwrap()).await;

    let invoke =
        |plugin_id: &str, operation_id: &str, schema_suffix: &str, arguments: &str, key: &str| {
            let root = root.to_path_buf();
            let scope = scope.clone();
            let schema_id =
                format!("agentcpm-pd.plugin.{plugin_id}.op.{operation_id}.{schema_suffix}");
            let plugin_id = plugin_id.to_string();
            let operation_id = operation_id.to_string();
            let arguments = arguments.to_string();
            let key = key.to_string();
            async move {
                await_cli(
                    &root,
                    &[
                        "plugins",
                        "invoke",
                        "--output",
                        "json",
                        "--scope",
                        &scope,
                        "--plugin-id",
                        &plugin_id,
                        "--operation-id",
                        &operation_id,
                        "--schema-id",
                        &schema_id,
                        "--arguments",
                        &arguments,
                        "--request-key",
                        &key,
                    ],
                )
            }
        };

    // 未知操作 → NOT_FOUND。
    let out = invoke(
        "e2e-plugin",
        "not-declared",
        "input",
        "{\"text\":\"hello\"}",
        "key-inv-unknown",
    )
    .await;
    assert_eq!(expect_fail("plugins.invoke", &out), "NOT_FOUND");

    // schemaId 不匹配 → SCHEMA_MISMATCH。
    let out = invoke(
        "e2e-plugin",
        "analyze",
        "result",
        "{\"text\":\"hello\"}",
        "key-inv-schema",
    )
    .await;
    assert_eq!(expect_fail("plugins.invoke", &out), "SCHEMA_MISMATCH");

    // 参数不合 schema → SCHEMA_MISMATCH。
    let out = invoke(
        "e2e-plugin",
        "analyze",
        "input",
        "{\"text\":42}",
        "key-inv-args",
    )
    .await;
    assert_eq!(expect_fail("plugins.invoke", &out), "SCHEMA_MISMATCH");

    // 禁用插件 → UNAVAILABLE（基线 503 语义）。
    let out = invoke(
        "e2e-plugin",
        "analyze",
        "input",
        "{\"text\":\"hello\"}",
        "key-inv-disabled",
    )
    .await;
    assert_eq!(expect_fail("plugins.invoke", &out), "UNAVAILABLE");

    // 启用后：受理 → 无执行桥诚实失败。
    cli_json(
        root,
        &[
            "plugins",
            "transition",
            "--output",
            "json",
            "--scope",
            &scope,
            "--plugin-id",
            "e2e-plugin",
            "--action",
            "enable",
            "--expected-generation",
            "absent",
            "--request-key",
            "key-inv-enable",
        ],
    )
    .await;
    let receipt = invoke(
        "e2e-plugin",
        "analyze",
        "input",
        "{\"text\":\"hello\"}",
        "key-inv-accepted",
    )
    .await;
    let receipt_json: Value = serde_json::from_str(&receipt.stdout).unwrap();
    let operation = wait_operation(root, receipt_json["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "failed");
    assert_eq!(operation["result"]["code"], "PLUGIN_EXECUTION_UNAVAILABLE");
    drop(home);
}

/// hooks 配置面 + 异步回执流：触发 → 轮询 → 提交 → run 复活；重复/冲突
/// 回执语义。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_receipt_flow_delivers_and_revives_the_run() {
    let (home, workspace, project_id, _instance_id) = seed_workspace("hook-e2e-flow").await;
    let scope = project_scope(&project_id);
    let root = home.path();
    let source = seed_plugin_source(&workspace, "e2e-plugin");
    let receipt = install(
        root,
        &scope,
        "e2e-plugin",
        &source.display().to_string(),
        "1.0.0",
        Value::Null,
        "key-flow-install",
    )
    .await;
    wait_operation(root, receipt["operationId"].as_str().unwrap()).await;

    // 未配置层：revision=absent。
    let listed = cli_json(
        root,
        &["hooks", "list", "--output", "json", "--scope", &scope],
    )
    .await;
    assert_eq!(listed["revision"], "absent");
    assert_eq!(listed["hooks"].as_array().map(Vec::len), Some(0));

    // 非法 trigger / 未知 owner 拒绝。
    let out = await_cli(
        root,
        &[
            "hooks",
            "configure",
            "--output",
            "json",
            "--scope",
            &scope,
            "--bindings",
            &json!([{
                "hookId": "h1",
                "trigger": "NotARealEvent",
                "ownerId": "e2e-plugin",
                "operationId": "analyze",
                "enabled": true,
                "deadlineMs": 30000,
            }])
            .to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(expect_fail("hooks.configure", &out), "INVALID_ARGUMENT");

    let out = await_cli(
        root,
        &[
            "hooks",
            "configure",
            "--output",
            "json",
            "--scope",
            &scope,
            "--bindings",
            &json!([{
                "hookId": "h1",
                "trigger": "Stop",
                "ownerId": "ghost-plugin",
                "operationId": "analyze",
                "enabled": true,
                "deadlineMs": 30000,
            }])
            .to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(expect_fail("hooks.configure", &out), "INVALID_ARGUMENT");

    // 合法配置 + list 投影（未加载不标可用）。
    let configured = hooks_configure(root, &scope, "on-stop", "Stop", 30000, true, "absent").await;
    let revision = configured["revision"].as_str().unwrap().to_owned();
    assert!(revision.starts_with("hrev-"));
    assert_eq!(configured["applied"], false);

    let listed = cli_json(
        root,
        &["hooks", "list", "--output", "json", "--scope", &scope],
    )
    .await;
    assert_eq!(listed["revision"], json!(revision));
    let hooks = listed["hooks"].as_array().cloned().unwrap_or_default();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["hookId"], "on-stop");
    assert_eq!(hooks[0]["trigger"], "Stop");
    assert_eq!(hooks[0]["deadlineMs"], 30000);
    assert_eq!(
        hooks[0]["resultSchemaId"],
        "agentcpm-pd.plugin.e2e-plugin.op.analyze.result/1"
    );
    assert_eq!(hooks[0]["runtime"]["available"], false);
    assert_eq!(
        hooks[0]["runtime"]["reasonCode"],
        "HOOK_TRIGGER_NOT_ASSEMBLED"
    );

    // 触发（离线驱动的确定性触发桥）→ invocation → 轮询。
    let session_id = "s-hook-flow";
    seed_session(root, &project_id, session_id).await;
    let submitted = submit_run(
        root,
        session_id,
        "please finish [pd-driver:fire-hook=on-stop]",
        "key-flow-run",
    )
    .await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    let invoked = wait_hook_invoked(root, &run_id).await;
    let invocation_id = invoked["invocationId"].as_str().unwrap().to_owned();
    let token = invoked["completionTokenRef"].as_str().unwrap().to_owned();
    assert!(token.starts_with("hkct-"));
    let schema_id = invoked["resultSchemaId"].as_str().unwrap().to_owned();

    let get_invocation = || {
        let root = root.to_path_buf();
        let session_id = session_id.to_owned();
        let invocation_id = invocation_id.clone();
        async move {
            cli_json(
                &root,
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
            )
            .await
        }
    };
    let invocation = get_invocation().await;
    assert_eq!(invocation["state"], "pending");
    assert_eq!(invocation["hookId"], "on-stop");
    assert_eq!(invocation["runId"], json!(run_id));
    assert_eq!(invocation["resultSchemaId"], json!(schema_id));
    assert_eq!(invocation["receiptId"], Value::Null);

    // 错令牌 → PERMISSION_DENIED；跨会话读取 → NOT_FOUND（不泄露）。
    let out = await_cli(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            "hkct-forged",
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\"}",
            "--request-key",
            "key-forged",
        ],
    );
    assert_eq!(
        expect_fail("hooks.submit_result", &out),
        "PERMISSION_DENIED"
    );

    let out = await_cli(
        root,
        &[
            "hooks",
            "get-invocation",
            "--output",
            "json",
            "--session-id",
            "s-other-session",
            "--invocation-id",
            &invocation_id,
        ],
    );
    assert_eq!(expect_fail("hooks.get_invocation", &out), "NOT_FOUND");

    // 结果不合钉版 Schema（未知字段）→ SCHEMA_MISMATCH。
    let out = await_cli(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\",\"shell\":\"bash\"}",
            "--request-key",
            "key-bad-result",
        ],
    );
    assert_eq!(expect_fail("hooks.submit_result", &out), "SCHEMA_MISMATCH");

    // 合法提交 → accepted；run 复活并携带 hook 结果。
    let submitted_result = cli_json(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\"}",
            "--request-key",
            "key-submit-1",
        ],
    )
    .await;
    assert_eq!(submitted_result["disposition"], "accepted");
    assert_eq!(submitted_result["runId"], json!(run_id));
    let receipt_id = submitted_result["receiptId"].as_str().unwrap().to_owned();

    let run = wait_run_terminal(root, &run_id).await;
    assert_eq!(run["state"], "succeeded");
    assert_eq!(run["result"]["hookResult"]["verdict"], "allow");

    // 同 key 重放 → 原回执；同结果新 key → duplicate；冲突结果 → IDEMPOTENCY_CONFLICT。
    let replay = cli_json(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\"}",
            "--request-key",
            "key-submit-1",
        ],
    )
    .await;
    assert_eq!(replay["disposition"], "accepted");
    assert_eq!(replay["receiptId"], json!(receipt_id));

    let duplicate = cli_json(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"allow\"}",
            "--request-key",
            "key-submit-2",
        ],
    )
    .await;
    assert_eq!(duplicate["disposition"], "duplicate");
    assert_eq!(duplicate["receiptId"], json!(receipt_id));

    let out = await_cli(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"deny\"}",
            "--request-key",
            "key-submit-3",
        ],
    );
    assert_eq!(
        expect_fail("hooks.submit_result", &out),
        "IDEMPOTENCY_CONFLICT"
    );

    // invocation 反映 delivered + receipt。
    let invocation = get_invocation().await;
    assert_eq!(invocation["state"], "delivered");
    assert_eq!(invocation["receiptId"], json!(receipt_id));
    drop(home);
}

/// 过期回执不能复活 run：短 deadline 触发后 run 失败；迟到回执被拒且无
/// 副作用。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_receipts_do_not_revive_runs() {
    let (home, workspace, project_id, _instance_id) = seed_workspace("hook-e2e-expiry").await;
    let scope = project_scope(&project_id);
    let root = home.path();
    let source = seed_plugin_source(&workspace, "e2e-plugin");
    let receipt = install(
        root,
        &scope,
        "e2e-plugin",
        &source.display().to_string(),
        "1.0.0",
        Value::Null,
        "key-exp-install",
    )
    .await;
    wait_operation(root, receipt["operationId"].as_str().unwrap()).await;

    // 短 deadline：过期可确定性观察。
    hooks_configure(root, &scope, "quick-hook", "Stop", 800, true, "absent").await;
    let session_id = "s-hook-expiry";
    seed_session(root, &project_id, session_id).await;
    let submitted = submit_run(
        root,
        session_id,
        "[pd-driver:fire-hook=quick-hook]",
        "key-exp-run",
    )
    .await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    let invoked = wait_hook_invoked(root, &run_id).await;
    let invocation_id = invoked["invocationId"].as_str().unwrap().to_owned();
    let token = invoked["completionTokenRef"].as_str().unwrap().to_owned();
    let schema_id = invoked["resultSchemaId"].as_str().unwrap().to_owned();

    // run 过期失败（HOOK_INVOCATION_EXPIRED），invocation 转过期。
    let run = wait_run_terminal(root, &run_id).await;
    assert_eq!(run["state"], "failed");
    assert_eq!(run["outcomeCode"], "HOOK_INVOCATION_EXPIRED");

    let invocation = cli_json(
        root,
        &[
            "hooks",
            "get-invocation",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
        ],
    )
    .await;
    assert_eq!(invocation["state"], "expired");

    // 迟到回执拒绝且不复活 run（拒绝无副作用）。
    let out = await_cli(
        root,
        &[
            "hooks",
            "submit-result",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--invocation-id",
            &invocation_id,
            "--completion-token-ref",
            &token,
            "--schema-id",
            &schema_id,
            "--result",
            "{\"verdict\":\"late\"}",
            "--request-key",
            "key-late",
        ],
    );
    assert_eq!(
        expect_fail("hooks.submit_result", &out),
        "INVOCATION_EXPIRED"
    );

    let run = cli_json(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    )
    .await;
    assert_eq!(
        run["run"]["state"], "failed",
        "the expired run stays failed"
    );
    drop(home);
}
