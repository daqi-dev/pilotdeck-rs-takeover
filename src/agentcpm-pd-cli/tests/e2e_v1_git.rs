//! t014（Git 能力）端到端场景：16 个 `git.*` / `git_identity.*` 操作经真实
//! CLI 二进制与 `/v1/ws` 传输。
//!
//! 装置（brief `_tmp/20261118-s3/briefs/git.md` §6）：
//! - `HOME=<TestHome>`、`GIT_CONFIG_GLOBAL=/dev/null`、
//!   `GIT_CONFIG_SYSTEM=/dev/null`——宿主全局 git config（身份、gpgsign）
//!   不泄漏进 Gateway 派生的 git 进程，`IDENTITY_REQUIRED` 才可确定验收；
//! - 授权根播种走 `V1Store::declare_authorized_root`（t013 先例；t012 公开
//!   声明操作落地前的等价入口）；
//! - push/publish/fetch/pull 用本地 bare 仓库当 remote（路径 remote，
//!   零网络）；
//! - 用例开头 `git --version` 探测，失败 skip 并注明。
//!
//! `expectedStateToken` 是 secure 输入字段，一律经 `--input-json` 文件提交。
//! completionResult 的深查询依赖 `operations.get`（collections 并行任务）：
//! e2e 层先验收 receipt 形态与文件系统效果，域内测试覆盖 completionResult。
//!
//! 场景对应 design.json 关联用例（c022/c023/contract.git-state/
//! git.operation.\*/source-flow 等）的核心断言面：stateToken 前后变化、
//! 两段恢复重验、远端 CAS、uncommit、分支生命周期、identity 读写。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::{BIN, CLI_TIMEOUT, CliOutcome, TestHome, expect_ok, init_home, init_home_instance};

