//! 本地路径交接（t013，S3；007 §4 / §6.7 L265）。
//!
//! 主连接（宿主 ↔ CLI 的 stdio）不承载文件字节：上传/下载走
//! [`FileTransferSession`] —— 一条**专用**的 Gateway `/v1/ws` 连接
//! （`PdClient::connect` 同一目标与凭据再开一条），字节经公共操作
//! `uploads.create → uploads.part×N → uploads.complete` 与 `artifacts.read`
//! 传输。路径限制在**宿主声明的根目录内**：所有源/目标路径 canonical 复核
//! 不得越根（符号链接逃逸拒绝）。
//!
//! 上传：流式计算整文件 sha256（不整载内存），按服务端 `upload.partSize`
//! 分块、边读边发；分块响应丢失/重连后重发同一块由服务端 deduplicated
//! 兜底。下载：`artifacts.read` 循环至 eof，落盘走「暂存 → 校验 sha256 →
//! rename」；中断/部分读取的错误信息**显式携带已取得范围与完整性状态**
//! （合同 artifact-lifecycle 验收），不以部分字节冒充完整产物。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::ClientError;
use crate::ws_client::unwrap_rpc_outcome;
use crate::{PdClient, PdClientOptions};

// ---------------------------------------------------------------------------
// 内置 base64（标准字母表 + 填充）。客户端 crate 无 base64 依赖且本轮禁改
// Cargo.toml；分块传输只需要这一对编解码。
// ---------------------------------------------------------------------------

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[(triple >> 18) as usize & 0x3F] as char);
        out.push(BASE64_ALPHABET[(triple >> 12) as usize & 0x3F] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[(triple >> 6) as usize & 0x3F] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[triple as usize & 0x3F] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, ClientError> {
    fn value_of(ch: char) -> Result<u32, ClientError> {
        match ch {
            'A'..='Z' => Ok((ch as u32) - b'A' as u32),
            'a'..='z' => Ok((ch as u32) - b'a' as u32 + 26),
            '0'..='9' => Ok((ch as u32) - b'0' as u32 + 52),
            '+' => Ok(62),
            '/' => Ok(63),
            other => Err(ClientError::usage(format!(
                "invalid base64 character `{other}`"
            ))),
        }
    }
    let bytes: Vec<char> = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return Err(ClientError::usage("base64 length must be a multiple of 4"));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|ch| **ch == '=').count();
        if pad > 2 || (pad > 0 && chunk[..4 - pad].contains(&'=')) {
            return Err(ClientError::usage("misplaced base64 padding"));
        }
        let mut triple: u32 = 0;
        for (index, ch) in chunk.iter().enumerate() {
            let value = if *ch == '=' { 0 } else { value_of(*ch)? };
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
    Ok(out)
}

/// 客户端侧分块上限（与服务端 `uploads.limits.maxPartBytes` 对齐的钳制；
/// 实际块大小以服务端返回的 `upload.partSize` 为准）。
const CLIENT_PART_CEILING: u64 = 512 * 1024;
/// 上传完成后等待服务端终态的轮询上限。
const UPLOAD_TERMINAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const UPLOAD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
/// 下载循环的总 deadline（与上传侧终态轮询的 600s 对齐：`artifacts.read`
/// 循环必须自行有界，不能依赖对端一定给出 eof——rb7-store-client
/// boundary P0 / concurrency #6）。
const DOWNLOAD_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// 下载暂存名的进程内单调序号（`config.rs::atomic_write_file` 的同一
/// 先例）：并发下载同 stem 不再共用一个暂存路径互相截断（rb7-store-client
/// boundary P1）。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 一次成功上传的产物。
pub struct UploadedFile {
    pub upload_id: String,
    pub artifact_id: String,
    pub upload: Value,
    pub artifact: Value,
}

/// 一次成功下载的核对结果。
pub struct DownloadedFile {
    pub artifact_id: String,
    pub path: PathBuf,
    pub byte_length: u64,
    pub sha256: String,
}

/// 本地路径交接会话：专用第二条连接 + 宿主声明根。
pub struct FileTransferSession {
    client: PdClient,
    allowed_root: PathBuf,
}

impl FileTransferSession {
    /// 打开会话：`opts` 与主连接同一目标/凭据（007 §4：文件传输另开一条
    /// Gateway 连接）；`allowed_root` 是宿主声明根（canonical 化后保存）。
    pub async fn open(
        opts: PdClientOptions,
        allowed_root: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        let root = allowed_root.as_ref().to_path_buf();
        if !root.is_absolute() {
            return Err(ClientError::usage("allowed_root must be an absolute path"));
        }
        std::fs::create_dir_all(&root)
            .map_err(|err| ClientError::usage(format!("allowed_root unusable: {err}")))?;
        let canonical = std::fs::canonicalize(&root)
            .map_err(|err| ClientError::usage(format!("allowed_root not resolvable: {err}")))?;
        let client = PdClient::connect(opts).await?;
        Ok(Self {
            client,
            allowed_root: canonical,
        })
    }

    /// 会话底层的传输连接（P2 装置等直接驱动 uploads.* 用）。
    pub fn client(&self) -> &PdClient {
        &self.client
    }

    fn resolve_within_root(&self, path: &Path) -> Result<PathBuf, ClientError> {
        resolve_within_root(&self.allowed_root, path)
    }

    /// 写目标复核（目标尚不存在，canonical 化最近存在祖先）。
    fn resolve_write_target(&self, path: &Path) -> Result<PathBuf, ClientError> {
        resolve_write_target_under(&self.allowed_root, path)
    }

    /// 下载暂存路径：`staging_candidate` 的唯一名 + 与目标同强度的
    /// `resolve_write_target` 复核（预置同名 symlink 指向根外时，最近
    /// 存在祖先就是暂存路径自身，canonical 解析越根被拒）；打开时的
    /// `create_new`（O_EXCL，不跟随 symlink）再拦 dangling symlink 一道。
    fn staging_path(&self, destination: &Path) -> Result<PathBuf, ClientError> {
        let candidate = staging_candidate(destination, TMP_SEQ.fetch_add(1, Ordering::Relaxed));
        self.resolve_write_target(&candidate)
    }

    /// 上传一个本地文件为不可变产物。`source` 限制在宿主声明根内；
    /// `display_name` 只是产物显示名，不是服务端路径。
    pub async fn upload_file(
        &self,
        source: impl AsRef<Path>,
        project_id: &str,
        display_name: &str,
        media_type: &str,
    ) -> Result<UploadedFile, ClientError> {
        let source = self.resolve_within_root(source.as_ref())?;
        let meta = std::fs::metadata(&source)
            .map_err(|err| ClientError::usage(format!("source stat failed: {err}")))?;
        if !meta.is_file() {
            return Err(ClientError::usage("upload source is not a regular file"));
        }
        let byte_length = meta.len();
        let total_sha = hash_file(&source)
            .map_err(|err| ClientError::usage(format!("source hashing failed: {err}")))?;

        // create（幂等由 ledger/服务端双方兜底）。
        let created = self
            .client
            .invoke(
                "uploads.create",
                json!({
                    "projectId": project_id,
                    "displayName": display_name,
                    "mediaType": media_type,
                    "byteLength": byte_length,
                    "sha256": total_sha,
                }),
                None,
                None,
                None,
            )
            .await?;
        let upload = unwrap_rpc_outcome(&created);
        let upload_id = upload["upload"]["uploadId"]
            .as_str()
            .ok_or_else(|| ClientError::usage("uploads.create result missing uploadId"))?
            .to_owned();
        let part_size = upload["upload"]["partSize"]
            .as_u64()
            .unwrap_or(CLIENT_PART_CEILING);
        let part_size = part_size.clamp(1, CLIENT_PART_CEILING);

        // 分块（0 起始，顺序发送；服务端支持乱序，顺序即最简）。
        let mut file = std::fs::File::open(&source)
            .map_err(|err| ClientError::usage(format!("source open failed: {err}")))?;
        let mut part_number: u64 = 0;
        let mut sent: u64 = 0;
        while sent < byte_length {
            let want = part_size.min(byte_length - sent) as usize;
            let mut buffer = vec![0u8; want];
            file.read_exact(&mut buffer)
                .map_err(|err| ClientError::usage(format!("source read failed: {err}")))?;
            let part_sha = {
                let mut hasher = Sha256::new();
                hasher.update(&buffer);
                format!("{:x}", hasher.finalize())
            };
            // 传输层瞬时失败重试同一块（服务端 deduplicated 保证不重复占用）。
            let mut attempt = 0u32;
            loop {
                match self
                    .client
                    .invoke(
                        "uploads.part",
                        json!({
                            "uploadId": upload_id,
                            "partNumber": part_number,
                            "bytesBase64": base64_encode(&buffer),
                            "sha256": part_sha,
                        }),
                        None,
                        None,
                        None,
                    )
                    .await
                {
                    Ok(_) => break,
                    Err(err) if attempt < 2 && !matches!(err, ClientError::Contract(_)) => {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    Err(err) => {
                        return Err(ClientError::usage(format!(
                            "uploads.part {part_number} failed after {sent}/{byte_length} bytes \
                             (upload `{upload_id}` remains resumable): {err}"
                        )));
                    }
                }
            }
            sent += want as u64;
            part_number += 1;
        }

        // complete（异步 receipt）→ 轮询 uploads.get 至终态。
        let _receipt = self
            .client
            .invoke(
                "uploads.complete",
                json!({"uploadId": upload_id, "expectedSha256": total_sha}),
                None,
                None,
                None,
            )
            .await?;
        let deadline = tokio::time::Instant::now() + UPLOAD_TERMINAL_TIMEOUT;
        loop {
            let got = self
                .client
                .invoke(
                    "uploads.get",
                    json!({"uploadId": upload_id}),
                    None,
                    None,
                    None,
                )
                .await?;
            // PdClient::invoke returns an OperationResponse (a struct, not a
            // Value): the operation result rides in its `result` field via
            // unwrap_rpc_outcome.
            let upload_state = unwrap_rpc_outcome(&got);
            let state = upload_state["upload"]["state"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            match state.as_str() {
                "complete" => {
                    let artifact_id = upload_state["upload"]["artifactId"]
                        .as_str()
                        .ok_or_else(|| ClientError::usage("complete upload missing artifactId"))?
                        .to_owned();
                    let artifact = self
                        .client
                        .invoke(
                            "artifacts.get",
                            json!({"artifactId": artifact_id}),
                            None,
                            None,
                            None,
                        )
                        .await?;
                    return Ok(UploadedFile {
                        upload_id,
                        artifact_id,
                        upload: unwrap_rpc_outcome(&got),
                        artifact: unwrap_rpc_outcome(&artifact),
                    });
                }
                "failed" | "cancelled" | "expired" => {
                    return Err(ClientError::usage(format!(
                        "upload `{upload_id}` terminalized as `{state}`"
                    )));
                }
                _ => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(ClientError::usage(format!(
                            "upload `{upload_id}` did not terminalize within {}s \
                             (operationId receipt returned; re-query uploads.get)",
                            UPLOAD_TERMINAL_TIMEOUT.as_secs()
                        )));
                    }
                    tokio::time::sleep(UPLOAD_POLL_INTERVAL).await;
                }
            }
        }
    }

    /// 下载一个产物到本地路径（限制在宿主声明根内）。分块循环至 eof，落盘
    /// 前 sha256 全量校验；中断的错误信息携带已取得范围与完整性状态。
    pub async fn download_artifact(
        &self,
        artifact_id: &str,
        destination: impl AsRef<Path>,
    ) -> Result<DownloadedFile, ClientError> {
        let got = self
            .client
            .invoke(
                "artifacts.get",
                json!({"artifactId": artifact_id}),
                None,
                None,
                None,
            )
            .await?;
        let artifact = unwrap_rpc_outcome(&got);
        let total = artifact["artifact"]["byteLength"].as_u64().unwrap_or(0);
        let expected_sha = artifact["artifact"]["sha256"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        let destination = self.resolve_write_target(destination.as_ref())?;
        // destination 为已存在目录（含 allowed_root 本身）时拒绝：旧
        // `with_extension` 派生的暂存名会落到根外兄弟路径，rename 也不成立。
        ensure_not_directory(&destination)?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| ClientError::usage(format!("destination mkdir failed: {err}")))?;
        }
        let temp = self.staging_path(&destination)?;
        let mut hasher = Sha256::new();
        let mut received: u64 = 0;
        let cleanup = |temp: &Path| {
            let _ = std::fs::remove_file(temp);
        };
        {
            // `create_new`（O_EXCL）不跟随 symlink：预置在暂存名上的
            // symlink（含 dangling）得到 EEXIST 而不是被跟随写出根外。
            let mut out = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)
                .map_err(|err| ClientError::usage(format!("staging create failed: {err}")))?;
            // 总 deadline：循环不依赖对端一定推进或给出 eof。
            let deadline = tokio::time::Instant::now() + DOWNLOAD_TOTAL_TIMEOUT;
            loop {
                if tokio::time::Instant::now() >= deadline {
                    cleanup(&temp);
                    return Err(ClientError::usage(format!(
                        "download of `{artifact_id}` did not finish within {}s at \
                         {received}/{total} bytes (integrity unverified, partial bytes \
                         discarded)",
                        DOWNLOAD_TOTAL_TIMEOUT.as_secs()
                    )));
                }
                let page = match self
                    .client
                    .invoke(
                        "artifacts.read",
                        json!({
                            "artifactId": artifact_id,
                            "offset": received,
                            "maxBytes": CLIENT_PART_CEILING,
                        }),
                        None,
                        None,
                        None,
                    )
                    .await
                {
                    Ok(page) => page,
                    Err(err) => {
                        cleanup(&temp);
                        return Err(ClientError::usage(format!(
                            "download interrupted at {received}/{total} bytes of `{artifact_id}` \
                             (integrity unverified, partial bytes discarded): {err}"
                        )));
                    }
                };
                let chunk = &unwrap_rpc_outcome(&page)["chunk"];
                let (bytes, eof) = match validate_download_chunk(chunk, received, total) {
                    Ok(valid) => valid,
                    Err(err) => {
                        cleanup(&temp);
                        return Err(err);
                    }
                };
                hasher.update(&bytes);
                if let Err(err) = out.write_all(&bytes) {
                    cleanup(&temp);
                    return Err(ClientError::usage(format!("staging write failed: {err}")));
                }
                received += bytes.len() as u64;
                if eof {
                    break;
                }
            }
            if let Err(err) = out.sync_all() {
                cleanup(&temp);
                return Err(ClientError::usage(format!("staging fsync failed: {err}")));
            }
        }
        let actual_sha = format!("{:x}", hasher.finalize());
        if received != total || actual_sha != expected_sha {
            cleanup(&temp);
            return Err(ClientError::usage(format!(
                "download integrity failed: got {received}/{total} bytes, \
                 sha256 `{actual_sha}` != recorded `{expected_sha}`"
            )));
        }
        std::fs::rename(&temp, &destination).map_err(|err| {
            cleanup(&temp);
            ClientError::usage(format!("final rename failed: {err}"))
        })?;
        Ok(DownloadedFile {
            artifact_id: artifact_id.to_owned(),
            path: destination,
            byte_length: received,
            sha256: actual_sha,
        })
    }
}

