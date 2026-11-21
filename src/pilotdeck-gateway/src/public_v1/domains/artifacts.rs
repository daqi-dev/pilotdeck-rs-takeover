//! `artifacts.*` — 不可变产物存储（t013，S3）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `artifacts.get` /
//! `artifacts.read` 条目（行 34784–35080）。产物是**不可变字节 + 元数据**：
//! 字节落在 `<AGENTCPM_PD_HOME>/v1/artifacts/<artifactId>`（暂存 → fsync →
//! rename 原子发布，发布后只读），SQLite `artifacts` 表登记长度、整文件
//! sha256、mediaType、归属项目。`files.write` 的 `content.artifactId` 引用、
//! `files.delete`/`files.write` 的 recovery 备份、`documents.preview` 的输出
//! 产物都通过本文件的跨域 API 写入。
//!
//! 跨域写入口（memory/t018 detail、git/t014 diff 等后续消费方直接用）：
//! - [`publish_artifact`]：字节 → 不可变产物，返回合同 `Artifact` JSON；
//! - [`copy_artifact_into`]：流式拷出（不整载内存）；
//! - [`read_artifact_bytes`]：小产物整读（recovery 备份等）；
//! - [`artifact_json`]：元信息 JSON（`artifacts.get` 同构）。
//!
//! `artifacts.read` 分块读取与 `files.read` 一样受 512 KiB 响应字节上限
//! （1 MiB 帧预算，007 §6.1）。

use std::fs::File;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_LIMIT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::FILE_CHUNK_MAX_BYTES;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

pub(crate) const CODE_RANGE_INVALID: &str = "RANGE_INVALID";
pub(crate) const CODE_ARTIFACT_NOT_READY: &str = "ARTIFACT_NOT_READY";
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

const ARTIFACTS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS artifacts (
    artifact_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    media_type TEXT NOT NULL,
    byte_length INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    revision TEXT NOT NULL,
    display_name TEXT,
    origin TEXT NOT NULL DEFAULT 'upload',
    created_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("artifacts", ARTIFACTS_TABLE_DDL);
    reg.handler("artifacts.get", |rt, input, _key, _rev| {
        Box::pin(artifacts_get(rt, input))
    });
    reg.handler("artifacts.read", |rt, input, _key, _rev| {
        Box::pin(artifacts_read(rt, input))
    });
}

// ---------------------------------------------------------------------------
// 存储路径
// ---------------------------------------------------------------------------

fn artifacts_dir(store: &V1Store) -> PathBuf {
    store.home().join("v1").join("artifacts")
}

fn artifact_path(store: &V1Store, artifact_id: &str) -> PathBuf {
    artifacts_dir(store).join(artifact_id)
}

fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn io_unavailable(err: std::io::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("artifact storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn artifact_not_found(artifact_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("artifact `{artifact_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn validate_artifact_leaf_name(artifact_id: &str) -> Result<(), ContractError> {
    if artifact_id.is_empty()
        || artifact_id == "."
        || artifact_id == ".."
        || artifact_id.contains('/')
        || artifact_id.contains('\\')
        || artifact_id.contains('\0')
    {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("invalid artifactId `{artifact_id}`"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 跨域 API（uploads / files / documents 及后续 memory、git 消费）
// ---------------------------------------------------------------------------

pub(crate) struct StoredArtifact {
    pub artifact_id: String,
    pub project_id: String,
    pub media_type: String,
    pub byte_length: i64,
    pub sha256: String,
    pub revision: String,
    pub display_name: Option<String>,
    pub created_at: String,
}

impl StoredArtifact {
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "artifactId": self.artifact_id,
            "projectId": self.project_id,
            "mediaType": self.media_type,
            "byteLength": self.byte_length,
            "sha256": self.sha256,
            "revision": self.revision,
            "createdAt": self.created_at,
        });
        if let Some(name) = &self.display_name {
            out["displayName"] = json!(name);
        }
        out
    }
}

/// 发布一个不可变产物：`bytes` 暂存 → fsync → rename 原子落到
/// `<home>/v1/artifacts/<artifactId>`，SQLite 登记元数据后返回合同
/// `Artifact` JSON（含 artifactId/byteLength/sha256/revision/createdAt）。
/// 发布完成后字节不可变（后续无人改写该路径）。
///
/// `origin` 是审计标记（`upload`/`recovery`/`preview`/…），不进入合同
/// 结果，只落在 SQLite 行里。
pub(crate) fn publish_artifact(
    store: &V1Store,
    project_id: &str,
    media_type: &str,
    display_name: Option<&str>,
    bytes: &[u8],
    origin: &str,
) -> Result<Value, ContractError> {
    publish_artifact_from_reader(
        store,
        project_id,
        media_type,
        display_name,
        &mut std::io::Cursor::new(bytes),
        origin,
    )
}

/// 流式发布（uploads.complete 的整装路径：500MB 不进内存）。`reader` 的
/// 字节被完整消费：暂存 → fsync → rename → 登记，sha256 与长度随流计算。
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_artifact_from_reader(
    store: &V1Store,
    project_id: &str,
    media_type: &str,
    display_name: Option<&str>,
    reader: &mut dyn Read,
    origin: &str,
) -> Result<Value, ContractError> {
    if media_type.is_empty() {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "artifact mediaType must not be empty",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let artifact_id = store.next_id();
    validate_artifact_leaf_name(&artifact_id)?;
    let dir = artifacts_dir(store);
    let temp_name = format!(".staging-{artifact_id}");
    let mut hasher = Sha256::new();
    let mut length: u64 = 0;

    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
        crate::project_files::create_directories_beneath(store.home(), &dir)
            .map_err(io_unavailable)?;
        let dir_fd = crate::project_files::open_directory_beneath(store.home(), &dir)
            .map_err(io_unavailable)?;
        struct TempAtGuard<'a> {
            dir: &'a File,
            name: String,
            armed: bool,
        }
        impl Drop for TempAtGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    let _ = unlinkat(self.dir, &self.name, AtFlags::empty());
                }
            }
        }
        let mut temp_guard = TempAtGuard {
            dir: &dir_fd,
            name: temp_name.clone(),
            armed: false,
        };
        {
            let fd = openat(
                &dir_fd,
                &temp_name,
                OFlags::WRONLY
                    | OFlags::CREATE
                    | OFlags::EXCL
                    | OFlags::CLOEXEC
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|err| io_unavailable(err.into()))?;
            temp_guard.armed = true;
            let mut file = File::from(fd);
            let mut buffer = vec![0u8; 256 * 1024];
            loop {
                let read = reader.read(&mut buffer).map_err(io_unavailable)?;
                if read == 0 {
                    break;
                }
                length = length.saturating_add(read as u64);
                if length > MAX_ARTIFACT_BYTES {
                    return Err(ContractError::new(
                        CODE_RESOURCE_LIMIT,
                        format!(
                            "artifact exceeds maximum allowed size of {MAX_ARTIFACT_BYTES} bytes"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                hasher.update(&buffer[..read]);
                file.write_all(&buffer[..read]).map_err(io_unavailable)?;
            }
            file.sync_all().map_err(io_unavailable)?;
        }
        renameat(&dir_fd, &temp_name, &dir_fd, &artifact_id)
            .map_err(|err| io_unavailable(err.into()))?;
        temp_guard.armed = false;
        if let Ok(rd) = crate::public_v1::domains::files::open_dir_readable_nofollow(
            &dir_fd,
            std::ffi::OsStr::new("."),
        ) {
            let _ = rd.sync_all();
        }
    }

    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir).map_err(io_unavailable)?;
        let temp = dir.join(&temp_name);
        struct TempPathGuard {
            path: PathBuf,
            armed: bool,
        }
        impl Drop for TempPathGuard {
            fn drop(&mut self) {
                if self.armed {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
        let mut temp_guard = TempPathGuard {
            path: temp.clone(),
            armed: true,
        };
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(io_unavailable)?;
            let mut buffer = vec![0u8; 256 * 1024];
            loop {
                let read = reader.read(&mut buffer).map_err(io_unavailable)?;
                if read == 0 {
                    break;
                }
                length = length.saturating_add(read as u64);
                if length > MAX_ARTIFACT_BYTES {
                    return Err(ContractError::new(
                        CODE_RESOURCE_LIMIT,
                        format!(
                            "artifact exceeds maximum allowed size of {MAX_ARTIFACT_BYTES} bytes"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                hasher.update(&buffer[..read]);
                file.write_all(&buffer[..read]).map_err(io_unavailable)?;
            }
            file.sync_all().map_err(io_unavailable)?;
        }
        let final_path = artifact_path(store, &artifact_id);
        std::fs::rename(&temp, &final_path).map_err(io_unavailable)?;
        temp_guard.armed = false;
    }

    let sha = format!("{:x}", hasher.finalize());
    let revision = format!("arev-{sha}");
    let created_at = now_rfc3339();
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        if let Err(err) = conn.execute(
            "INSERT INTO artifacts (artifact_id, project_id, media_type, byte_length, sha256, revision, display_name, origin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                artifact_id,
                project_id,
                media_type,
                length as i64,
                sha,
                revision,
                display_name,
                origin,
                created_at,
            ],
        ) {
            let _ = std::fs::remove_file(artifact_path(store, &artifact_id));
            return Err(sql_err(err));
        }
    }
    let mut out = json!({
        "artifactId": artifact_id,
        "projectId": project_id,
        "mediaType": media_type,
        "byteLength": length,
        "sha256": sha,
        "revision": revision,
        "createdAt": created_at,
    });
    if let Some(name) = display_name {
        out["displayName"] = json!(name);
    }
    Ok(out)
}

fn fetch_artifact_row(
    conn: &Connection,
    artifact_id: &str,
) -> Result<Option<StoredArtifact>, ContractError> {
    let row = conn
        .query_row(
            "SELECT artifact_id, project_id, media_type, byte_length, sha256, revision, display_name, created_at
             FROM artifacts WHERE artifact_id = ?1",
            params![artifact_id],
            |r| {
                Ok(StoredArtifact {
                    artifact_id: r.get(0)?,
                    project_id: r.get(1)?,
                    media_type: r.get(2)?,
                    byte_length: r.get(3)?,
                    sha256: r.get(4)?,
                    revision: r.get(5)?,
                    display_name: r.get(6)?,
                    created_at: r.get(7)?,
                })
            },
        )
        .optional()
        .map_err(sql_err)?;
    Ok(row)
}

/// 在已持有的 SQLite 连接/事务内查询产物 JSON（供 uploads.complete 重放等场景）。
pub(crate) fn artifact_json_in_tx(
    conn: &Connection,
    artifact_id: &str,
) -> Result<Value, ContractError> {
    let row =
        fetch_artifact_row(conn, artifact_id)?.ok_or_else(|| artifact_not_found(artifact_id))?;
    Ok(row.to_json())
}

/// 元信息查询（合同 `Artifact` JSON 同构）。
pub(crate) fn artifact_json(store: &V1Store, artifact_id: &str) -> Result<Value, ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    artifact_json_in_tx(&conn, artifact_id)
}

/// 打开产物字节文件（pinned `artifacts_dir` + `O_NOFOLLOW | O_NONBLOCK` + `fstat` 核实普通文件）。
/// 元数据行存在但字节文件缺失 → ARTIFACT_NOT_READY。
pub(crate) fn open_artifact_file(
    store: &V1Store,
    artifact_id: &str,
) -> Result<File, ContractError> {
    validate_artifact_leaf_name(artifact_id)?;
    let dir = artifacts_dir(store);
    let not_ready = || {
        ContractError::new(
            CODE_ARTIFACT_NOT_READY,
            format!("artifact `{artifact_id}` bytes are not published yet"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::NotApplied,
        )
    };
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, statat};
        let dir_fd = match crate::project_files::open_directory_beneath(store.home(), &dir) {
            Ok(fd) => fd,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(not_ready()),
            Err(err) if crate::project_files::nofollow_loop(&err) => {
                return Err(ContractError::new(
                    "PATH_ESCAPE",
                    format!("artifacts directory symlink refused: {err}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            Err(err) => return Err(io_unavailable(err)),
        };
        let fd = match openat(
            &dir_fd,
            artifact_id,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Err(not_ready()),
            Err(err) => {
                if matches!(err, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
                    && let Ok(stat) = statat(&dir_fd, artifact_id, AtFlags::SYMLINK_NOFOLLOW)
                    && FileType::from_raw_mode(stat.st_mode) == FileType::Symlink
                {
                    return Err(ContractError::new(
                        "PATH_ESCAPE",
                        format!("artifact `{artifact_id}` is a symlink (NOFOLLOW refused)"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                return Err(io_unavailable(err.into()));
            }
        };
        let file = File::from(fd);
        let stat = fstat(&file).map_err(|err| io_unavailable(err.into()))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!("artifact `{artifact_id}` is not a regular file"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = &dir;
        let path = artifact_path(store, artifact_id);
        let file = File::open(&path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                not_ready()
            } else {
                io_unavailable(err)
            }
        })?;
        let meta = file.metadata().map_err(io_unavailable)?;
        if !meta.is_file() {
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!("artifact `{artifact_id}` is not a regular file"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Ok(file)
    }
}

/// 带字节上限地读取产物字节：先验 SQLite 行大小再验文件 `fstat` 大小，最后仅读
/// `.take(max_bytes + 1)`，防止对 2 GiB 上传产物整读入内存触发 OOM DoS（P0#4）。
pub(crate) fn read_artifact_bytes_bounded(
    store: &V1Store,
    artifact_id: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, ContractError> {
    let row = {
        let conn = store.conn.lock().expect("sqlite mutex");
        fetch_artifact_row(&conn, artifact_id)?.ok_or_else(|| artifact_not_found(artifact_id))?
    };
    if row.byte_length < 0 || row.byte_length as u64 > max_bytes {
        return Err(ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!(
                "artifact `{artifact_id}` exceeds maximum allowed read limit of {max_bytes} bytes ({} bytes)",
                row.byte_length
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let mut file = open_artifact_file(store, artifact_id)?;
    let meta = file.metadata().map_err(io_unavailable)?;
    if meta.len() > max_bytes {
        return Err(ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!(
                "artifact `{artifact_id}` exceeds maximum allowed read limit of {max_bytes} bytes ({} bytes)",
                meta.len()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let mut bytes = Vec::with_capacity(meta.len().min(max_bytes) as usize);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_unavailable)?;
    if bytes.len() as u64 > max_bytes {
        return Err(ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!(
                "artifact `{artifact_id}` exceeds maximum allowed read limit of {max_bytes} bytes"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(bytes)
}

/// 小产物整读（recovery 备份、预览输入等）。大文件用 [`copy_artifact_into`]。
pub(crate) fn read_artifact_bytes(
    store: &V1Store,
    artifact_id: &str,
) -> Result<Vec<u8>, ContractError> {
    read_artifact_bytes_bounded(store, artifact_id, MAX_ARTIFACT_BYTES)
}

/// 流式拷出（不整载内存）。返回拷出字节数；同时给出流式 sha256 校验。
pub(crate) fn copy_artifact_into(
    store: &V1Store,
    artifact_id: &str,
    out: &mut dyn Write,
) -> Result<(u64, String), ContractError> {
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        fetch_artifact_row(&conn, artifact_id)?.ok_or_else(|| artifact_not_found(artifact_id))?;
    }
    let mut file = open_artifact_file(store, artifact_id)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = file.read(&mut buffer).map_err(io_unavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read]).map_err(io_unavailable)?;
        total += read as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

/// 范围读取：返回 (窗口字节, 总长度)。`offset` 超过总长度 → RANGE_INVALID
/// （合同验收：范围越界明确，不伪装空读）。
pub(crate) fn read_artifact_range(
    store: &V1Store,
    artifact_id: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<(Vec<u8>, u64), ContractError> {
    let row = {
        let conn = store.conn.lock().expect("sqlite mutex");
        fetch_artifact_row(&conn, artifact_id)?.ok_or_else(|| artifact_not_found(artifact_id))?
    };
    let mut file = open_artifact_file(store, artifact_id)?;
    let total = row.byte_length.max(0) as u64;
    if offset > total {
        return Err(ContractError::new(
            CODE_RANGE_INVALID,
            format!(
                "offset {offset} is beyond the artifact length {total}; this is not an empty read"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(io_unavailable)?;
    let want = (max_bytes as u64).min(total - offset) as usize;
    let mut window = vec![0u8; want];
    file.read_exact(&mut window).map_err(io_unavailable)?;
    Ok((window, total))
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn artifacts_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let artifact_id = input["artifactId"].as_str().unwrap_or_default().to_owned();
    let artifact = tokio::task::spawn_blocking(move || artifact_json(rt.store(), &artifact_id))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, json!({"artifact": artifact})))
}

async fn artifacts_read(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let artifact_id = input["artifactId"].as_str().unwrap_or_default().to_owned();
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let max_bytes = input
        .get("maxBytes")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(FILE_CHUNK_MAX_BYTES as u64) as usize;

    let (artifact, chunk) = tokio::task::spawn_blocking(move || {
        let artifact = artifact_json(&store, &artifact_id)?;
        let (window, total) = read_artifact_range(&store, &artifact_id, offset, max_bytes)?;
        let byte_length = window.len();
        let eof = offset.saturating_add(byte_length as u64) >= total;
        let chunk = json!({
            "bytesBase64": base64::engine::general_purpose::STANDARD.encode(&window),
            "offset": offset,
            "byteLength": byte_length,
            "totalByteLength": total,
            "revision": artifact["revision"].clone(),
            "eof": eof,
        });
        Ok::<_, ContractError>((artifact, chunk))
    })
    .await
    .map_err(join_err)?
    .map_err(DispatchError::Application)?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"artifact": artifact, "chunk": chunk}),
    ))
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("artifacts worker task failed: {err}"),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::domains::files::sha256_hex;
    use crate::public_v1::runtime::PublicV1Runtime;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;

    fn open_runtime(root_dir: &std::path::Path) -> Arc<PublicV1Runtime> {
        let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(root_dir, meta).unwrap();
        PublicV1Runtime::new(store)
    }

    #[tokio::test]
    async fn publish_get_read_roundtrip_and_range_invalid() {
        let tmp = TempDir::new("artifacts-round");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        let store = rt.store();

        let bytes: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        let artifact = publish_artifact(
            store,
            "proj-a",
            "application/octet-stream",
            Some("fixture.bin"),
            &bytes,
            "upload",
        )
        .unwrap();
        let artifact_id = artifact["artifactId"].as_str().unwrap().to_owned();
        assert_eq!(artifact["byteLength"], json!(1024));
        assert_eq!(artifact["sha256"], json!(sha256_hex(&bytes)));

        let (_, got) = rt
            .dispatch(
                "artifacts.get",
                json!({"artifactId": artifact_id}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("artifacts.get", &got)
            .unwrap();
        assert_eq!(got["artifact"], artifact);

        // 分块读取 + 越界。
        let (_, page) = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": artifact_id, "offset": 1000, "maxBytes": 100}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("artifacts.read", &page)
            .unwrap();
        assert_eq!(page["chunk"]["offset"], json!(1000));
        assert_eq!(page["chunk"]["byteLength"], json!(24));
        assert_eq!(page["chunk"]["totalByteLength"], json!(1024));
        assert_eq!(page["chunk"]["eof"], json!(true));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(page["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, &bytes[1000..]);

        let err = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": artifact_id, "offset": 4096, "maxBytes": 10}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, "RANGE_INVALID"),
            other => panic!("expected application error, got {other:?}"),
        }

        // 未知 artifact → NOT_FOUND。
        let err = rt
            .dispatch(
                "artifacts.get",
                json!({"artifactId": "missing"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, "NOT_FOUND"),
            other => panic!("expected application error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bounded_read_and_nofollow_fifo_fail_closed_and_display_name_omitted() {
        let tmp = TempDir::new("artifacts-hardened");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        let store = rt.store();

        // displayName=None 时省略字段（schema 非可空 string）。
        let no_name = publish_artifact(
            store,
            "proj-a",
            "application/octet-stream",
            None,
            b"abcdef",
            "upload",
        )
        .unwrap();
        assert!(no_name.get("displayName").is_none());
        let id = no_name["artifactId"].as_str().unwrap().to_owned();
        let (_, got) = rt
            .dispatch("artifacts.get", json!({"artifactId": id}), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("artifacts.get", &got)
            .unwrap();

        // read_artifact_bytes_bounded 按行元数据及实际流双层拦截超限读取。
        let over_err = read_artifact_bytes_bounded(store, &id, 5).unwrap_err();
        assert_eq!(over_err.code, CODE_RESOURCE_LIMIT);
        assert_eq!(
            read_artifact_bytes_bounded(store, &id, 6).unwrap(),
            b"abcdef"
        );

        // 即使 DB 行被篡改写小，实际文件超限也会被 .take(max+1) 拦截。
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE artifacts SET byte_length = 2 WHERE artifact_id = ?1",
                params![id],
            )
            .unwrap();
        }
        let lied_err = read_artifact_bytes_bounded(store, &id, 4).unwrap_err();
        assert_eq!(lied_err.code, CODE_RESOURCE_LIMIT);

        // artifacts 目录内的符号链接与 FIFO 必须 fail-closed，绝不越界或挂死。
        #[cfg(unix)]
        {
            let outside = tmp.0.join("outside.txt");
            std::fs::write(&outside, b"secret").unwrap();
            let art_dir = artifacts_dir(store);

            let link_id = "art-symlink";
            std::os::unix::fs::symlink(&outside, art_dir.join(link_id)).unwrap();
            let sym_err = open_artifact_file(store, link_id).unwrap_err();
            assert_eq!(sym_err.code, "PATH_ESCAPE");

            let fifo_id = "art-fifo";
            let status = std::process::Command::new("mkfifo")
                .arg(art_dir.join(fifo_id))
                .status()
                .expect("mkfifo");
            assert!(status.success());
            let fifo_err = open_artifact_file(store, fifo_id).unwrap_err();
            assert_eq!(fifo_err.code, CODE_UNAVAILABLE);
        }
    }
}
