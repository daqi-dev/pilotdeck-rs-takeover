//! S5 故障旅程 F9（上传中断续传）端到端：`_tmp/20261119-s5/journeys-execution-plan.md`
//! §3 F9 的命令序列与判据，经真实 `agentcpm-pd` 二进制、隔离
//! `AGENTCPM_PD_HOME`、`common_v1` 挂具的 envelope schema 校验。
//!
//! 注入方式 = SIGKILL 测试自有的上传客户端（`api connect` 子进程，F2/t9 同
//! 法），只作用于测试 spawn 的子进程，不按名字匹配任何系统进程。
//!
//! - `f9_local_client_death_mid_upload_resumes_and_publishes_identical_artifact`：
//!   本地轮。5 分片上传传到一半 kill 客户端（第 3 块已发出未等回执——覆盖
//!   「请求在途时客户端崩溃」窗口），新进程 `uploads get` 显示已收分片
//!   （已确认块必在、未发送块必不在、在途块两可）；缺块 `uploads complete`
//!   → MISSING_PARTS（续传是真需要的）；续传剩余分片（重发已收块 →
//!   accept_part 字节级去重 deduplicated）→ complete 校验整文件 sha256 →
//!   产物发布（fsync+rename 后不可变：暂存零残留、产物字节/摘要/revision
//!   稳定）；`uploads cancel` 分支清理临时块（释放量对账 + 磁盘目录消失 +
//!   取消后拒收分片 + 重复取消安全）；最终 artifact 与一次完整上传的
//!   artifact 逐字节一致（同 revision）。
//! - `f9_remote_round_client_death_over_remote_pair`：远程轮（同机 remote_pair
//!   模拟，J8 拓扑）。kill A 侧远程客户端 → A 侧新进程经 `--target` 续传 →
//!   B 侧本地视图核对完成态与产物落盘；A 不落任何 gateway 数据。
//! - `f9_crosshost_round_client_death_resume`：跨主机轮 `#[ignore]`（W4 验收
//!   层；由 journeys 远程 shell 驱动注入环境坐标
//!   `F9_CROSSHOST_WS_URL` / `F9_CROSSHOST_TOKEN_FILE` /
//!   `F9_CROSSHOST_INSTANCE_ID`，未设置时诚实早退）。
//!
//! 大文件变体（500MB 生成器）属 P2 复核（`e2e_v1_files_uploads.rs` 已有
//! `#[ignore]` 装置），不在本文件重复。

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common_v1::{
    CliOutcome, ConnectSession, TestHome, await_cli, cli_with_stdin, expect_ok, init_home,
    init_home_instance, remote_pair, rpc_request, seed_remote_project_session, signal_pid,
};

/// 服务端分片上限（`uploads.limits` 的 maxPartBytes，uploads.rs:60）。
const PART_BYTES: usize = 512 * 1024;
/// 末尾分片字节数：4 个满片 + 100 字节尾巴 ⇒ 5 分片（F9 要求 ≥3）。
const TAIL_BYTES: usize = 100;
/// 一个 `uploads get` 轮询周期内等待终态的上限。
/// 满载预算（判据不变）：等产物装配/发布操作落终态，30s→60s。
const UPLOAD_DEADLINE: Duration = Duration::from_secs(60);

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("base64")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 确定性合成负载：4×512KiB + 100B（P2 生成器同式），不预存大文件。
fn whole_bytes() -> Vec<u8> {
    let mut whole = Vec::with_capacity(4 * PART_BYTES + TAIL_BYTES);
    for index in 0..4u64 {
        for i in 0..PART_BYTES {
            whole.push(((index.wrapping_mul(131_542_391) + i as u64) % 251) as u8);
        }
    }
    for i in 0..TAIL_BYTES {
        whole.push(((4u64.wrapping_mul(131_542_391) + i as u64) % 251) as u8);
    }
    whole
}