/// Runs one `agentcpm-pd` argv invocation to completion from a synchronous
/// context, with the isolated git environment (host global git config —
/// identity, gpgsign — must not leak into the gateway's git children). A
/// plain `std::process` implementation like `common_v1::await_cli`, so it is
/// independent of the tokio runtime flavor: the scenarios run on the default
/// `#[tokio::test]` current_thread runtime, where the previous
/// `block_in_place` bridge panicked ("can call blocking only when running on
/// the multi-threaded runtime"). Pipes drain on helper threads and the wait is
/// bounded, exactly like `await_cli`.
fn await_git_cli(home: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = std::process::Command::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    let mut child = cmd.spawn().expect("spawn agentcpm-pd");
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_handle = std::thread::spawn(move || read_pipe_to_end(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe_to_end(stderr_pipe));
    let deadline = std::time::Instant::now() + CLI_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait agentcpm-pd") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("cli invocation timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_handle.join().expect("read cli stdout");
    let stderr = stderr_handle.join().expect("read cli stderr");
    CliOutcome {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

/// Reads one child pipe to EOF on its helper thread.
fn read_pipe_to_end(mut pipe: impl std::io::Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
    buf
}

/// Fixture-side system git (same isolation as the gateway's spawns).
fn git_sync(cwd: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
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

fn git_present() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// 播种一个带真实授权根与 git 仓库的项目（宿主声明面等价入口）。
async fn seed_git_project(tag: &str) -> (TestHome, PathBuf, String) {
    let home = TestHome::new(tag);
    init_home(home.path());
    let instance_id = init_home_instance(home.path());

    // 授权根放在 TestHome 内部（每次运行唯一，随 home 一起清理）。之前的
    // `home.path().parent()` 落在共享的 $TMPDIR 上：上一次运行留下的
    // `.git` 让 `git init` 变 re-init、`remote add` 报已存在、种子提交
    // 报 nothing to commit，且从不清理。
    let declared_root = home.path().join(format!("{tag}-authorized"));
    let workspace = declared_root.join("ws");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    {
        let (file, _token) =
            pilotdeck_gateway::public_v1::bootstrap::load_local_instance_and_token(
                home.path(),
                None,
            )
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
        let store =
            pilotdeck_gateway::public_v1::store::V1Store::open_and_recover(home.path(), meta)
                .expect("open store for seeding");
        store
            .declare_authorized_root(&format!("root-{tag}"), &declared_root)
            .expect("declare authorized root");
    }
    git_sync(&workspace, &["init", "-q", "-b", "main"]);
    git_sync(&workspace, &["config", "commit.gpgsign", "false"]);

    let project_id = format!("p-{tag}");
    let out = await_git_cli(
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "T014 E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"root-{tag}\",\"relativePath\":\"ws\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    (home, workspace, project_id)
}

fn status_state(home: &Path, project_id: &str) -> Value {
    let out = await_git_cli(
        home,
        &[
            "git",
            "status",
            "--output",
            "json",
            "--project-id",
            project_id,
        ],
    );
    expect_ok("git.status", &out)["state"].clone()
}

fn state_token(state: &Value) -> String {
    state["stateToken"].as_str().expect("stateToken").to_owned()
}

/// Writes an input object to a file and returns the `--input-json` argv pair.
fn input_json_file(home: &Path, name: &str, input: &Value) -> (String, String) {
    let path = home.join(format!("{name}.json"));
    std::fs::write(&path, input.to_string()).unwrap();
    (
        "--input-json".to_owned(),
        path.to_str().expect("utf8 path").to_owned(),
    )
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

fn dispatch_input(home: &Path, name: &str, method_path: &[&str], input: &Value) -> CliOutcome {
    let (flag, file) = input_json_file(home, name, input);
    let mut args: Vec<String> = Vec::new();
    for part in method_path {
        args.push((*part).to_owned());
    }
    args.push("--output".to_owned());
    args.push("json".to_owned());
    args.push(flag);
    args.push(file);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    await_git_cli(home, &refs)
}

/// c022/contract.git-state：unborn 状态、未跟踪文件、stateToken；身份缺失
/// 拒绝提交；设置项目身份后 initial commit + uncommit 回到 unborn 且内容
/// 仍暂存（TS 基线 soft reset 语义）。
#[tokio::test]
async fn status_commit_uncommit_flow_with_identity_gate() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-flow").await;
    write_file(&workspace.join("hello.txt"), "hello\n");

    let state = status_state(home.path(), &project_id);
    assert_eq!(state["unborn"], json!(true));
    assert_eq!(state["remoteNames"], json!([]));
    let files = state["files"].as_array().expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"]["value"], "hello.txt");
    assert_eq!(files[0]["untracked"], json!(true));
    let token = state_token(&state);

    // 隔离环境下没有任何身份：IDENTITY_REQUIRED（不产生提交）。
    let out = dispatch_input(
        home.path(),
        "commit-no-id",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
            "message": "Initial commit",
            "initial": true,
        }),
    );
    assert_eq!(expect_failure("git.commit", &out), "IDENTITY_REQUIRED");

    // 设置项目身份（expectedRevision="" 表示当前未设置）。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--name",
            "E2E User",
            "--email",
            "e2e@example.com",
            "--expected-revision",
            "",
        ],
    );
    expect_ok("git_identity.set", &out);

    // 同 key 重放拒绝：换个 key 后提交成功（initial commit）。
    let state = status_state(home.path(), &project_id);
    let token = state_token(&state);
    let out = dispatch_input(
        home.path(),
        "commit-initial",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
            "message": "Initial commit",
            "initial": true,
        }),
    );
    let receipt = expect_ok("git.commit", &out);
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["state"], "succeeded");

    let state = status_state(home.path(), &project_id);
    assert_eq!(state["unborn"], json!(false));
    let head = state["headOid"].as_str().expect("headOid").to_owned();
    let token_after = state_token(&state);
    assert_ne!(token, token_after, "stateToken must change on commit");

    // 过期 stateToken 拒绝。
    let out = dispatch_input(
        home.path(),
        "commit-stale",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
            "message": "Second commit",
        }),
    );
    assert_eq!(expect_failure("git.commit", &out), "REVISION_CONFLICT");

    // uncommit（HEAD CAS）回到 unborn 且内容仍暂存。
    let out = dispatch_input(
        home.path(),
        "uncommit",
        &["git", "uncommit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token_after,
            "expectedHeadOid": head,
        }),
    );
    expect_ok("git.uncommit", &out);
    let state = status_state(home.path(), &project_id);
    assert_eq!(state["unborn"], json!(true));
    let files = state["files"].as_array().expect("files");
    assert!(
        files
            .iter()
            .any(|f| f["path"]["value"] == "hello.txt" && f["indexStatus"] == "A"),
        "uncommitted content stays staged: {files:?}"
    );
}

