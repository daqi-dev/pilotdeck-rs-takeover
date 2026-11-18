//! cli.task.automation（探索/工作周期/组合任务）端到端场景：14 个
//! `discovery.*` / `automation.*` / `work_cycles.*` / `agent_workflow.*`
//! 操作经真实 CLI 二进制与 `/v1/ws` 传输。
//!
//! 装置：
//! - `HOME=<TestHome>`、`GIT_CONFIG_GLOBAL=/dev/null`、
//!   `GIT_CONFIG_SYSTEM=/dev/null`（git 同款隔离，workflow 的 git 步骤可
//!   确定性验收）；
//! - 授权根播种走 `V1Store::declare_authorized_root`（t013 先例）；
//! - 计划/周期播种走域生产者缝隙 `record_discovery_plan` /
//!   `record_work_cycle`（发现 runtime 的写入面；本套件同入口）；
//! - 组合任务的克隆源用本地 bare 仓库（`file://`，零网络）。
//!
//! 场景对应 design.json 关联用例（c030 / contract.capability.{a034,
//! project-discovery,project-work-cycles,external-agent-workflow}）的核心
//! 断言面：未配置显式不可用、虚拟项目拒绝、计划版本与报告关联、周期
//! CAS 应用与归档、发布效果显式授权、临时克隆清理归属。
//! 本机构建冻结（2026-11-18）：本文件按简报写好并标注「本地未跑，待
//! cb8」。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use common_v1::*;