/// 第 `number` 分片的服务端期望切法（partSize = min(512KiB, byteLength)）。
fn part_slice(whole: &[u8], number: u64) -> &[u8] {
    let start = number as usize * PART_BYTES;
    let end = ((number as usize + 1) * PART_BYTES).min(whole.len());
    &whole[start..end]
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

/// 拼装带可选 `--target` 的 argv（远程轮复用同一组助手）。
fn args_with_target(prefix: &[&str], target: Option<&str>, tail: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = prefix.iter().map(|s| (*s).to_owned()).collect();
    if let Some(t) = target {
        args.push("--target".to_owned());
        args.push(t.to_owned());
    }
    args.extend(tail.iter().map(|s| (*s).to_owned()));
    args
}

async fn run_cli_json(home: &Path, args: &[String]) -> CliOutcome {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    await_cli(home, &refs)
}

/// 播种一个带真实授权根的项目（t013/t018 e2e 同法；uploads 只需项目存在）。
async fn seed_project(tag: &str) -> (TestHome, PathBuf, String) {
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
            "S5 Journey F9",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"{workspace_rel}\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    (home, workspace, project_id)
}

async fn upload_create(
    home: &Path,
    target: Option<&str>,
    project_id: &str,
    display: &str,
    byte_length: usize,
    sha: &str,
    key: &str,
) -> Value {
    let args = args_with_target(
        &["uploads", "create"],
        target,
        &[
            "--output",
            "json",
            "--project-id",
            project_id,
            "--display-name",
            display,
            "--media-type",
            "application/octet-stream",
            "--byte-length",
            &byte_length.to_string(),
            "--sha256",
            sha,
            "--request-key",
            key,
        ],
    );
    let out = run_cli_json(home, &args).await;
    expect_ok("uploads.create", &out)
}

async fn upload_get(home: &Path, target: Option<&str>, upload_id: &str) -> Value {
    let args = args_with_target(
        &["uploads", "get"],
        target,
        &["--output", "json", "--upload-id", upload_id],
    );
    let out = run_cli_json(home, &args).await;
    expect_ok("uploads.get", &out)
}

/// 分片提交走 `--input-json -`（stdin）：512KiB 分片的 base64 约 691KB，
/// 不进 argv（长度红线）也不超客户端 1MiB 帧上限。
async fn upload_part_outcome(
    home: &Path,
    target: Option<&str>,
    upload_id: &str,
    number: u64,
    bytes: &[u8],
    key: &str,
) -> CliOutcome {
    let input = json!({
        "uploadId": upload_id,
        "partNumber": number,
        "bytesBase64": b64_encode(bytes),
        "sha256": sha256_hex(bytes),
    });
    let args = args_with_target(
        &["uploads", "part"],
        target,
        &[
            "--output",
            "json",
            "--request-key",
            key,
            "--input-json",
            "-",
        ],
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    cli_with_stdin(home, &refs, Some(&input.to_string())).await
}

async fn upload_part(
    home: &Path,
    target: Option<&str>,
    upload_id: &str,
    number: u64,
    bytes: &[u8],
    key: &str,
) -> Value {
    let out = upload_part_outcome(home, target, upload_id, number, bytes, key).await;
    expect_ok("uploads.part", &out)
}

async fn upload_complete_outcome(
    home: &Path,
    target: Option<&str>,
    upload_id: &str,
    sha: &str,
    key: &str,
) -> CliOutcome {
    let args = args_with_target(
        &["uploads", "complete"],
        target,
        &[
            "--output",
            "json",
            "--upload-id",
            upload_id,
            "--expected-sha256",
            sha,
            "--request-key",
            key,
        ],
    );
    run_cli_json(home, &args).await
}

async fn upload_cancel(home: &Path, target: Option<&str>, upload_id: &str, key: &str) -> Value {
    let args = args_with_target(
        &["uploads", "cancel"],
        target,
        &[
            "--output",
            "json",
            "--upload-id",
            upload_id,
            "--request-key",
            key,
        ],
    );
    let out = run_cli_json(home, &args).await;
    expect_ok("uploads.cancel", &out)
}

/// 轮询 `operations get` 至终态，返回 operation JSON（含 result）。
async fn await_operation(home: &Path, target: Option<&str>, operation_id: &str) -> Value {
    let deadline = Instant::now() + UPLOAD_DEADLINE;
    loop {
        let args = args_with_target(
            &["operations", "get"],
            target,
            &["--output", "json", "--operation-id", operation_id],
        );
        let out = run_cli_json(home, &args).await;
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
            "operation `{operation_id}` never reached a terminal state: {got}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 分页读回整个产物（512KiB 页，与 maxBytes 上限一致）。
async fn read_artifact_bytes(home: &Path, target: Option<&str>, artifact_id: &str) -> Vec<u8> {
    let mut assembled = Vec::new();
    let mut offset = 0u64;
    loop {
        let args = args_with_target(
            &["artifacts", "read"],
            target,
            &[
                "--output",
                "json",
                "--artifact-id",
                artifact_id,
                "--offset",
                &offset.to_string(),
                "--max-bytes",
                &(512 * 1024).to_string(),
            ],
        );
        let out = run_cli_json(home, &args).await;
        let page = expect_ok("artifacts.read", &out);
        let chunk = &page["chunk"];
        let bytes = b64_decode(chunk["bytesBase64"].as_str().unwrap_or_default());
        assert_eq!(chunk["offset"], json!(offset), "{page}");
        assembled.extend_from_slice(&bytes);
        offset += bytes.len() as u64;
        if chunk["eof"] == json!(true) {
            return assembled;
        }
    }
}

/// 一次完整（不中断）上传：create → 全部分片 → complete → 等终态。
async fn one_shot_upload(
    home: &Path,
    target: Option<&str>,
    project_id: &str,
    whole: &[u8],
    tag: &str,
) -> (String, String) {
    let total_sha = sha256_hex(whole);
    let created = upload_create(
        home,
        target,
        project_id,
        &format!("{tag}.bin"),
        whole.len(),
        &total_sha,
        &format!("{tag}-create"),
    )
    .await;
    let upload_id = created["upload"]["uploadId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let total = whole.len().div_ceil(PART_BYTES) as u64;
    for number in 0..total {
        upload_part(
            home,
            target,
            &upload_id,
            number,
            part_slice(whole, number),
            &format!("{tag}-part-{number}"),
        )
        .await;
    }
    let receipt_out = upload_complete_outcome(
        home,
        target,
        &upload_id,
        &total_sha,
        &format!("{tag}-complete"),
    )
    .await;
    let receipt = expect_ok("uploads.complete", &receipt_out);
    let operation = await_operation(
        home,
        target,
        receipt["operationId"].as_str().unwrap_or_default(),
    )
    .await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    let artifact_id = operation["result"]["artifact"]["artifactId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    (upload_id, artifact_id)
}

/// `uploads get` 结果里的已收分片号（升序）。
fn received_numbers(upload: &Value) -> Vec<u64> {
    upload["upload"]["receivedParts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p["partNumber"].as_u64())
        .collect()
}

/// kill 后的服务端读数先沉降再断言：在途分片（若有）至多落地一次，且客户端
/// 已死、不再有新帧可到；两次间隔 700ms 的读数一致即稳定。
async fn settled_upload_get(home: &Path, target: Option<&str>, upload_id: &str) -> Value {
    let mut last = upload_get(home, target, upload_id).await;
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let got = upload_get(home, target, upload_id).await;
        if received_numbers(&got) == received_numbers(&last) {
            return got;
        }
        last = got;
    }
    panic!("upload `{upload_id}` state never settled after the client kill: {last}");
}

/// 会话侧分片提交（被 kill 的上传客户端）。
async fn session_upload_part(
    conn: &mut ConnectSession,
    upload_id: &str,
    number: u64,
    bytes: &[u8],
    key: &str,
) -> Value {
    let resp = conn
        .call(
            "uploads.part",
            json!({
                "uploadId": upload_id,
                "partNumber": number,
                "bytesBase64": b64_encode(bytes),
                "sha256": sha256_hex(bytes),
            }),
            Some(key),
        )
        .await;
    assert!(
        resp.get("error").is_none(),
        "session part {number} failed: {resp}"
    );
    resp["result"]["result"].clone()
}

// ---------------------------------------------------------------------------
// 本地轮：kill 客户端中断续传
// ---------------------------------------------------------------------------

/// F9 本地旅程：上传客户端（`api connect` 子进程）在 3/5 分片确认、第 4 块
/// 在途时被 SIGKILL——已确认块必须在服务端、未发送块必须不在；缺块 complete
/// 被拒（MISSING_PARTS）；新进程续传（重发已收块走 accept_part 字节级去重）
/// 后 complete 校验整文件 sha256 并发布不可变产物；cancel 分支清理临时块；
/// 最终 artifact 与一次完整上传逐字节一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f9_local_client_death_mid_upload_resumes_and_publishes_identical_artifact() {
    let (home, _workspace, project_id) = seed_project("f9-local").await;
    let root = home.path();
    let whole = whole_bytes();
    let total_sha = sha256_hex(&whole);

    // ---- 中断的上传客户端：api connect 会话（网关由首个 CLI 自动拉起，
    // 与客户端进程分离，客户端死亡不影响服务端暂存）。
    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;
    let created = conn
        .call(
            "uploads.create",
            json!({
                "projectId": project_id,
                "displayName": "f9-interrupted.bin",
                "mediaType": "application/octet-stream",
                "byteLength": whole.len(),
                "sha256": total_sha,
            }),
            Some("f9a-create"),
        )
        .await;
    assert!(
        created.get("error").is_none(),
        "session create failed: {created}"
    );
    let upload_id = created["result"]["result"]["upload"]["uploadId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        created["result"]["result"]["upload"]["state"],
        json!("receiving"),
        "{created}"
    );

    // 分片 0..=2 确认落服务端。
    for number in 0..=2u64 {
        let acked = session_upload_part(
            &mut conn,
            &upload_id,
            number,
            part_slice(&whole, number),
            &format!("f9a-part-{number}"),
        )
        .await;
        assert_eq!(acked["deduplicated"], json!(false), "{acked}");
        assert_eq!(acked["acceptedBytes"], json!(PART_BYTES), "{acked}");
    }

    // 分片 3 只发不等回执，随即 SIGKILL——「请求在途时客户端崩溃」窗口：
    // 服务端要么完整处理（块+幂等记录都在），要么根本没收到。
    let fire_id = conn.fresh_id("f9fire");
    conn.send(&rpc_request(
        &fire_id,
        "uploads.part",
        json!({
            "input": {
                "uploadId": upload_id,
                "partNumber": 3,
                "bytesBase64": b64_encode(part_slice(&whole, 3)),
                "sha256": sha256_hex(part_slice(&whole, 3)),
            },
            "idempotencyKey": "f9a-part-3"
        }),
    ))
    .await;
    let cli_pid = conn.child.id().expect("cli pid");
    signal_pid(cli_pid, rustix::process::Signal::KILL);
    let _ = conn.child.wait().await;

    // ---- 新客户端（argv）：服务端保住了部分上传（读数先沉降——在途的第 3
    // 块若已到达服务端，至多再落地一次）。
    let got = settled_upload_get(root, None, &upload_id).await;
    assert_eq!(got["upload"]["state"], json!("receiving"), "{got}");
    assert_eq!(got["upload"]["byteLength"], json!(whole.len()));
    let numbers = received_numbers(&got);
    for confirmed in [0u64, 1, 2] {
        assert!(
            numbers.contains(&confirmed),
            "acknowledged part {confirmed} must be on the server: {got}"
        );
    }
    assert!(
        !numbers.contains(&4),
        "part 4 was never sent; it must not be on the server: {got}"
    );
    // 已收分片的逐块摘要对账。
    for part in got["upload"]["receivedParts"].as_array().unwrap() {
        let number = part["partNumber"].as_u64().unwrap();
        assert_eq!(
            part["sha256"],
            json!(sha256_hex(part_slice(&whole, number))),
            "part {number} digest: {part}"
        );
    }
    let part3_landed = numbers.contains(&3);

    // ---- 续传是真需要的：此刻 complete → MISSING_PARTS（第 4 块从未发出，
    // 无论在途的第 3 块落没落，都缺块）。
    let premature = upload_complete_outcome(root, None, &upload_id, &total_sha, "f9a-early").await;
    assert_eq!(
        expect_failure("uploads.complete", &premature),
        "MISSING_PARTS",
        "completing with a gap must be refused before any assembly"
    );

    // ---- 续传：重发在途/缺块分片。分片提交的 deduplicated 是 accept_part 的
    // 字节级判定（同号同字节 → deduplicated；uploads.part 不记幂等回执，键只在
    // 受理时被要求存在）——在途块已落则去重、未落则新写。
    let resumed3 = upload_part(
        root,
        None,
        &upload_id,
        3,
        part_slice(&whole, 3),
        "f9a-part-3",
    )
    .await;
    assert_eq!(
        resumed3["deduplicated"],
        json!(part3_landed),
        "the in-flight part must deduplicate iff it had landed: {resumed3}"
    );
    assert_eq!(resumed3["acceptedBytes"], json!(PART_BYTES), "{resumed3}");
    let resumed4 = upload_part(
        root,
        None,
        &upload_id,
        4,
        part_slice(&whole, 4),
        "f9a-part-4",
    )
    .await;
    assert_eq!(resumed4["deduplicated"], json!(false), "{resumed4}");
    assert_eq!(resumed4["acceptedBytes"], json!(TAIL_BYTES), "{resumed4}");

    // 重发早已确认的分片（新进程、原键）→ 字节级去重，不重复落盘。
    let replay0 = upload_part(
        root,
        None,
        &upload_id,
        0,
        part_slice(&whole, 0),
        "f9a-part-0",
    )
    .await;
    assert_eq!(replay0["deduplicated"], json!(true), "{replay0}");
    assert_eq!(replay0["acceptedBytes"], json!(PART_BYTES), "{replay0}");

    // ---- complete：整文件 sha256 校验 + 产物发布。
    let receipt_out =
        upload_complete_outcome(root, None, &upload_id, &total_sha, "f9a-complete").await;
    let receipt = expect_ok("uploads.complete", &receipt_out);
    let operation = await_operation(
        root,
        None,
        receipt["operationId"].as_str().unwrap_or_default(),
    )
    .await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(
        operation["result"]["upload"]["state"],
        json!("complete"),
        "{operation}"
    );
    let artifact = &operation["result"]["artifact"];
    let artifact_id = artifact["artifactId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!artifact_id.is_empty(), "{operation}");
    // 发布即不可变登记：长度/摘要/revision（arev-<sha>）与负载一致。
    assert_eq!(artifact["byteLength"], json!(whole.len()), "{artifact}");
    assert_eq!(artifact["sha256"], json!(total_sha), "{artifact}");
    assert_eq!(
        artifact["revision"],
        json!(format!("arev-{total_sha}")),
        "{artifact}"
    );

    let got = upload_get(root, None, &upload_id).await;
    assert_eq!(got["upload"]["state"], json!("complete"), "{got}");
    assert_eq!(got["upload"]["artifactId"], json!(artifact_id), "{got}");

    // 落盘终态：产物原子落位、上传暂存目录清空、无装配/发布暂存残留。
    let artifact_path = root.join("v1").join("artifacts").join(&artifact_id);
    assert!(artifact_path.is_file(), "{}", artifact_path.display());
    assert!(!root.join("v1").join("uploads").join(&upload_id).exists());
    for dir in [
        root.join("v1").join("uploads"),
        root.join("v1").join("artifacts"),
    ] {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(".assemble-") && !name.starts_with(".staging-"),
                "staging leftover {} in {}",
                name,
                dir.display()
            );
        }
    }

    // ---- 一次完整上传的逐字节对照：同负载不中断地传完 → 相同字节产物。
    let (one_shot_upload_id, one_shot_artifact_id) =
        one_shot_upload(root, None, &project_id, &whole, "f9b").await;
    assert_ne!(one_shot_artifact_id, artifact_id, "distinct uploads");
    let interrupted_bytes = read_artifact_bytes(root, None, &artifact_id).await;
    let one_shot_bytes = read_artifact_bytes(root, None, &one_shot_artifact_id).await;
    assert_eq!(interrupted_bytes, whole, "resumed artifact payload");
    assert_eq!(one_shot_bytes, whole, "one-shot artifact payload");
    assert_eq!(
        interrupted_bytes, one_shot_bytes,
        "the resumed upload's artifact must equal the one-shot upload's, byte for byte"
    );

    // ---- cancel 分支：清理临时块。
    let cancel_whole: Vec<u8> = whole[..PART_BYTES + TAIL_BYTES].to_vec();
    let cancel_sha = sha256_hex(&cancel_whole);
    let created = upload_create(
        root,
        None,
        &project_id,
        "f9-cancel.bin",
        cancel_whole.len(),
        &cancel_sha,
        "f9c-create",
    )
    .await;
    let cancel_upload_id = created["upload"]["uploadId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    upload_part(
        root,
        None,
        &cancel_upload_id,
        0,
        &cancel_whole[..PART_BYTES],
        "f9c-part-0",
    )
    .await;
    let cancel_dir = root.join("v1").join("uploads").join(&cancel_upload_id);
    assert!(cancel_dir.is_dir(), "temp blocks staged: {cancel_dir:?}");

    let cancelled = upload_cancel(root, None, &cancel_upload_id, "f9c-cancel-1").await;
    assert_eq!(cancelled["state"], json!("cancelled"), "{cancelled}");
    // 释放量对账：只计负载暂存文件（part-0），不含内部 journal（parts.log）。
    assert_eq!(
        cancelled["temporaryBytesReleased"],
        json!(PART_BYTES),
        "{cancelled}"
    );
    assert!(
        !cancel_dir.exists(),
        "cancel must remove the upload's temp block directory"
    );

    // 取消后：分片拒收（UPLOAD_CLOSED）+ 查询显示终态 + 重复取消安全。
    let rejected = upload_part_outcome(
        root,
        None,
        &cancel_upload_id,
        1,
        &cancel_whole[PART_BYTES..],
        "f9c-part-1",
    )
    .await;
    assert_eq!(
        expect_failure("uploads.part", &rejected),
        "UPLOAD_CLOSED",
        "a cancelled upload must not accept parts"
    );
    let got = upload_get(root, None, &cancel_upload_id).await;
    assert_eq!(got["upload"]["state"], json!("cancelled"), "{got}");
    let again = upload_cancel(root, None, &cancel_upload_id, "f9c-cancel-2").await;
    assert_eq!(again["state"], json!("cancelled"), "{again}");
    assert_eq!(again["temporaryBytesReleased"], json!(0), "{again}");

    // ---- 不可变复核：后续上传/取消活动之后，已发布产物字节不动。
    assert_eq!(std::fs::read(&artifact_path).unwrap(), whole);
    let reread = read_artifact_bytes(root, None, &artifact_id).await;
    assert_eq!(reread, whole, "a published artifact is immutable");
}

// ---------------------------------------------------------------------------
// 远程轮（同机 remote_pair 模拟）与跨主机轮（#[ignore]）
// ---------------------------------------------------------------------------

/// 远程侧故障旅程主体（A 侧视角，远程轮与跨主机轮共用）：select 档案 → 远程
/// 播种项目 → `api connect`（经已选档案走 ws 远程跳）上传 2/5 分片 →
/// SIGKILL A 的上传客户端 → A 侧新进程经 `--target` 续传 + complete + 远程读回
/// 产物字节。返回 (upload_id, artifact_id)。
async fn run_remote_client_death_upload_journey(
    home_a: &Path,
    target_id: &str,
    server_instance_id: &str,
    tag: &str,
) -> (String, String) {
    // select：无显式 --target 的 `api connect` 随已选档案走远程跳（t005）。
    let got = expect_ok(
        "target.get",
        &await_cli(
            home_a,
            &[
                "target",
                "get",
                "--output",
                "json",
                "--target-id",
                target_id,
            ],
        ),
    );
    let revision = got["target"]["revision"].as_str().unwrap().to_owned();
    expect_ok(
        "target.select",
        &await_cli(
            home_a,
            &[
                "target",
                "select",
                "--output",
                "json",
                "--target-id",
                target_id,
                "--expected-revision",
                &revision,
            ],
        ),
    );

    let (project_id, _session_id) =
        seed_remote_project_session(home_a, target_id, server_instance_id, tag).await;
    let whole = whole_bytes();
    let total_sha = sha256_hex(&whole);

    // 远程上传客户端：会话握手必须已骑在 B 的身份上（不是本机回落）。
    let mut conn = ConnectSession::start(home_a).await;
    let init = conn.initialize().await;
    assert_eq!(
        init["result"]["target"]["instanceId"],
        json!(server_instance_id),
        "the upload client must ride the remote hop: {init}"
    );
    let created = conn
        .call(
            "uploads.create",
            json!({
                "projectId": project_id,
                "displayName": format!("{tag}-interrupted.bin"),
                "mediaType": "application/octet-stream",
                "byteLength": whole.len(),
                "sha256": total_sha,
            }),
            Some(&format!("{tag}-create")),
        )
        .await;
    assert!(
        created.get("error").is_none(),
        "remote create failed: {created}"
    );
    let upload_id = created["result"]["result"]["upload"]["uploadId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    // 传到一半：分片 0、1 确认后 kill（远程轮取确定性中断点——两块确认、
    // 无在途请求；在途窗口由本地轮覆盖）。
    for number in 0..=1u64 {
        let acked = session_upload_part(
            &mut conn,
            &upload_id,
            number,
            part_slice(&whole, number),
            &format!("{tag}-part-{number}"),
        )
        .await;
        assert_eq!(acked["deduplicated"], json!(false), "{acked}");
    }
    let cli_pid = conn.child.id().expect("cli pid");
    signal_pid(cli_pid, rustix::process::Signal::KILL);
    let _ = conn.child.wait().await;

    // A 侧新进程经 --target 观察与续传（远程失败绝不回落本机）。
    let got = upload_get(home_a, Some(target_id), &upload_id).await;
    assert_eq!(got["upload"]["state"], json!("receiving"), "{got}");
    assert_eq!(received_numbers(&got), vec![0u64, 1], "{got}");

    let premature = upload_complete_outcome(
        home_a,
        Some(target_id),
        &upload_id,
        &total_sha,
        &format!("{tag}-early"),
    )
    .await;
    assert_eq!(
        expect_failure("uploads.complete", &premature),
        "MISSING_PARTS",
        "the remote server must refuse completion while parts are missing"
    );

    for number in 2..=4u64 {
        upload_part(
            home_a,
            Some(target_id),
            &upload_id,
            number,
            part_slice(&whole, number),
            &format!("{tag}-part-{number}"),
        )
        .await;
    }
    // 跨进程 + 远程跳重发已确认分片 0 → 字节级去重（同块同字节）。
    let replay = upload_part(
        home_a,
        Some(target_id),
        &upload_id,
        0,
        part_slice(&whole, 0),
        &format!("{tag}-part-0"),
    )
    .await;
    assert_eq!(replay["deduplicated"], json!(true), "{replay}");

    let receipt_out = upload_complete_outcome(
        home_a,
        Some(target_id),
        &upload_id,
        &total_sha,
        &format!("{tag}-complete"),
    )
    .await;
    let receipt = expect_ok("uploads.complete", &receipt_out);
    let operation = await_operation(
        home_a,
        Some(target_id),
        receipt["operationId"].as_str().unwrap_or_default(),
    )
    .await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    let artifact_id = operation["result"]["artifact"]["artifactId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!artifact_id.is_empty(), "{operation}");
    assert_eq!(
        operation["result"]["artifact"]["sha256"],
        json!(total_sha),
        "{operation}"
    );

    // A 侧经远程跳读回整产物字节。
    let bytes = read_artifact_bytes(home_a, Some(target_id), &artifact_id).await;
    assert_eq!(
        bytes, whole,
        "the artifact must read back over the remote hop"
    );
    (upload_id, artifact_id)
}

/// F9 远程轮（同机 remote_pair 模拟）：kill A 侧远程上传客户端后，B 的服务端
/// 保住部分上传；A 续传完成；B 侧本地视图核对终态与产物落盘；A 不落任何
/// gateway 数据。跨主机验收层见下方 `#[ignore]` 测试。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f9_remote_round_client_death_over_remote_pair() {
    let pair = remote_pair("f9r");
    let (upload_id, artifact_id) = run_remote_client_death_upload_journey(
        pair.home_a.path(),
        &pair.target_id,
        &pair.instance_id_b(),
        "f9r",
    )
    .await;

    // B 侧本地视图（home B 的本地 CLI 复用在跑 serve，不二启网关）。
    let got = upload_get(pair.home_b.path(), None, &upload_id).await;
    assert_eq!(got["upload"]["state"], json!("complete"), "{got}");
    assert_eq!(got["upload"]["artifactId"], json!(artifact_id), "{got}");

    // B 的落盘：产物原子落位、状态库在 B；A 不落任何 gateway 数据。
    let artifact_path = pair
        .home_b
        .path()
        .join("v1")
        .join("artifacts")
        .join(&artifact_id);
    assert!(artifact_path.is_file(), "{}", artifact_path.display());
    assert_eq!(std::fs::read(&artifact_path).unwrap(), whole_bytes());
    assert!(pair.home_b.path().join("v1").join("state.sqlite").is_file());
    assert!(!pair.home_a.path().join("v1").exists());
    assert!(!pair.home_a.path().join("run").exists());
    assert_eq!(
        pair.home_a.gateway_pid(),
        None,
        "no gateway was ever auto-started in home A"
    );
}

/// F9 跨主机轮（验收证据层，W4 shell 驱动）：注入点与远程轮相同（kill A 侧
/// 上传客户端），区别只在 B 是另一台真实主机的 serve。驱动需设置：
/// - `F9_CROSSHOST_WS_URL`：B 的 ws:// 端点；
/// - `F9_CROSSHOST_TOKEN_FILE`：含 B 的 local token 的文件路径；
/// - `F9_CROSSHOST_INSTANCE_ID`：B 的 instanceId。
/// 未设置时诚实早退（`#[ignore]` + 环境坐标双保险，不进常规 nextest）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "cross-host device: the W4 journeys remote-gate driver provides a real second machine via F9_CROSSHOST_{WS_URL,TOKEN_FILE,INSTANCE_ID}"]
async fn f9_crosshost_round_client_death_resume() {
    let Some(ws_url) = std::env::var("F9_CROSSHOST_WS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        eprintln!(
            "skipping: F9_CROSSHOST_WS_URL is not set (the W4 cross-host driver provides it)"
        );
        return;
    };
    let Some(token_file) = std::env::var("F9_CROSSHOST_TOKEN_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        eprintln!(
            "skipping: F9_CROSSHOST_TOKEN_FILE is not set (the W4 cross-host driver provides it)"
        );
        return;
    };
    let Some(instance_id) = std::env::var("F9_CROSSHOST_INSTANCE_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        eprintln!(
            "skipping: F9_CROSSHOST_INSTANCE_ID is not set (the W4 cross-host driver provides it)"
        );
        return;
    };

    // A 侧：隔离客户端 home + 凭据引用 + 远程档案（remote_pair 第④步形态）。
    let home_a = TestHome::new("f9x-a");
    let token = std::fs::read_to_string(&token_file)
        .unwrap_or_else(|e| panic!("read {token_file}: {e}"))
        .trim()
        .to_owned();
    std::fs::write(
        home_a.path().join("credentials.json"),
        serde_json::to_vec(&json!({"tokens": {"f9x-cred": token}})).unwrap(),
    )
    .unwrap();
    let wrote = expect_ok(
        "target.write",
        &await_cli(
            home_a.path(),
            &[
                "target",
                "write",
                "--output",
                "json",
                "--target-id",
                "f9x",
                "--name",
                "F9 cross-host server",
                "--kind",
                "https",
                "--endpoint",
                &ws_url,
                "--expected-instance-id",
                &instance_id,
                "--credential-ref",
                "f9x-cred",
            ],
        ),
    );
    assert_eq!(wrote["target"]["kind"], json!("https"));
    assert_eq!(wrote["target"]["endpoint"], json!(ws_url));

    let (_upload_id, _artifact_id) =
        run_remote_client_death_upload_journey(home_a.path(), "f9x", &instance_id, "f9x").await;
}