/// c023/git.operation.git.commit：INDEX_SCOPE_CONFLICT —— paths 选择不得
/// 静默扩大既有暂存。
#[tokio::test]
async fn commit_paths_selection_conflicts_with_foreign_staged_files() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-scope").await;
    // 用系统 git 造基线提交（绕过域内身份门，属装置数据）。
    write_file(&workspace.join("a.txt"), "a\n");
    write_file(&workspace.join("b.txt"), "b\n");
    git_sync(&workspace, &["add", "a.txt", "b.txt"]);
    git_sync(
        &workspace,
        &[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "-m",
            "seed",
        ],
    );
    // a.txt 的改动单独暂存（「外来暂存文件」），b.txt 保持未暂存修改。
    write_file(&workspace.join("a.txt"), "a2\n");
    git_sync(&workspace, &["add", "a.txt"]);
    write_file(&workspace.join("b.txt"), "b2\n");

    // 域内 git.commit 走身份门：先设置项目身份（种子提交是装置用系统 git
    // 造的，不经过门）。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--name",
            "E2E User",
            "--email",
            "e2e@example.com",
            "--expected-revision",
            "",
        ],
    );
    expect_ok("git_identity.set", &out);

    let state = status_state(home.path(), &project_id);
    let token = state_token(&state);
    let out = dispatch_input(
        home.path(),
        "commit-paths",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {
                "mode": "paths",
                "paths": [{"encoding": "utf8", "value": "b.txt"}],
            },
            "message": "Only b",
        }),
    );
    assert_eq!(
        expect_failure("git.commit", &out),
        "INDEX_SCOPE_CONFLICT",
        "a.txt is staged outside the selection"
    );

    // 显式 index 模式提交既有暂存。
    let out = dispatch_input(
        home.path(),
        "commit-index",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "index"},
            "message": "Staged only",
        }),
    );
    expect_ok("git.commit", &out);
}