fn hash_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 词法拒绝 `..` 逃逸 + canonical 复核（符号链接逃逸拒绝）。
fn resolve_within_root(allowed_root: &Path, path: &Path) -> Result<PathBuf, ClientError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        allowed_root.join(path)
    };
    reject_parent_components(&candidate)?;
    let canonical = std::fs::canonicalize(&candidate).map_err(|err| {
        ClientError::usage(format!(
            "path `{}` does not resolve inside the allowed root: {err}",
            candidate.display()
        ))
    })?;
    if !canonical.starts_with(allowed_root) {
        return Err(ClientError::usage(format!(
            "path `{}` escapes the allowed root `{}`",
            canonical.display(),
            allowed_root.display()
        )));
    }
    Ok(canonical)
}

/// 写目标复核（目标尚不存在，canonical 化最近存在祖先）。抽成自由函数
/// 使负控测试直接压在真实判定路径上，而非只压它内部调用的某个子函数
/// （rb7-store-client boundary P1 / #8）。
fn resolve_write_target_under(allowed_root: &Path, path: &Path) -> Result<PathBuf, ClientError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        allowed_root.join(path)
    };
    // 与读侧 `resolve_within_root` 同一规则：先词法拒绝 `..`。没有这一
    // 层，`missing/../../outside/f` 能借「最近存在祖先」的锚点校验
    // （锚 `/r` 本身合法）放行未归一化的 candidate，随后 create_dir_all
    // 先建出根内前缀、再经 `..` 建到根外。读写两侧强度必须一致
    // （rb7-dsh boundary P1）。
    reject_parent_components(&candidate)?;
    let mut nearest = candidate.clone();
    while !nearest.exists() {
        match nearest.parent() {
            Some(parent) => nearest = parent.to_path_buf(),
            None => {
                return Err(ClientError::usage(format!(
                    "destination `{}` has no existing ancestor",
                    candidate.display()
                )));
            }
        }
    }
    let canonical = std::fs::canonicalize(&nearest)
        .map_err(|err| ClientError::usage(format!("destination anchor failed: {err}")))?;
    if !canonical.starts_with(allowed_root) {
        return Err(ClientError::usage(format!(
            "destination `{}` escapes the allowed root `{}`",
            candidate.display(),
            allowed_root.display()
        )));
    }
    Ok(candidate)
}