fn git_present() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn git_sync(cwd: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
    .args(["-c", "user.name=e2e", "-c", "user.email=e2e@test"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn git")
        .success();
    assert!(ok, "git {} failed in {}", args.join(" "), cwd.display());
}

async fn cli(root: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = base_command(root);
    cmd.args(args)
        .env("HOME", root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let child = cmd.spawn().expect("spawn agentcpm-pd");
    let output = tokio::time::timeout(TIMEOUT_CLI, child.wait_with_output())
        .await
        .expect("cli invocation timed out")
        .expect("wait for cli");
    CliOutcome {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

const TIMEOUT_CLI: std::time::Duration = std::time::Duration::from_secs(120);

async fn cli_json(root: &Path, args: &[&str]) -> Value {
    let out = await_cli(root, args);
    let method = if args.len() > 1 && !args[1].starts_with('-') {
        format!("{}.{}", args[0].replace('-', "_"), args[1].replace('-', "_"))
    } else {
        args[0].replace('-', "_")
    };
    expect_ok(&method, &out)
}

/// 失败断言：非零退出 + `outcome=failed` + `error.code`。
async fn cli_failure(root: &Path, args: &[&str]) -> String {
    let out = cli(root, args).await;
    assert_ne!(out.code, 0, "`{}` must not exit 0: {}", args[0], out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{}` stdout: {}", args[0], out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{}` stdout not JSON: {e}\n{}", args[0], out.stdout));
    assert_eq!(
        value["outcome"], "failed",
        "`{}` envelope: {value}",
        args[0]
    );
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// 播种一个带真实授权根与 git 仓库工作区的项目（git e2e 同款）。
async fn seed_project(home: &Path, tag: &str) -> (String, PathBuf) {
    let instance_id = init_home_instance(home);
    let declared_root = home.parent().unwrap().join(format!("{tag}-authorized"));
    let workspace = declared_root.join("ws");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    {
        let (file, _token) =
            pilotdeck_gateway::public_v1::bootstrap::load_local_instance_and_token(home, None)
                .expect("load instance");
        let meta = pilotdeck_gateway::public_v1::store::InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-e2e".to_owned(),
            server_revision: "srv-e2e".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = pilotdeck_gateway::public_v1::store::V1Store::open_and_recover(home, meta)
            .expect("open store for seeding");
        store
            .declare_authorized_root(&format!("root-{tag}"), &declared_root)
            .expect("declare authorized root");
    }
    git_sync(&workspace, &["init", "-q", "-b", "main"]);
    git_sync(&workspace, &["config", "commit.gpgsign", "false"]);
    std::fs::write(workspace.join("README.md"), "# e2e\n").unwrap();
    git_sync(&workspace, &["add", "-A"]);
    git_sync(&workspace, &["commit", "-q", "-m", "e2e init"]);

    let project_id = format!("p-{tag}");
    let out = cli(
        home,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "Automation E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"root-{tag}\",\"relativePath\":\"ws\"}}"
            ),
        ],
    )
    .await;
    expect_ok("project.register", &out);
    (project_id, workspace)
}

/// 注册一个虚拟项目（无宿主声明授权根）。
async fn seed_virtual_project(home: &Path, tag: &str) -> String {
    let instance_id = init_home_instance(home);
    let project_id = format!("pv-{tag}");
    let out = cli(
        home,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "Virtual",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"undeclared-{tag}\",\"relativePath\":\"ws\"}}"
            ),
        ],
    )
    .await;
    expect_ok("project.register", &out);
    project_id
}

/// 轮询直到谓词成立（有界；probe 每轮发起一次异步探测）。
async fn poll_until<T, F>(mut probe: impl FnMut() -> F, what: &str) -> T
where
    F: std::future::Future<Output = Option<T>>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`{what}` never became ready"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// 轮询一个异步操作到终态，返回其完成记录（operations.get 的 result）。
async fn wait_completion(root: &Path, operation_id: &str) -> Value {
    let root = root.to_path_buf();
    let operation_id = operation_id.to_owned();
    poll_until(
        || {
            let root = root.clone();
            let operation_id = operation_id.clone();
            async move {
                let out = cli(
                    &root,
                    &[
                        "operations",
                        "get",
                        "--output",
                        "json",
                        "--operation-id",
                        &operation_id,
                    ],
                )
                .await;
                let lines: Vec<&str> = out
                    .stdout
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                let value: Value =
                    serde_json::from_str(lines.first().copied().unwrap_or("")).ok()?;
                let state = value["operation"]["state"].as_str()?;
                if matches!(state, "succeeded" | "failed") {
                    Some(value["operation"]["result"].clone())
                } else {
                    None
                }
            }
        },
        "operation completion",
    )
    .await
}

/// 读取一次命令的 JSON 输出（轮询探测的内联版）。
async fn probe_json(root: &Path, args: &[&str]) -> Option<Value> {
    let out = cli(root, args).await;
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    serde_json::from_str(lines.first().copied().unwrap_or("")).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_context_plans_and_report_over_transport() {
    let home = TestHome::new("aw-disc");
    let root = home.path();
    init_home(root);
    let (project_id, _workspace) = seed_project(root, "disc").await;
    let virtual_id = seed_virtual_project(root, "disc-v").await;

    // 真实项目上下文。
    let context = cli_json(
        root,
        &[
            "discovery",
            "get-context",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    assert_eq!(context["projectId"], project_id);
    assert_eq!(context["workspaceAvailable"], true);
    assert!(context["context"].as_str().is_some_and(|c| !c.is_empty()));
    assert_eq!(context["activePlanIds"], json!([]));

    // 虚拟项目拒绝；未知项目 NOT_FOUND。
    let code = cli_failure(
        root,
        &[
            "discovery",
            "get-context",
            "--output",
            "json",
            "--project-id",
            &virtual_id,
        ],
    )
    .await;
    assert_eq!(code, "VIRTUAL_PROJECT");
    let code = cli_failure(
        root,
        &[
            "discovery",
            "get-context",
            "--output",
            "json",
            "--project-id",
            "p-missing",
        ],
    )
    .await;
    assert_eq!(code, "NOT_FOUND");

    // 空目录；虚拟项目拒绝。
    let page = cli_json(
        root,
        &[
            "discovery",
            "list-plans",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["nextCursor"], Value::Null);
    let code = cli_failure(
        root,
        &[
            "discovery",
            "list-plans",
            "--output",
            "json",
            "--project-id",
            &virtual_id,
        ],
    )
    .await;
    assert_eq!(code, "VIRTUAL_PROJECT");

    // 生产者缝隙播种计划 → 目录可见（正文与固定 revision）。
    let revision = pilotdeck_gateway::public_v1::domains::discovery::record_discovery_plan(
        root,
        &project_id,
        "plan-e2e",
        "E2E plan",
        "explore everything [ignored-directives]",
    )
    .expect("seed plan");
    let page = cli_json(
        root,
        &[
            "discovery",
            "list-plans",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--plan-id",
            "plan-e2e",
        ],
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["revision"], revision);
    assert_eq!(page["items"][0]["title"], "E2E plan");

    // 执行未知计划 → NOT_FOUND；版本错 → REVISION_CONFLICT。
    let code = cli_failure(
        root,
        &[
            "discovery",
            "execute",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--plan-id",
            "missing",
            "--expected-revision",
            "r",
            "--request-key",
            "e2e-disc-missing",
        ],
    )
    .await;
    assert_eq!(code, "NOT_FOUND");
    let code = cli_failure(
        root,
        &[
            "discovery",
            "execute",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--plan-id",
            "plan-e2e",
            "--expected-revision",
            "stale",
            "--request-key",
            "e2e-disc-stale",
        ],
    )
    .await;
    assert_eq!(code, "REVISION_CONFLICT");

    // 执行 → receipt → 报告关联 run 与计划版本。
    let receipt = cli_json(
        root,
        &[
            "discovery",
            "execute",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--plan-id",
            "plan-e2e",
            "--expected-revision",
            &revision,
            "--request-key",
            "e2e-disc-1",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["state"], "queued");
    let report = {
        let root = root.to_path_buf();
        let project_id = project_id.clone();
        poll_until(
            || {
                let root = root.clone();
                let project_id = project_id.clone();
                async move {
                    let report = probe_json(
                        &root,
                        &[
                            "discovery",
                            "get-report",
                            "--output",
                            "json",
                            "--project-id",
                            &project_id,
                            "--plan-id",
                            "plan-e2e",
                        ],
                    )
                    .await?;
                    let state = report["state"].as_str().unwrap_or_default();
                    (state != "queued" && state != "running").then_some(report)
                }
            },
            "discovery report",
        )
        .await
    };
    assert_eq!(report["state"], "succeeded");
    let run_id = report["runId"].as_str().expect("report run").to_owned();
    assert!(!run_id.is_empty());
    assert!(report["reportArtifactId"].as_str().is_some());
    let content = report["content"].as_str().expect("report content");
    assert!(content.contains(&run_id), "报告关联本次 run");
    assert!(content.contains(&revision), "报告关联计划版本");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_cycles_apply_and_archive_over_transport() {
    let home = TestHome::new("aw-cycles");
    let root = home.path();
    init_home(root);
    let (project_id, workspace) = seed_project(root, "cycles").await;

    // 空目录；未知周期 NOT_FOUND。
    let page = cli_json(
        root,
        &[
            "work-cycles",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    assert_eq!(page["items"], json!([]));
    let code = cli_failure(
        root,
        &[
            "work-cycles",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "missing",
            "--expected-revision",
            "r",
            "--expected-workspace-revision",
            "w",
            "--request-key",
            "e2e-wc-missing",
        ],
    )
    .await;
    assert_eq!(code, "NOT_FOUND");

    // 播种周期（生产者缝隙）→ 应用 → 变更落盘 + run 关联。
    let seeded = pilot_deck_record_work_cycle(
        root,
        &project_id,
        "cycle-e2e",
        &[(
            "notes/applied.md".to_owned(),
            b"cycle applied content\n".to_vec(),
        )],
    );
    let cycle_revision = seeded["revision"].as_str().unwrap().to_owned();
    let ws_revision = seeded["workspaceRevision"].as_str().unwrap().to_owned();

    // 工作区变化 → REVISION_CONFLICT。
    std::fs::write(workspace.join("notes/applied.md"), "drift").unwrap();
    let code = cli_failure(
        root,
        &[
            "work-cycles",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-e2e",
            "--expected-revision",
            &cycle_revision,
            "--expected-workspace-revision",
            &ws_revision,
            "--request-key",
            "e2e-wc-drift",
        ],
    )
    .await;
    assert_eq!(code, "REVISION_CONFLICT");
    std::fs::write(workspace.join("notes/applied.md"), "").unwrap();
    std::fs::remove_file(workspace.join("notes/applied.md")).unwrap();

    let receipt = cli_json(
        root,
        &[
            "work-cycles",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-e2e",
            "--expected-revision",
            &cycle_revision,
            "--expected-workspace-revision",
            &ws_revision,
            "--request-key",
            "e2e-wc-1",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    // 轮询操作完成（operations.get）。
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &operation_id).await;
    assert_eq!(
        completion["changedPaths"],
        json!(["notes/applied.md"]),
        "changedPaths 如实报告"
    );
    let applied = std::fs::read_to_string(workspace.join("notes/applied.md")).unwrap();
    assert_eq!(applied, "cycle applied content\n");
    assert!(completion["runId"].as_str().is_some());
    assert!(completion["recoveryArtifactId"].as_str().is_some());

    // 归档 + 事件 + 不删工作区。
    let page = cli_json(
        root,
        &[
            "work-cycles",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-e2e",
        ],
    )
    .await;
    assert_eq!(page["items"][0]["state"], "applied");
    let new_revision = page["items"][0]["revision"].as_str().unwrap().to_owned();
    let archived = cli_json(
        root,
        &[
            "work-cycles",
            "archive",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-e2e",
            "--expected-revision",
            &new_revision,
        ],
    )
    .await;
    assert_eq!(archived["cycle"]["state"], "archived");
    assert!(workspace.exists());
    assert!(workspace.join("notes/applied.md").exists(), "归档不删文件");
}

/// e2e 侧的周期播种（经生产者缝隙；pub API 适配）。
fn pilot_deck_record_work_cycle(
    home: &Path,
    project_id: &str,
    cycle_id: &str,
    changes: &[(String, Vec<u8>)],
) -> Value {
    pilotdeck_gateway::public_v1::domains::work_cycles::record_work_cycle(
        home,
        project_id,
        cycle_id,
        "e2e cycle",
        changes,
    )
    .expect("seed work cycle")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automation_unavailable_apply_and_abort_over_transport() {
    let home = TestHome::new("aw-automation");
    let root = home.path();
    init_home(root);
    let (project_id, _workspace) = seed_project(root, "auto").await;

    // 未配置 → 显式 UNAVAILABLE（不伪成功）。
    let code = cli_failure(
        root,
        &[
            "automation",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    assert_eq!(code, "UNAVAILABLE");

    // 播种计划 → apply → get 可见绑定；同 key 重放不重复应用。
    let revision = pilotdeck_gateway::public_v1::domains::discovery::record_discovery_plan(
        root,
        &project_id,
        "plan-auto",
        "Automation plan",
        "run the automation",
    )
    .expect("seed plan");
    let receipt = cli_json(
        root,
        &[
            "automation",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-e2e",
            "--plan-id",
            "plan-auto",
            "--expected-revision",
            &revision,
            "--request-key",
            "e2e-auto-1",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &operation_id).await;
    let run_id = completion["runId"]
        .as_str()
        .expect("automation run")
        .to_owned();
    assert!(!run_id.is_empty());

    let status = cli_json(
        root,
        &[
            "automation",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-e2e",
        ],
    )
    .await;
    assert_eq!(status["automationId"], "aw-e2e");
    assert_eq!(status["status"]["available"], true);
    assert_eq!(status["planId"], "plan-auto");
    assert_eq!(status["lastResult"], "succeeded");

    // 同 key 重放受理。
    let replay = cli_json(
        root,
        &[
            "automation",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-e2e",
            "--plan-id",
            "plan-auto",
            "--expected-revision",
            &revision,
            "--request-key",
            "e2e-auto-1",
        ],
    )
    .await;
    assert_eq!(replay["deduplicated"], true);

    // abort 已终态 run → 完成记录回报终态。
    let receipt = cli_json(
        root,
        &[
            "automation",
            "abort",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-e2e",
            "--run-id",
            &run_id,
            "--request-key",
            "e2e-auto-abort",
        ],
    )
    .await;
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &operation_id).await;
    assert_eq!(completion["state"], "succeeded");

    // 外来 run → NOT_FOUND。
    let code = cli_failure(
        root,
        &[
            "automation",
            "abort",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-e2e",
            "--run-id",
            "r-foreign",
            "--request-key",
            "e2e-auto-foreign",
        ],
    )
    .await;
    assert_eq!(code, "NOT_FOUND");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_workflow_plan_submit_and_cleanup_over_transport() {
    if !git_present() {
        return;
    }
    let home = TestHome::new("aw-workflow");
    let root = home.path();
    init_home(root);
    let (project_id, workspace) = seed_project(root, "wf").await;

    // 目标恰选其一。
    let code = cli_failure(
        root,
        &["workflow", "plan", "--output", "json", "--prompt", "hello"],
    )
    .await;
    assert_eq!(code, "INVALID_ARGUMENT");
    let code = cli_failure(
        root,
        &[
            "workflow",
            "plan",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--repository-url",
            "https://example.invalid/x.git",
            "--prompt",
            "hello",
        ],
    )
    .await;
    assert_eq!(code, "INVALID_ARGUMENT");
    // PR 工件无公开操作对应。
    let code = cli_failure(
        root,
        &[
            "workflow",
            "plan",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--prompt",
            "hello",
            "--artifacts",
            "pullRequest",
        ],
    )
    .await;
    assert_eq!(code, "UNAVAILABLE");

    // 计划形态（无发布动作时无 push 步骤）。
    let plan = cli_json(
        root,
        &[
            "workflow",
            "plan",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--prompt",
            "change the readme",
            "--artifacts",
            "branch,commit",
        ],
    )
    .await;
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0]["stepId"], "run");
    assert_eq!(steps[1]["stepId"], "branch");
    assert_eq!(steps[2]["stepId"], "commit");
    assert_eq!(plan["temporaryResources"], json!([]));
    let plan_id = plan["planId"].as_str().unwrap().to_owned();
    let plan_hash = plan["planHash"].as_str().unwrap().to_owned();

    // 工作区留未提交修改（commit 步素材）。
    std::fs::write(workspace.join("wf-e2e.txt"), "workflow e2e\n").unwrap();

    // push 未授权（plan 无 push 步骤 → 不需要授权）；提交执行组合。
    let receipt = cli_json(
        root,
        &[
            "workflow",
            "submit",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &operation_id).await;
    let steps = completion["steps"].as_array().unwrap();
    for step in steps {
        assert_eq!(step["state"], "succeeded", "步骤状态可查询：{step}");
    }
    let workflow_id = completion["workflowId"].as_str().unwrap().to_owned();
    assert!(completion["runId"].as_str().is_some());
    // git 效果：分支 + 提交（组合的公开 git 操作语义落盘）。
    git_sync(
        &workspace,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/agent-workflow/{workflow_id}"),
        ],
    );

    // 计划一次性：再次提交 → PLAN_STALE。
    let code = cli_failure(
        root,
        &[
            "workflow",
            "submit",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--request-key",
            "e2e-wf-2",
        ],
    )
    .await;
    assert_eq!(code, "PLAN_STALE");

    // 外来资源清理拒绝。
    let code = cli_failure(
        root,
        &[
            "workflow",
            "cleanup",
            "--output",
            "json",
            "--workflow-id",
            &workflow_id,
            "--resource-ids",
            "foreign",
            "--request-key",
            "e2e-wf-cleanup-foreign",
        ],
    )
    .await;
    assert_eq!(code, "RESOURCE_NOT_OWNED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_workflow_repository_clone_and_cleanup_over_transport() {
    if !git_present() {
        return;
    }
    let home = TestHome::new("aw-clone");
    let root = home.path();
    init_home(root);

    // 本地 bare 源（file://，零网络）。
    let source = home.path().parent().unwrap().join("aw-clone-source");
    std::fs::create_dir_all(&source).expect("create source");
    git_sync(&source, &["init", "-q", "-b", "main"]);
    git_sync(&source, &["config", "user.email", "src@example.invalid"]);
    git_sync(&source, &["config", "user.name", "Source"]);
    std::fs::write(source.join("README.md"), "# source\n").unwrap();
    git_sync(&source, &["add", "-A"]);
    git_sync(&source, &["commit", "-q", "-m", "init"]);
    let bare = home.path().parent().unwrap().join("aw-clone-source.git");
    git_sync(
        &source,
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    let url = format!("file://{}", bare.display());

    let plan = cli_json(
        root,
        &[
            "workflow",
            "plan",
            "--output",
            "json",
            "--repository-url",
            &url,
            "--prompt",
            "inspect the repo",
        ],
    )
    .await;
    assert_eq!(plan["temporaryResources"], json!(["temp-clone"]));
    let plan_id = plan["planId"].as_str().unwrap().to_owned();
    let plan_hash = plan["planHash"].as_str().unwrap().to_owned();

    let receipt = cli_json(
        root,
        &[
            "workflow",
            "submit",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--plan-hash",
            &plan_hash,
            "--request-key",
            "e2e-clone-1",
        ],
    )
    .await;
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &operation_id).await;
    let workflow_id = completion["workflowId"].as_str().unwrap().to_owned();
    assert!(
        !completion["projectId"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
    assert_eq!(
        completion["ownedTemporaryResourceIds"],
        json!(["clone"]),
        "克隆登记为 workflow 拥有的临时资源"
    );

    // cleanup 移除本 workflow 的临时克隆。
    let receipt = cli_json(
        root,
        &[
            "workflow",
            "cleanup",
            "--output",
            "json",
            "--workflow-id",
            &workflow_id,
            "--resource-ids",
            "clone",
            "--request-key",
            "e2e-cleanup-1",
        ],
    )
    .await;
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &operation_id).await;
    assert_eq!(completion["removedResourceIds"], json!(["clone"]));
    assert_eq!(completion["retainedResourceIds"], json!([]));
}
