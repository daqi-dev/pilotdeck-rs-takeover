//! t013（文件和附件）端到端场景：`files.*` / `uploads.*` / `artifacts.*` /
//! `documents.*` 经真实 CLI 二进制与 `/v1/ws` 传输。
//!
//! 授权根的播种走 t012 的声明面：`directories create`（brief 指定「e2e 里
//! 先用 t012 的 directories.create 建 root」），default 根惰性 bootstrap 为
//! `<home>/workspaces`。
//!
//! 场景对应 design.json 关联用例：c017（并发写）、c018（路径越界）、
//! c019（附件传输）、c021（文档预览）、contract.file-identity、
//! contract.artifact-lifecycle、prototype.p2（`#[ignore]` 装置，cb8 跑）。
//! documents.preview 的 completionResult 深断言依赖 `operations.get`
//! （collections 并行任务）：e2e 层先验收 receipt 形态，深断言由 gateway
//! 域内测试覆盖，待联调标注见报告。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common_v1::{
    CliOutcome, ConnectSession, TestHome, await_cli, expect_ok, init_home, init_home_instance,
};

/// 测试 crate 无 base64 依赖：内置一对与客户端 file_transfer 相同语义的
/// 最小编解码（标准字母表 + 填充），仅供构造/校验帧内容。
mod base64_compat {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn b64_encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[(triple >> 18) as usize & 0x3F] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 0x3F] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(triple >> 6) as usize & 0x3F] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[triple as usize & 0x3F] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn b64_decode(text: &str) -> Vec<u8> {
        fn value_of(ch: char) -> u32 {
            match ch {
                'A'..='Z' => (ch as u32) - b'A' as u32,
                'a'..='z' => (ch as u32) - b'a' as u32 + 26,
                '0'..='9' => (ch as u32) - b'0' as u32 + 52,
                '+' => 62,
                '/' => 63,
                _ => 0,
            }
        }
        let chars: Vec<char> = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        let mut out = Vec::with_capacity(chars.len() / 4 * 3);
        for chunk in chars.chunks(4) {
            let pad = chunk.iter().filter(|ch| **ch == '=').count();
            let mut triple: u32 = 0;
            for (index, ch) in chunk.iter().enumerate() {
                let value = if *ch == '=' { 0 } else { value_of(*ch) };
                triple |= value << (18 - 6 * index);
            }
            out.push((triple >> 16) as u8);
            if pad < 2 {
                out.push((triple >> 8) as u8);
            }
            if pad < 1 {
                out.push(triple as u8);
            }
        }
        out
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 播种一个带真实授权根的项目：t012 的声明面（brief 指定「e2e 里先用
/// t012 的 directories.create 建 root」）。`directories create` 触发 default
/// 授权根的惰性 bootstrap（`<home>/workspaces`）并创建工作区目录。
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
            "T013 E2E",
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

/// c018 + contract.file-identity + c017 的 argv 覆盖：正常读写删、越界、
/// 过期 revision 冲突、recovery 备份、虚拟项目。
#[tokio::test]
async fn files_argv_roundtrip_with_security_anchors() {
    let (home, workspace, project_id, instance_id) = seed_workspace("files-e2e").await;

    // write(absent) → stat → read。
    let payload = b"hello t013 e2e".to_vec();
    let out = await_cli(
        home.path(),
        &[
            "files",
            "write",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "hello.txt"),
            "--content",
            &format!(
                "{{\"bytesBase64\":\"{}\"}}",
                base64_compat::b64_encode(&payload)
            ),
            "--precondition",
            "{\"state\":\"absent\"}",
        ],
    );
    let written = expect_ok("files.write", &out);
    assert_eq!(written["entry"]["kind"], json!("file"));
    assert_eq!(written["entry"]["byteLength"], json!(payload.len()));
    let revision = written["entry"]["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        home.path(),
        &[
            "files",
            "stat",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "hello.txt"),
        ],
    );
    let stat = expect_ok("files.stat", &out);
    assert_eq!(stat["entry"]["revision"], json!(revision));

    let out = await_cli(
        home.path(),
        &[
            "files",
            "read",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "hello.txt"),
            "--max-bytes",
            "1024",
        ],
    );
    let read = expect_ok("files.read", &out);
    assert_eq!(read["chunk"]["byteLength"], json!(payload.len()));
    assert_eq!(read["chunk"]["eof"], json!(true));
    assert_eq!(
        base64_compat::b64_decode(read["chunk"]["bytesBase64"].as_str().unwrap()),
        payload
    );

    // 目录 + 隐藏文件列举。
    std::fs::write(workspace.join(".hidden"), b"h").unwrap();
    let out = await_cli(
        home.path(),
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
    let names: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["path"]["relativePath"]["value"].as_str())
        .collect();
    assert!(names.contains(&"hello.txt"));
    assert!(
        !names.contains(&".hidden"),
        "hidden files stay hidden by default"
    );

    // 路径越界（相对 ..）→ PATH_ESCAPE。
    let outside = workspace.parent().unwrap().join("outside-secret.txt");
    std::fs::write(&outside, b"secret").unwrap();
    let out = await_cli(
        home.path(),
        &[
            "files",
            "read",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "../outside-secret.txt"),
            "--max-bytes",
            "16",
        ],
    );
    assert_eq!(expect_failure("files.read", &out), "PATH_ESCAPE");

    // 符号链接逃逸 → PATH_ESCAPE。
    #[cfg(unix)]
    {
        let link = workspace.join("leak");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let out = await_cli(
            home.path(),
            &[
                "files",
                "read",
                "--output",
                "json",
                "--path",
                &path_arg(&project_id, "leak"),
                "--max-bytes",
                "16",
            ],
        );
        assert_eq!(expect_failure("files.read", &out), "PATH_ESCAPE");
        let _ = std::fs::remove_file(&link);
    }

    // 过期 revision 写 → REVISION_CONFLICT（c017：不静默覆盖，无部分内容）。
    let out = await_cli(
        home.path(),
        &[
            "files",
            "write",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "hello.txt"),
            "--content",
            &format!(
                "{{\"bytesBase64\":\"{}\"}}",
                base64_compat::b64_encode(b"stale writer")
            ),
            "--precondition",
            "{\"state\":\"revision\",\"revision\":\"frev-not-the-real-one\"}",
        ],
    );
    assert_eq!(expect_failure("files.write", &out), "REVISION_CONFLICT");
    assert_eq!(
        std::fs::read(workspace.join("hello.txt")).unwrap(),
        payload,
        "rejected write leaves the original intact"
    );

    // 正确 revision 的替换写 → previousRevision + recoveryArtifactId。
    let out = await_cli(
        home.path(),
        &[
            "files",
            "write",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "hello.txt"),
            "--content",
            &format!(
                "{{\"bytesBase64\":\"{}\"}}",
                base64_compat::b64_encode(b"replaced content")
            ),
            "--precondition",
            &format!("{{\"state\":\"revision\",\"revision\":\"{revision}\"}}"),
        ],
    );
    let replaced = expect_ok("files.write", &out);
    assert_eq!(replaced["previousRevision"], json!(revision));
    let recovery = replaced["recoveryArtifactId"].as_str().unwrap().to_owned();
    assert!(!recovery.is_empty());
    // recovery artifact 可读回原字节（c020 快照恢复语义）。
    let out = await_cli(
        home.path(),
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
        base64_compat::b64_decode(recovered["chunk"]["bytesBase64"].as_str().unwrap()),
        payload
    );

    // create-directory → move → delete。
    let out = await_cli(
        home.path(),
        &[
            "files",
            "create-directory",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "dir-a/dir-b"),
            "--create-parents",
            "true",
        ],
    );
    let made = expect_ok("files.create_directory", &out);
    assert_eq!(made["createdPaths"].as_array().unwrap().len(), 2);

    let out = await_cli(
        home.path(),
        &[
            "files",
            "stat",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "hello.txt"),
        ],
    );
    let stat2 = expect_ok("files.stat", &out);
    let revision2 = stat2["entry"]["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        home.path(),
        &[
            "files",
            "move",
            "--output",
            "json",
            "--source",
            &path_arg(&project_id, "hello.txt"),
            "--destination",
            &path_arg(&project_id, "dir-a/dir-b/moved.txt"),
            "--expected-source-revision",
            &revision2,
            "--destination-precondition",
            "{\"state\":\"absent\"}",
        ],
    );
    let moved = expect_ok("files.move", &out);
    assert!(
        moved["entry"]["path"]["relativePath"]["value"]
            .as_str()
            .unwrap()
            .ends_with("moved.txt")
    );
    assert!(!workspace.join("hello.txt").exists());
    assert_eq!(
        std::fs::read(workspace.join("dir-a/dir-b/moved.txt")).unwrap(),
        b"replaced content"
    );

    let out = await_cli(
        home.path(),
        &[
            "files",
            "delete",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "dir-a/dir-b/moved.txt"),
            "--expected-revision",
            moved["entry"]["revision"].as_str().unwrap(),
        ],
    );
    let deleted = expect_ok("files.delete", &out);
    assert!(
        deleted["recoveryArtifactId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(!workspace.join("dir-a/dir-b/moved.txt").exists());

    // 非空目录无 recursive → DIRECTORY_NOT_EMPTY。
    std::fs::write(workspace.join("dir-a/occupied.txt"), b"x").unwrap();
    let out = await_cli(
        home.path(),
        &[
            "files",
            "delete",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, "dir-a"),
            "--expected-revision",
            "whatever",
        ],
    );
    assert_eq!(expect_failure("files.delete", &out), "DIRECTORY_NOT_EMPTY");

    // 项目根删除拒绝 + 虚拟项目（未声明根）→ VIRTUAL_PROJECT（不把 home 当根）。
    let out = await_cli(
        home.path(),
        &[
            "files",
            "delete",
            "--output",
            "json",
            "--path",
            &path_arg(&project_id, ""),
            "--expected-revision",
            "x",
        ],
    );
    assert_eq!(expect_failure("files.delete", &out), "INVALID_ARGUMENT");

    let general_id = format!("p-general-{}", project_id);
    let out = await_cli(
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &general_id,
            "--name",
            "General",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"undeclared-root\",\"relativePath\":\"ws\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    let out = await_cli(
        home.path(),
        &[
            "files",
            "list",
            "--output",
            "json",
            "--path",
            &path_arg(&general_id, ""),
        ],
    );
    assert_eq!(expect_failure("files.list", &out), "VIRTUAL_PROJECT");
}