/// git.operation.git.{create_branch,delete_branch,checkout}：CAS 锚点与
/// 脏工作树保护。
#[tokio::test]
async fn branch_lifecycle_with_delete_cas_and_dirty_checkout() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-branch").await;
    write_file(&workspace.join("seed.txt"), "v1\n");
    git_sync(&workspace, &["add", "seed.txt"]);
    git_sync(
        &workspace,
        &[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "-m",
            "seed",
        ],
    );
    let state = status_state(home.path(), &project_id);
    let token = state_token(&state);
    let head = state["headOid"].as_str().expect("headOid").to_owned();

    let out = dispatch_input(
        home.path(),
        "branch-create",
        &["git", "create-branch"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "branchName": "feature/e2e",
            "startOid": head,
        }),
    );
    expect_ok("git.create_branch", &out);

    // 删除当前分支被拒。
    let out = dispatch_input(
        home.path(),
        "branch-del-current",
        &["git", "delete-branch"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "branchName": "main",
            "expectedBranchOid": head,
        }),
    );
    assert_eq!(
        expect_failure("git.delete_branch", &out),
        "BRANCH_CHECKED_OUT"
    );

    // 错误的 expectedBranchOid 拒绝。
    let out = dispatch_input(
        home.path(),
        "branch-del-stale",
        &["git", "delete-branch"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "branchName": "feature/e2e",
            "expectedBranchOid": "0000000000000000000000000000000000000000",
        }),
    );
    assert_eq!(
        expect_failure("git.delete_branch", &out),
        "REVISION_CONFLICT"
    );

    // 正确 CAS 删除。
    let out = dispatch_input(
        home.path(),
        "branch-del-ok",
        &["git", "delete-branch"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "branchName": "feature/e2e",
            "expectedBranchOid": head,
        }),
    );
    expect_ok("git.delete_branch", &out);

    // 脏工作树 + 分叉：checkout 不得覆盖本地修改。
    let out = dispatch_input(
        home.path(),
        "branch-dirty",
        &["git", "create-branch"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "branchName": "feature/dirty",
            "startOid": head,
        }),
    );
    expect_ok("git.create_branch", &out);
    // 域内 git.commit 走身份门：先设置项目身份（种子提交是装置用系统 git
    // 造的，不经过门）。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--name",
            "E2E User",
            "--email",
            "e2e@example.com",
            "--expected-revision",
            "",
        ],
    );
    expect_ok("git_identity.set", &out);
    write_file(&workspace.join("seed.txt"), "v2\n");
    let out = dispatch_input(
        home.path(),
        "commit-diverge",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": status_state(home.path(), &project_id)
                ["stateToken"].as_str().unwrap(),
            "selection": {"mode": "all"},
            "message": "Diverge",
        }),
    );
    expect_ok("git.commit", &out);
    write_file(&workspace.join("seed.txt"), "dirty\n");
    let token = state_token(&status_state(home.path(), &project_id));
    let out = dispatch_input(
        home.path(),
        "checkout-dirty",
        &["git", "checkout"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "targetRef": "feature/dirty",
        }),
    );
    assert_eq!(expect_failure("git.checkout", &out), "DIRTY_WORKTREE");
    assert_eq!(
        std::fs::read_to_string(workspace.join("seed.txt")).unwrap(),
        "dirty\n",
        "the dirty change must survive the refusal"
    );
}

/// 两段式恢复：plan 报告逐路径冲突，restore 执行重验；性质变化使计划
/// 失效（PARTIAL_FAILURE）。
#[tokio::test]
async fn two_phase_restore_plans_validates_and_executes_per_path() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-restore").await;
    write_file(&workspace.join("tracked.txt"), "v1\n");
    git_sync(&workspace, &["add", "tracked.txt"]);
    git_sync(
        &workspace,
        &[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "-m",
            "seed",
        ],
    );
    write_file(&workspace.join("tracked.txt"), "v2\n");
    write_file(&workspace.join("untracked.txt"), "scratch\n");
    let token = state_token(&status_state(home.path(), &project_id));

    // discardTracked 计划对未跟踪路径报告冲突（不自动扩大范围）。
    let out = dispatch_input(
        home.path(),
        "plan-discard",
        &["git", "plan-restore"],
        &json!({
            "projectId": project_id,
            "action": "discardTracked",
            "paths": [
                {"encoding": "utf8", "value": "tracked.txt"},
                {"encoding": "utf8", "value": "untracked.txt"},
            ],
            "expectedStateToken": token,
        }),
    );
    let planned = expect_ok("git.plan_restore", &out);
    let plan = &planned["plan"];
    assert_eq!(plan["affectedResources"], json!(["tracked.txt"]));
    assert_eq!(plan["conflicts"][0]["resourceId"], "untracked.txt");
    assert_eq!(plan["destructive"], json!(true));
    let plan_id = plan["planId"].as_str().unwrap().to_owned();
    let plan_revision = plan["planRevision"].as_str().unwrap().to_owned();

    // 执行计划：tracked.txt 恢复，untracked.txt 不受影响。
    let out = dispatch_input(
        home.path(),
        "restore-ok",
        &["git", "restore"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "planId": plan_id,
            "expectedPlanRevision": plan_revision,
        }),
    );
    expect_ok("git.restore", &out);
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "v1\n"
    );
    assert!(workspace.join("untracked.txt").exists());

    // 计划执行后状态变化：同一计划重放被 REVISION_CONFLICT 拒绝。
    let out = dispatch_input(
        home.path(),
        "restore-replay",
        &["git", "restore"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "planId": plan_id,
            "expectedPlanRevision": plan_revision,
        }),
    );
    assert_eq!(expect_failure("git.restore", &out), "REVISION_CONFLICT");
}

