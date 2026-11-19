//! S5 业务旅程 J1（项目全生命周期）端到端验收。
//!
//! 旅程正本是 `_tmp/20261119-s5/journeys-execution-plan.md` §3 J1：单一用户
//! 在一个实例上走完「初始化 → 目录/工作区 → 项目与会话 → 附件与文档预览 →
//! 运行与交互（审批/问答/对话编辑租约）→ 事件洪泛下的审批应答 → 终端 →
//! git 提交与推送 → 搜索/历史/归档」的完整业务闭环。命令名按计划 §2 映射表
//! 改写为实际 CLI 形态（`terminal` 单数、`git identity set` 等）。
//!
//! 两条场景（每条一个 `#[tokio::test]`，全部驱动真实 `agentcpm-pd` 二进制、
//! 隔离 `AGENTCPM_PD_HOME`、`--output json` envelope 经嵌入 registry 的
//! schema 校验）：
//!
//! * 正向旅程（`journey_j1_project_full_lifecycle`）：
//!   1. `--version` 与握手 `target.gatewayVersion` 一致（A2，段首一次）；
//!   2. `instance init` → `directories create` ×2 → `workspace prepare`
//!      （clone 模式，本地裸仓 file://，零网络）→ `projects register` →
//!      `sessions create`；
//!   3. `uploads create` → `part` ×2 → `complete`（sha256 校验 + 产物落盘
//!      另加分块读回逐字节一致）→ `documents preview`（manifest + sheet，
//!      长作业经 `operations get` 至终态；`operations events` 对预览操作
//!      诚实报空帧，事件游标单调判据落在 clone 操作的生命周期 + 域事件
//!      上——uploads-plane 异步操作无事件日志，见文件内注释与报告）；
//!   4. 审批 run：`interaction list/get` → `approval respond`（allow）；
//!      问答 run：`interaction claim`（显式 ttlMs）→ `interaction respond`
//!      → `interaction release`（租约不授予应答权、应答后释放只撤租约）；
//!   5. 事件洪泛（`[pd-driver:events=2000]`，另一会话）回放期间应答一次
//!      审批并计时（T7 判据：控制面延迟上限内、事件不重不漏、completed
//!      终态、订阅账目闭合）；
//!   6. 终端：`terminal create` → `control`（claim）→ `input`（inputSeq
//!      连续）→ `resize` → `output`（游标链式读回、byteOffset 严格单调）
//!      → `release` → `terminal attach --machine` 子进程回显 → `close`
//!      （graceful，attach 以 term.exit + 退出码 0 收尾）；
//!   7. git：`identity set` → `status` → `suggest-message`（无模型装配，
//!      MODEL_UNAVAILABLE 诚实失败）→ `commit`（initial，身份门经产品
//!      身份通过）→ `push`（创建式远端 CAS，expectedRemoteOid=""）；
//!   8. 搜索与历史：播种历史（存储 API 联调点）→ `search start/results`
//!      → `history read` → `history fork`（截断边界）→ `history export`
//!      （archive 落盘 + `archive get/entries` + manifest 读回——公开描述
//!      符与盘上 blob 逐字节比对，归档 blob 不入 workspace 产物库）→
//!      会话归档（`sessions update` + `sessions close`，close 后历史可读）；
//!   9. 落盘总断言：state.sqlite、工作区树、`.git` 对象、artifacts 目录、
//!      裸仓 ref、导出 archive 目录。
//!
//! * 故障负例（`journey_j1_fault_lying_digest_and_stale_git_token`）：
//!   分片摘要说谎 → `CHECKSUM_MISMATCH` 且零副作用（parts 无落盘），
//!   正确重传后 complete 成功；git 提交的过期 stateToken →
//!   `REVISION_CONFLICT` 且仓库零变化（unborn 不变、无 ref），正确 token
//!   重提交成功。
//!
//! 装置边界：git 相关调用一律经隔离 env（HOME=<home>、
//! GIT_CONFIG_GLOBAL/SYSTEM=/dev/null，t014 先例）——宿主全局 git 身份不
//! 泄漏进 Gateway 派生的 git 进程，`git identity set` 的产品身份门才可
//! 确定性验收；本套件不涉及远程（J8 的 remote_pair 挂具不在本旅程范围）。
//! 本地未跑（2026-11-18 起本机构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use common_v1::*;

// ---------------------------------------------------------------------------
// 隔离挂具：git 隔离的 CLI 调用、失败断言、异步操作轮询
// ---------------------------------------------------------------------------