/// destination 为已存在目录（含 allowed_root 本身，含指向目录的
/// symlink）时拒绝下载落盘：这种目标既无法 rename 交付，旧实现按
/// `with_extension` 派生的暂存名还会落到根外。
fn ensure_not_directory(path: &Path) -> Result<(), ClientError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Err(ClientError::usage(format!(
            "destination `{}` is a directory",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// 暂存名（`config.rs::atomic_write_file` 的 TMP_SEQ 先例）：保留完整
/// 目标文件名 + pid + 进程内单调序号，与目标同目录。旧的
/// `with_extension("agentcpm-pd-part-<pid>")` 只保留 stem：同 stem 不同
/// 扩展（`a.txt`/`a.bin`，`x.tar.gz`/`x.tar.bin`）的并发下载共用同一
/// 暂存路径互相截断，且在带扩展名的目录上派生出根外兄弟路径。
fn staging_candidate(destination: &Path, seq: u64) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    destination.with_file_name(format!(
        ".{name}.agentcpm-pd-part.{}.{}",
        std::process::id(),
        seq
    ))
}

/// 单个 `artifacts.read` chunk 的协议守卫（rb7-store-client boundary
/// P0）。「字节之后不得再有可挂起的等待」的对端行为全部按协议错误
/// 处理，不允许零进展自旋：
///
/// - 契约 `workspace__ByteChunk` 要求 `bytesBase64`/`offset`/`eof` 均
///   **显式存在**（`state/cli-v1-operations.json` 的 required 列表）；
///   缺失不再是「默认空字节/默认 offset/默认非 eof」。
/// - 空块只允许作为 eof 终止形态（本仓网关只在 `offset == total` 时
///   产出）；空块且非 eof 是零进展，直接失败。
/// - 任何一帧都不得把已收字节推过 `artifacts.get` 声明的总长。
///
/// 返回 `(bytes, eof)`；`offset == received` 亦在此复核。
fn validate_download_chunk(
    chunk: &Value,
    received: u64,
    total: u64,
) -> Result<(Vec<u8>, bool), ClientError> {
    let eof = match chunk.get("eof") {
        Some(Value::Bool(eof)) => *eof,
        _ => {
            return Err(ClientError::usage(
                "artifacts.read chunk missing required boolean `eof`",
            ));
        }
    };
    let offset = chunk.get("offset").and_then(Value::as_u64).ok_or_else(|| {
        ClientError::usage("artifacts.read chunk missing required integer `offset`")
    })?;
    let encoded = chunk
        .get("bytesBase64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ClientError::usage("artifacts.read chunk missing required string `bytesBase64`")
        })?;
    let bytes = base64_decode(encoded)
        .map_err(|err| ClientError::usage(format!("chunk decode failed: {err}")))?;
    if offset != received {
        return Err(ClientError::usage(format!(
            "chunk offset regression: expected {received}, server returned {offset}"
        )));
    }
    if bytes.is_empty() && !eof {
        return Err(ClientError::usage(format!(
            "empty non-terminal chunk at offset {received} of {total} bytes \
             (zero progress; refusing to spin)"
        )));
    }
    if received + bytes.len() as u64 > total {
        return Err(ClientError::usage(format!(
            "server sent {} bytes beyond the declared artifact length {total} at offset {received}",
            received + bytes.len() as u64 - total
        )));
    }
    Ok((bytes, eof))
}

/// 读写两侧共用的词法防线：候选路径（含已拼接 allowed_root 的形态）不得
/// 含 `..` 分量。读侧随后还有 canonical 复核；写侧的目标文件尚不存在、
/// canonical 只锚定最近存在祖先，锚点合法不代表未归一化的 candidate 在
/// 根内——所以这层词法拒绝在写侧是唯一的完整防线，必须与读侧同强度。
fn reject_parent_components(candidate: &Path) -> Result<(), ClientError> {
    for component in candidate.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(ClientError::usage(format!(
                "path `{}` must not contain `..`",
                candidate.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_codec_round_trips_arbitrary_lengths() {
        for len in [0usize, 1, 2, 3, 4, 5, 255, 256, 1024] {
            let data: Vec<u8> = (0..len as u32).map(|i| (i * 7 + 13) as u8).collect();
            let encoded = base64_encode(&data);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(decoded, data, "len {len}");
        }
        assert_eq!(base64_encode(b"hello t013"), "aGVsbG8gdDAxMw==");
        assert!(base64_decode("aGVsbG8gdDAxMw==").is_ok());
        assert!(base64_decode("!!!!").is_err());
        assert!(base64_decode("abc").is_err());
    }

    #[test]
    fn relative_paths_reject_parent_components_and_links() {
        let tmp =
            std::env::temp_dir().join(format!("agentcpm-pd-ft-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(tmp.join("root/sub")).unwrap();
        std::fs::write(tmp.join("root/sub/inside.txt"), b"ok").unwrap();
        std::fs::create_dir_all(tmp.join("outside")).unwrap();
        std::fs::write(tmp.join("outside/secret.txt"), b"secret").unwrap();
        let root = std::fs::canonicalize(tmp.join("root")).unwrap();

        // 词法 .. 拒绝。
        let err =
            resolve_within_root(&root, Path::new("sub/../../outside/secret.txt")).unwrap_err();
        assert!(err.to_string().contains(".."));
        // 绝对路径越界拒绝。
        let outside = std::fs::canonicalize(tmp.join("outside/secret.txt")).unwrap();
        let err = resolve_within_root(&root, &outside).unwrap_err();
        assert!(err.to_string().contains("escapes"));
        // 符号链接逃逸拒绝。
        #[cfg(unix)]
        {
            let link = tmp.join("root/leak");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(tmp.join("outside/secret.txt"), &link).unwrap();
            let err = resolve_within_root(&root, Path::new("leak")).unwrap_err();
            assert!(err.to_string().contains("escapes"), "{err}");
        }
        // 根内正常通过。
        let ok = resolve_within_root(&root, Path::new("sub/inside.txt")).unwrap();
        assert!(ok.ends_with("inside.txt"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_side_lexical_rejection_covers_missing_prefix_escalation() {
        // rb7-dsh boundary P1 负控：`missing/../../outside/f` 的最近存在
        // 祖先是根本身（锚点合法），没有词法层时 create_dir_all 会先建
        // 根内前缀再借 `..` 建到根外。两条防线共用 reject_parent_components。
        let tmp = std::env::temp_dir().join(format!(
            "agentcpm-pd-ftw-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(tmp.join("root/sub")).unwrap();
        let root = std::fs::canonicalize(tmp.join("root")).unwrap();

        let escalating = Path::new("missing/../../outside/f");
        let candidate = root.join(escalating);
        // 词法层先行拒绝（写侧 resolve_write_target 的第一道）。
        let err = reject_parent_components(&candidate).unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
        // 读侧同路径同样被拒（同一函数）。
        let err = resolve_within_root(&root, escalating).unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_side_resolver_rejects_escalation_through_the_real_path() {
        // rb7-store-client #8：负控必须压在真实判定路径上——直接调
        // `resolve_write_target_under`（生产 `resolve_write_target` 的
        // 同一实现），删掉词法层后此测试必须红。
        let tmp = std::env::temp_dir().join(format!(
            "agentcpm-pd-ftr-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(tmp.join("root/sub")).unwrap();
        let root = std::fs::canonicalize(tmp.join("root")).unwrap();

        let err =
            resolve_write_target_under(&root, Path::new("missing/../../outside/f")).unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
        // 根内不存在目标的正常通过（锚点为根）。
        let ok = resolve_write_target_under(&root, Path::new("new/nested/file.txt")).unwrap();
        assert!(ok.ends_with("file.txt"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn staging_names_are_unique_per_call_and_per_stem() {
        // rb7-store-client P1：旧的 `with_extension("agentcpm-pd-part-<pid>")`
        // 只保留 stem，同 stem 不同扩展（`a.txt`/`a.bin`、
        // `x.tar.gz`/`x.tar.bin`）的并发下载共用一个暂存路径互相截断。
        // 新构造保留完整文件名 + 进程内单调序号。
        let first = staging_candidate(Path::new("/r/report.txt"), 0);
        let second = staging_candidate(Path::new("/r/report.txt"), 1);
        assert_ne!(first, second, "seq must disambiguate same-name downloads");
        let gz = staging_candidate(Path::new("/r/x.tar.gz"), 0);
        let bin = staging_candidate(Path::new("/r/x.tar.bin"), 0);
        assert_ne!(
            gz, bin,
            "same-stem destinations must no longer share a staging name"
        );
        let name = first.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(
            name.starts_with(".report.txt.agentcpm-pd-part."),
            "staging stays beside the destination: {name}"
        );
        assert_eq!(first.parent(), Some(Path::new("/r")));
    }

    #[test]
    fn preexisting_staging_symlink_escape_is_rejected_and_create_new_refuses_to_follow() {
        // rb7-store-client P1：派生暂存路径此前不做根内复核，预置同名
        // symlink 即可让 `File::create` 写出根外。
        let tmp =
            std::env::temp_dir().join(format!("agentcpm-pd-fs-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(tmp.join("root/sub")).unwrap();
        std::fs::create_dir_all(tmp.join("outside")).unwrap();
        std::fs::write(tmp.join("outside/secret.txt"), b"secret").unwrap();
        let root = std::fs::canonicalize(tmp.join("root")).unwrap();
        let destination = root.join("sub/report.txt");

        #[cfg(unix)]
        {
            // (a) 暂存名是指向根外已存在文件的 symlink：最近存在祖先就是
            // 暂存路径自身，canonical 解析越根被拒（同强度复核）。
            let hijacked = staging_candidate(&destination, 41);
            std::os::unix::fs::symlink(tmp.join("outside/secret.txt"), &hijacked).unwrap();
            let err = resolve_write_target_under(&root, &hijacked).unwrap_err();
            assert!(
                err.to_string().contains("escapes"),
                "pre-existing staging symlink must be rejected: {err}"
            );

            // (b) dangling symlink 指向根外尚不存在路径：锚点在根内父
            // 目录，复核拦不住——`create_new`（O_EXCL，不跟随 symlink）
            // 得到 EEXIST，根外不会被创建。这道防线在 download_artifact
            // 的 `OpenOptions::create_new(true)` 上。
            let dangling = staging_candidate(&destination, 42);
            std::os::unix::fs::symlink(tmp.join("outside/never-created"), &dangling).unwrap();
            let err = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&dangling)
                .unwrap_err();
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::AlreadyExists,
                "create_new must refuse to follow a pre-existing symlink"
            );
            assert!(
                !tmp.join("outside/never-created").exists(),
                "the out-of-root target must not be created"
            );
        }
        // 干净的暂存路径正常通过（锚点 = 根内父目录）。
        let ok = resolve_write_target_under(&root, &staging_candidate(&destination, 43)).unwrap();
        assert!(ok.file_name().unwrap().to_str().unwrap().contains("part"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn directory_destination_is_rejected() {
        // rb7-store-client P1：destination 为已存在目录（含
        // allowed_root 本身）时拒绝——旧实现的暂存名会派生到根外。
        let tmp =
            std::env::temp_dir().join(format!("agentcpm-pd-fd-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(tmp.join("root/sub")).unwrap();
        let root = std::fs::canonicalize(tmp.join("root")).unwrap();

        let err = ensure_not_directory(&root).unwrap_err();
        assert!(err.to_string().contains("directory"), "{err}");
        let err = ensure_not_directory(&root.join("sub")).unwrap_err();
        assert!(err.to_string().contains("directory"), "{err}");
        // 不存在的目标与普通文件目标均放行（rename 覆盖语义不变）。
        ensure_not_directory(&root.join("new.txt")).unwrap();
        std::fs::write(root.join("existing.txt"), b"x").unwrap();
        ensure_not_directory(&root.join("existing.txt")).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn chunk_json(bytes: &[u8], offset: u64, eof: bool) -> Value {
        json!({
            "bytesBase64": base64_encode(bytes),
            "offset": offset,
            "byteLength": bytes.len(),
            "totalByteLength": 10,
            "revision": "rev",
            "eof": eof,
        })
    }

    #[test]
    fn download_chunk_guards_reject_zero_progress_overrun_and_missing_fields() {
        // 正常中间块。
        let (bytes, eof) = validate_download_chunk(&chunk_json(b"abc", 0, false), 0, 10).unwrap();
        assert_eq!(bytes, b"abc");
        assert!(!eof);
        // 空 + eof 终止块：本仓网关在 offset == total 的合法形态。
        let (bytes, eof) = validate_download_chunk(&chunk_json(b"", 10, true), 10, 10).unwrap();
        assert!(bytes.is_empty());
        assert!(eof);

        // P0：空块且非 eof —— 零进展自旋点，直接协议错误。
        let err = validate_download_chunk(&chunk_json(b"", 4, false), 4, 10).unwrap_err();
        assert!(err.to_string().contains("empty non-terminal"), "{err}");
        // P0：received + 本帧越过声明总长。
        let err = validate_download_chunk(&chunk_json(b"abcdef", 8, false), 8, 10).unwrap_err();
        assert!(err.to_string().contains("beyond"), "{err}");
        // P0：eof 缺失（旧实现按非 true 继续循环）与非布尔 eof。
        let mut missing_eof = chunk_json(b"abc", 0, true);
        missing_eof.as_object_mut().unwrap().remove("eof");
        let err = validate_download_chunk(&missing_eof, 0, 10).unwrap_err();
        assert!(err.to_string().contains("eof"), "{err}");
        let mut bad_eof = chunk_json(b"abc", 0, true);
        bad_eof["eof"] = json!("true");
        let err = validate_download_chunk(&bad_eof, 0, 10).unwrap_err();
        assert!(err.to_string().contains("eof"), "{err}");
        // offset 缺失（旧实现默认用 received 静默接受）与回退。
        let mut missing_offset = chunk_json(b"abc", 0, false);
        missing_offset.as_object_mut().unwrap().remove("offset");
        let err = validate_download_chunk(&missing_offset, 0, 10).unwrap_err();
        assert!(err.to_string().contains("offset"), "{err}");
        let err = validate_download_chunk(&chunk_json(b"abc", 3, false), 4, 10).unwrap_err();
        assert!(err.to_string().contains("regression"), "{err}");
        // bytesBase64 缺失（旧实现默认空字节静默接受）。
        let mut missing_bytes = chunk_json(b"abc", 0, false);
        missing_bytes.as_object_mut().unwrap().remove("bytesBase64");
        let err = validate_download_chunk(&missing_bytes, 0, 10).unwrap_err();
        assert!(err.to_string().contains("bytesBase64"), "{err}");
    }
}