/// 远端 CAS：expectedRemoteOid 空串=目标应不存在；过期 OID 拒绝；publish
/// 设置上游；fetch 更新远端跟踪 refs。
#[tokio::test]
async fn remote_cas_push_publish_fetch_pull() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-remote").await;
    // 本地 bare 仓库当 remote（零网络）。放在 TestHome 内：每次运行一个
    // 全新 bare（共享 $TMPDIR 里的旧 bare 会带着上次的 refs 干扰
    // expectedRemoteOid="" 的创建式断言）。
    let bare = home.path().join("git-e2e-remote-bare.git");
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&bare)
            .status()
            .expect("git init --bare")
            .success()
    );
    git_sync(
        &workspace,
        &["remote", "add", "origin", bare.to_str().expect("utf8 path")],
    );
    write_file(&workspace.join("a.txt"), "a\n");
    git_sync(&workspace, &["add", "a.txt"]);
    git_sync(
        &workspace,
        &[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "-m",
            "seed",
        ],
    );
    let state = status_state(home.path(), &project_id);
    let token = state_token(&state);
    let head = state["headOid"].as_str().expect("headOid").to_owned();
    assert_eq!(state["remoteNames"], json!(["origin"]));

    // expectedRemoteOid=""：目标应不存在（创建式推送成功）。
    let out = dispatch_input(
        home.path(),
        "push-create",
        &["git", "push"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "remoteName": "origin",
            "sourceRef": "main",
            "destinationRef": "main",
            "expectedRemoteOid": "",
        }),
    );
    expect_ok("git.push", &out);

    // 推送成功后 stateToken 已前进（远端跟踪 refs 变化）；重放/过期断言
    // 必须用新 token，否则先撞 REVISION_CONFLICT，到不了远端 CAS 判定。
    let token = state_token(&status_state(home.path(), &project_id));

    // 同一期望重放：远端已存在 → REMOTE_REF_CONFLICT。
    let out = dispatch_input(
        home.path(),
        "push-stale-empty",
        &["git", "push"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "remoteName": "origin",
            "sourceRef": "main",
            "destinationRef": "main",
            "expectedRemoteOid": "",
        }),
    );
    assert_eq!(expect_failure("git.push", &out), "REMOTE_REF_CONFLICT");

    // 过期具体 OID 同样拒绝。
    let out = dispatch_input(
        home.path(),
        "push-stale-oid",
        &["git", "push"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "remoteName": "origin",
            "sourceRef": "main",
            "destinationRef": "main",
            "expectedRemoteOid": "1111111111111111111111111111111111111111",
        }),
    );
    assert_eq!(expect_failure("git.push", &out), "REMOTE_REF_CONFLICT");

    // publish：远端推送 + 上游配置双段报告。
    let out = dispatch_input(
        home.path(),
        "publish",
        &["git", "publish"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "remoteName": "origin",
            "branchName": "main",
            "expectedRemoteOid": head,
        }),
    );
    expect_ok("git.publish", &out);
    let upstream = std::process::Command::new("git")
        .args(["config", "--get", "branch.main.remote"])
        .current_dir(&workspace)
        .output()
        .expect("git config");
    assert_eq!(String::from_utf8_lossy(&upstream.stdout).trim(), "origin");

    // 远端新提交（装置造出 behind）→ pull fastForwardOnly 前进一步。
    let clone_dir = home.path().join("git-e2e-remote-clone");
    std::process::Command::new("git")
        .args(["clone", "-q", "-b", "main"])
        .arg(&bare)
        .arg(&clone_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git clone");
    write_file(&clone_dir.join("remote.txt"), "from remote\n");
    // remote.txt 是新文件：`commit -a` 不暂存未跟踪文件，先显式 add。
    git_sync(&clone_dir, &["add", "remote.txt"]);
    git_sync(
        &clone_dir,
        &[
            "-c",
            "user.name=Remote",
            "-c",
            "user.email=remote@example.com",
            "commit",
            "-m",
            "remote commit",
        ],
    );
    git_sync(&clone_dir, &["push", "-q", "origin", "main:main"]);
    let _ = std::fs::remove_dir_all(&clone_dir);

    // fetch：远端跟踪 refs 更新（stateToken 变化）。
    let token = state_token(&status_state(home.path(), &project_id));
    let out = dispatch_input(
        home.path(),
        "fetch",
        &["git", "fetch"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "remoteName": "origin",
        }),
    );
    expect_ok("git.fetch", &out);
    let token_after_fetch = state_token(&status_state(home.path(), &project_id));
    assert_ne!(token, token_after_fetch, "fetch must move remote refs");

    // pull fastForwardOnly：快进合入远端新提交。
    let out = dispatch_input(
        home.path(),
        "pull-ff",
        &["git", "pull"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token_after_fetch,
            "remoteName": "origin",
            "remoteBranch": "main",
            "strategy": "fastForwardOnly",
        }),
    );
    expect_ok("git.pull", &out);
    assert!(
        workspace.join("remote.txt").exists(),
        "pull must advance the worktree"
    );
    let state = status_state(home.path(), &project_id);
    let head_now = state["headOid"].as_str().unwrap().to_owned();
    assert_ne!(head_now, head);
}

