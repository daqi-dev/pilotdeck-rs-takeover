//! End-to-end suite for the workspaces domain (`cli.task.clone`, S3):
//! `workspace prepare` (the `workspaces.prepare` operation) driven through
//! the real `agentcpm-pd` argv against an isolated `AGENTCPM_PD_HOME` and
//! the real `/v1/ws` transport.
//!
//! Acceptance anchors covered here (the gateway unit suite covers the same
//! anchors at the store level):
//! * existing registers an already-present directory with zero cloning
//!   (the marker file survives untouched) and reports an empty
//!   `createdPaths`;
//! * new creates the missing chain and `createdPaths` reports exactly the
//!   created levels;
//! * a non-empty destination is refused (`ALREADY_EXISTS`) with the
//!   existing content untouched;
//! * clone from a local `file://` bare repository moves the payload into
//!   the destination, emits `projects.changed` + `git.changed`, and leaves
//!   no staging directory behind;
//! * an invalid URL is refused before acceptance
//!   (`INVALID_REPOSITORY_URL`), and the same `--request-key` replays
//!   the original acceptance instead of re-executing;
//! * cancelling an already-terminal operation answers `alreadyTerminal`
//!   and never disturbs the completed workspace.
//!
//! Local execution note: this machine is under a build freeze (disk
//! incident, 2026-11-18), so the suite was written but not executed here;
//! it runs in the integrator's cb8 verification pass.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