/// Runs one `agentcpm-pd` argv invocation to completion with the isolated git
/// environment（t014 先例：HOME=<home> + 关闭宿主全局 git config，使 Gateway
/// 派生的 git 进程与宿主身份隔离）。本旅程的全部 argv 都走这条通道，保证
/// 无论哪条命令触发 gateway 自动启动，gateway 继承的都是隔离 env。
fn journey_cli(home: &Path, args: &[&str]) -> CliOutcome {
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
    let deadline = Instant::now() + CLI_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait agentcpm-pd") {
            break status;
        }
        if Instant::now() >= deadline {
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

/// 失败断言：非零退出码 + `outcome=failed` + `error.code`。
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must not exit 0: {}", out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout not JSON ({e}): {}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}`: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// 装置侧系统 git（与 Gateway 的派生同隔离）。
fn git_sync(cwd: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

/// 播种一个可克隆的本地裸仓（确定性内容，零网络）。
fn seed_bare_repo(parent: &Path, name: &str, empty: bool) -> PathBuf {
    let bare = parent.join(format!("{name}.git"));
    let _ = std::fs::remove_dir_all(&bare);
    if empty {
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "--bare", "-b", "main"])
                .arg(&bare)
                .status()
                .expect("git init --bare")
                .success()
        );
        return bare;
    }
    let work = parent.join(format!("{name}-work"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create seed worktree");
    git_sync(&work, &["init", "-q", "-b", "main"]);
    git_sync(&work, &["config", "commit.gpgsign", "false"]);
    git_sync(&work, &["config", "user.email", "seed@example.invalid"]);
    git_sync(&work, &["config", "user.name", "Seed"]);
    std::fs::write(work.join("README.md"), "j1 seed\n").unwrap();
    git_sync(&work, &["add", "."]);
    git_sync(&work, &["commit", "-q", "-m", "seed"]);
    git_sync(
        &work,
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    let _ = std::fs::remove_dir_all(&work);
    bare
}

/// 授权根内的 server path（default 根惰性 bootstrap 为 `<home>/workspaces`）。
fn server_path(relative: &str) -> String {
    json!({
        "authorizedRootId": "default",
        "relativePath": {"encoding": "utf8", "value": relative},
    })
    .to_string()
}

/// 把输入对象写进 home 内的受保护文件并返回其路径（`--input-json` 通道；
/// secure 字段与大体量分片一律走文件，不进 argv）。
fn input_file(home: &Path, name: &str, input: &Value) -> String {
    let dir = home.join("inputs");
    std::fs::create_dir_all(&dir).expect("create inputs dir");
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, input.to_string()).expect("write input file");
    path.to_str().expect("utf8 path").to_owned()
}

/// argv 命令 + `--input-json` 文件（secure 字段通道，t014 `dispatch_input`
/// 先例）：`<path...> --output json [--request-key k] --input-json <file>`。
fn dispatch_input(
    home: &Path,
    name: &str,
    path: &[&str],
    input: &Value,
    key: Option<&str>,
) -> CliOutcome {
    let file = input_file(home, name, input);
    let mut args: Vec<String> = path.iter().map(|p| (*p).to_owned()).collect();
    args.push("--output".to_owned());
    args.push("json".to_owned());
    if let Some(key) = key {
        args.push("--request-key".to_owned());
        args.push(key.to_owned());
    }
    args.push("--input-json".to_owned());
    args.push(file);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    journey_cli(home, &refs)
}

/// `api invoke <methodId> --input-json <file>`（uploads/documents 等以 JSON
/// 对象为主体的操作；大体量分片同样走文件，argv 不承载）。
fn invoke_input(
    home: &Path,
    name: &str,
    method: &str,
    input: &Value,
    key: Option<&str>,
) -> CliOutcome {
    let file = input_file(home, name, input);
    let mut args: Vec<String> = vec![
        "api".to_owned(),
        "invoke".to_owned(),
        "--output".to_owned(),
        "json".to_owned(),
        method.to_owned(),
    ];
    if let Some(key) = key {
        args.push("--request-key".to_owned());
        args.push(key.to_owned());
    }
    args.push("--input-json".to_owned());
    args.push(file);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    journey_cli(home, &refs)
}

/// 轮询 `operations get` 至终态并返回 operation 投影。
fn wait_terminal_operation(home: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = journey_cli(
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 逐页读回操作的事件流（`--page {"limit":1}` 强制分页 + `--after-cursor`
/// 链式），断言事件游标跨页严格单调（J13 先例）。返回事件类型序列。
fn paged_operation_events(home: &Path, operation_id: &str) -> Vec<String> {
    let mut types = Vec::new();
    let mut cursors: Vec<u64> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..32 {
        let mut args: Vec<String> = vec![
            "operations".to_owned(),
            "events".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--operation-id".to_owned(),
            operation_id.to_owned(),
            "--page".to_owned(),
            r#"{"limit":1}"#.to_owned(),
        ];
        if let Some(cursor) = &after {
            args.push("--after-cursor".to_owned());
            args.push(cursor.clone());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let page = expect_ok("operations.events", &journey_cli(home, &refs));
        let frame = &page["frame"];
        for event in frame["events"].as_array().cloned().unwrap_or_default() {
            types.push(event["type"].as_str().unwrap_or_default().to_owned());
            let cursor = event["cursor"].as_str().unwrap_or_default().to_owned();
            let seq = cursor
                .trim_start_matches("cur-")
                .parse::<u64>()
                .unwrap_or(u64::MAX);
            if let Some(last) = cursors.last() {
                assert!(
                    seq > *last,
                    "operation event cursor must be strictly increasing: {last} -> {seq}"
                );
            }
            cursors.push(seq);
        }
        if frame["terminal"] == true {
            assert_eq!(frame["caughtUp"], true, "terminal page must be caught up");
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

/// 轮询 `interaction list`（state=pending）直到 `run_id` 的待处理交互出现，
/// 返回 (interactionId, revision)。
async fn poll_pending(home: &Path, run_id: &str) -> (String, String) {
    // 满载预算（判据不变）：等 worker 开出的待办交互出现，15s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = journey_cli(
            home,
            &[
                "interaction",
                "list",
                "--output",
                "json",
                "--state",
                "pending",
            ],
        );
        let listed = expect_ok("interaction.list", &out);
        for item in listed["items"].as_array().cloned().unwrap_or_default() {
            if item["runId"].as_str() == Some(run_id) {
                return (
                    item["interactionId"].as_str().unwrap().to_owned(),
                    item["revision"].as_str().unwrap().to_owned(),
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "no pending interaction for run `{run_id}`"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 等待 `<home>/v1/archives/` 出现可通过 `archive get` 读回的 archive
/// （t012 先例：id 经落盘布局发现，读取本身走公开操作）。
async fn wait_for_archive_ready(home: &Path) -> String {
    let dir = home.join("v1").join("archives");
    // 满载预算（判据不变）：等待同一事件落定的墙钟上限，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let found: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        if let Some(archive_id) = found.first() {
            let out = journey_cli(
                home,
                &[
                    "archive",
                    "get",
                    "--output",
                    "json",
                    "--archive-id",
                    archive_id,
                ],
            );
            if out.code == 0 {
                expect_ok("archive.get", &out);
                return archive_id.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "no readable archive appeared under {} within 60s",
        dir.display()
    );
}

/// 打开实例的存储联调点（j12 先例：285 操作合同无历史写入方，历史行经
/// 存储 API 落地——未来会话投影将调用的同一写入口）。只在全部 run 终态后
/// 打开，`open_and_recover` 的启动恢复无物可打断。
fn seeding_store(root: &Path) -> std::sync::Arc<pilotdeck_gateway::public_v1::store::V1Store> {
    use pilotdeck_gateway::public_v1::store::{InstanceMeta, V1Store};

    let raw = std::fs::read_to_string(root.join("local.json")).expect("local.json");
    let local: Value = serde_json::from_str(&raw).expect("parse local.json");
    let ready = std::fs::read_to_string(root.join("run").join("ready.json")).expect("ready.json");
    let ready: Value = serde_json::from_str(&ready).expect("parse ready.json");
    let meta = InstanceMeta {
        instance_id: local["instanceId"].as_str().unwrap().to_owned(),
        owner_id: local["ownerId"].as_str().unwrap().to_owned(),
        product: local["product"].as_str().unwrap().to_owned(),
        protocol_version: local["protocolVersion"].as_str().unwrap().to_owned(),
        node_id: local["nodeId"].as_str().unwrap().to_owned(),
        generation: ready["generation"].as_str().unwrap().to_owned(),
        server_revision: ready["serverRevision"].as_str().unwrap().to_owned(),
        initialization_revision: local["initializationRevision"].as_str().unwrap().to_owned(),
    };
    V1Store::open_and_recover(root, meta).expect("open seeding store")
}

async fn session_revision(home: &Path, session_id: &str) -> String {
    let out = journey_cli(
        home,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    expect_ok("session.get", &out)["session"]["revision"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn git_status(home: &Path, project_id: &str) -> Value {
    let out = journey_cli(
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

/// 旅程 J1 正向主干：见文件头步骤清单。每步 stdout envelope 都过嵌入
/// registry 的 schema 校验，关键状态（revision/token/游标/终态）逐项断言。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j1_project_full_lifecycle() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let home = TestHome::new("j1-full");
    let root = home.path();

    // ------------------------------------------------------------------
    // 1. instance init：唯一身份 + 协议版本。
    // ------------------------------------------------------------------
    let init = init_home(root);
    let instance_id = init["identity"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();
    assert!(!instance_id.is_empty());
    assert_eq!(init["identity"]["protocolVersion"], json!(OP_VERSION));
    assert_eq!(init["configured"], true);

    // ------------------------------------------------------------------
    // 2. 目录与工作区：clone 目录（workspace prepare，本地裸仓）+ 工作区
    //    目录（directories create），再登记旅程项目与会话。
    // ------------------------------------------------------------------
    let bare_clone = seed_bare_repo(root, "j1-bare-clone", false);
    let clone_url = format!("file://{}", bare_clone.display());

    // 工作区目录（journey project 的根）。
    let out = journey_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "j1-dir-ws",
            "--path",
            &server_path("j1-ws"),
        ],
    );
    let made = expect_ok("directories.create", &out);
    assert_eq!(made["created"], true);
    let workspaces_root = root.join("workspaces");
    let workspace = workspaces_root.join("j1-ws");
    assert!(
        workspace.is_dir(),
        "工作区目录落盘: {}",
        workspace.display()
    );

    // clone 目录：workspace prepare（clone 模式）从本地裸仓克隆并登记项目。
    let out = journey_cli(
        root,
        &[
            "workspace",
            "prepare",
            "--output",
            "json",
            "--mode",
            "clone",
            "--destination",
            &server_path("j1-clone"),
            "--repository-url",
            &clone_url,
            "--branch",
            "main",
            "--display-name",
            "J1 Clone",
            "--request-key",
            "j1-prepare-clone",
        ],
    );
    let receipt = expect_ok("workspaces.prepare", &out);
    let clone_operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let clone_operation = wait_terminal_operation(root, &clone_operation_id);
    assert_eq!(clone_operation["state"], "succeeded", "{clone_operation}");
    let clone_completion = &clone_operation["result"];
    assert_eq!(clone_completion["mode"], "clone");
    let cloned_project_id = clone_completion["projectId"].as_str().unwrap().to_owned();
    let cloned_dir = workspaces_root.join("j1-clone");
    assert!(cloned_dir.join("README.md").is_file(), "克隆内容就位");
    let staging: Vec<_> = std::fs::read_dir(&workspaces_root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".pd-staging-"))
        .collect();
    assert!(staging.is_empty(), "staging 无残留: {staging:?}");

    // ------------------------------------------------------------------
    // 3. A2 版本一致（段首一次）：--version 与握手 target.gatewayVersion。
    //    （gateway 已由上面的命令以隔离 env 启动，这里连接既有进程。）
    // ------------------------------------------------------------------
    let version_out = std::process::Command::new(BIN)
        .arg("--version")
        .output()
        .expect("spawn agentcpm-pd --version");
    assert!(version_out.status.success(), "version exit");
    let version_line = String::from_utf8_lossy(&version_out.stdout)
        .trim()
        .to_owned();
    let cli_version = version_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    assert!(
        version_line.starts_with("agentcpm-pd ") && !cli_version.is_empty(),
        "unexpected --version output: {version_line}"
    );
    let mut version_conn = ConnectSession::start(root).await;
    let handshake = version_conn.initialize().await;
    assert_eq!(
        handshake["result"]["target"]["gatewayVersion"],
        json!(cli_version),
        "--version 与握手 gatewayVersion 必须一致: {version_line}"
    );
    assert_eq!(handshake["result"]["abi_version"], 1);
    assert!(
        handshake["result"]["connectionId"]
            .as_str()
            .is_some_and(|c| !c.is_empty())
    );
    version_conn.close_stdin().await;
    version_conn
        .wait_exit(0, "version probe api connect exits cleanly")
        .await;

    // ------------------------------------------------------------------
    // 4. 旅程项目与会话（projects register on j1-ws）。
    // ------------------------------------------------------------------
    let project_id = "p-j1";
    let out = journey_cli(
        root,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--name",
            "J1 Journey",
            "--environment-id",
            "local",
            "--root",
            &json!({
                "instanceId": instance_id,
                "rootId": "default",
                "relativePath": "j1-ws",
            })
            .to_string(),
        ],
    );
    expect_ok("project.register", &out);
    let session_id = "s-j1";
    let out = journey_cli(
        root,
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

    // ------------------------------------------------------------------
    // 5. 附件：uploads create → part×2 → complete → 分块读回。
    // ------------------------------------------------------------------
    let part_zero = {
        let mut bytes = vec![0xa5u8; 512 * 1024];
        for (i, slot) in bytes.iter_mut().enumerate() {
            *slot ^= (i % 251) as u8;
        }
        bytes
    };
    let part_one = b"j1 second part payload".to_vec();
    let mut whole = Vec::new();
    whole.extend_from_slice(&part_zero);
    whole.extend_from_slice(&part_one);
    let total_sha = sha256_hex(&whole);

    let out = invoke_input(
        root,
        "up-create",
        "uploads.create",
        &json!({
            "projectId": project_id,
            "displayName": "j1.bin",
            "mediaType": "application/octet-stream",
            "byteLength": whole.len(),
            "sha256": total_sha,
        }),
        Some("j1-up-create"),
    );
    let created = expect_ok("uploads.create", &out);
    let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();
    assert_eq!(created["upload"]["state"], "receiving");

    let out = invoke_input(
        root,
        "up-part-0",
        "uploads.part",
        &json!({
            "uploadId": upload_id,
            "partNumber": 0,
            "bytesBase64": b64_encode(&part_zero),
            "sha256": sha256_hex(&part_zero),
        }),
        Some("j1-up-p0"),
    );
    let accepted_zero = expect_ok("uploads.part", &out);
    assert_eq!(accepted_zero["deduplicated"], false);
    let out = invoke_input(
        root,
        "up-part-1",
        "uploads.part",
        &json!({
            "uploadId": upload_id,
            "partNumber": 1,
            "bytesBase64": b64_encode(&part_one),
            "sha256": sha256_hex(&part_one),
        }),
        Some("j1-up-p1"),
    );
    expect_ok("uploads.part", &out);

    let out = invoke_input(
        root,
        "up-complete",
        "uploads.complete",
        &json!({"uploadId": upload_id, "expectedSha256": total_sha}),
        Some("j1-up-complete"),
    );
    let receipt = expect_ok("uploads.complete", &out);
    assert_eq!(receipt["kind"], "operation");
    let upload_operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(upload_operation["state"], "succeeded", "{upload_operation}");

    // uploads.get 至 complete + 产物读回（分块、逐字节一致）。
    let mut artifact_id = None;
    // 满载预算（判据不变）：等待同一事件落定的墙钟上限，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let out = invoke_input(
            root,
            "up-get",
            "uploads.get",
            &json!({"uploadId": upload_id}),
            None,
        );
        let got = expect_ok("uploads.get", &out);
        if got["upload"]["state"] == "complete" {
            artifact_id = got["upload"]["artifactId"].as_str().map(str::to_owned);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let artifact_id = artifact_id.expect("upload reaches complete");

    let out = journey_cli(
        root,
        &[
            "artifacts",
            "get",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
        ],
    );
    let meta = expect_ok("artifacts.get", &out)["artifact"].clone();
    assert_eq!(meta["byteLength"], json!(whole.len()));
    assert_eq!(meta["sha256"], json!(total_sha));
    let mut assembled = Vec::new();
    let mut offset = 0u64;
    loop {
        let out = journey_cli(
            root,
            &[
                "artifacts",
                "read",
                "--output",
                "json",
                "--artifact-id",
                &artifact_id,
                "--offset",
                &offset.to_string(),
                "--max-bytes",
                "262144",
            ],
        );
        let page = expect_ok("artifacts.read", &out);
        let chunk = &page["chunk"];
        let bytes = b64_decode(chunk["bytesBase64"].as_str().unwrap_or_default());
        assert_eq!(chunk["offset"], json!(offset));
        assembled.extend_from_slice(&bytes);
        offset += bytes.len() as u64;
        if chunk["eof"] == true {
            break;
        }
    }
    assert_eq!(assembled, whole, "产物与上传内容逐字节一致");
    assert!(
        root.join("v1")
            .join("artifacts")
            .join(&artifact_id)
            .is_file(),
        "产物不可变落盘: {artifact_id}"
    );

    // ------------------------------------------------------------------
    // 6. 文档预览（长作业）：CSV → manifest + sheet，事件流游标单调。
    // ------------------------------------------------------------------
    let csv = "a,b\n1,2\n3,4\n".to_owned();
    let csv_sha = sha256_hex(csv.as_bytes());
    let out = invoke_input(
        root,
        "csv-create",
        "uploads.create",
        &json!({
            "projectId": project_id,
            "displayName": "grid.csv",
            "mediaType": "text/csv",
            "byteLength": csv.len(),
            "sha256": csv_sha,
        }),
        Some("j1-csv-create"),
    );
    let csv_upload = expect_ok("uploads.create", &out)["upload"]["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = invoke_input(
        root,
        "csv-part",
        "uploads.part",
        &json!({
            "uploadId": csv_upload,
            "partNumber": 0,
            "bytesBase64": b64_encode(csv.as_bytes()),
            "sha256": csv_sha,
        }),
        Some("j1-csv-part"),
    );
    expect_ok("uploads.part", &out);
    let out = invoke_input(
        root,
        "csv-complete",
        "uploads.complete",
        &json!({"uploadId": csv_upload, "expectedSha256": csv_sha}),
        Some("j1-csv-complete"),
    );
    let receipt = expect_ok("uploads.complete", &out);
    let csv_operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(csv_operation["state"], "succeeded", "{csv_operation}");
    let mut csv_artifact = None;
    // 满载预算（判据不变）：等待同一事件落定的墙钟上限，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let out = invoke_input(
            root,
            "csv-get",
            "uploads.get",
            &json!({"uploadId": csv_upload}),
            None,
        );
        let got = expect_ok("uploads.get", &out);
        if got["upload"]["state"] == "complete" {
            csv_artifact = got["upload"]["artifactId"].as_str().map(str::to_owned);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let csv_artifact = csv_artifact.expect("csv upload completes");
    let out = journey_cli(
        root,
        &[
            "artifacts",
            "get",
            "--output",
            "json",
            "--artifact-id",
            &csv_artifact,
        ],
    );
    let csv_revision = expect_ok("artifacts.get", &out)["artifact"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = invoke_input(
        root,
        "doc-manifest",
        "documents.preview",
        &json!({
            "source": {"artifactId": csv_artifact},
            "expectedRevision": csv_revision,
            "output": "spreadsheetManifest",
        }),
        Some("j1-doc-manifest"),
    );
    let receipt = expect_ok("documents.preview", &out);
    let preview_operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let preview_operation = wait_terminal_operation(root, &preview_operation_id);
    assert_eq!(
        preview_operation["state"], "succeeded",
        "{preview_operation}"
    );
    let preview_result = preview_operation["result"].clone();
    assert!(
        preview_result["artifacts"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "manifest 预览必须携带产物: {preview_result}"
    );
    assert!(preview_result["manifestSchemaId"].as_str().is_some());

    let out = invoke_input(
        root,
        "doc-sheet",
        "documents.preview",
        &json!({
            "source": {"artifactId": csv_artifact},
            "expectedRevision": csv_revision,
            "output": "spreadsheetSheet",
            "sheetId": "0",
            "rowStart": 1,
            "rowCount": 1,
        }),
        Some("j1-doc-sheet"),
    );
    let receipt = expect_ok("documents.preview", &out);
    let sheet_operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(sheet_operation["state"], "succeeded", "{sheet_operation}");

    // 订阅 operation 事件（判据面）：
    // * documents.preview 走 uploads 的 async_operations 数据面（无事件
    //   日志），其可观察进度是 operations.get 的状态转移——终态后
    //   `operations events` 诚实报告空帧 + terminal/caughtUp；
    // * 事件游标单调的判据落在 clone 操作（operations 长操作面：生命周期
    //   + 域事件 projects.changed/git.changed 都以 operation 为资源入日志）。
    let out = journey_cli(
        root,
        &[
            "operations",
            "events",
            "--output",
            "json",
            "--operation-id",
            &preview_operation_id,
        ],
    );
    let preview_events = expect_ok("operations.events", &out);
    assert_eq!(
        preview_events["frame"]["events"].as_array().map(Vec::len),
        Some(0),
        "uploads-plane 异步操作无事件日志（进度面是 operations.get）: {preview_events}"
    );
    assert_eq!(preview_events["frame"]["terminal"], true);
    assert_eq!(preview_events["frame"]["caughtUp"], true);

    let clone_event_types = paged_operation_events(root, &clone_operation_id);
    assert_eq!(
        clone_event_types.first().map(String::as_str),
        Some("operation.queued"),
        "克隆操作的事件序列: {clone_event_types:?}"
    );
    assert!(
        clone_event_types.iter().any(|t| t == "operation.completed"),
        "克隆操作完成事件: {clone_event_types:?}"
    );
    assert!(
        clone_event_types.iter().any(|t| t == "projects.changed"),
        "克隆登记事件: {clone_event_types:?}"
    );
    assert!(
        clone_event_types.iter().any(|t| t == "git.changed"),
        "克隆 git 事件: {clone_event_types:?}"
    );

    // ------------------------------------------------------------------
    // 7. 审批与问答交互（含对话编辑租约，ttlMs 显式）。
    // ------------------------------------------------------------------
    let approval = submit_run(root, session_id, "[pd-driver:approval]", "j1-app").await;
    let approval_run = approval["runId"].as_str().unwrap().to_owned();
    let (approval_id, approval_rev) = poll_pending(root, &approval_run).await;
    let out = journey_cli(
        root,
        &[
            "interaction",
            "get",
            "--output",
            "json",
            "--interaction-id",
            &approval_id,
        ],
    );
    let got = expect_ok("interaction.get", &out)["interaction"].clone();
    assert_eq!(got["kind"], "approval");
    assert_eq!(got["state"], "pending");

    let out = journey_cli(
        root,
        &[
            "approval",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &approval_id,
            "--expected-revision",
            &approval_rev,
            "--decision",
            "allow",
        ],
    );
    let responded = expect_ok("approval.respond", &out);
    assert_eq!(responded["interaction"]["state"], "answered");
    let waited = expect_ok("run.wait", &wait_run(root, &approval_run));
    assert_eq!(waited["run"]["state"], "succeeded");

    // 问答 + 租约：claim（显式 ttlMs）→ respond（租约不授予应答权）→ release。
    let question = submit_run(root, session_id, "[pd-driver:question]", "j1-q").await;
    let question_run = question["runId"].as_str().unwrap().to_owned();
    let (question_id, question_rev) = poll_pending(root, &question_run).await;
    let out = journey_cli(
        root,
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &question_id,
            "--expected-revision",
            &question_rev,
            "--lease-id",
            "j1-lease",
            "--ttl-ms",
            "60000",
        ],
    );
    let claim = expect_ok("interaction.claim", &out);
    assert_eq!(claim["leaseId"], "j1-lease");
    assert!(claim["expiresAt"].as_str().is_some());
    let rev_after_claim = claim["revision"].as_str().unwrap().to_owned();

    let out = journey_cli(
        root,
        &[
            "interaction",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &question_id,
            "--expected-revision",
            &rev_after_claim,
            "--answer",
            "{\"text\":\"j1 leased answer\"}",
        ],
    );
    let answered = expect_ok("interaction.respond", &out);
    assert_eq!(answered["interaction"]["state"], "answered");
    let rev_after_answer = answered["interaction"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = journey_cli(
        root,
        &[
            "interaction",
            "release",
            "--output",
            "json",
            "--interaction-id",
            &question_id,
            "--expected-revision",
            &rev_after_answer,
            "--lease-id",
            "j1-lease",
        ],
    );
    let released = expect_ok("interaction.release", &out);
    assert_eq!(released["released"], true);
    let got = journey_cli(
        root,
        &[
            "interaction",
            "get",
            "--output",
            "json",
            "--interaction-id",
            &question_id,
        ],
    );
    let after = expect_ok("interaction.get", &got)["interaction"].clone();
    assert_eq!(after["state"], "answered");
    assert_eq!(after["leaseOwner"], Value::Null, "释放只撤租约");
    let waited = expect_ok("run.wait", &wait_run(root, &question_run));
    assert_eq!(waited["run"]["state"], "succeeded");

    // ------------------------------------------------------------------
    // 8. 事件洪泛下的审批应答（T7 判据切片）：另一会话洪泛 2000 事件，
    //    回放期间应答审批并计时；事件不重不漏、completed 终态、账目闭合。
    // ------------------------------------------------------------------
    let flood_session = "s-j1-flood";
    let out = journey_cli(
        root,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            flood_session,
            "--project-id",
            project_id,
        ],
    );
    expect_ok("session.create", &out);
    let flood_app_session = "s-j1-flood-app";
    let out = journey_cli(
        root,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            flood_app_session,
            "--project-id",
            project_id,
        ],
    );
    expect_ok("session.create", &out);

    let flood = submit_run(root, flood_session, "[pd-driver:events=2000]", "j1-flood").await;
    let flood_run = flood["runId"].as_str().unwrap().to_owned();
    let flood_approval = submit_run(
        root,
        flood_app_session,
        "[pd-driver:approval]",
        "j1-flood-app",
    )
    .await;
    let flood_approval_run = flood_approval["runId"].as_str().unwrap().to_owned();

    // 写入风暴先行落盘（T7 教训：fsync 风暴期间测量是盘状态函数），
    // 测量窗口只覆盖投递路径。
    let burst_wait = wait_run(root, &flood_run);
    let burst = expect_envelope("run.wait", &burst_wait);
    assert_eq!(burst["run"]["state"], "succeeded");

    let (flood_approval_id, flood_approval_rev) = poll_pending(root, &flood_approval_run).await;

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;
    let mut rx = conn.take_rx();
    let mut last_cursor: Option<String> = None;
    let mut active_sub: Option<String> = None;
    let mut open_id: Option<String> = None;
    let mut chain: HashSet<String> = HashSet::new();
    let mut reopen_budget = 16usize;
    let mut events_seen = 0usize;
    let mut respond_sent_at: Option<Instant> = None;
    let mut respond_latency: Option<Duration> = None;
    let mut flood_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);

    'flood: while tokio::time::Instant::now() < deadline {
        if active_sub.is_none() && open_id.is_none() {
            let mut input = json!({"runId": flood_run});
            if let Some(cursor) = &last_cursor {
                input["afterCursor"] = json!(cursor);
            }
            let id = conn.fresh_id("sub");
            conn.send(&rpc_request(
                &id,
                "sub.open",
                json!({"stream": "run.events", "input": input, "credit": 300}),
            ))
            .await;
            open_id = Some(id);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = match tokio::time::timeout(remaining, rx.recv_frame()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) => break,
        };
        if let Some(id) = frame.get("id").and_then(Value::as_str) {
            if id == "j1-flood-respond" {
                assert!(
                    frame.get("error").is_none(),
                    "洪泛期间的审批应答失败: {frame}; stderr:\n{}",
                    conn.stderr_log.lock().unwrap()
                );
                assert_eq!(
                    frame["result"]["result"]["interaction"]["state"], "answered",
                    "应答回执: {frame}"
                );
                respond_latency = Some(respond_sent_at.expect("sent").elapsed());
                continue;
            }
            if open_id.as_deref() == Some(id) {
                let sub = frame["result"]["subscriptionId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("flood sub.open failed: {frame}"))
                    .to_owned();
                chain.insert(sub.clone());
                conn.note_opened_sub(&sub);
                active_sub = Some(sub);
                open_id = None;
            }
            continue;
        }
        let sub = frame["params"]["subscriptionId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") if chain.contains(&sub) => {
                events_seen += 1;
                if let Some(cursor) = frame["params"]["event"]["cursor"].as_str() {
                    last_cursor = Some(cursor.to_owned());
                }
                // 恒定补给已消费的 credit，在途窗口不增长。
                if events_seen.is_multiple_of(50)
                    && let Some(active) = active_sub.clone()
                {
                    conn.grant_credit(&active, 50).await;
                }
                // 首个事件到达即应答审批（构造性洪泛中样本）。
                if respond_sent_at.is_none() {
                    respond_sent_at = Some(Instant::now());
                    conn.send(&rpc_request(
                        "j1-flood-respond",
                        "approval.respond",
                        json!({
                            "input": {
                                "interactionId": flood_approval_id,
                                "expectedRevision": flood_approval_rev,
                                "decision": "allow",
                            }
                        }),
                    ))
                    .await;
                }
            }
            Some("sub.end") if active_sub.as_deref() == Some(sub.as_str()) => {
                match frame["params"]["terminal"].as_str() {
                    Some("completed") => {
                        flood_done = true;
                        break 'flood;
                    }
                    Some("slow_consumer") => {
                        reopen_budget -= 1;
                        assert!(
                            reopen_budget > 0,
                            "洪泛订阅反复 slow_consumer: {frame}; stderr:\n{}",
                            conn.stderr_log.lock().unwrap()
                        );
                        if let Some(resume) = frame["params"]["resumeCursor"].as_str() {
                            last_cursor = Some(resume.to_owned());
                        }
                        active_sub = None; // 从 resumeCursor 重开
                    }
                    other => panic!(
                        "意外的洪泛终态 {other:?}: {frame}; stderr:\n{}",
                        conn.stderr_log.lock().unwrap()
                    ),
                }
            }
            _ => {}
        }
    }

    assert!(flood_done, "洪泛必须到达 completed 终态");
    assert!(
        events_seen > 1500,
        "洪泛事件必须真实流动（2003 事件，读到 {events_seen}）"
    );
    let latency = respond_latency.expect("审批应答必须收到回执");
    println!("J1 approval.respond during {events_seen}-event flood: {latency:?} (T7 上限判据)");
    assert!(
        latency < Duration::from_millis(2000),
        "洪泛期间审批应答延迟 {latency:?} 超出 T7 守护上限"
    );
    let waited = expect_ok("run.wait", &wait_run(root, &flood_approval_run));
    assert_eq!(waited["run"]["state"], "succeeded");
    conn.drain_channel_to_journal();
    conn.assert_subscription_accounting();
    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after the flood probe").await;

    // ------------------------------------------------------------------
    // 9. 终端：租约 → 输入 → 调整 → 游标链式读回 → attach → close。
    // ------------------------------------------------------------------
    let out = journey_cli(
        root,
        &[
            "terminal",
            "create",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--rows",
            "24",
            "--columns",
            "80",
        ],
    );
    let terminal = expect_ok("terminals.create", &out)["terminal"].clone();
    let terminal_id = terminal["terminalId"].as_str().unwrap().to_owned();
    let server_epoch = terminal["serverEpoch"].as_str().unwrap().to_owned();
    assert_eq!(terminal["state"], "running");

    // claim（leaseToken 是 secure 字段 → --input-json 文件）。
    let out = dispatch_input(
        root,
        "term-claim",
        &["terminal", "control"],
        &json!({
            "terminalId": terminal_id,
            "action": "acquire",
            "expectedControlEpoch": 0,
        }),
        None,
    );
    let lease = expect_ok("terminals.control", &out)["lease"].clone();
    let lease_token = lease["leaseToken"].as_str().unwrap().to_owned();
    let control_epoch = lease["controlEpoch"].as_i64().unwrap();
    assert_eq!(control_epoch, 1);

    // 输入（inputSeq 连续）。
    let out = dispatch_input(
        root,
        "term-input",
        &["terminal", "input"],
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": control_epoch,
            "leaseToken": lease_token,
            "inputSeq": 1,
            "bytesBase64": b64_encode(b"printf j1mark\\n\n"),
        }),
        None,
    );
    let ack = expect_ok("terminals.input", &out);
    assert_eq!(ack["acceptedThroughSeq"], 1);
    assert_eq!(ack["deduplicated"], false);

    // 调整尺寸。
    let out = dispatch_input(
        root,
        "term-resize",
        &["terminal", "resize"],
        &json!({
            "terminalId": terminal_id,
            "controlEpoch": control_epoch,
            "leaseToken": lease_token,
            "rows": 30,
            "columns": 100,
        }),
        None,
    );
    expect_ok("terminals.resize", &out);
    let out = journey_cli(
        root,
        &[
            "terminal",
            "get",
            "--output",
            "json",
            "--terminal-id",
            &terminal_id,
        ],
    );
    let resized = expect_ok("terminals.get", &out)["terminal"].clone();
    assert_eq!(resized["rows"], 30);
    assert_eq!(resized["columns"], 100);

    // 游标链式读回：byteOffset 跨 bytes 帧严格单调不重，直到标记出现。
    let out = journey_cli(
        root,
        &[
            "terminal",
            "output",
            "--output",
            "json",
            "--terminal-id",
            &terminal_id,
        ],
    );
    let mut frame = expect_ok("terminals.output", &out);
    let output_epoch = frame["cursor"]["outputEpoch"].as_str().unwrap().to_owned();
    let mut offset = frame["cursor"]["byteOffset"].as_u64().unwrap();
    let mut text = String::from_utf8_lossy(&b64_decode(
        frame["bytesBase64"].as_str().unwrap_or_default(),
    ))
    .into_owned();
    let mut frames = 1usize;
    while !text.contains("j1mark") {
        assert!(
            frames < 120,
            "marker never reached the output stream: {text}"
        );
        frames += 1;
        let cursor_json = json!({"outputEpoch": output_epoch, "byteOffset": offset}).to_string();
        let out = journey_cli(
            root,
            &[
                "terminal",
                "output",
                "--output",
                "json",
                "--terminal-id",
                &terminal_id,
                "--cursor",
                &cursor_json,
            ],
        );
        frame = expect_ok("terminals.output", &out);
        match frame["type"].as_str().unwrap_or_default() {
            "bytes" => {
                let start = frame["cursor"]["byteOffset"].as_u64().unwrap();
                assert!(start >= offset, "输出游标不重不退: {start} < {offset}");
                let next = frame["nextCursor"]["byteOffset"].as_u64().unwrap();
                assert!(next > start, "bytes 帧必须前进: {start} -> {next}");
                text.push_str(&String::from_utf8_lossy(&b64_decode(
                    frame["bytesBase64"].as_str().unwrap_or_default(),
                )));
                offset = next;
            }
            "heartbeat" => std::thread::sleep(Duration::from_millis(100)),
            other => panic!("意外的输出帧类型 {other}: {frame}"),
        }
    }
    assert!(text.contains("j1mark"), "标记到达输出流: {text}");

    // 释放租约（attach 进程要自行 acquire）。
    let out = dispatch_input(
        root,
        "term-release",
        &["terminal", "control"],
        &json!({
            "terminalId": terminal_id,
            "action": "release",
            "expectedControlEpoch": control_epoch,
            "leaseToken": lease_token,
        }),
        None,
    );
    let released = expect_ok("terminals.control", &out)["terminal"].clone();
    assert_eq!(released["controlEpoch"], control_epoch + 1);

    // attach（机器模式子进程）：回显一次按键。
    let mut cmd = base_command(root);
    cmd.args([
        "terminal",
        "attach",
        "--machine",
        "--terminal-id",
        &terminal_id,
    ]);
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn terminal attach");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Value>(256);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(frame) = serde_json::from_str::<Value>(&line)
                && frame_tx.send(frame).await.is_err()
            {
                break;
            }
        }
    });
    let key = b64_encode(b"q");
    stdin
        .write_all(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"k1\",\"method\":\"term.input\",\"params\":{{\"bytesBase64\":\"{key}\"}}}}\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    // 满载预算（判据不变）：等 PTY 回显按键的单帧，15s→60s。
    let echoed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let frame = frame_rx
                .recv()
                .await
                .expect("frame channel stays open while the terminal lives");
            let matched = frame["method"] == "term.output"
                && frame["params"]["bytesBase64"]
                    .as_str()
                    .is_some_and(|b| b64_decode(b).contains(&b'q'));
            if matched {
                return frame;
            }
        }
    })
    .await
    .expect("the pty echoes the key within 60s");
    assert!(
        echoed["params"]["cursor"]["outputEpoch"].is_string(),
        "term.output 携带输出游标: {echoed}"
    );

    // close（graceful）：attach 收到 term.exit 并以 0 退出。
    let out = journey_cli(
        root,
        &[
            "terminal",
            "close",
            "--output",
            "json",
            "--terminal-id",
            &terminal_id,
            "--expected-server-epoch",
            &server_epoch,
            "--mode",
            "graceful",
        ],
    );
    let close_receipt = expect_ok("terminals.close", &out);
    assert_eq!(close_receipt["kind"], "operation");
    // 满载预算（判据不变）：graceful close 后等 attach 的 term.exit 帧，20s→60s。
    let exit = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let frame = frame_rx.recv().await.expect("frame channel open");
            if frame["method"] == "term.exit" {
                return frame;
            }
        }
    })
    .await
    .expect("attach reports term.exit after close");
    assert_eq!(exit["params"]["state"], "exited");
    drop(stdin);
    // 满载预算（判据不变）：等 attach 子进程随终端退出，20s→60s。
    let code = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("attach exits after the terminal")
        .expect("wait attach")
        .code();
    assert_eq!(code, Some(0), "attach 在终端退出后以 0 退出");

    // ------------------------------------------------------------------
    // 10. git：产品身份门 → 提交 → 创建式推送（expectedRemoteOid=""）。
    // ------------------------------------------------------------------
    git_sync(&workspace, &["init", "-q", "-b", "main"]);
    git_sync(&workspace, &["config", "commit.gpgsign", "false"]);
    write_file(&workspace.join("hello.txt"), "j1 hello\n");
    let bare_push = seed_bare_repo(root, "j1-bare-push", true);
    git_sync(
        &workspace,
        &["remote", "add", "origin", bare_push.to_str().unwrap()],
    );

    // 身份（产品路径）。
    let out = journey_cli(
        root,
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            project_id,
            "--name",
            "J1 User",
            "--email",
            "j1@example.com",
            "--expected-revision",
            "",
        ],
    );
    let identity = expect_ok("git_identity.set", &out);
    let identity_revision = identity["revision"].as_str().unwrap().to_owned();
    assert!(!identity_revision.is_empty());
    let out = journey_cli(
        root,
        &[
            "git",
            "identity",
            "get",
            "--output",
            "json",
            "--project-id",
            project_id,
        ],
    );
    let got = expect_ok("git_identity.get", &out);
    assert_eq!(got["source"], "project");
    assert_eq!(got["name"], "J1 User");

    let state = git_status(root, project_id);
    assert_eq!(state["unborn"], true);
    assert_eq!(state["remoteNames"], json!(["origin"]));
    let token = state_token(&state);

    // suggest-message：无模型装配 → MODEL_UNAVAILABLE（不以空串伪装成功）。
    let out = dispatch_input(
        root,
        "git-suggest",
        &["git", "suggest-message"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
        }),
        None,
    );
    assert_eq!(
        expect_fail("git.suggest_message", &out),
        "MODEL_UNAVAILABLE",
        "无模型时建议不得伪装成功"
    );

    // initial commit（产品身份门通过）。
    let out = dispatch_input(
        root,
        "git-commit",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
            "message": "J1 initial commit",
            "initial": true,
        }),
        Some("j1-git-commit"),
    );
    let receipt = expect_ok("git.commit", &out);
    let commit_operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(commit_operation["state"], "succeeded", "{commit_operation}");

    let state = git_status(root, project_id);
    assert_eq!(state["unborn"], false, "{state}");
    let head_oid = state["headOid"].as_str().expect("headOid").to_owned();
    let token_after_commit = state_token(&state);
    assert_ne!(token, token_after_commit, "stateToken 在提交后前进");

    // push（创建式远端 CAS：目标 ref 应不存在）。
    let out = dispatch_input(
        root,
        "git-push",
        &["git", "push"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token_after_commit,
            "remoteName": "origin",
            "sourceRef": "main",
            "destinationRef": "main",
            "expectedRemoteOid": "",
        }),
        Some("j1-git-push"),
    );
    let receipt = expect_ok("git.push", &out);
    let push_operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(push_operation["state"], "succeeded", "{push_operation}");

    // 落盘：.git 对象与裸仓 ref。
    let objects_dir = workspace.join(".git").join("objects");
    let object_count = std::fs::read_dir(&objects_dir).unwrap().flatten().count();
    assert!(object_count > 0, ".git/objects 非空: {objects_dir:?}");
    let pushed_ref = std::fs::read_to_string(bare_push.join("refs").join("heads").join("main"))
        .expect("bare ref after push");
    assert_eq!(pushed_ref.trim(), head_oid, "裸仓 main == 本地 HEAD");

    // ------------------------------------------------------------------
    // 11. 搜索 / 历史 / 归档（历史经存储 API 联调点播种）。
    // ------------------------------------------------------------------
    let store = seeding_store(root);
    store
        .append_history_entries(
            session_id,
            &[
                json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "j1 first question about needle"}]}),
                json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "j1 first answer"}]}),
                json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "second needle question"}]}),
                json!({"turnId": "t-2", "role": "assistant", "content": [{"kind": "text", "text": "second answer"}]}),
            ],
        )
        .unwrap();
    drop(store);

    let out = journey_cli(
        root,
        &[
            "search",
            "start",
            "--output",
            "json",
            "--scope",
            &json!({
                "kind": "session",
                "projectId": project_id,
                "sessionId": session_id,
            })
            .to_string(),
            "--query",
            "needle",
            "--request-key",
            "j1-search",
        ],
    );
    let receipt = expect_ok("search.start", &out);
    assert_eq!(receipt["kind"], "operation");
    let search_operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(search_operation["state"], "succeeded", "{search_operation}");
    assert_eq!(search_operation["result"]["resultCount"], 2);
    assert_eq!(search_operation["result"]["indexingComplete"], true);
    let out = journey_cli(
        root,
        &[
            "search",
            "results",
            "--output",
            "json",
            "--operation-id",
            search_operation["result"]["searchId"].as_str().unwrap(),
        ],
    );
    let results = expect_ok("search.results", &out);
    let hits = results["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(hits.len(), 2);
    assert_eq!(results["indexingComplete"], true);

    let out = journey_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let history = expect_ok("history.read", &out);
    let entries = history["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(entries.len(), 4);
    let through_entry = entries[2]["entryId"].as_str().unwrap().to_owned();
    assert_eq!(entries[2]["turnId"], "t-2");

    // fork（截断边界）。
    let fork_session = "s-j1-forked";
    let fork_revision = session_revision(root, session_id).await;
    let out = journey_cli(
        root,
        &[
            "history",
            "fork",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--through-entry-id",
            &through_entry,
            "--expected-revision",
            &fork_revision,
            "--new-session-id",
            fork_session,
            "--name",
            "J1 Fork",
        ],
    );
    let fork = expect_ok("history.fork", &out);
    assert_eq!(fork["sourceSessionId"], json!(session_id));
    assert_eq!(fork["throughEntryId"], json!(through_entry));
    assert_eq!(fork["session"]["sessionState"], "open");
    let out = journey_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            fork_session,
        ],
    );
    let forked_history = expect_ok("history.read", &out);
    assert_eq!(
        forked_history["items"].as_array().map(Vec::len),
        Some(3),
        "fork 截断在 through-entry 边界（含该条）"
    );

    // export（源会话全量）→ archive 落盘 + 读回。
    let export_revision = session_revision(root, session_id).await;
    let out = journey_cli(
        root,
        &[
            "history",
            "export",
            "--output",
            "json",
            "--request-key",
            "j1-export",
            "--session-id",
            session_id,
            "--expected-revision",
            &export_revision,
        ],
    );
    let exported = expect_ok("history.export", &out);
    assert_eq!(exported["kind"], "operation");
    let export_operation = wait_terminal_operation(root, exported["operationId"].as_str().unwrap());
    assert_eq!(export_operation["state"], "succeeded", "{export_operation}");
    let archive_id = wait_for_archive_ready(root).await;

    let out = journey_cli(
        root,
        &[
            "archive",
            "get",
            "--output",
            "json",
            "--archive-id",
            &archive_id,
        ],
    );
    let manifest = expect_ok("archive.get", &out);
    assert_eq!(manifest["entryCount"], 4);
    assert_eq!(manifest["sourceRevision"], json!(export_revision));
    assert_eq!(
        manifest["manifest"]["artifactId"].as_str().unwrap(),
        format!("{archive_id}.manifest")
    );
    let out = journey_cli(
        root,
        &[
            "archive",
            "entries",
            "--output",
            "json",
            "--archive-id",
            &archive_id,
        ],
    );
    let entries = expect_ok("archive.entries", &out);
    assert_eq!(entries["items"].as_array().map(Vec::len), Some(4));
    assert_eq!(entries["pageInfo"]["complete"], true);
    let artifacts = entries["artifacts"].as_array().cloned().unwrap_or_default();
    assert!(!artifacts.is_empty());
    for artifact in &artifacts {
        assert_eq!(artifact["sha256"].as_str().map(str::len), Some(64));
    }
    // 导出产物落盘：entries + manifest 两个 blob 都在 archive 目录里。
    let archive_dir = root.join("v1").join("archives").join(&archive_id);
    assert!(
        archive_dir.join("entries.json").is_file(),
        "archive 目录落盘: {archive_id}"
    );
    assert!(
        archive_dir.join("manifest.json").is_file(),
        "archive manifest 落盘: {archive_id}"
    );
    // manifest 读回经 `archive.get`：合同把 manifest 定义为归档自有的载荷
    // 描述符（bytes/sha256），workspace 的 `artifacts.read` 只服务
    // uploads/files/preview 发布进产物库的产物（合同里是另一个
    // workspace__Artifact 形状，含 projectId/revision/createdAt），归档
    // blob 不在该库里（t012 验收同口径）。读回强度不打折：公开描述符的
    // bytes/sha256 必须与盘上 manifest blob 逐字节一致。
    let manifest_blob =
        std::fs::read(archive_dir.join("manifest.json")).expect("read manifest.json");
    assert!(
        !manifest_blob.is_empty(),
        "manifest blob 非空: {archive_id}"
    );
    assert_eq!(
        manifest["manifest"]["bytes"].as_u64(),
        Some(manifest_blob.len() as u64),
        "archive.get 报告的 manifest 字节数与盘上一致: {manifest}"
    );
    let manifest_blob_sha = sha256_hex(&manifest_blob);
    assert_eq!(
        manifest["manifest"]["sha256"].as_str(),
        Some(manifest_blob_sha.as_str()),
        "archive.get 报告的 manifest sha256 与盘上 blob 一致: {manifest}"
    );

    // 会话归档收尾：rename + close，close 后历史仍可读。
    let fork_rev_now = session_revision(root, fork_session).await;
    let out = journey_cli(
        root,
        &[
            "sessions",
            "update",
            "--output",
            "json",
            "--session-id",
            fork_session,
            "--expected-revision",
            &fork_rev_now,
            "--name",
            "J1 Fork Renamed",
        ],
    );
    let updated = expect_ok("session.update", &out);
    assert_eq!(updated["session"]["name"], "J1 Fork Renamed");
    let closed_revision = updated["session"]["revision"].as_str().unwrap().to_owned();
    let out = journey_cli(
        root,
        &[
            "sessions",
            "close",
            "--output",
            "json",
            "--session-id",
            fork_session,
            "--expected-revision",
            &closed_revision,
        ],
    );
    let closed = expect_ok("session.close", &out);
    assert_eq!(closed["session"]["sessionState"], "closed");
    let out = journey_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            fork_session,
        ],
    );
    let closed_history = expect_ok("history.read", &out);
    assert_eq!(closed_history["items"].as_array().map(Vec::len), Some(3));

    // ------------------------------------------------------------------
    // 12. 落盘总断言。
    // ------------------------------------------------------------------
    assert!(root.join("v1").join("state.sqlite").is_file());
    assert!(cloned_dir.join("README.md").is_file());
    let artifacts_dir = root.join("v1").join("artifacts");
    let artifact_files = std::fs::read_dir(&artifacts_dir).unwrap().flatten().count();
    assert!(
        artifact_files >= 3,
        "上传产物 + 预览产物 ×2（manifest/sheet，归档 blob 不入产物库）: {artifact_files}"
    );
    // 语义核实（uploads.rs complete 终态 CAS 分支）：完成后分片目录与暂存一并
    // 清理，审计记录已入 upload_parts 表——目录不留存。
    assert!(
        !root.join("v1").join("uploads").join(&upload_id).exists(),
        "上传分片目录在完成后清理: {upload_id}"
    );
    // 克隆登记的项目可读（旅程侧旁证）。
    let out = journey_cli(
        root,
        &[
            "projects",
            "get",
            "--output",
            "json",
            "--project-id",
            &cloned_project_id,
        ],
    );
    let cloned = expect_ok("project.get", &out);
    assert_eq!(cloned["project"]["name"], "J1 Clone");
}

