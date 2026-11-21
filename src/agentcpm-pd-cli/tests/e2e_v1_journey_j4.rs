//! S5 业务旅程 J4（快照与两段式恢复）端到端验收。
//!
//! 旅程正本是 `_tmp/20261119-s5/journeys-execution-plan.md` §3 J4：在已知
//! 文件树上做破坏性编辑并用两段式（plan → restore）恢复。命令名按计划 §2
//! 映射表改写（`snapshots plan_restore` → `snapshots plan-restore`、git 同
//! 形态）。
//!
//! **快照捕获接线状态（见 `_tmp/20261119-s5/snap-wire-report.md` §5 方案
//! A）**：PR-1 把捕获 seam 接进生产——files 域三写点登记捕获台账（A1）、
//! run 终态聚合提交（A2）、CLI 直写按 A6b 即时落 `run_id=NULL` 快照；
//! PR-2 补上 A5 的离线 driver 文件写指令
//! （`[pd-driver:write=<relpath>@<内容字面量>]`，走 files 域同一生产写路
//! 径），「run 完成快照自动产生 → plan-restore → 破坏 → restore → 落盘
//! 对照」全主干已闭合（w1a 报告 §3.1 (a) 已执行）。本文件验收：
//!
//! * 正向旅程一：`files write`×N 建已知树 + stat/read/list 往返 + 破坏性
//!   编辑（replace/move/delete）逐项落盘对照与 recovery artifact 读回 +
//!   snapshots 读面（A6b 直写快照可见、runId 精确过滤）与
//!   NOT_FOUND/INVALID_ARGUMENT 门；
//! * 正向旅程二（git 分支，全量可驱动）：`git plan-restore`（ChangePlan
//!   预览：planId + planRevision + expiresAt + affectedResources + conflicts，
//!   只读无副作用）→ 破坏性 `git restore` 执行 → 落盘逐项对照；
//! * 正向旅程三：run 归属快照的真实链路——driver 写指令建树 → run 终态
//!   后 `snapshots list/get` 出现 runId=turnId=runId 的快照（条目
//!   action=create + after 产物读回）→ 两段式恢复（含「受理后文件再改
//!   动 → REVISION_CONFLICT」门与 before/after 双方向）→ 落盘逐项对照；
//! * 故障注入：过期 expectedPlanRevision → REVISION_CONFLICT；计划生成后
//!   工作树再变动 → stateToken CAS 拒绝（计划失效）；已执行计划重放 →
//!   REVISION_CONFLICT。快照侧 `snapshots.restore` 的 PLAN_EXPIRED 需 10
//!   分钟真实等待或 W0 注入缝（F10，未就绪）。
//!
//! 全部驱动真实 `agentcpm-pd` 二进制、隔离 `AGENTCPM_PD_HOME`、`--output
//! json` envelope 经嵌入 registry 的 schema 校验；git 场景带
//! `GIT_CONFIG_GLOBAL=/dev/null` 隔离（t014 先例：宿主全局 git config 不泄
//! 漏进 Gateway 派生的 git 进程）。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use common_v1::{
    CliOutcome, TestHome, await_cli, expect_ok, init_home, init_home_instance, submit_run, wait_run,
};

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("base64")
}

/// 播种一个带真实授权根的项目（t012 声明面：`directories create` 触发
/// default 授权根的惰性 bootstrap 并创建工作区目录）。
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
            "J4 E2E",
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

fn path_arg(project_id: &str, relative: &str) -> String {
    json!({
        "projectId": project_id,
        "relativePath": {"encoding": "utf8", "value": relative},
    })
    .to_string()
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

async fn files_write(
    home: &Path,
    project_id: &str,
    relative: &str,
    bytes: &[u8],
    precondition: &str,
) -> Value {
    let out = await_cli(
        home,
        &[
            "files",
            "write",
            "--output",
            "json",
            "--path",
            &path_arg(project_id, relative),
            "--content",
            &format!("{{\"bytesBase64\":\"{}\"}}", b64_encode(bytes)),
            "--precondition",
            precondition,
        ],
    );
    expect_ok("files.write", &out)
}

async fn files_stat(home: &Path, project_id: &str, relative: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "files",
            "stat",
            "--output",
            "json",
            "--path",
            &path_arg(project_id, relative),
        ],
    );
    expect_ok("files.stat", &out)
}

/// 递归收集工作区文件树（跳过点开头条目——`.git` 与 staging 残留不进对照），
/// 返回 relative path → bytes 的有序映射。
fn walk_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(dir: &Path, prefix: String, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                visit(&path, format!("{prefix}{name}/"), out);
            } else {
                out.insert(
                    format!("{prefix}{name}"),
                    std::fs::read(&path).expect("read file"),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, String::new(), &mut out);
    out
}