/// suggest_message：模型缝合点未就绪时返回 MODEL_UNAVAILABLE，绝不以空串
/// 伪装成功（TS 基线的 "chore: update files" 回退已按合同移除）。
#[tokio::test]
async fn suggest_message_reports_model_unavailable_without_fake_success() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-suggest").await;
    write_file(&workspace.join("hello.txt"), "hello\n");
    let token = state_token(&status_state(home.path(), &project_id));
    let out = dispatch_input(
        home.path(),
        "suggest",
        &["git", "suggest-message"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
        }),
    );
    assert_eq!(
        expect_failure("git.suggest_message", &out),
        "MODEL_UNAVAILABLE"
    );
}

/// git_identity：revision CAS、读回、清除；项目身份覆盖产品默认。
#[tokio::test]
async fn identity_revision_cas_and_effective_precedence() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, _workspace, project_id) = seed_git_project("git-e2e-identity").await;

    // 隔离环境下未设置任何身份。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let got = expect_ok("git_identity.get", &out);
    assert_eq!(got["source"], json!("unset"));
    assert_eq!(got["revision"], json!(""));

    // 产品默认身份 + 项目身份：项目优先。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "productDefault",
            "--name",
            "Product Default",
            "--email",
            "default@example.com",
            "--expected-revision",
            "",
        ],
    );
    expect_ok("git_identity.set", &out);
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let got = expect_ok("git_identity.get", &out);
    assert_eq!(got["source"], json!("productDefault"));

    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--name",
            "Project User",
            "--email",
            "project@example.com",
            "--expected-revision",
            "",
        ],
    );
    let set = expect_ok("git_identity.set", &out);
    let revision = set["revision"].as_str().unwrap().to_owned();
    assert!(!revision.is_empty());

    // 旧 revision 再写 → REVISION_CONFLICT。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--name",
            "Other",
            "--email",
            "other@example.com",
            "--expected-revision",
            "",
        ],
    );
    assert_eq!(
        expect_failure("git_identity.set", &out),
        "REVISION_CONFLICT"
    );

    // 非法 email → INVALID_IDENTITY。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--name",
            "Other",
            "--email",
            "not-an-email",
            "--expected-revision",
            &revision,
        ],
    );
    assert_eq!(expect_failure("git_identity.set", &out), "INVALID_IDENTITY");

    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let got = expect_ok("git_identity.get", &out);
    assert_eq!(got["source"], json!("project"));
    assert_eq!(got["name"], json!("Project User"));

    // 清除项目身份后回落到产品默认。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--clear",
            "--expected-revision",
            &revision,
        ],
    );
    expect_ok("git_identity.set", &out);
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let got = expect_ok("git_identity.get", &out);
    assert_eq!(got["source"], json!("productDefault"));
}