/// Parses a failed argv invocation: exactly one envelope line,
/// `outcome: "failed"`, non-zero exit. Returns the `error` object.
fn expect_failure(method: &str, out: &CliOutcome) -> Value {
    assert_ne!(
        out.code, 0,
        "`{method}` must exit non-zero; stdout:\n{}\nstderr:\n{}",
        out.stdout, out.stderr
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
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "{value}");
    value["error"].clone()
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

/// A cloneable local bare repository (deterministic, no network).
fn seed_bare_repo(parent: &Path, name: &str) -> PathBuf {
    let work = parent.join(format!("{name}-work"));
    // A stale worktree from a previous run makes `git commit` a no-change
    // exit-1 (the automation suites' known failure mode) — start clean.
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(parent.join(format!("{name}.git")));
    std::fs::create_dir_all(&work).expect("create seed worktree");
    git_sync(&work, &["init", "-q", "-b", "main"]);
    git_sync(&work, &["config", "commit.gpgsign", "false"]);
    git_sync(&work, &["config", "user.email", "seed@example.invalid"]);
    git_sync(&work, &["config", "user.name", "Seed"]);
    std::fs::write(work.join("README.md"), "seeded\n").unwrap();
    git_sync(&work, &["add", "."]);
    git_sync(&work, &["commit", "-q", "-m", "seed"]);
    let bare = parent.join(format!("{name}.git"));
    git_sync(
        &work,
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    bare
}

/// The lazily-registered default authorized root (`<home>/workspaces`).
fn workspaces_root(home: &Path) -> PathBuf {
    home.join("workspaces")
}

fn server_path_arg(relative: &str) -> String {
    json!({
        "authorizedRootId": "default",
        "relativePath": {"encoding": "utf8", "value": relative},
    })
    .to_string()
}

fn prepare_args(
    mode: &str,
    relative: &str,
    key: Option<&str>,
    extra: &[(&str, String)],
) -> Vec<String> {
    let mut args = vec![
        "workspace".to_owned(),
        "prepare".to_owned(),
        "--output".to_owned(),
        "json".to_owned(),
        "--mode".to_owned(),
        mode.to_owned(),
        "--destination".to_owned(),
        server_path_arg(relative),
    ];
    if let Some(key) = key {
        args.push("--request-key".to_owned());
        args.push(key.to_owned());
    }
    for (flag, value) in extra {
        args.push((*flag).to_owned());
        args.push(value.clone());
    }
    args
}

fn await_prepare(home: &Path, args: &[String]) -> Value {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(home, &refs);
    expect_ok("workspaces.prepare", &out)
}

fn operations_get(home: &Path, operation_id: &str) -> Value {
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
    expect_ok("operations.get", &out)
}

/// Polls `operations.get` until the operation is terminal (bounded). The
/// Operation projection has no `terminal` field — `state` reaching one of
/// the terminal values is the contract's terminality (the same set
/// `common_v1::wait_terminal_operation` uses).
fn await_terminal_operation(home: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let value = operations_get(home, operation_id);
        let terminal = value["operation"]["state"]
            .as_str()
            .is_some_and(|state| {
                matches!(
                    state,
                    "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
                )
            });
        if terminal {
            return value["operation"].clone();
        }
        if Instant::now() > deadline {
            panic!("operation `{operation_id}` never reached a terminal state");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn operations_events(home: &Path, operation_id: &str) -> Vec<String> {
    let out = await_cli(
        home,
        &[
            "operations",
            "events",
            "--output",
            "json",
            "--operation-id",
            operation_id,
        ],
    );
    let value = expect_ok("operations.events", &out);
    value["frame"]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e["type"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// c022/contract.workspace-prepare（existing/new 正反例）：
/// * existing 登记已存在的目录，零克隆零创建（marker 原样、createdPaths 空）；
/// * new 创建缺失链并精确报告 createdPaths；
/// * 非空目标拒绝且既有内容零触碰。
#[test]
fn existing_and_new_flows() {
    let home = TestHome::new("ws-e2e-existing");
    init_home(home.path());
    let _instance_id = init_home_instance(home.path());
    let root = workspaces_root(home.path());

    // --- existing：登记已存在目录 ---
    let target = root.join("already-there");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("marker.txt"), b"existing content").unwrap();

    let receipt = await_prepare(
        home.path(),
        &prepare_args(
            "existing",
            "already-there",
            Some("ws-e2e-key-1"),
            &[("--display-name", "Existing E2E".to_owned())],
        ),
    );
    assert_eq!(receipt["kind"], "operation", "{receipt}");
    assert_eq!(receipt["deduplicated"], false, "{receipt}");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

    let operation = await_terminal_operation(home.path(), &operation_id);
    assert_eq!(operation["state"], "succeeded", "{operation}");
    let completion = &operation["result"];
    assert_eq!(completion["mode"], "existing");
    assert_eq!(
        completion["createdPaths"],
        json!([]),
        "existing 零克隆零创建: {completion}"
    );
    let project_id = completion["projectId"].as_str().unwrap().to_owned();
    assert_eq!(
        completion["serverPath"]["relativePath"]["value"],
        "already-there"
    );
    assert_eq!(
        std::fs::read(target.join("marker.txt")).unwrap(),
        b"existing content",
        "既有文件零触碰"
    );

    // 登记可见。
    let out = await_cli(
        home.path(),
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
    assert_eq!(project["project"]["name"], "Existing E2E");
    assert_eq!(project["project"]["root"]["rootId"], "default");

    // --- new：创建缺失链 ---
    let receipt = await_prepare(
        home.path(),
        &prepare_args("new", "team/deep/ws-b", Some("ws-e2e-key-2"), &[]),
    );
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_terminal_operation(home.path(), &operation_id);
    assert_eq!(operation["state"], "succeeded", "{operation}");
    let created = operation["result"]["createdPaths"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(created.len(), 3, "三级目录都新建: {created:?}");
    assert_eq!(created[0]["relativePath"]["value"], "team");
    assert!(root.join("team/deep/ws-b").is_dir());

    // 同 key 重放：不重复创建。
    let replay = await_prepare(
        home.path(),
        &prepare_args("new", "team/deep/ws-b", Some("ws-e2e-key-2"), &[]),
    );
    assert_eq!(replay["deduplicated"], true, "{replay}");
    assert_eq!(replay["operationId"], operation_id);

    // requests.lookup 恢复受理。
    let out = await_cli(
        home.path(),
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "workspaces.prepare",
            "--idempotency-key",
            "ws-e2e-key-2",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(lookup["resource"]["operationId"], operation_id);

    // --- 非空目标拒绝 ---
    let occupied = root.join("occupied");
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("data.txt"), b"keep me").unwrap();
    let args = prepare_args("new", "occupied", Some("ws-e2e-key-3"), &[]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(home.path(), &refs);
    let error = expect_failure("workspaces.prepare", &out);
    assert_eq!(error["code"], "ALREADY_EXISTS", "{error}");
    assert_eq!(
        std::fs::read(occupied.join("data.txt")).unwrap(),
        b"keep me",
        "被拒目标的既有内容零触碰"
    );
}

/// c022/contract.workspace-prepare（clone 正反例）：
/// * file:// 本地 bare 仓库克隆进 destination，projects.changed +
///   git.changed 事件，staging 无残留；
/// * 非法 URL 受理前拒绝；取消终态操作回答 alreadyTerminal 且不破坏
///   完成的交付物。
#[test]
fn clone_flow_and_rejections() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let home = TestHome::new("ws-e2e-clone");
    init_home(home.path());
    let _instance_id = init_home_instance(home.path());
    let root = workspaces_root(home.path());

    let bare = seed_bare_repo(home.path().parent().unwrap(), "ws-e2e-seed");
    let url = format!("file://{}", bare.display());

    // 非法 URL（无 scheme）：受理前拒绝，零副作用。
    let args = prepare_args(
        "clone",
        "bad-url",
        Some("ws-e2e-key-bad-url"),
        &[("--repository-url", "github.com/org/repo".to_owned())],
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(home.path(), &refs);
    let error = expect_failure("workspaces.prepare", &out);
    assert_eq!(error["code"], "INVALID_REPOSITORY_URL", "{error}");

    // URL 内嵌凭据：拒绝（凭据与 URL 分离）。
    let args = prepare_args(
        "clone",
        "cred-url",
        Some("ws-e2e-key-cred-url"),
        &[(
            "--repository-url",
            "https://user:secret@example.invalid/org/repo".to_owned(),
        )],
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(home.path(), &refs);
    let error = expect_failure("workspaces.prepare", &out);
    assert_eq!(error["code"], "INVALID_REPOSITORY_URL", "{error}");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("secret"),
        "错误消息不得回显 URL 凭据: {message}"
    );

    // 正常克隆。
    let receipt = await_prepare(
        home.path(),
        &prepare_args(
            "clone",
            "cloned/project-x",
            Some("ws-e2e-key-clone-1"),
            &[
                ("--repository-url", url.clone()),
                ("--branch", "main".to_owned()),
            ],
        ),
    );
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_terminal_operation(home.path(), &operation_id);
    assert_eq!(operation["state"], "succeeded", "{operation}");
    let completion = &operation["result"];
    assert_eq!(completion["mode"], "clone");
    // createdPaths 浅→深（与 new 模式的 [0]=="team" 同序）：父目录
    // `cloned` 在前，交付物 `cloned/project-x` 在后。
    assert_eq!(
        completion["createdPaths"][0]["relativePath"]["value"],
        "cloned"
    );
    assert_eq!(
        completion["createdPaths"][1]["relativePath"]["value"],
        "cloned/project-x"
    );
    assert!(
        completion["repositoryId"].as_str().is_some(),
        "repositoryId 必报: {completion}"
    );

    let destination = root.join("cloned/project-x");
    assert!(destination.join("README.md").is_file(), "克隆内容就位");
    let staging: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".pd-staging-"))
        .collect();
    assert!(staging.is_empty(), "staging 无残留: {staging:?}");

    // 事件面：projects.changed + git.changed。
    let types = operations_events(home.path(), &operation_id);
    assert!(
        types.iter().any(|t| t == "projects.changed"),
        "projects.changed 缺失: {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "git.changed"),
        "git.changed 缺失: {types:?}"
    );

    // 同 destination 再克隆：非空拒绝（不能覆盖）。
    let args = prepare_args(
        "clone",
        "cloned/project-x",
        Some("ws-e2e-key-clone-2"),
        &[("--repository-url", url)],
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(home.path(), &refs);
    let error = expect_failure("workspaces.prepare", &out);
    assert_eq!(error["code"], "ALREADY_EXISTS", "{error}");
    assert!(destination.join("README.md").is_file(), "既有克隆零触碰");

    // 取消一个已终态的操作：alreadyTerminal，不破坏交付物。
    let out = await_cli(
        home.path(),
        &[
            "operations",
            "cancel",
            "--output",
            "json",
            "--operation-id",
            &operation_id,
        ],
    );
    let cancel = expect_ok("operations.cancel", &out);
    assert_eq!(
        cancel["receipt"]["disposition"], "alreadyTerminal",
        "{cancel}"
    );
    assert!(
        destination.join("README.md").is_file(),
        "完成的交付物不被取消破坏"
    );
}
