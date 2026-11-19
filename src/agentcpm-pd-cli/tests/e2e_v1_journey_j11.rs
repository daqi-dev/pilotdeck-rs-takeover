//! S5 业务旅程 J11（任务编排与自动化）端到端验收。
//!
//! 旅程正本是 `_tmp/20261119-s5/journeys-execution-plan.md` §3 J11：单一
//! 用户在一个实例上完成「集成探测（未配置态）→ taskmaster 初始化与文档/
//! 任务链 → 探索计划执行与报告 → 工作周期应用与归档 → 外部请求组合流程
//! （Git 产物）→ 自动化绑定、重跑与终止」的完整业务闭环。命令名按计划 §2
//! 映射表改写为实际 CLI 形态（`taskmaster read-documents/write-document/
//! parse-document/list-tasks/write-task`、`work-cycles`、`workflow
//! plan/submit`、`automation rerun-plan` 等）。
//!
//! 两条场景（每条一个 `#[tokio::test]`，全部驱动真实 `agentcpm-pd` 二进制、
//! 隔离 `AGENTCPM_PD_HOME`、`--output json` envelope 经嵌入 registry 的
//! schema 校验；git 相关场景带 `HOME=<home>` + `GIT_CONFIG_{GLOBAL,SYSTEM}=
//! /dev/null` 隔离）：
//!
//! * 主链（taskmaster → discovery → work-cycles → automation）：
//!   `integrations detect` 未配置态 → `taskmaster initialize`（操作事件流
//!   逐页核对 started→completed + 游标严格递增；`.taskmaster` 落盘）→
//!   detect 反映已初始化 → 文档（手写 PRD + 模板生成）→ parse 生成任务 →
//!   write-task 三模式 → discovery 上下文/目录/执行/报告 → work-cycles
//!   应用（落盘字节）与归档（不删工作区）→ automation 未配置显式
//!   UNAVAILABLE → apply/abort → rerun-plan（新 run + previousRunId 关联 +
//!   同键重放幂等）。
//! * 组合流程分支（Git 产物 + 两段式门）：`git identity set` → `workflow
//!   plan`（目标恰选其一、PR 工件 UNAVAILABLE）→ 篡改 planHash →
//!   PLAN_STALE → `workflow submit`（分支落盘 `refs/heads/agent-workflow/
//!   <workflowId>`）→ 已消费计划重放 → PLAN_STALE → 外来资源清理拒绝 →
//!   本地裸仓克隆变体（temporaryResources + cleanup 交接）。
//!
//! 与计划的差异（详见 `_tmp/20261119-s5/w2b-report.md`）：
//! * **TTL 注入缝**：`agent_workflow.plan` 的两段式 TTL 是编译常量
//!   `PLAN_TTL_SECS = 600`（agent_workflow.rs:87），无注入缝，且 plan 结果
//!   不投影 `expiresAt`（planId/planHash/steps/temporaryResources）。按计划
//!   的替代方案取「缩小语义」：本轮以两个可达的 PLAN_STALE 故障点（篡改
//!   planHash、已消费计划重放）覆盖该门，不做 10 分钟真实等待。
//! * 计划/周期播种走域生产者缝隙 `record_discovery_plan` /
//!   `record_work_cycle`（发现 runtime 的唯一写入面；e2e 同入口先例）；
//!   taskmaster 文档与任务全走公开 CLI。
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

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code.
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert!(
        out.code != 0,
        "`{method}` must exit non-zero on a contract failure (got {}); stderr: {}",
        out.code,
        out.stderr
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
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON ({e}): {}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` outcome: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The envelope's `result` when the invocation succeeded/accepted, `None` for
/// a failure envelope (poll windows tolerate NOT_FOUND).
fn ok_result(out: &CliOutcome) -> Option<Value> {
    let line = out.stdout.lines().find(|l| !l.trim().is_empty())?;
    let value: Value = serde_json::from_str(line).ok()?;
    if value["outcome"] == "succeeded" || value["outcome"] == "accepted" {
        Some(value["result"].clone())
    } else {
        None
    }
}

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

/// One argv invocation with git isolation (HOME = the test home; no ambient
/// git config), so the workflow's git steps are deterministic.
async fn cli(root: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = base_command(root);
    cmd.args(args)
        .env("HOME", root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let child = cmd.spawn().expect("spawn agentcpm-pd");
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output())
        .await
        .expect("cli invocation timed out")
        .expect("wait for cli");
    CliOutcome {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Runs one argv command and validates its envelope against the embedded
/// registry. Domain prefixes that differ from the CLI group word are mapped
/// here (`workflow` → `agent_workflow`, `work-cycles` → `work_cycles`, …).
async fn op(root: &Path, args: &[&str]) -> Value {
    let out = cli(root, args).await;
    let domain = match args[0] {
        "work-cycles" => "work_cycles",
        "workflow" => "agent_workflow",
        "projects" => "project",
        "sessions" => "session",
        other => other,
    };
    let method = if args.len() > 1 && !args[1].starts_with('-') {
        format!("{}.{}", domain, args[1].replace('-', "_"))
    } else {
        domain.to_owned()
    };
    expect_ok(&method, &out)
}

/// Polls `operations get` until the operation is terminal; every operation in
/// the positive journey must end `succeeded` (returns the completion result).
async fn wait_completion(root: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let got = op(
            root,
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
            assert_eq!(
                state, "succeeded",
                "operation `{operation_id}` ended `{state}`: {}",
                got["operation"]
            );
            return got["operation"]["result"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never became terminal (state `{state}`)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Reads one operation's whole event journal page by page (`--page
/// {"limit":1}` forces pagination) and asserts the journey's stream criterion:
/// the event types arrive exactly in the lifecycle order and the cursor
/// sequence is strictly increasing across pages.
async fn paged_operation_events(root: &Path, operation_id: &str) -> Vec<String> {
    let mut types = Vec::new();
    let mut cursors: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..32 {
        let mut args = vec![
            "operations".to_owned(),
            "events".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--operation-id".to_owned(),
            operation_id.to_owned(),
            "--page".to_owned(),
            "{\"limit\":1}".to_owned(),
        ];
        if let Some(cursor) = &after {
            args.push("--after-cursor".to_owned());
            args.push(cursor.clone());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let page = op(root, &refs).await;
        let frame = &page["frame"];
        for event in frame["events"].as_array().unwrap() {
            types.push(event["type"].as_str().unwrap().to_owned());
            let cursor = event["cursor"].as_str().unwrap().to_owned();
            if let Some(last) = cursors.last() {
                assert!(
                    cursor.trim_start_matches("cur-").parse::<u64>().unwrap()
                        > last.trim_start_matches("cur-").parse::<u64>().unwrap(),
                    "cursor sequence must be strictly increasing: {last} -> {cursor}"
                );
            }
            cursors.push(cursor);
        }
        if frame["terminal"] == true {
            assert_eq!(
                frame["caughtUp"], true,
                "a terminal frame must be caught up"
            );
            return types;
        }
        after = frame["nextCursor"].as_str().map(str::to_owned);
        assert!(
            after.is_some(),
            "a non-terminal page must carry the next cursor: {frame}"
        );
    }
    panic!("operations.events pagination did not converge for {operation_id}");
}

/// Polls `discovery get-report` until the report is terminal (the report row
/// appears with the execution; a NOT_FOUND window is tolerated).
async fn wait_report(root: &Path, project_id: &str, plan_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let out = cli(
            root,
            &[
                "discovery",
                "get-report",
                "--output",
                "json",
                "--project-id",
                project_id,
                "--plan-id",
                plan_id,
            ],
        )
        .await;
        if let Some(report) = ok_result(&out) {
            let state = report["state"].as_str().unwrap_or_default();
            if !state.is_empty() && state != "queued" && state != "running" {
                return report;
            }
        }
        assert!(
            Instant::now() < deadline,
            "discovery report for `{plan_id}` never became terminal"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Seeds a project whose authorized root is a real workspace directory (the
/// automation-suite precedent: `V1Store::declare_authorized_root` via the
/// producer seam), with a git repo when git is available (discovery's git
/// signal and the workflow branch). Returns (project id, workspace path).
async fn seed_project(home: &Path, tag: &str) -> (String, PathBuf) {
    let instance_id = init_home_instance(home);
    let declared_root = home.parent().unwrap().join(format!("{tag}-authorized"));
    // Clean up any state from a previous test run: git commit exits 1 when
    // there is nothing new to commit, so a stale repo breaks every rerun.
    let _ = std::fs::remove_dir_all(&declared_root);
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
    if git_present() {
        git_sync(&workspace, &["init", "-q", "-b", "main"]);
        git_sync(&workspace, &["config", "commit.gpgsign", "false"]);
        std::fs::write(workspace.join("README.md"), "# j11\n").unwrap();
        git_sync(&workspace, &["add", "-A"]);
        git_sync(&workspace, &["commit", "-q", "-m", "j11 init"]);
    }
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
            "J11 E2E",
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

const INIT_SCHEMA_ID: &str = "agentcpm-pd.taskmaster.init/1";

/// 旅程 J11 主链：taskmaster 文档与任务 → 探索计划 → 工作周期 → 自动化
/// 绑定与重跑。判据：`.taskmaster` 落盘；操作事件流 started→completed 且游
/// 标严格递增；rerun-plan 新 run + previousRunId 关联 + 同键重放幂等。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j11_taskmaster_discovery_workcycles_automation() {
    let home = TestHome::new("j11-e2e-main");
    let root = home.path();
    init_home(root);
    let (project_id, workspace) = seed_project(root, "j11").await;

    // ---- 1. 集成探测（未配置态）：项目在列、taskmaster 未初始化。
    let detected = op(root, &["integrations", "detect", "--output", "json"]).await;
    let projects = detected["projects"].as_array().cloned().unwrap_or_default();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["projectId"], project_id);
    assert_eq!(projects[0]["initialized"], false);
    assert_eq!(projects[0]["reasonCode"], "TASKMASTER_DIR_MISSING");

    // ---- 2. taskmaster initialize：receipt → 事件流 → .taskmaster 落盘。
    let receipt = op(
        root,
        &[
            "taskmaster",
            "initialize",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--schema-id",
            INIT_SCHEMA_ID,
            "--configuration",
            r#"{"rules":["default"]}"#,
            "--request-key",
            "j11-tm-init-1",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["durable"], true);
    let init_operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &init_operation_id).await;
    assert_eq!(completion["projectId"], project_id);
    assert_eq!(completion["initialized"], true);
    assert!(
        completion["createdRelativePaths"]
            .as_array()
            .is_some_and(|paths| paths.contains(&json!(".taskmaster/tasks/tasks.json"))),
        "createdRelativePaths must name the task set: {completion}"
    );
    assert!(completion["configurationRevision"].as_str().is_some());
    let taskmaster_dir = workspace.join(".taskmaster");
    let tasks_file = taskmaster_dir.join("tasks").join("tasks.json");
    assert!(tasks_file.is_file(), "the task set landed in the workspace");
    let task_set =
        serde_json::from_str::<Value>(&std::fs::read_to_string(&tasks_file).unwrap()).unwrap();
    // tasks.json 的落盘形状是 task-master 兼容的 tagged 形态
    // （`{master: {metadata, tasks}}`；读取端兼容 legacy 数组与简单
    // `{tasks}` 三形态，写入端固定 tagged——taskmaster.rs 的
    // `encode_task_set`）。空任务集 = master 标签下空数组。
    assert_eq!(
        task_set
            .as_object()
            .map(|obj| obj.keys().cloned().collect::<Vec<String>>()),
        Some(vec!["master".to_owned()]),
        "the on-disk task set lives under the master tag: {task_set}"
    );
    assert_eq!(
        task_set["master"]["tasks"].as_array().map(Vec::len),
        Some(0)
    );

    // 操作事件流：生命周期序 + 游标严格递增（一页一条强制翻页）。
    let types = paged_operation_events(root, &init_operation_id).await;
    assert_eq!(
        types,
        vec![
            "operation.started".to_owned(),
            "operation.completed".to_owned()
        ],
        "the initialize operation's lifecycle events in order"
    );

    // 重复初始化不覆盖已编辑结构。
    let out = cli(
        root,
        &[
            "taskmaster",
            "initialize",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--schema-id",
            INIT_SCHEMA_ID,
            "--configuration",
            "{}",
            "--request-key",
            "j11-tm-init-2",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("taskmaster.initialize", &out),
        "ALREADY_INITIALIZED"
    );

    // 探测现在反映已初始化。
    let detected = op(root, &["integrations", "detect", "--output", "json"]).await;
    let projects = detected["projects"].as_array().cloned().unwrap_or_default();
    assert_eq!(projects[0]["initialized"], true);
    assert_eq!(projects[0]["reasonCode"], Value::Null);

    // ---- 3. 文档：手写 PRD（两个小节）+ 模板生成 PRD。
    let created = op(
        root,
        &[
            "taskmaster",
            "write-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--content",
            "# Spec\n\n## Alpha\n\nDo the alpha step\n\n## Beta\n\nDo the beta step\n",
            "--expected-revision",
            "absent",
        ],
    )
    .await;
    let doc_revision = created["document"]["revision"].as_str().unwrap().to_owned();

    // absent-on-existing 不覆盖。
    let out = cli(
        root,
        &[
            "taskmaster",
            "write-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--content",
            "overwritten",
            "--expected-revision",
            "absent",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("taskmaster.write_document", &out),
        "REVISION_CONFLICT"
    );

    let generated = op(
        root,
        &[
            "taskmaster",
            "write-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "api-prd.md",
            "--template-id",
            "api",
            "--template-variables",
            r#"[{"name":"Date","value":"2026-11-19"}]"#,
            "--expected-revision",
            "absent",
        ],
    )
    .await;
    assert_eq!(generated["document"]["templateId"], "api");

    let read = op(
        root,
        &[
            "taskmaster",
            "read-documents",
            "--output",
            "json",
            "--kind",
            "prd",
            "--project-id",
            &project_id,
            "--document-id",
            "api-prd.md",
        ],
    )
    .await;
    assert!(
        read["items"][0]["content"]
            .as_str()
            .is_some_and(|content| content.contains("2026-11-19")),
        "the generated template carries the substituted date"
    );
    let listed = op(
        root,
        &[
            "taskmaster",
            "read-documents",
            "--output",
            "json",
            "--kind",
            "prd",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert!(items.len() >= 2, "both documents are listed: {listed}");
    assert!(items.iter().all(|item| item["content"].is_null()));
    assert!(listed["snapshotRevision"].as_str().is_some());
    let templates = op(
        root,
        &[
            "taskmaster",
            "read-documents",
            "--output",
            "json",
            "--kind",
            "template",
        ],
    )
    .await;
    assert_eq!(templates["items"].as_array().map(Vec::len), Some(4));
    // 文档落盘：`.taskmaster/docs/` 下逐个在。
    assert!(taskmaster_dir.join("docs").join("prd.txt").is_file());
    assert!(taskmaster_dir.join("docs").join("api-prd.md").is_file());

    // ---- 4. parse 生成任务 → list → write-task 三模式。
    let empty = op(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    let set_revision = empty["snapshotRevision"].as_str().unwrap().to_owned();

    let receipt = op(
        root,
        &[
            "taskmaster",
            "parse-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--document-revision",
            &doc_revision,
            "--expected-task-revision",
            &set_revision,
            "--request-key",
            "j11-tm-parse-1",
        ],
    )
    .await;
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["documentRevision"], doc_revision);
    assert_eq!(
        completion["createdTaskIds"].as_array().map(Vec::len),
        Some(2),
        "both sections became tasks: {completion}"
    );
    let first_task_id = completion["createdTaskIds"][0].as_str().unwrap().to_owned();

    // 过期任务集 revision 拒绝二次解析。
    let out = cli(
        root,
        &[
            "taskmaster",
            "parse-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--document-revision",
            &doc_revision,
            "--expected-task-revision",
            &set_revision,
            "--request-key",
            "j11-tm-parse-2",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("taskmaster.parse_document", &out),
        "REVISION_CONFLICT"
    );

    let listed = op(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["title"], "Alpha");
    assert_eq!(items[0]["status"], "pending");
    let first_task_revision = items[0]["revision"].as_str().unwrap().to_owned();

    // createGenerated：首行标题 + 次行描述。
    let receipt = op(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "createGenerated",
            "--prompt",
            "Add export\nSupport exporting the report",
            "--expected-revision",
            "absent",
            "--request-key",
            "j11-tm-add-1",
        ],
    )
    .await;
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    let task = &completion["task"];
    assert_eq!(task["title"], "Add export");
    assert_eq!(task["status"], "pending");
    let generated_id = task["taskId"].as_str().unwrap().to_owned();
    let generated_revision = task["revision"].as_str().unwrap().to_owned();

    // 未知依赖拒绝。
    let out = cli(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "createGenerated",
            "--prompt",
            "Depends on nothing",
            "--dependency-ids",
            r#"["99"]"#,
            "--expected-revision",
            "absent",
            "--request-key",
            "j11-tm-add-2",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("taskmaster.write_task", &out),
        "DEPENDENCY_CONFLICT"
    );

    // setStatus 只动状态。
    let receipt = op(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "setStatus",
            "--task-id",
            &generated_id,
            "--status",
            "done",
            "--expected-revision",
            &generated_revision,
            "--request-key",
            "j11-tm-set-1",
        ],
    )
    .await;
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["task"]["status"], "done");
    assert_eq!(completion["task"]["title"], "Add export");
    let set_revision = completion["task"]["revision"].as_str().unwrap().to_owned();

    // updateFields 保留未给字段。
    let receipt = op(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "updateFields",
            "--task-id",
            &generated_id,
            "--priority",
            "high",
            "--expected-revision",
            &set_revision,
            "--request-key",
            "j11-tm-upd-1",
        ],
    )
    .await;
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["task"]["priority"], "high");
    assert_eq!(completion["task"]["status"], "done");

    // 过期任务 revision 拒绝更新。
    let out = cli(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "updateFields",
            "--task-id",
            &generated_id,
            "--priority",
            "low",
            "--expected-revision",
            &first_task_revision,
            "--request-key",
            "j11-tm-upd-2",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("taskmaster.write_task", &out),
        "REVISION_CONFLICT"
    );

    // 任务集读回 + next 跳过 done。
    let listed = op(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    )
    .await;
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 3, "two parsed + one generated: {listed}");
    let next = op(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--selection",
            "next",
        ],
    )
    .await;
    assert_eq!(next["items"][0]["taskId"], first_task_id);

    // ---- 5. 探索：上下文 → 目录 → 执行 → 报告。
    let context = op(
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
    assert!(
        context["context"].as_str().is_some_and(|c| !c.is_empty()),
        "the context text is non-empty"
    );
    assert_eq!(context["activePlanIds"], json!([]));
    let context_text = context["context"].as_str().unwrap().to_owned();
    assert!(context_text.contains("Project root:"), "{context_text}");
    if git_present() {
        assert!(
            context_text.contains("Git status:"),
            "untracked .taskmaster keeps the git signal visible: {context_text}"
        );
    }
    let out = cli(
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
    assert_eq!(expect_fail("discovery.get_context", &out), "NOT_FOUND");

    let page = op(
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

    // 播种计划（生产者缝隙）→ 目录可见。
    let revision = pilotdeck_gateway::public_v1::domains::discovery::record_discovery_plan(
        root,
        &project_id,
        "plan-j11",
        "J11 plan",
        "explore everything [ignored-directives]",
    )
    .expect("seed discovery plan");
    let page = op(
        root,
        &[
            "discovery",
            "list-plans",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--plan-id",
            "plan-j11",
        ],
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["revision"], revision);
    assert_eq!(page["items"][0]["title"], "J11 plan");

    // 未知计划 NOT_FOUND；过期 revision REVISION_CONFLICT。
    let out = cli(
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
            "j11-disc-missing",
        ],
    )
    .await;
    assert_eq!(expect_fail("discovery.execute", &out), "NOT_FOUND");
    let out = cli(
        root,
        &[
            "discovery",
            "execute",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--plan-id",
            "plan-j11",
            "--expected-revision",
            "stale",
            "--request-key",
            "j11-disc-stale",
        ],
    )
    .await;
    assert_eq!(expect_fail("discovery.execute", &out), "REVISION_CONFLICT");

    // 执行 → 报告关联 run 与计划版本。
    let receipt = op(
        root,
        &[
            "discovery",
            "execute",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--plan-id",
            "plan-j11",
            "--expected-revision",
            &revision,
            "--request-key",
            "j11-disc-1",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    let disc_completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    assert!(
        disc_completion["runId"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "the execution records its run: {disc_completion}"
    );
    let report = wait_report(root, &project_id, "plan-j11").await;
    assert_eq!(report["state"], "succeeded");
    let run_id = report["runId"].as_str().expect("report run").to_owned();
    assert!(!run_id.is_empty());
    assert!(report["reportArtifactId"].as_str().is_some());
    let content = report["content"].as_str().expect("report content");
    assert!(content.contains(&run_id), "报告关联本次 run");
    assert!(content.contains(&revision), "报告关联计划版本");

    // ---- 6. 工作周期：目录空 → 播种 → 漂移门 → 应用落盘 → 归档不删。
    let page = op(
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
    let out = cli(
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
            "j11-wc-missing",
        ],
    )
    .await;
    assert_eq!(expect_fail("work_cycles.apply", &out), "NOT_FOUND");

    let seeded = pilotdeck_gateway::public_v1::domains::work_cycles::record_work_cycle(
        root,
        &project_id,
        "cycle-j11",
        "j11 cycle",
        &[
            (
                "notes/applied.md".to_owned(),
                b"j11 cycle applied content\n".to_vec(),
            ),
            ("notes/second.md".to_owned(), b"j11 second file\n".to_vec()),
        ],
    )
    .expect("seed work cycle");
    let cycle_revision = seeded["revision"].as_str().unwrap().to_owned();
    let ws_revision = seeded["workspaceRevision"].as_str().unwrap().to_owned();

    // 工作区漂移 → REVISION_CONFLICT（目标路径首次出现，父目录先建）。
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/applied.md"), "drift").unwrap();
    let out = cli(
        root,
        &[
            "work-cycles",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-j11",
            "--expected-revision",
            &cycle_revision,
            "--expected-workspace-revision",
            &ws_revision,
            "--request-key",
            "j11-wc-drift",
        ],
    )
    .await;
    assert_eq!(expect_fail("work_cycles.apply", &out), "REVISION_CONFLICT");
    std::fs::remove_file(workspace.join("notes/applied.md")).unwrap();

    let receipt = op(
        root,
        &[
            "work-cycles",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-j11",
            "--expected-revision",
            &cycle_revision,
            "--expected-workspace-revision",
            &ws_revision,
            "--request-key",
            "j11-wc-1",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(
        completion["changedPaths"],
        json!(["notes/applied.md", "notes/second.md"]),
        "changedPaths 如实报告: {completion}"
    );
    assert!(completion["runId"].as_str().is_some());
    assert!(completion["recoveryArtifactId"].as_str().is_some());
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/applied.md")).unwrap(),
        "j11 cycle applied content\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/second.md")).unwrap(),
        "j11 second file\n"
    );

    let page = op(
        root,
        &[
            "work-cycles",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-j11",
        ],
    )
    .await;
    assert_eq!(page["items"][0]["state"], "applied");
    let applied_revision = page["items"][0]["revision"].as_str().unwrap().to_owned();
    let archived = op(
        root,
        &[
            "work-cycles",
            "archive",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--cycle-id",
            "cycle-j11",
            "--expected-revision",
            &applied_revision,
        ],
    )
    .await;
    assert_eq!(archived["cycle"]["state"], "archived");
    assert!(workspace.join("notes/applied.md").is_file(), "归档不删文件");

    // ---- 7. 自动化：未配置显式 UNAVAILABLE → apply → get → rerun → abort。
    let out = cli(
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
    assert_eq!(
        expect_fail("automation.get", &out),
        "UNAVAILABLE",
        "未配置的自动化显式不可用，不伪成功"
    );

    let receipt = op(
        root,
        &[
            "automation",
            "apply",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-j11",
            "--plan-id",
            "plan-j11",
            "--expected-revision",
            &revision,
            "--request-key",
            "j11-auto-apply-1",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    let first_run_id = completion["runId"]
        .as_str()
        .expect("automation run")
        .to_owned();
    assert!(!first_run_id.is_empty());

    let status = op(
        root,
        &[
            "automation",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-j11",
        ],
    )
    .await;
    assert_eq!(status["automationId"], "aw-j11");
    assert_eq!(status["status"]["available"], true);
    assert_eq!(status["planId"], "plan-j11");
    assert_eq!(status["lastResult"], "succeeded");

    // rerun-plan：新 run + previousRunId 关联；同键重放幂等（不重复应用）。
    let receipt = op(
        root,
        &[
            "automation",
            "rerun-plan",
            "--output",
            "json",
            "--request-key",
            "j11-auto-rerun-1",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-j11",
            "--plan-id",
            "plan-j11",
            "--previous-run-id",
            &first_run_id,
            "--expected-revision",
            &revision,
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    let rerun_operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let completion = wait_completion(root, &rerun_operation_id).await;
    let rerun_run_id = completion["runId"].as_str().expect("rerun run").to_owned();
    assert_ne!(rerun_run_id, first_run_id, "重跑必须显式创建新 run");
    assert_eq!(completion["previousRunId"], first_run_id);

    // 同键同输入重放：回放原回执，不再创建 run。
    let replay = op(
        root,
        &[
            "automation",
            "rerun-plan",
            "--output",
            "json",
            "--request-key",
            "j11-auto-rerun-1",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-j11",
            "--plan-id",
            "plan-j11",
            "--previous-run-id",
            &first_run_id,
            "--expected-revision",
            &revision,
        ],
    )
    .await;
    assert_eq!(replay["operationId"], rerun_operation_id.as_str());
    assert_eq!(replay["deduplicated"], true);

    // 外来 run → NOT_FOUND。
    let out = cli(
        root,
        &[
            "automation",
            "rerun-plan",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-j11",
            "--plan-id",
            "plan-j11",
            "--previous-run-id",
            "r-foreign",
            "--expected-revision",
            &revision,
            "--request-key",
            "j11-auto-foreign",
        ],
    )
    .await;
    assert_eq!(expect_fail("automation.rerun_plan", &out), "NOT_FOUND");

    // abort：已终态 run 回报终态，不改计划。
    let receipt = op(
        root,
        &[
            "automation",
            "abort",
            "--output",
            "json",
            "--request-key",
            "j11-auto-abort-1",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-j11",
            "--run-id",
            &first_run_id,
        ],
    )
    .await;
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["state"], "succeeded");
    let out = cli(
        root,
        &[
            "automation",
            "abort",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--automation-id",
            "aw-j11",
            "--run-id",
            "r-foreign",
            "--request-key",
            "j11-auto-abort-foreign",
        ],
    )
    .await;
    assert_eq!(expect_fail("automation.abort", &out), "NOT_FOUND");

    // 业务落盘：整个旅程的状态都进了 <home>/v1/state.sqlite。
    assert!(
        root.join("v1").join("state.sqlite").is_file(),
        "the journey's business state must land in state.sqlite"
    );
}

/// 旅程 J11 组合流程分支：workflow 的 Git 产物与两段式门（含克隆变体）。
/// PLAN_STALE 的两个可达故障点覆盖「两段式门」；真实 TTL 过期（10 分钟常
/// 量、无注入缝、plan 结果不投影 expiresAt）未做，见文件头差异登记。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j11_workflow_git_artifacts_and_two_phase_gates() {
    if !git_present() {
        return;
    }
    let home = TestHome::new("j11-e2e-workflow");
    let root = home.path();
    init_home(root);
    let (project_id, workspace) = seed_project(root, "j11wf").await;

    // 提交身份走公开 `git identity set`（隔离 home 无全局 git 配置，否则
    // commit 步会 IDENTITY_REQUIRED）。
    let out = cli(
        root,
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "productDefault",
            "--expected-revision",
            "",
            "--name",
            "J11 Workflow",
            "--email",
            "j11-workflow@example.invalid",
        ],
    )
    .await;
    expect_ok("git_identity.set", &out);

    // 目标恰选其一；PR 工件无公开操作对应。
    let out = cli(
        root,
        &["workflow", "plan", "--output", "json", "--prompt", "hello"],
    )
    .await;
    assert_eq!(expect_fail("agent_workflow.plan", &out), "INVALID_ARGUMENT");
    let out = cli(
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
    assert_eq!(expect_fail("agent_workflow.plan", &out), "INVALID_ARGUMENT");
    let out = cli(
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
    assert_eq!(expect_fail("agent_workflow.plan", &out), "UNAVAILABLE");

    // 计划形态（无发布动作 → 无 push 步骤）。
    let plan = op(
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

    // 篡改 planHash → PLAN_STALE（计划未被消费，真 hash 仍可提交）。
    let out = cli(
        root,
        &[
            "workflow",
            "submit",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--plan-hash",
            "tampered-hash",
            "--authorized-effects",
            "[]",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("agent_workflow.submit", &out),
        "PLAN_STALE",
        "a mismatched planHash must reject before any effect"
    );

    // 提交素材：工作区留未提交修改（commit 步素材）。
    std::fs::write(workspace.join("wf-j11.txt"), "j11 workflow e2e\n").unwrap();

    // push 未在计划内（无 push 步骤）：authorized-effects 空数组 = 无发布
    // 效果需要授权。
    let receipt = op(
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
            "--authorized-effects",
            "[]",
        ],
    )
    .await;
    assert_eq!(receipt["kind"], "operation");
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    for step in completion["steps"].as_array().unwrap() {
        assert_eq!(step["state"], "succeeded", "步骤状态可查询：{step}");
    }
    let workflow_id = completion["workflowId"].as_str().unwrap().to_owned();
    assert!(completion["runId"].as_str().is_some());
    // Git 产物：分支 + 提交落在工作区仓（组合的公开 git 操作语义）。
    git_sync(
        &workspace,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/agent-workflow/{workflow_id}"),
        ],
    );
    let committed = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=e2e",
            "-c",
            "user.email=e2e@test",
            "show",
            &format!("refs/heads/agent-workflow/{workflow_id}:wf-j11.txt"),
        ])
        .current_dir(&workspace)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git show");
    assert!(
        committed.status.success()
            && String::from_utf8_lossy(&committed.stdout) == "j11 workflow e2e\n",
        "the commit must carry the journey's change on the branch"
    );

    // 计划一次性：已消费计划重放 → PLAN_STALE。
    let out = cli(
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
            "--authorized-effects",
            "[]",
            "--request-key",
            "j11-wf-2",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("agent_workflow.submit", &out),
        "PLAN_STALE",
        "a consumed plan must not replay"
    );

    // 外来资源清理拒绝。
    let out = cli(
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
            "j11-wf-cleanup-foreign",
        ],
    )
    .await;
    assert_eq!(
        expect_fail("agent_workflow.cleanup", &out),
        "RESOURCE_NOT_OWNED"
    );

    // ---- 克隆变体：本地裸仓（file://，零网络）→ 临时克隆登记 → cleanup。
    let source = home.path().parent().unwrap().join("j11wf-clone-source");
    let bare = home.path().parent().unwrap().join("j11wf-clone-source.git");
    let _ = std::fs::remove_dir_all(&source);
    let _ = std::fs::remove_dir_all(&bare);
    std::fs::create_dir_all(&source).expect("create clone source");
    git_sync(&source, &["init", "-q", "-b", "main"]);
    git_sync(&source, &["config", "user.email", "src@example.invalid"]);
    git_sync(&source, &["config", "user.name", "Source"]);
    std::fs::write(source.join("README.md"), "# source\n").unwrap();
    git_sync(&source, &["add", "-A"]);
    git_sync(&source, &["commit", "-q", "-m", "init"]);
    git_sync(
        &source,
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    let url = format!("file://{}", bare.display());

    let plan = op(
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
    let clone_plan_id = plan["planId"].as_str().unwrap().to_owned();
    let clone_plan_hash = plan["planHash"].as_str().unwrap().to_owned();

    let receipt = op(
        root,
        &[
            "workflow",
            "submit",
            "--output",
            "json",
            "--plan-id",
            &clone_plan_id,
            "--plan-hash",
            &clone_plan_hash,
            "--authorized-effects",
            "[]",
            "--request-key",
            "j11-clone-1",
        ],
    )
    .await;
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    let clone_workflow_id = completion["workflowId"].as_str().unwrap().to_owned();
    assert!(
        !completion["projectId"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the clone variant registers its project: {completion}"
    );
    assert_eq!(
        completion["ownedTemporaryResourceIds"],
        json!(["clone"]),
        "克隆登记为 workflow 拥有的临时资源"
    );

    let receipt = op(
        root,
        &[
            "workflow",
            "cleanup",
            "--output",
            "json",
            "--workflow-id",
            &clone_workflow_id,
            "--resource-ids",
            "clone",
            "--request-key",
            "j11-cleanup-1",
        ],
    )
    .await;
    let completion = wait_completion(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(completion["removedResourceIds"], json!(["clone"]));
    assert_eq!(completion["retainedResourceIds"], json!([]));
}