/// 旅程 J4 正向一：已知文件树的建立、读回与破坏性编辑，以及 snapshots
/// 读面在当前构建下的诚实空集与错误门。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j4_file_tree_snapshot_surface_and_destructive_edits() {
    let (home, workspace, project_id, _iid) = seed_workspace("j4-files").await;
    let root = home.path();

    // ---- 建树：files write ×3（含一个先建目录的嵌套路径）。
    let a_v1 = b"j4 alpha one".to_vec();
    let b_v1 = b"j4 beta one".to_vec();
    let c_v1 = b"j4 gamma one".to_vec();

    let written = files_write(root, &project_id, "a.txt", &a_v1, "{\"state\":\"absent\"}").await;
    assert_eq!(written["entry"]["kind"], json!("file"));
    assert_eq!(written["entry"]["byteLength"], json!(a_v1.len()));
    let a_rev1 = written["entry"]["revision"].as_str().unwrap().to_owned();
    assert_eq!(std::fs::read(workspace.join("a.txt")).unwrap(), a_v1);

    let out = await_cli(
        root,
        &[
            "files",
            "create-directory",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "dir"),
            "--create-parents",
            "true",
        ],
    );
    let made = expect_ok("files.create_directory", &out);
    assert_eq!(made["createdPaths"].as_array().unwrap().len(), 1);

    files_write(
        root,
        &project_id,
        "dir/b.txt",
        &b_v1,
        "{\"state\":\"absent\"}",
    )
    .await;
    files_write(root, &project_id, "c.txt", &c_v1, "{\"state\":\"absent\"}").await;
    assert_eq!(
        std::fs::read(workspace.join("dir/b.txt")).unwrap(),
        b_v1,
        "the tree must be on disk exactly as written"
    );

    // ---- 读回：stat revision 一致、read 字节一致、list 顶层可见。
    let stat = files_stat(root, &project_id, "a.txt").await;
    assert_eq!(stat["entry"]["revision"], json!(a_rev1));
    let out = await_cli(
        root,
        &[
            "files",
            "read",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "a.txt"),
            "--max-bytes",
            "1024",
        ],
    );
    let read = expect_ok("files.read", &out);
    assert_eq!(read["chunk"]["byteLength"], json!(a_v1.len()));
    assert_eq!(read["chunk"]["eof"], json!(true));
    assert_eq!(
        b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
        a_v1
    );

    let out = await_cli(
        root,
        &[
            "files",
            "list",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, ""),
        ],
    );
    let listed = expect_ok("files.list", &out);
    let names: Vec<String> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| {
            e["path"]["relativePath"]["value"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert!(names.contains(&"a.txt".to_owned()));
    assert!(names.contains(&"c.txt".to_owned()));

    // ---- snapshots 读面（A6b，snap-wire §5）：CLI 直写的文件副作用即时
    // 产生 run 归缺席快照——建树的 3 次 files.write 各一条（create 条目带
    // after 内容产物）；传 runId 过滤时精确为空（合同 acceptance 双向满
    // 足：不传 runId 全可见 / 传 runId 精确过滤）。
    let out = await_cli(
        root,
        &[
            "snapshots",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let snapshots = expect_ok("snapshots.list", &out);
    assert_eq!(
        snapshots["snapshots"].as_array().unwrap().len(),
        3,
        "each direct files.write must leave one run-less snapshot"
    );
    assert!(snapshots["nextCursor"].is_null());
    for snapshot in snapshots["snapshots"].as_array().unwrap() {
        assert!(snapshot["runId"].is_null(), "direct writes carry no run");
        assert!(snapshot["turnId"].is_null());
        let entries = snapshot["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["action"], json!("create"));
        assert!(
            entries[0]["artifactId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "the after content must be productized"
        );
    }
    let out = await_cli(
        root,
        &[
            "snapshots",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--run-id",
            "run-never-existed",
        ],
    );
    let filtered = expect_ok("snapshots.list", &out);
    assert_eq!(filtered["snapshots"].as_array().unwrap().len(), 0);

    let out = await_cli(
        root,
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

    // 非法 direction 在存在性检查之前被拒（合同校验顺序）。
    let out = await_cli(
        root,
        &[
            "snapshots",
            "plan-restore",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--snapshot-ids",
            r#"["missing-snapshot"]"#,
            "--direction",
            "sideways",
        ],
    );
    assert_eq!(
        expect_failure("snapshots.plan_restore", &out),
        "INVALID_ARGUMENT"
    );

    let out = await_cli(
        root,
        &[
            "snapshots",
            "plan-restore",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--snapshot-ids",
            r#"["missing-snapshot"]"#,
            "--direction",
            "after",
        ],
    );
    assert_eq!(
        expect_failure("snapshots.plan_restore", &out),
        "NOT_FOUND",
        "a missing snapshot must never be presented as restorable"
    );

    let out = await_cli(
        root,
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
            "j4-snap-restore-1",
        ],
    );
    assert_eq!(expect_failure("snapshots.restore", &out), "NOT_FOUND");

    // ---- 破坏性编辑一：replace（旧内容进 recovery artifact，可读回）。
    let a_v2 = b"j4 alpha two (replaced)".to_vec();
    let replaced = files_write(
        root,
        &project_id,
        "a.txt",
        &a_v2,
        &format!("{{\"state\":\"revision\",\"revision\":\"{a_rev1}\"}}"),
    )
    .await;
    assert_eq!(replaced["previousRevision"], json!(a_rev1));
    let recovery = replaced["recoveryArtifactId"].as_str().unwrap().to_owned();
    assert!(!recovery.is_empty());
    let out = await_cli(
        root,
        &[
            "artifacts",
            "read",
            "--output",
            "json",
            "--artifact-id",
            &recovery,
            "--max-bytes",
            "4096",
        ],
    );
    let recovered = expect_ok("artifacts.read", &out);
    assert_eq!(
        b64_decode(recovered["chunk"]["bytesBase64"].as_str().unwrap()),
        a_v1,
        "the recovery artifact must hold the pre-replace bytes"
    );
    assert_eq!(std::fs::read(workspace.join("a.txt")).unwrap(), a_v2);

    // ---- 破坏性编辑二：move（源消失、目的落盘、字节一致）。
    let b_stat = files_stat(root, &project_id, "dir/b.txt").await;
    let b_rev = b_stat["entry"]["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "files",
            "move",
            "--output",
            "json",
            "--source",
            &path_arg(&project_id, "dir/b.txt"),
            "--destination",
            &path_arg(&project_id, "dir/renamed.txt"),
            "--expected-source-revision",
            &b_rev,
            "--destination-precondition",
            "{\"state\":\"absent\"}",
        ],
    );
    let moved = expect_ok("files.move", &out);
    assert!(
        moved["entry"]["path"]["relativePath"]["value"]
            .as_str()
            .unwrap()
            .ends_with("renamed.txt")
    );
    assert!(!workspace.join("dir/b.txt").exists());
    assert_eq!(
        std::fs::read(workspace.join("dir/renamed.txt")).unwrap(),
        b_v1
    );

    // ---- 破坏性编辑三：delete（recovery artifact + 磁盘消失）。
    let c_stat = files_stat(root, &project_id, "c.txt").await;
    let c_rev = c_stat["entry"]["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "files",
            "delete",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "c.txt"),
            "--expected-revision",
            &c_rev,
        ],
    );
    let deleted = expect_ok("files.delete", &out);
    let c_recovery = deleted["recoveryArtifactId"]
        .as_str()
        .is_some_and(|id| !id.is_empty());
    assert!(c_recovery, "delete must leave a recovery artifact");
    assert!(!workspace.join("c.txt").exists());

    // ---- 落盘逐项对照：最终树 = {a.txt: v2, dir/renamed.txt: b_v1}。
    let tree = walk_files(&workspace);
    let mut expected = BTreeMap::new();
    expected.insert("a.txt".to_owned(), a_v2);
    expected.insert("dir/renamed.txt".to_owned(), b_v1);
    assert_eq!(
        tree, expected,
        "the on-disk tree must match the post-edit expectation exactly"
    );
}

// ---------------------------------------------------------------------------
// git 分支：两段式恢复（plan → restore），真实 git 仓库 + 隔离 HOME
// ---------------------------------------------------------------------------

/// 带 git 环境隔离的 CLI 调用（t014 先例：宿主全局 git config——身份、
/// gpgsign——不得泄漏进 Gateway 派生的 git 进程）。委托给 `common_v1::await_cli`，
/// 其从首条命令起即隔离 `HOME`/`AGENTCPM_PD_INSTALL_ROOT`/`GIT_CONFIG_*` 并
/// 保留 `AGENTCPM_PD_ENABLE_OFFLINE_DIRECTIVES=1`。
fn await_git_cli(home: &Path, args: &[&str]) -> CliOutcome {
    await_cli(home, args)
}

/// 装置侧系统 git（与 Gateway 的派生同隔离）。
fn git_sync(cwd: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd.parent().unwrap_or(cwd))
        .env("XDG_CONFIG_HOME", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git")
        .success();
    assert!(ok, "git {} failed in {}", args.join(" "), cwd.display());
}

fn git_present() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
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

/// `expectedStateToken` 是 secure 输入字段：git 变更一律经 `--input-json`
/// 文件提交（t014 先例）。
fn dispatch_git_input(home: &Path, name: &str, method_path: &[&str], input: &Value) -> CliOutcome {
    let path = home.join(format!("{name}.json"));
    std::fs::write(&path, input.to_string()).unwrap();
    let mut args: Vec<String> = Vec::new();
    for part in method_path {
        args.push((*part).to_owned());
    }
    args.push("--output".to_owned());
    args.push("json".to_owned());
    args.push("--input-json".to_owned());
    args.push(path.to_str().expect("utf8 path").to_owned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    await_git_cli(home, &refs)
}

/// 旅程 J4 正向二（git 分支）：两段式丢弃工作树改动——plan 是只读预览
/// （planId + planRevision + expiresAt + 逐路径 affected/conflicts），restore
/// 按 CAS 重验后落盘执行；恢复后的文件树与计划逐项一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j4_git_two_phase_restore_discards_worktree_changes() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let (home, workspace, project_id, _iid) = seed_workspace("j4-git").await;
    let root = home.path();

    // 种子仓库：一个已提交的 tracked.txt(v1)，随后工作树改为 v2 + 未跟踪
    // scratch.txt（种子提交走装置系统 git，身份经 -c 注入，不经过域内身份门）。
    git_sync(&workspace, &["init", "-q", "-b", "main"]);
    git_sync(&workspace, &["config", "commit.gpgsign", "false"]);
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
    write_file(&workspace.join("scratch.txt"), "scratch\n");

    let state = status_state(root, &project_id);
    assert_eq!(state["unborn"], json!(false));
    let token = state_token(&state);
    let files = state["files"].as_array().expect("files");
    let tracked = files
        .iter()
        .find(|f| f["path"]["value"] == "tracked.txt")
        .expect("tracked.txt modified in status");
    assert_eq!(tracked["untracked"], json!(false));
    assert_eq!(tracked["worktreeStatus"], json!("M"));
    let scratch = files
        .iter()
        .find(|f| f["path"]["value"] == "scratch.txt")
        .expect("scratch.txt untracked in status");
    assert_eq!(scratch["untracked"], json!(true));

    // ---- 第一跳：plan（只读预览）。discardTracked 对未跟踪路径报冲突，
    // 不静默扩大范围。
    let out = dispatch_git_input(
        root,
        "j4-plan",
        &["git", "plan-restore"],
        &json!({
            "projectId": project_id,
            "action": "discardTracked",
            "paths": [
                {"encoding": "utf8", "value": "tracked.txt"},
                {"encoding": "utf8", "value": "scratch.txt"},
            ],
            "expectedStateToken": token,
        }),
    );
    let planned = expect_ok("git.plan_restore", &out);
    let plan = &planned["plan"];
    assert_eq!(plan["affectedResources"], json!(["tracked.txt"]));
    assert_eq!(plan["conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(plan["conflicts"][0]["resourceId"], "scratch.txt");
    assert_eq!(plan["conflicts"][0]["code"], "PATH_UNTRACKED");
    assert_eq!(plan["destructive"], json!(true));
    let plan_id = plan["planId"].as_str().unwrap().to_owned();
    let plan_revision = plan["planRevision"].as_str().unwrap().to_owned();
    assert!(
        !plan_id.is_empty() && !plan_revision.is_empty(),
        "the plan must carry its two-phase identity: {plan}"
    );
    assert!(
        plan["expiresAt"].as_str().is_some_and(|t| !t.is_empty()),
        "the plan must carry an expiry: {plan}"
    );

    // 只读性：plan 不改仓库状态、不触碰磁盘内容。
    let after_plan = status_state(root, &project_id);
    assert_eq!(state_token(&after_plan), token, "plan must not move state");
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "v2\n",
        "plan must not touch the worktree"
    );
    assert!(workspace.join("scratch.txt").exists());

    // ---- 故障一：过期 expectedPlanRevision → REVISION_CONFLICT（无副作用）。
    let out = dispatch_git_input(
        root,
        "j4-restore-stale-rev",
        &["git", "restore"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "planId": plan_id,
            "expectedPlanRevision": "gplan-not-the-real-one",
        }),
    );
    assert_eq!(expect_failure("git.restore", &out), "REVISION_CONFLICT");
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "v2\n",
        "the refused restore must leave the worktree intact"
    );

    // ---- 故障二：计划生成后工作树再变动（新未跟踪文件）→ stateToken CAS
    // 拒绝（计划失效，不静默重验通过）。
    write_file(&workspace.join("unplanned.txt"), "unplanned\n");
    let out = dispatch_git_input(
        root,
        "j4-restore-stale-state",
        &["git", "restore"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "planId": plan_id,
            "expectedPlanRevision": plan_revision,
        }),
    );
    assert_eq!(expect_failure("git.restore", &out), "REVISION_CONFLICT");
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "v2\n",
        "the invalidated plan must not restore anything"
    );
    std::fs::remove_file(workspace.join("unplanned.txt")).unwrap();

    // ---- 第二跳：restore（正确 CAS）→ 磁盘恢复到计划的目标态。
    let fresh_token = state_token(&status_state(root, &project_id));
    let out = dispatch_git_input(
        root,
        "j4-restore-ok",
        &["git", "restore"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": fresh_token,
            "planId": plan_id,
            "expectedPlanRevision": plan_revision,
        }),
    );
    expect_ok("git.restore", &out);
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "v1\n",
        "discardTracked must restore the committed content"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("scratch.txt")).unwrap(),
        "scratch\n",
        "the conflicted path must stay untouched"
    );

    // 恢复后的状态：tracked.txt 不再出现在 status（干净），scratch.txt 仍
    // 未跟踪。
    let restored_state = status_state(root, &project_id);
    let files = restored_state["files"].as_array().expect("files");
    assert!(
        !files.iter().any(|f| f["path"]["value"] == "tracked.txt"),
        "tracked.txt must be clean after the restore: {files:?}"
    );
    assert!(
        files
            .iter()
            .any(|f| f["path"]["value"] == "scratch.txt" && f["untracked"] == true)
    );

    // ---- 故障三：同一计划按执行前的 stateToken 重放 → REVISION_CONFLICT
    // （执行后状态已前进；不自动重放）。
    let out = dispatch_git_input(
        root,
        "j4-restore-replay",
        &["git", "restore"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": fresh_token,
            "planId": plan_id,
            "expectedPlanRevision": plan_revision,
        }),
    );
    assert_eq!(expect_failure("git.restore", &out), "REVISION_CONFLICT");

    // ---- 落盘逐项对照：恢复后的树 = 计划目标态（tracked.txt=v1，
    // scratch.txt 原样），无计划外变更。
    let tree = walk_files(&workspace);
    let mut expected = BTreeMap::new();
    expected.insert("tracked.txt".to_owned(), b"v1\n".to_vec());
    expected.insert("scratch.txt".to_owned(), b"scratch\n".to_vec());
    assert_eq!(
        tree, expected,
        "the restored tree must match the plan item by item"
    );
}