/// 旅程 J1 故障负例：分片摘要说谎被拒（零落盘副作用）后正确重传恢复；
/// git 提交的过期 stateToken 被 CAS 拒绝（仓库零变化）后正确重提交恢复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j1_fault_lying_digest_and_stale_git_token() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let home = TestHome::new("j1-fault");
    let root = home.path();
    let init = init_home(root);
    let instance_id = init["identity"]["instanceId"].as_str().unwrap().to_owned();

    // 装置：工作区 + 项目。
    let out = journey_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "j1f-dir",
            "--path",
            &server_path("j1f-ws"),
        ],
    );
    expect_ok("directories.create", &out);
    let workspace = root.join("workspaces").join("j1f-ws");
    let project_id = "p-j1f";
    let out = journey_cli(
        root,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--name",
            "J1 Fault",
            "--environment-id",
            "local",
            "--root",
            &json!({
                "instanceId": instance_id,
                "rootId": "default",
                "relativePath": "j1f-ws",
            })
            .to_string(),
        ],
    );
    expect_ok("project.register", &out);

    // ------------------------------------------------------------------
    // 故障 A：分片摘要说谎 → CHECKSUM_MISMATCH，零副作用，正确重传恢复。
    // ------------------------------------------------------------------
    let payload = b"j1 fault payload bytes".to_vec();
    let payload_sha = sha256_hex(&payload);
    let out = invoke_input(
        root,
        "f-up-create",
        "uploads.create",
        &json!({
            "projectId": project_id,
            "displayName": "j1f.bin",
            "mediaType": "application/octet-stream",
            "byteLength": payload.len(),
            "sha256": payload_sha,
        }),
        Some("j1f-up-create"),
    );
    let created = expect_ok("uploads.create", &out);
    let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();

    // 说谎的 sha（与字节不符）→ 接纳前拒绝。
    let out = invoke_input(
        root,
        "f-up-part-lying",
        "uploads.part",
        &json!({
            "uploadId": upload_id,
            "partNumber": 0,
            "bytesBase64": b64_encode(&payload),
            "sha256": sha256_hex(b"not the payload"),
        }),
        Some("j1f-up-part-lying"),
    );
    assert_eq!(
        expect_fail("uploads.part", &out),
        "CHECKSUM_MISMATCH",
        "摘要说谎必须在接纳前拒绝"
    );

    // 零副作用：uploads.get 仍 receiving；分片文件未落盘。
    let out = invoke_input(
        root,
        "f-up-get",
        "uploads.get",
        &json!({"uploadId": upload_id}),
        None,
    );
    let got = expect_ok("uploads.get", &out);
    assert_eq!(got["upload"]["state"], "receiving");
    assert!(
        !root
            .join("v1")
            .join("uploads")
            .join(&upload_id)
            .join("part-0")
            .exists(),
        "被拒分片不得落盘: {upload_id}"
    );

    // 恢复：同块正确重传 → complete → 产物逐字节一致。
    let out = invoke_input(
        root,
        "f-up-part-ok",
        "uploads.part",
        &json!({
            "uploadId": upload_id,
            "partNumber": 0,
            "bytesBase64": b64_encode(&payload),
            "sha256": payload_sha,
        }),
        Some("j1f-up-part-ok"),
    );
    expect_ok("uploads.part", &out);
    let out = invoke_input(
        root,
        "f-up-complete",
        "uploads.complete",
        &json!({"uploadId": upload_id, "expectedSha256": payload_sha}),
        Some("j1f-up-complete"),
    );
    let receipt = expect_ok("uploads.complete", &out);
    let operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(operation["state"], "succeeded", "{operation}");
    // 产物 id 以 uploads.get 为准（不猜测命名）。
    let mut artifact_id = None;
    // 满载预算（判据不变）：等待同一事件落定的墙钟上限，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let out = invoke_input(
            root,
            "f-up-get2",
            "uploads.get",
            &json!({"uploadId": upload_id}),
            None,
        );
        let got = expect_ok("uploads.get", &out);
        if got["upload"]["state"] == "complete" {
            artifact_id = got["upload"]["artifactId"].as_str().map(str::to_owned);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let artifact_id = artifact_id.expect("fault upload completes");
    let out = journey_cli(
        root,
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
    let chunk = expect_ok("artifacts.read", &out)["chunk"].clone();
    assert_eq!(chunk["eof"], true);
    assert_eq!(
        b64_decode(chunk["bytesBase64"].as_str().unwrap_or_default()),
        payload,
        "恢复上传的产物与内容一致"
    );

    // ------------------------------------------------------------------
    // 故障 B：git 提交的过期 stateToken → REVISION_CONFLICT，仓库零变化，
    // 正确 token 重提交恢复。
    // ------------------------------------------------------------------
    git_sync(&workspace, &["init", "-q", "-b", "main"]);
    git_sync(&workspace, &["config", "commit.gpgsign", "false"]);
    write_file(&workspace.join("fault.txt"), "j1 fault content\n");

    let out = journey_cli(
        root,
        &[
            "git",
            "identity",
            "set",
            "--output",
            "json",
            "--scope",
            "project",
            "--project-id",
            project_id,
            "--name",
            "J1 Fault User",
            "--email",
            "j1f@example.com",
            "--expected-revision",
            "",
        ],
    );
    expect_ok("git_identity.set", &out);

    let state = git_status(root, project_id);
    assert_eq!(state["unborn"], true);
    let token = state_token(&state);

    // 过期 token（格式合法但非当前值）→ CAS 拒绝。
    let out = dispatch_input(
        root,
        "f-git-commit-stale",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": "gst-j1-stale-token",
            "selection": {"mode": "all"},
            "message": "must not land",
            "initial": true,
        }),
        Some("j1f-git-commit-stale"),
    );
    assert_eq!(
        expect_fail("git.commit", &out),
        "REVISION_CONFLICT",
        "过期 stateToken 必须被 CAS 拒绝"
    );

    // 零副作用：unborn 不变、stateToken 不变、无 ref 产生、内容原样。
    let after = git_status(root, project_id);
    assert_eq!(after["unborn"], true, "{after}");
    assert_eq!(state_token(&after), token, "被拒提交不得推进 stateToken");
    assert!(
        !workspace
            .join(".git")
            .join("refs")
            .join("heads")
            .join("main")
            .exists(),
        "被拒提交不得产生分支 ref"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("fault.txt")).unwrap(),
        "j1 fault content\n"
    );

    // 恢复：正确 token 重提交。
    let out = dispatch_input(
        root,
        "f-git-commit-ok",
        &["git", "commit"],
        &json!({
            "projectId": project_id,
            "expectedStateToken": token,
            "selection": {"mode": "all"},
            "message": "j1 fault recovery commit",
            "initial": true,
        }),
        Some("j1f-git-commit-ok"),
    );
    let receipt = expect_ok("git.commit", &out);
    let operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(operation["state"], "succeeded", "{operation}");
    let after = git_status(root, project_id);
    assert_eq!(after["unborn"], false, "{after}");

    assert!(root.join("v1").join("state.sqlite").is_file());
}