/// git.inspect：六个 view 的 artifact 形态（receipt 层验收：dataSchemaId +
/// dataArtifact 描述符经 schema 校验）。
#[tokio::test]
async fn inspect_views_return_validated_artifacts() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-inspect").await;
    write_file(&workspace.join("hello.txt"), "hello\n");
    write_file(&workspace.join("中文.txt"), "中文内容\n");
    git_sync(&workspace, &["add", "hello.txt", "中文.txt"]);
    git_sync(
        &workspace,
        &[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "-m",
            "seed",
        ],
    );
    write_file(&workspace.join("hello.txt"), "hello v2\n");

    for view in ["diff", "commits", "branches", "remoteStatus"] {
        let out = await_git_cli(
            home.path(),
            &[
                "git",
                "inspect",
                "--output",
                "json",
                "--project-id",
                &project_id,
                "--view",
                view,
            ],
        );
        let result = expect_ok("git.inspect", &out);
        assert_eq!(result["view"], json!(view), "view {view}");
        assert_eq!(
            result["dataSchemaId"],
            json!(format!("git.{view}.v1")),
            "view {view}"
        );
        assert!(
            result["dataArtifact"]["artifactId"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "view {view} must carry an artifact descriptor"
        );
    }

    // fileWithDiff（带 path）与 commitDiff（带 targetOid）。
    let head = status_state(home.path(), &project_id)["headOid"]
        .as_str()
        .expect("headOid")
        .to_owned();
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "inspect",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--view",
            "fileWithDiff",
            "--path",
            "{\"encoding\":\"utf8\",\"value\":\"hello.txt\"}",
        ],
    );
    let result = expect_ok("git.inspect", &out);
    assert_eq!(result["dataSchemaId"], json!("git.fileWithDiff.v1"));

    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "inspect",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--view",
            "commitDiff",
            "--target-oid",
            &head,
        ],
    );
    let result = expect_ok("git.inspect", &out);
    assert_eq!(result["dataSchemaId"], json!("git.commitDiff.v1"));
}