// ---------------------------------------------------------------------------
// 旅程三（PR-2）：run 归属快照的真实链路 —— driver 写指令 → 自动捕获 →
// 两段式恢复（snap-wire 报告 §5 A5 + A2）
// ---------------------------------------------------------------------------

/// 轮询 `snapshots list --run-id`，直到出现该 run 归属的快照（边界聚合
/// 在终态迁移之后异步落库）。满载预算 60 s。
async fn poll_run_snapshots(home: &Path, project_id: &str, run_id: &str) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = await_cli(
            home,
            &[
                "snapshots",
                "list",
                "--output",
                "json",
                "--project-id",
                project_id,
                "--run-id",
                run_id,
            ],
        );
        let listed = expect_ok("snapshots.list", &out);
        let snaps = listed["snapshots"].as_array().cloned().unwrap_or_default();
        if !snaps.is_empty() {
            return snaps;
        }
        assert!(
            Instant::now() < deadline,
            "no snapshot attributed to run `{run_id}` ever appeared"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 轮询 `operations get` 至终态，返回 operation JSON（restore 的执行体在
/// 受理后异步落盘，完成以 operation 终态为准）。
async fn await_operation(home: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
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

/// 快照条目断言：路径命中、action=create、after 产物存在且字节=写的
/// 内容（`artifacts.read` 读回对照）。
async fn assert_create_entry(home: &Path, entries: &[Value], path: &str, bytes: &[u8]) {
    let entry = entries
        .iter()
        .find(|e| e["path"]["relativePath"]["value"] == json!(path))
        .unwrap_or_else(|| panic!("missing snapshot entry for `{path}`: {entries:?}"));
    assert_eq!(entry["action"], json!("create"), "`{path}`: {entry}");
    let artifact_id = entry["artifactId"].as_str().unwrap_or_default().to_owned();
    assert!(
        !artifact_id.is_empty(),
        "`{path}` entry must productize the after content: {entry}"
    );
    let out = await_cli(
        home,
        &[
            "artifacts",
            "read",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--max-bytes",
            "4096",
        ],
    );
    let read = expect_ok("artifacts.read", &out);
    assert_eq!(
        b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
        bytes,
        "`{path}` after-artifact bytes must equal the run-written content"
    );
}

/// `snapshots plan-restore` 的成功调用面（返回含 plan/entries 的完整结果）。
async fn plan_restore(home: &Path, project_id: &str, snapshot_ids: &str, direction: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "snapshots",
            "plan-restore",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--snapshot-ids",
            snapshot_ids,
            "--direction",
            direction,
        ],
    );
    expect_ok("snapshots.plan_restore", &out)
}