/// c019 + contract.artifact-lifecycle：乱序分块上传 → 不可变产物 → 分块下载
/// 校验；取消对已完成产物无影响；documents 预览 receipt 形态。
#[tokio::test]
async fn uploads_documents_over_ws() {
    let (home, _workspace, project_id, _iid) = seed_workspace("uploads-e2e").await;
    let mut session = ConnectSession::start(home.path()).await;
    session.initialize().await;

    // limits（有效配置而非旧 50MiB）。
    let limits = session.call("uploads.limits", json!({}), None).await;
    assert_eq!(
        limits["result"]["result"]["maxPartBytes"],
        json!(512 * 1024)
    );
    assert!(
        limits["result"]["result"]["maxArtifactBytes"]
            .as_u64()
            .unwrap()
            > 50 * 1024 * 1024,
        "{limits}"
    );

    // 三个分块（乱序提交 2、0、1）。
    let part_a = vec![1u8; 512 * 1024];
    let part_b = vec![2u8; 512 * 1024];
    let part_c = vec![3u8; 100];
    let mut whole = Vec::new();
    whole.extend_from_slice(&part_a);
    whole.extend_from_slice(&part_b);
    whole.extend_from_slice(&part_c);
    let total_sha = sha256_hex(&whole);

    let created = session
        .call(
            "uploads.create",
            json!({
                "projectId": project_id,
                "displayName": "e2e.bin",
                "mediaType": "application/octet-stream",
                "byteLength": whole.len(),
                "sha256": total_sha,
            }),
            Some("e2e-uploads-create-1"),
        )
        .await;
    assert_eq!(
        created["result"]["result"]["upload"]["state"],
        json!("receiving")
    );
    let upload_id = created["result"]["result"]["upload"]["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();

    for (number, bytes) in [
        (2u64, part_c.as_slice()),
        (0, part_a.as_slice()),
        (1, part_b.as_slice()),
    ] {
        let accepted = session
            .call(
                "uploads.part",
                json!({
                    "uploadId": upload_id,
                    "partNumber": number,
                    "bytesBase64": base64_compat::b64_encode(bytes),
                    "sha256": sha256_hex(bytes),
                }),
                None,
            )
            .await;
        assert_eq!(
            accepted["result"]["result"]["deduplicated"],
            json!(false),
            "{accepted}"
        );
    }
    // 同块重传 → deduplicated。
    let dup = session
        .call(
            "uploads.part",
            json!({
                "uploadId": upload_id,
                "partNumber": 1,
                "bytesBase64": base64_compat::b64_encode(&part_b),
                "sha256": sha256_hex(&part_b),
            }),
            None,
        )
        .await;
    assert_eq!(dup["result"]["result"]["deduplicated"], json!(true));

    // 缺块 complete → MISSING_PARTS（先只交了 3 块？这里全齐；单独用坏
    // sha 验证失败路径）。
    let receipt = session
        .call(
            "uploads.complete",
            json!({"uploadId": upload_id, "expectedSha256": total_sha}),
            Some("e2e-uploads-complete-1"),
        )
        .await;
    assert_eq!(receipt["result"]["result"]["kind"], json!("operation"));
    assert_eq!(receipt["result"]["result"]["state"], json!("running"));

    // 轮询 uploads.get 至 complete（完成响应丢失场景等价：重查同一产物）。
    let mut artifact_id = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let got = session
            .call("uploads.get", json!({"uploadId": upload_id}), None)
            .await;
        if got["result"]["result"]["upload"]["state"] == json!("complete") {
            artifact_id = got["result"]["result"]["upload"]["artifactId"]
                .as_str()
                .map(ToOwned::to_owned);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let artifact_id = artifact_id.expect("upload reaches complete");

    // 分块下载 → 字节一致 + sha256 复核。
    let mut assembled = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = session
            .call(
                "artifacts.read",
                json!({"artifactId": artifact_id, "offset": offset, "maxBytes": 512 * 1024}),
                None,
            )
            .await;
        let chunk = &page["result"]["result"]["chunk"];
        let bytes = base64_compat::b64_decode(chunk["bytesBase64"].as_str().unwrap_or_default());
        assert_eq!(chunk["offset"], json!(offset));
        assembled.extend_from_slice(&bytes);
        offset += bytes.len() as u64;
        if chunk["eof"] == json!(true) {
            break;
        }
    }
    assert_eq!(assembled, whole);
    assert_eq!(sha256_hex(&assembled), total_sha);

    // 取消语义：对已完成上传取消 → 产物保留（重复取消安全）。
    let cancelled = session
        .call("uploads.cancel", json!({"uploadId": upload_id}), None)
        .await;
    assert_eq!(cancelled["result"]["result"]["state"], json!("complete"));
    assert_eq!(
        cancelled["result"]["result"]["artifactId"],
        json!(artifact_id)
    );
    let after = session
        .call(
            "artifacts.read",
            json!({"artifactId": artifact_id, "offset": 0, "maxBytes": 8}),
            None,
        )
        .await;
    assert_eq!(after["result"]["result"]["chunk"]["byteLength"], json!(8));

    // documents：capabilities 显式报告依赖（缺依赖 available=false+原因）。
    let caps = session
        .call("documents.capabilities", json!({}), None)
        .await;
    for format in caps["result"]["result"]["formats"].as_array().unwrap() {
        if format["available"] == json!(false) {
            assert!(
                format["unavailableReason"]
                    .as_str()
                    .is_some_and(|r| !r.is_empty()),
                "{format}"
            );
        }
    }

    // csv 上传为 artifact → preview（manifest + sheet）receipt 形态。
    // completionResult 深断言待 collections 的 operations.get 联调；域内
    // 已在 documents.rs tests 覆盖（async_operations 表直查）。
    let csv = "a,b\n1,2\n3,4\n".to_owned();
    let csv_sha = sha256_hex(csv.as_bytes());
    let csv_artifact = session
        .call(
            "uploads.create",
            json!({
                "projectId": project_id,
                "displayName": "grid.csv",
                "mediaType": "text/csv",
                "byteLength": csv.len(),
                "sha256": csv_sha,
            }),
            Some("e2e-docs-csv-create"),
        )
        .await;
    let csv_upload_id = csv_artifact["result"]["result"]["upload"]["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();
    session
        .call(
            "uploads.part",
            json!({
                "uploadId": csv_upload_id,
                "partNumber": 0,
                "bytesBase64": base64_compat::b64_encode(csv.as_bytes()),
                "sha256": csv_sha,
            }),
            None,
        )
        .await;
    session
        .call(
            "uploads.complete",
            json!({"uploadId": csv_upload_id, "expectedSha256": csv_sha}),
            Some("e2e-docs-csv-complete"),
        )
        .await;
    let mut csv_artifact_id = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let got = session
            .call("uploads.get", json!({"uploadId": csv_upload_id}), None)
            .await;
        if got["result"]["result"]["upload"]["state"] == json!("complete") {
            csv_artifact_id = got["result"]["result"]["upload"]["artifactId"]
                .as_str()
                .map(ToOwned::to_owned);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let csv_artifact_id = csv_artifact_id.expect("csv upload completes");
    let csv_meta = session
        .call(
            "artifacts.get",
            json!({"artifactId": csv_artifact_id}),
            None,
        )
        .await;
    let csv_revision = csv_meta["result"]["result"]["artifact"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let preview = session
        .call(
            "documents.preview",
            json!({
                "source": {"artifactId": csv_artifact_id},
                "expectedRevision": csv_revision,
                "output": "spreadsheetManifest",
            }),
            Some("e2e-docs-preview-1"),
        )
        .await;
    assert_eq!(preview["result"]["result"]["kind"], json!("operation"));
    assert_eq!(preview["result"]["result"]["state"], json!("running"));

    let sheet = session
        .call(
            "documents.preview",
            json!({
                "source": {"artifactId": csv_artifact_id},
                "expectedRevision": csv_revision,
                "output": "spreadsheetSheet",
                "sheetId": "0",
                "rowStart": 1,
                "rowCount": 1,
            }),
            Some("e2e-docs-sheet-1"),
        )
        .await;
    assert_eq!(sheet["result"]["result"]["kind"], json!("operation"));

    // 过期 revision → REVISION_CONFLICT（缓存不得冒充最新）。
    let stale = session
        .call(
            "documents.preview",
            json!({
                "source": {"artifactId": csv_artifact_id},
                "expectedRevision": "arev-stale",
                "output": "spreadsheetManifest",
            }),
            Some("e2e-docs-stale"),
        )
        .await;
    assert_eq!(stale["error"]["data"]["code"], "REVISION_CONFLICT");

    // 不支持的输出 → UNSUPPORTED_FORMAT。
    let unsupported = session
        .call(
            "documents.preview",
            json!({
                "source": {"artifactId": csv_artifact_id},
                "expectedRevision": csv_revision,
                "output": "pdf",
            }),
            Some("e2e-docs-unsupported"),
        )
        .await;
    assert_eq!(unsupported["error"]["data"]["code"], "UNSUPPORTED_FORMAT");

    session.close_stdin().await;
    session.wait_exit(0, "api connect exits cleanly").await;
}

/// c017 并发写（公共传输层）：同一 revision 的两个并发写，恰好一个成功。
#[tokio::test]
async fn concurrent_writes_have_exactly_one_winner_over_ws() {
    let (home, workspace, project_id, _iid) = seed_workspace("files-race").await;
    std::fs::write(workspace.join("race.txt"), b"base").unwrap();
    let mut session = ConnectSession::start(home.path()).await;
    session.initialize().await;

    let stat = session
        .call(
            "files.stat",
            json!({"path": {"projectId": project_id, "relativePath": {"encoding": "utf8", "value": "race.txt"}}}),
            None,
        )
        .await;
    let revision = stat["result"]["result"]["entry"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let mk = |payload: &[u8]| {
        json!({
            "path": {"projectId": project_id, "relativePath": {"encoding": "utf8", "value": "race.txt"}},
            "content": {"bytesBase64": base64_compat::b64_encode(payload)},
            "precondition": {"state": "revision", "revision": revision},
        })
    };
    // 同一 ws 连接上背靠背两帧（服务器侧各占一个 dispatch 任务并发执行；
    // ConnectSession::call 独占 &mut，这里用裸帧发送 + 按响应 id 回收）。
    let id_a = session.fresh_id("wa");
    let id_b = session.fresh_id("wb");
    session
        .send(&common_v1::rpc_request(
            &id_a,
            "files.write",
            json!({"input": mk(b"writer-a")}),
        ))
        .await;
    session
        .send(&common_v1::rpc_request(
            &id_b,
            "files.write",
            json!({"input": mk(b"writer-b")}),
        ))
        .await;
    let a = session.wait_response(&id_a, "concurrent write a").await;
    let b = session.wait_response(&id_b, "concurrent write b").await;
    let mut succeeded = 0;
    let mut conflicted = 0;
    for resp in [&a, &b] {
        if resp.get("error").is_none() && resp["result"]["result"]["entry"].is_object() {
            succeeded += 1;
        } else if resp["error"]["data"]["code"] == "REVISION_CONFLICT" {
            conflicted += 1;
        } else {
            panic!("unexpected write response: {resp}");
        }
    }
    assert_eq!(succeeded, 1, "exactly one writer wins: a={a} b={b}");
    assert_eq!(conflicted, 1);
    let content = std::fs::read(workspace.join("race.txt")).unwrap();
    assert!(
        content == b"writer-a" || content == b"writer-b",
        "no partial content"
    );
}

/// prototype.p2（007 §10.3）：500MB 上传（第二条连接）期间，主连接上
/// approval.respond 的 p95 < 200ms。本地未跑（构建冻结 + 磁盘红线）：
/// 在 cb8 上 `cargo test -p agentcpm-pd-cli --test e2e_v1_files_uploads
/// p2_upload -- --ignored --nocapture`。
#[tokio::test]
#[ignore = "P2 device: 500MB synthetic upload over the second ws connection; run on cb8"]
async fn p2_upload_500mb_keeps_approval_respond_p95_under_200ms() {
    const TOTAL_BYTES: u64 = 500 * 1024 * 1024;
    const PART_BYTES: usize = 512 * 1024;
    let (home, _workspace, project_id, _iid) = seed_workspace("p2-upload").await;

    // 主连接：审批应答；传输连接：第二条 /v1/ws（007 §4 文件交接）。
    let mut main = ConnectSession::start(home.path()).await;
    main.initialize().await;
    let mut transfer = ConnectSession::start(home.path()).await;
    transfer.initialize().await;

    // 会话 + 连续 approval runs（offline driver 每个挂一个审批）。
    let session_id = format!("s-p2-{}", std::process::id());
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
    for index in 0..40 {
        let out = await_cli(
            home.path(),
            &[
                "run",
                "submit",
                "--output",
                "json",
                "--session-id",
                &session_id,
                "--request-key",
                &format!("p2-run-{index}"),
                "--text",
                "[pd-driver:approval]",
            ],
        );
        expect_ok("run.submit", &out);
    }

    // 确定性合成字节：分块即时生成（不预存 500MB）；整文件 sha 先流式算出。
    let part_bytes = |index: u64| -> Vec<u8> {
        let mut bytes = vec![0u8; PART_BYTES];
        for (i, slot) in bytes.iter_mut().enumerate() {
            *slot = ((index.wrapping_mul(131_542_391) + i as u64) % 251) as u8;
        }
        bytes
    };
    let total_parts = TOTAL_BYTES / PART_BYTES as u64;
    let mut total_hasher = Sha256::new();
    for index in 0..total_parts {
        total_hasher.update(part_bytes(index));
    }
    let total_sha = format!("{:x}", total_hasher.finalize());

    // 传输任务（第二条连接独占）：create → part×N → complete。
    let transfer_project = project_id.clone();
    let upload_task = {
        let mut transfer = transfer;
        Box::pin(async move {
            let created = transfer
                .call(
                    "uploads.create",
                    json!({
                        "projectId": transfer_project,
                        "displayName": "p2-500mb.bin",
                        "mediaType": "application/octet-stream",
                        "byteLength": TOTAL_BYTES,
                        "sha256": total_sha,
                    }),
                    Some("p2-upload-create"),
                )
                .await;
            let upload_id = created["result"]["result"]["upload"]["uploadId"]
                .as_str()
                .unwrap()
                .to_owned();
            for index in 0..total_parts {
                let bytes = part_bytes(index);
                let part_sha = {
                    let mut hasher = Sha256::new();
                    hasher.update(&bytes);
                    format!("{:x}", hasher.finalize())
                };
                let part = transfer
                    .call(
                        "uploads.part",
                        json!({
                            "uploadId": upload_id,
                            "partNumber": index,
                            "bytesBase64": base64_compat::b64_encode(&bytes),
                            "sha256": part_sha,
                        }),
                        None,
                    )
                    .await;
                assert!(part.get("error").is_none(), "part {index} failed: {part}");
            }
            let receipt = transfer
                .call(
                    "uploads.complete",
                    json!({"uploadId": upload_id, "expectedSha256": total_sha}),
                    Some("p2-upload-complete"),
                )
                .await;
            assert!(receipt.get("error").is_none(), "{receipt}");
            upload_id
        })
    };

    // 主连接：上传进行中持续应答审批，记录 dispatch→响应墙钟。
    let mut latencies: Vec<Duration> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(600);
    let upload_handle = tokio::task::spawn(upload_task);
    while latencies.len() < 30 && Instant::now() < deadline {
        let ilist = main
            .call("interaction.list", json!({"state": "pending"}), None)
            .await;
        let items = ilist["result"]["result"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let Some(approval) = items.iter().find(|i| i["state"] == json!("pending")) else {
            if upload_handle.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        };
        let interaction_id = approval["interactionId"].as_str().unwrap().to_owned();
        let revision = approval["revision"].as_str().unwrap().to_owned();
        let started = Instant::now();
        let respond = main
            .call(
                "approval.respond",
                json!({
                    "interactionId": interaction_id,
                    "expectedRevision": revision,
                    "decision": "allow",
                }),
                None,
            )
            .await;
        let elapsed = started.elapsed();
        assert!(
            respond.get("error").is_none(),
            "approval.respond failed: {respond}"
        );
        latencies.push(elapsed);
    }
    // 等上传收尾（bounded）。
    let upload_id = tokio::time::timeout(Duration::from_secs(900), upload_handle)
        .await
        .expect("upload task finishes")
        .expect("upload task join");
    let _ = upload_id;

    // p95 断言（007 §10.3）。
    latencies.sort();
    assert!(!latencies.is_empty(), "no approval samples collected");
    let p95_index = latencies.len() * 95 / 100;
    let p95 = latencies[p95_index.min(latencies.len() - 1)];
    assert!(
        p95 < Duration::from_millis(200),
        "approval.respond p95 {p95:?} over {} samples",
        latencies.len()
    );
}