/// fx-h P1：unborn HEAD 上 `git.inspect --view diff` 返回变更集而非
/// INVALID_REF。TS 基线（git.js:409-424）的 /diff 从不点名 HEAD——先
/// unstaged `git diff -- <path>`，回退 staged `git diff --cached -- <path>`，
/// 两者在 unborn 分支都可解析（`--cached` 对空树）；变更集视图按同一
/// per-path 偏好合并（a.txt 保留 unstaged 增量、untracked 合成新增），产物
/// payload 从实例 home 读回断言。
#[tokio::test]
async fn inspect_diff_on_unborn_head_returns_changeset() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-unborn-diff").await;
    write_file(&workspace.join("a.txt"), "line1\nline2\n");
    git_sync(&workspace, &["add", "a.txt"]);
    write_file(&workspace.join("a.txt"), "line1\nline2 modified\n");
    write_file(&workspace.join("u.txt"), "untracked\n");

    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "inspect",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--view",
            "diff",
        ],
    );
    let result = expect_ok("git.inspect", &out);
    assert_eq!(result["dataSchemaId"], json!("git.diff.v1"));
    let artifact_id = result["dataArtifact"]["artifactId"]
        .as_str()
        .expect("artifactId")
        .to_owned();
    let bytes = std::fs::read(home.path().join("v1").join("artifacts").join(&artifact_id))
        .unwrap_or_else(|e| panic!("read artifact {artifact_id}: {e}"));
    let payload: Value = serde_json::from_slice(&bytes).expect("artifact JSON");
    let files = payload["files"].as_array().expect("files");
    let a = files
        .iter()
        .find(|f| f["newPath"]["value"] == "a.txt")
        .unwrap_or_else(|| panic!("a.txt missing from unborn diff: {files:?}"));
    assert_eq!(
        a["change"],
        json!("modified"),
        "the unstaged delta must win over the staged addition: {a:?}"
    );
    let u = files
        .iter()
        .find(|f| f["newPath"]["value"] == "u.txt")
        .unwrap_or_else(|| panic!("untracked file missing from unborn diff: {files:?}"));
    assert_eq!(u["change"], json!("added"));
}

/// fx-h P1：失败 mutation 的幂等收据进入终态。同一 `--request-key` 重试
/// 确定性失败的 `git.commit`（NOTHING_TO_COMMIT）拿到同一确定性错误——
/// 修复前这里回放的是 `deduplicated=true, state="running"` 的假终态收据
/// （exit 0），客户端永远等不到终态。
#[tokio::test]
async fn failed_commit_same_key_retry_reports_terminal_failure() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id) = seed_git_project("git-e2e-idem-fail").await;
    write_file(&workspace.join("hello.txt"), "hello\n");

    // 身份 + initial commit（seed 一个干净的已提交树）。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            &project_id,
            "--name",
            "E2E User",
            "--email",
            "e2e@example.com",
            "--expected-revision",
            "",
        ],
    );
    expect_ok("git_identity.set", &out);
    let token = state_token(&status_state(home.path(), &project_id));
    let out = dispatch_input(
        home.path(),
        "commit-idem-seed",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
            "message": "seed",
            "initial": true,
        }),
    );
    expect_ok("git.commit", &out);

    // 确定性失败（干净树上 selection.mode=index 无可提交内容）。
    let token = state_token(&status_state(home.path(), &project_id));
    let input = json!({
        "projectId": project_id,
        "expectedStateToken": token,
        "selection": {"mode": "index"},
        "message": "empty",
    });
    let (flag, file) = input_json_file(home.path(), "commit-idem-empty", &input);
    let dispatch = || {
        await_git_cli(
            home.path(),
            &[
                "git",
                "commit",
                "--output",
                "json",
                "--request-key",
                "k-e2e-idem-fail",
                flag.as_str(),
                file.as_str(),
            ],
        )
    };
    let out = dispatch();
    assert_eq!(expect_failure("git.commit", &out), "NOTHING_TO_COMMIT");

    // 同 key + 同输入：重放确定性错误，而非永不合局的 running 收据。
    let out = dispatch();
    assert_eq!(
        expect_failure("git.commit", &out),
        "NOTHING_TO_COMMIT",
        "same-key retry must replay the deterministic failure, not a running receipt"
    );

    // 新 key 重新执行（correctRejectedInputWithNewKey）且同样确定性失败。
    let out = await_git_cli(
        home.path(),
        &[
            "git",
            "commit",
            "--output",
            "json",
            "--request-key",
            "k-e2e-idem-fail-2",
            flag.as_str(),
            file.as_str(),
        ],
    );
    assert_eq!(expect_failure("git.commit", &out), "NOTHING_TO_COMMIT");
}