/// `snapshots restore` 的成功受理面（Accepted 回执，含 operationId）。
async fn snapshots_restore(home: &Path, plan_id: &str, plan_revision: &str, key: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "snapshots",
            "restore",
            "--output",
            "json",
            "--plan-id",
            plan_id,
            "--expected-plan-revision",
            plan_revision,
            "--request-key",
            key,
        ],
    );
    expect_ok("snapshots.restore", &out)
}

/// 旅程 J4 正向三：run 归属快照的真实链路（PR-2，snap-wire §5 A5+A2）。
///
/// 离线 driver 的写指令（`[pd-driver:write=<relpath>@<字面量>]`）走
/// files 域同一生产写路径，run 的文件副作用在终态边界自动聚合为一条
/// runId=turnId=runId 的快照；随后走两段式恢复：before 方向预览（逆副
/// 作用）→ 破坏（files delete）→ 按正本顺序的 restore 被 CAS 拒绝
/// （REVISION_CONFLICT——a021「不自动覆盖无法确认的变更」）→ 破坏后重
/// 新 plan（after 方向）→ restore 落盘对照；最后反向对照：后续 run 的
/// 副作用按 before 方向整体回退（delete 目标态，更早的树原样）。
///
/// 与旅程正本箭头顺序的（有意）偏差：正本「plan-restore → 破坏 →
/// restore（同 planId）」在恢复面的 CAS 语义下必然落在 REVISION_CONFLICT
/// ——目标路径在计划受理后变动即拒绝、不无条件覆盖。本测试先按正本顺
/// 序走一遍并断言该拒绝（这本身就是 a021 的验收点），再以「破坏 → 重
/// 新 plan → restore」完成恢复主干。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j4_real_snapshot_chain_over_run_writes() {
    let (home, workspace, project_id, _iid) = seed_workspace("j4-run-writes").await;
    let root = home.path();

    // ---- 会话 + 嵌套父目录（driver 写不隐式建父目录，与 files.write
    // 同语义，先用 CLI 声明面建目录）。
    let session_id = "s-j4-run-writes".to_owned();
    let out = await_cli(
        root,
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
    let out = await_cli(
        root,
        &[
            "files",
            "create-directory",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "a/dir"),
            "--create-parents",
            "true",
        ],
    );
    expect_ok("files.create_directory", &out);

    // ---- run1：三条写指令（嵌套×2 + 顶层×1）走生产链路落盘。
    let c1 = b"run-owned alpha one".to_vec();
    let c2 = b"run-owned beta one".to_vec();
    let c3 = b"run-owned gamma one".to_vec();
    let text = concat!(
        "please write the run-owned tree\n",
        "[pd-driver:write=a/dir/file1.txt@run-owned alpha one]\n",
        "[pd-driver:write=a/dir/file2.txt@run-owned beta one]\n",
        "[pd-driver:write=top3.txt@run-owned gamma one]"
    );
    let submitted = submit_run(root, &session_id, text, "j4-rw-run1").await;
    let run1 = submitted["runId"].as_str().unwrap().to_owned();
    let waited = expect_ok("run.wait", &wait_run(root, &run1));
    assert_eq!(waited["run"]["state"], json!("succeeded"), "run1: {waited}");
    assert_eq!(
        std::fs::read(workspace.join("a/dir/file1.txt")).unwrap(),
        c1,
        "the driver-written files must be on disk verbatim"
    );
    assert_eq!(
        std::fs::read(workspace.join("a/dir/file2.txt")).unwrap(),
        c2
    );
    assert_eq!(std::fs::read(workspace.join("top3.txt")).unwrap(), c3);

    // ---- snapshots list（runId 过滤，边界聚合异步落库 → 轮询）：一条
    // run1 归属快照，runId=turnId=run1。
    let owned = poll_run_snapshots(root, &project_id, &run1).await;
    assert_eq!(
        owned.len(),
        1,
        "exactly one run1-owned snapshot for the three writes: {owned:?}"
    );
    let snap1 = owned.into_iter().next().unwrap();
    assert_eq!(snap1["runId"], json!(run1));
    assert_eq!(
        snap1["turnId"],
        json!(run1),
        "turn identity is the run id (v1 has no separate turn)"
    );
    assert_eq!(snap1["entries"].as_array().unwrap().len(), 3);
    let snapshot1_id = snap1["snapshotId"].as_str().unwrap().to_owned();

    // snapshots get：逐条目 action=create + path 对照 + after 产物读回。
    let out = await_cli(
        root,
        &[
            "snapshots",
            "get",
            "--output",
            "json",
            "--snapshot-id",
            &snapshot1_id,
        ],
    );
    let got = expect_ok("snapshots.get", &out);
    let snap_entries = got["snapshot"]["entries"].as_array().unwrap().clone();
    assert_eq!(snap_entries.len(), 3);
    assert_create_entry(root, &snap_entries, "a/dir/file1.txt", &c1).await;
    assert_create_entry(root, &snap_entries, "a/dir/file2.txt", &c2).await;
    assert_create_entry(root, &snap_entries, "top3.txt", &c3).await;

    // ---- 两段式·一段（正本顺序）：plan（before 方向预览 = 逆副作用序）
    // 只读无副作用，含 planId + planRevision + expiresAt。
    let plan1 = plan_restore(
        root,
        &project_id,
        &json!([snapshot1_id]).to_string(),
        "before",
    )
    .await;
    let plan1_id = plan1["plan"]["planId"].as_str().unwrap().to_owned();
    let plan1_rev = plan1["plan"]["planRevision"].as_str().unwrap().to_owned();
    assert!(
        plan1["plan"]["expiresAt"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "the plan must carry an expiry: {}",
        plan1["plan"]
    );
    let undo = plan1["entries"].as_array().unwrap().clone();
    assert_eq!(undo.len(), 3, "rewinding all three creates: {undo:?}");
    assert!(undo.iter().all(|e| e["action"] == json!("delete")));
    assert_eq!(plan1["plan"]["destructive"], json!(true));
    let mut expected_tree = BTreeMap::new();
    expected_tree.insert("a/dir/file1.txt".to_owned(), c1.clone());
    expected_tree.insert("a/dir/file2.txt".to_owned(), c2.clone());
    expected_tree.insert("top3.txt".to_owned(), c3.clone());
    assert_eq!(
        walk_files(&workspace),
        expected_tree,
        "plan-restore must not touch the worktree"
    );

    // ---- 破坏：files delete file1（revision 与快照条目 afterRevision 同
    // 一内容身份，stat 交叉验证）。
    let file1_rev = snap_entries
        .iter()
        .find(|e| e["path"]["relativePath"]["value"] == json!("a/dir/file1.txt"))
        .unwrap()["afterRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let stat = files_stat(root, &project_id, "a/dir/file1.txt").await;
    assert_eq!(
        stat["entry"]["revision"],
        json!(file1_rev),
        "the snapshot's afterRevision must be the live file revision"
    );
    let out = await_cli(
        root,
        &[
            "files",
            "delete",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "a/dir/file1.txt"),
            "--expected-revision",
            &file1_rev,
        ],
    );
    expect_ok("files.delete", &out);
    assert!(!workspace.join("a/dir/file1.txt").exists());

    // ---- 按正本顺序 restore（同 planId+expectedPlanRevision）：目标路
    // 径在计划受理后变动 → REVISION_CONFLICT，破坏态保持原样。
    let out = await_cli(
        root,
        &[
            "snapshots",
            "restore",
            "--output",
            "json",
            "--plan-id",
            &plan1_id,
            "--expected-plan-revision",
            &plan1_rev,
            "--request-key",
            "j4-rw-restore-stale",
        ],
    );
    assert_eq!(
        expect_failure("snapshots.restore", &out),
        "REVISION_CONFLICT"
    );
    assert!(
        !workspace.join("a/dir/file1.txt").exists(),
        "the refused restore must leave the broken tree as-is"
    );

    // ---- 破坏后重新 plan（after 方向 = 回到 run1 快照态）：只有被破坏
    // 的 file1 进计划（create + 快照 after 产物）。
    let plan2 = plan_restore(
        root,
        &project_id,
        &json!([snapshot1_id]).to_string(),
        "after",
    )
    .await;
    let plan2_id = plan2["plan"]["planId"].as_str().unwrap().to_owned();
    let plan2_rev = plan2["plan"]["planRevision"].as_str().unwrap().to_owned();
    let recover = plan2["entries"].as_array().unwrap().clone();
    assert_eq!(
        recover.len(),
        1,
        "only the broken path differs from the snapshot state: {recover:?}"
    );
    assert_eq!(recover[0]["action"], json!("create"));
    assert_eq!(
        recover[0]["path"]["relativePath"]["value"],
        json!("a/dir/file1.txt")
    );
    assert!(
        !recover[0]["artifactId"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );

    // ---- restore 二跳：CAS 通过 → 落盘恢复 → 逐项对照。
    let receipt = snapshots_restore(root, &plan2_id, &plan2_rev, "j4-rw-restore-1").await;
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(root, &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(
        operation["result"]["outcome"],
        json!("complete"),
        "{operation}"
    );
    assert_eq!(
        std::fs::read(workspace.join("a/dir/file1.txt")).unwrap(),
        c1,
        "file1 must be restored to the driver-written content"
    );
    assert_eq!(
        walk_files(&workspace),
        expected_tree,
        "the restored tree must match the run1 snapshot state item by item"
    );

    // ---- 反向破坏对照：再建一个 run 写 file4 → run2 归属快照；对它做
    // before 方向回退（选中 run2 的快照 = 回到它之前的状态：含
    // file1-3、不含 file4）→ restore → file4 缺失（delete 目标态）。
    let c4 = b"run-owned delta two".to_vec();
    let text2 = concat!(
        "write one more file for the reverse check\n",
        "[pd-driver:write=a/dir/file4.txt@run-owned delta two]"
    );
    let submitted2 = submit_run(root, &session_id, text2, "j4-rw-run2").await;
    let run2 = submitted2["runId"].as_str().unwrap().to_owned();
    let waited2 = expect_ok("run.wait", &wait_run(root, &run2));
    assert_eq!(
        waited2["run"]["state"],
        json!("succeeded"),
        "run2: {waited2}"
    );
    assert_eq!(
        std::fs::read(workspace.join("a/dir/file4.txt")).unwrap(),
        c4
    );

    let owned2 = poll_run_snapshots(root, &project_id, &run2).await;
    assert_eq!(
        owned2.len(),
        1,
        "exactly one run2-owned snapshot: {owned2:?}"
    );
    let snap2 = owned2.into_iter().next().unwrap();
    assert_eq!(snap2["runId"], json!(run2));
    assert_eq!(snap2["turnId"], json!(run2));
    assert_eq!(snap2["entries"].as_array().unwrap().len(), 1);
    let snapshot2_id = snap2["snapshotId"].as_str().unwrap().to_owned();

    let plan3 = plan_restore(
        root,
        &project_id,
        &json!([snapshot2_id]).to_string(),
        "before",
    )
    .await;
    let plan3_id = plan3["plan"]["planId"].as_str().unwrap().to_owned();
    let plan3_rev = plan3["plan"]["planRevision"].as_str().unwrap().to_owned();
    let rewind = plan3["entries"].as_array().unwrap().clone();
    assert_eq!(
        rewind.len(),
        1,
        "only run2's side effect rewinds: {rewind:?}"
    );
    assert_eq!(rewind[0]["action"], json!("delete"));
    assert_eq!(
        rewind[0]["path"]["relativePath"]["value"],
        json!("a/dir/file4.txt")
    );

    let receipt = snapshots_restore(root, &plan3_id, &plan3_rev, "j4-rw-restore-2").await;
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(root, &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");

    assert!(
        !workspace.join("a/dir/file4.txt").exists(),
        "the later run's file must be rolled back (delete target state)"
    );
    assert_eq!(
        walk_files(&workspace),
        expected_tree,
        "rolling back run2 must leave the earlier snapshot state intact"
    );
}

#[test]
fn common_v1_isolation_pins_home_git_and_offline_directives() {
    use std::ffi::OsStr;

    let home = TestHome::new("j4-env-iso");
    let cmd = common_v1::base_command(home.path());
    let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.as_std().get_envs().collect();
    assert_eq!(
        envs.get(OsStr::new("AGENTCPM_PD_HOME")),
        Some(&Some(home.path().as_os_str()))
    );
    assert_eq!(
        envs.get(OsStr::new("AGENTCPM_PD_ENABLE_OFFLINE_DIRECTIVES")),
        Some(&Some(OsStr::new("1")))
    );
    assert_eq!(
        envs.get(OsStr::new("HOME")),
        Some(&Some(home.path().as_os_str()))
    );
    let expected_install_root = home.path().join("install-root");
    assert_eq!(
        envs.get(OsStr::new("AGENTCPM_PD_INSTALL_ROOT")),
        Some(&Some(expected_install_root.as_os_str()))
    );
    assert_eq!(
        envs.get(OsStr::new("GIT_CONFIG_GLOBAL")),
        Some(&Some(OsStr::new("/dev/null")))
    );
    assert_eq!(
        envs.get(OsStr::new("GIT_CONFIG_SYSTEM")),
        Some(&Some(OsStr::new("/dev/null")))
    );
    assert_eq!(envs.get(OsStr::new("PILOT_HOME")), Some(&None));
}
