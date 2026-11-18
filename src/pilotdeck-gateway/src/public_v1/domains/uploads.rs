//! `uploads.*` — 0 起始乱序分块上传与不可变产物发布（t013，S3）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `uploads.*` 条目（行
//! 33436–34783）。协议与旧线 `upload/`（TS UploadStore.ts 对拍，多文件
//! manifest / clientFileId / HTTP multipart）**不同**，不复用其协议层；可
//! 复用的是语义：终态竞态「门内 CAS + 单一终态 + 拒绝后不写」（mod.rs
//! #1631/#2275）、过期清理与孤儿回收（mod.rs cleanup_expired /
//! reclaim_orphan）、删除前根/链接防护（mod.rs:470–498）。
//!
//! 协议（合同 §2）：create 声明总 byteLength + 整文件 sha256，服务端定
//! partSize（512 KiB，与 1 MiB 帧预算联动）；part 按 partNumber（从 0 起）
//! 乱序提交，同块同字节 deduplicated、异字节 PART_CONFLICT；complete 校验
//! 块齐全 + 按编号拼装后整文件 sha256 == expectedSha256，通过才生成不可
//! 变 artifact；cancel 只清本 upload 临时块。
//!
//! **热路径不进 SQLite**（brief §5.3，P2 装置的前提）：uploads.part 的状态
//! 读走进程内 meta 缓存（create/complete/cancel 维护），分块确认只落盘
//! （`part-<N>` 文件 + append-only `parts.log`，各 fsync 后才 ack）。SQLite
//! 只在状态迁移（create/complete/cancel/过期）时写。断点续传以磁盘为准。
//!
//! uploads.complete 是 `execution=async`：受理即返回公共 operation receipt
//! （业务结果在 completionResult{upload, artifact}，经 `uploads.get` /
//! operations.get 查询；完成响应丢失重查得到同一产物）。异步操作记录落在
//! `async_operations` 表（collections/t009 `operations.get` 的数据面，形状
//! 对齐其合同的 Operation 对象）。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_LIMIT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use base64::Engine;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

pub(crate) const CODE_PART_CONFLICT: &str = "PART_CONFLICT";
pub(crate) const CODE_CHECKSUM_MISMATCH: &str = "CHECKSUM_MISMATCH";
pub(crate) const CODE_UPLOAD_CLOSED: &str = "UPLOAD_CLOSED";
pub(crate) const CODE_MISSING_PARTS: &str = "MISSING_PARTS";

/// 单块字节上限。**必须**与 ws.rs 的 1 MiB 帧预算联动（007 §6.1）：512 KiB
/// base64 膨胀后 ~683 KiB，留 JSON 字段余量（brief §3.3）。
pub(crate) const MAX_PART_BYTES: u64 = 512 * 1024;
/// 产物总字节上限（有效配置，而非旧 HTTP 固定 50 MiB；验收 ops.json:33436）。
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 每项目并发在途上传上限（沿用旧线 mod.rs:196-202 的 3）。
pub(crate) const MAX_CONCURRENT_UPLOADS: i64 = 3;
/// 未完成上传保留期（TS uploads.js:73-76 的 15 分钟）。
pub(crate) const UPLOAD_TTL_SECONDS: i64 = 15 * 60;
/// 未限制媒体类型时的声明值。
pub(crate) const ALLOWED_MEDIA_TYPES: &[&str] = &["*/*"];

const UPLOADS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS uploads (
    upload_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    state TEXT NOT NULL,
    byte_length INTEGER NOT NULL,
    part_size INTEGER NOT NULL,
    total_sha256 TEXT NOT NULL,
    media_type TEXT NOT NULL,
    display_name TEXT NOT NULL,
    artifact_id TEXT,
    revision TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const UPLOAD_PARTS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS upload_parts (
    upload_id TEXT NOT NULL,
    part_number INTEGER NOT NULL,
    byte_length INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (upload_id, part_number)
)";
/// 异步操作记录（uploads.complete / documents.preview 的公共 receipt 数据
/// 面；`operations.get`（collections 域）按其合同 Operation 对象读取）。
const ASYNC_OPERATIONS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS async_operations (
    operation_id TEXT PRIMARY KEY,
    method_id TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    state TEXT NOT NULL,
    revision TEXT NOT NULL,
    result_json TEXT,
    error_code TEXT,
    error_message TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("uploads", UPLOADS_TABLE_DDL);
    reg.schema("upload_parts", UPLOAD_PARTS_TABLE_DDL);
    reg.schema("async_operations", ASYNC_OPERATIONS_TABLE_DDL);
    reg.handler("uploads.limits", |rt, input, _key, _rev| {
        Box::pin(uploads_limits(rt, input))
    });
    reg.handler("uploads.create", |rt, input, key, _rev| {
        Box::pin(uploads_create(rt, input, key))
    });
    reg.handler("uploads.get", |rt, input, _key, _rev| {
        Box::pin(uploads_get(rt, input))
    });
    reg.handler("uploads.part", |rt, input, _key, _rev| {
        Box::pin(uploads_part(rt, input))
    });
    reg.handler("uploads.complete", |rt, input, key, _rev| {
        Box::pin(uploads_complete(rt, input, key))
    });
    reg.handler("uploads.cancel", |rt, input, _key, _rev| {
        Box::pin(uploads_cancel(rt, input))
    });
}

// ---------------------------------------------------------------------------
// 存储布局与行类型
// ---------------------------------------------------------------------------

fn uploads_root(store: &V1Store) -> PathBuf {
    store.home().join("v1").join("uploads")
}

fn upload_dir(store: &V1Store, upload_id: &str) -> PathBuf {
    uploads_root(store).join(upload_id)
}

fn part_path(dir: &PathBuf, part_number: u64) -> PathBuf {
    dir.join(format!("part-{part_number}"))
}

fn parts_log_path(dir: &PathBuf) -> PathBuf {
    dir.join("parts.log")
}

#[derive(Clone)]
struct UploadRow {
    upload_id: String,
    project_id: String,
    state: String,
    byte_length: i64,
    part_size: i64,
    total_sha256: String,
    media_type: String,
    display_name: String,
    artifact_id: Option<String>,
    revision: String,
    expires_at: String,
}

const UPLOAD_ROW_COLUMNS: &str =
    "upload_id, project_id, state, byte_length, part_size, total_sha256,
                                  media_type, display_name, artifact_id, revision, expires_at";

fn map_upload_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UploadRow> {
    Ok(UploadRow {
        upload_id: r.get(0)?,
        project_id: r.get(1)?,
        state: r.get(2)?,
        byte_length: r.get(3)?,
        part_size: r.get(4)?,
        total_sha256: r.get(5)?,
        media_type: r.get(6)?,
        display_name: r.get(7)?,
        artifact_id: r.get(8)?,
        revision: r.get(9)?,
        expires_at: r.get(10)?,
    })
}

fn fetch_upload_row(
    conn: &Connection,
    upload_id: &str,
) -> Result<Option<UploadRow>, ContractError> {
    let sql = format!("SELECT {UPLOAD_ROW_COLUMNS} FROM uploads WHERE upload_id = ?1");
    let row = conn
        .query_row(&sql, params![upload_id], map_upload_row)
        .optional()
        .map_err(sql_err)?;
    Ok(row)
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
        format!("upload storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn upload_not_found(upload_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("upload `{upload_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn total_parts(byte_length: i64, part_size: i64) -> u64 {
    if byte_length <= 0 || part_size <= 0 {
        0
    } else {
        ((byte_length as u64) + (part_size as u64) - 1) / (part_size as u64)
    }
}

fn expected_part_len(byte_length: i64, part_size: i64, part_number: u64) -> u64 {
    let total = byte_length.max(0) as u64;
    let size = part_size.max(1) as u64;
    let remaining = total.saturating_sub(part_number * size);
    remaining.min(size)
}

/// 读取 parts.log（磁盘为准的确认块清单），按 partNumber 排序。
fn read_parts_log(dir: &PathBuf) -> Vec<(u64, u64, String)> {
    let mut parts: Vec<(u64, u64, String)> = Vec::new();
    let Ok(file) = File::open(parts_log_path(dir)) else {
        return parts;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let number = value.get("partNumber").and_then(Value::as_u64);
        let length = value.get("byteLength").and_then(Value::as_u64);
        let sha = value.get("sha256").and_then(Value::as_str);
        if let (Some(number), Some(length), Some(sha)) = (number, length, sha) {
            parts.push((number, length, sha.to_owned()));
        }
    }
    parts.sort_by_key(|(number, _, _)| *number);
    parts
}

fn upload_to_json(conn: &Connection, store: &V1Store, row: &UploadRow) -> Value {
    let dir = upload_dir(store, &row.upload_id);
    let mut parts = read_parts_log(&dir);
    if parts.is_empty() && !dir.is_dir() {
        // 终态（complete）后临时目录已清：分块审计行兜底（complete 时落
        // `upload_parts`，不占 part 热路径）。
        if let Ok(mut stmt) = conn.prepare(
            "SELECT part_number, byte_length, sha256 FROM upload_parts
             WHERE upload_id = ?1 ORDER BY part_number ASC",
        ) {
            if let Ok(rows) = stmt
                .query_map(params![row.upload_id], |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, String>(2)?,
                    ))
                })
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            {
                parts = rows;
            }
        }
    }
    let received: Vec<Value> = parts
        .into_iter()
        .map(|(number, length, sha)| {
            json!({"partNumber": number, "byteLength": length, "sha256": sha})
        })
        .collect();
    json!({
        "uploadId": row.upload_id,
        "projectId": row.project_id,
        "state": row.state,
        "byteLength": row.byte_length.max(0),
        "receivedParts": received,
        "partSize": row.part_size.max(1),
        "expiresAt": row.expires_at,
        "artifactId": row.artifact_id,
        "revision": row.revision,
    })
}

// ---------------------------------------------------------------------------
// 热路径 meta 缓存（uploads.part 不进 SQLite；P2 前提）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct UploadMeta {
    state: String,
    byte_length: i64,
    part_size: i64,
}

fn meta_cache() -> &'static RwLock<HashMap<String, UploadMeta>> {
    static CACHE: OnceLock<RwLock<HashMap<String, UploadMeta>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn cache_meta(row: &UploadRow) {
    meta_cache().write().expect("upload meta cache").insert(
        row.upload_id.clone(),
        UploadMeta {
            state: row.state.clone(),
            byte_length: row.byte_length,
            part_size: row.part_size,
        },
    );
}

fn is_expired(row: &UploadRow) -> bool {
    chrono::DateTime::parse_from_rfc3339(&row.expires_at)
        .map(|expires| Utc::now() > expires.with_timezone(&Utc))
        .unwrap_or(false)
}

/// 过期惰性评估：`receiving/validating` 且过 expiresAt → CAS 到 expired +
/// 删除临时目录。单语句 CAS，不新开事务（可在已有事务内调用）。
fn expire_row_if_past(
    store: &V1Store,
    conn: &Connection,
    row: &UploadRow,
) -> Result<UploadRow, ContractError> {
    if !is_expired(row) || (row.state != "receiving" && row.state != "validating") {
        return Ok(row.clone());
    }
    let updated = conn
        .execute(
            "UPDATE uploads SET state = 'expired', revision = ?1, updated_at = ?2
             WHERE upload_id = ?3 AND state = ?4",
            params![next_rev(conn)?, now_rfc3339(), row.upload_id, row.state],
        )
        .map_err(sql_err)?;
    if updated == 1 {
        let _ = std::fs::remove_dir_all(upload_dir(store, &row.upload_id));
    }
    let refreshed =
        fetch_upload_row(conn, &row.upload_id)?.ok_or_else(|| upload_not_found(&row.upload_id))?;
    cache_meta(&refreshed);
    Ok(refreshed)
}

/// 下一 revision（镜像 store.rs::next_rev_in_tx 的 meta 计数器，私有时同型复制）。
fn next_rev(conn: &Connection) -> Result<String, ContractError> {
    let current: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .unwrap_or_else(|| "1".to_owned());
    let next: i64 = current.parse().unwrap_or(1) + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("rev-{next:08}"))
}

// ---------------------------------------------------------------------------
// 事件与异步操作记录（uploads / documents 共用）
// ---------------------------------------------------------------------------

/// 追加一条持久事件（镜像 store.rs::append_event_in_tx 的两步 cursor 赋值）。
pub(crate) fn append_domain_event(
    store: &V1Store,
    resource_id: &str,
    event_type: &str,
    schema_id: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    append_domain_event_in_tx(&conn, resource_id, event_type, schema_id, payload)
}

pub(crate) fn append_domain_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    event_type: &str,
    schema_id: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    let placeholder = format!("tmp-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7)",
        params![placeholder, resource_id, event_type, schema_id, payload_str, payload_str, now_rfc3339()],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = crate::public_v1::store::format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 登记一条异步操作（receipt 数据面）。`operation_id` 由调用方先经
/// `store.next_id()` 分配。返回 revision。
pub(crate) fn create_async_operation(
    conn: &Connection,
    method_id: &str,
    resource_id: &str,
    operation_id: &str,
) -> Result<String, ContractError> {
    let revision = next_rev(conn)?;
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO async_operations (operation_id, method_id, resource_id, state, revision, result_json, error_code, error_message, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'running', ?4, NULL, NULL, NULL, ?5, ?6)",
        params![operation_id, method_id, resource_id, revision, now, now],
    )
    .map_err(sql_err)?;
    Ok(revision)
}

/// 终态化一条异步操作：`state` ∈ succeeded|failed|cancelled，成功带
/// completionResult，失败带 error。
pub(crate) fn finish_async_operation(
    store: &V1Store,
    operation_id: &str,
    state: &str,
    completion_result: Option<&Value>,
    error: Option<(&str, &str)>,
) -> Result<(), ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let result_str = completion_result.map(|v| serde_json::to_string(v).unwrap_or_default());
    conn.execute(
        "UPDATE async_operations SET state = ?1, result_json = ?2, error_code = ?3, error_message = ?4, updated_at = ?5
         WHERE operation_id = ?6",
        params![
            state,
            result_str,
            error.map(|(code, _)| code),
            error.map(|(_, message)| message),
            now_rfc3339(),
            operation_id,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 幂等记录（受理面）：复用 `idempotency_records`（与 run.submit 同表同义）。
fn record_accepted_in_tx(
    conn: &Connection,
    key: &str,
    method_id: &str,
    digest: &str,
    resource: &Value,
    receipt: &Value,
) -> Result<(), ContractError> {
    let now = now_rfc3339();
    let retained = (Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')",
        params![
            key,
            method_id,
            digest,
            serde_json::to_string(resource).unwrap_or_default(),
            serde_json::to_string(receipt).unwrap_or_default(),
            now,
            retained,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// GC（语义移植 upload/mod.rs cleanup_expired / reclaim_orphan）
// ---------------------------------------------------------------------------

/// 机会式清扫（uploads.create 时触发，无独立定时任务——常驻任务的挂载点
/// 在公共路径，留给集成者；见报告未决问题）：
/// 1. 过期未完成上传 → expired + 删目录；
/// 2. cancelled/failed/expired 行的残留目录 → 删除（终态临时字节应已清）；
/// 3. 无行孤儿目录（崩溃碎片）且 mtime 超过 TTL → 回收。
fn sweep_uploads(store: &V1Store) {
    let rows: Vec<UploadRow> = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let sql = format!("SELECT {UPLOAD_ROW_COLUMNS} FROM uploads");
        let Ok(rows) = conn.prepare(&sql).and_then(|mut stmt| {
            stmt.query_map([], map_upload_row)
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        }) else {
            return;
        };
        rows
    };
    let by_id: HashMap<String, UploadRow> = rows
        .into_iter()
        .map(|row| (row.upload_id.clone(), row))
        .collect();

    let Ok(entries) = std::fs::read_dir(uploads_root(store)) else {
        return;
    };
    let conn = store.conn.lock().expect("sqlite mutex");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // 暂存文件（.assemble-*），归在途装配管。
        }
        let dir = entry.path();
        let meta = std::fs::symlink_metadata(&dir);
        let is_plain_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        if !is_plain_dir {
            continue; // 非目录（或被换成链接）一律不碰（防链接删除）。
        }
        match by_id.get(&name) {
            Some(row) => {
                let terminal_or_expired =
                    matches!(row.state.as_str(), "cancelled" | "failed" | "expired");
                if terminal_or_expired {
                    let _ = std::fs::remove_dir_all(&dir);
                } else {
                    let _ = expire_row_if_past(store, &conn, row);
                }
            }
            None => {
                // 孤儿：无行且 mtime 早于 TTL（create 先写行再建目录，超过
                // 一个 TTL 仍无行即崩溃碎片）。
                let old = meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|mtime| mtime.elapsed().ok())
                    .map(|age| age.as_secs() > UPLOAD_TTL_SECONDS as u64)
                    .unwrap_or(false);
                if old {
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn uploads_limits(
    _rt: Arc<PublicV1Runtime>,
    _input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "maxArtifactBytes": MAX_ARTIFACT_BYTES,
            "maxPartBytes": MAX_PART_BYTES,
            "maxConcurrentUploads": MAX_CONCURRENT_UPLOADS,
            "uploadTtlSeconds": UPLOAD_TTL_SECONDS,
            "allowedMediaTypes": ALLOWED_MEDIA_TYPES,
        }),
    ))
}

async fn uploads_create(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let display_name = input["displayName"].as_str().unwrap_or_default().to_owned();
    let media_type = input["mediaType"].as_str().unwrap_or_default().to_owned();
    let byte_length = input["byteLength"].as_u64().unwrap_or(0);
    let sha256 = input["sha256"].as_str().unwrap_or_default().to_owned();

    // 结构校验（schema 之外的合同语义）。
    if media_type.is_empty() {
        return Err(DispatchError::Application(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "mediaType must not be empty",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DispatchError::Application(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "sha256 must be a 64-character hex digest of the whole file",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    if byte_length > MAX_ARTIFACT_BYTES {
        let mut err = ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!("byteLength {byte_length} exceeds maxArtifactBytes {MAX_ARTIFACT_BYTES}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
        err.details = Some(serde_json::Map::from_iter([(
            "maxArtifactBytes".to_owned(),
            json!(MAX_ARTIFACT_BYTES),
        )]));
        return Err(DispatchError::Application(err));
    }

    let outcome = tokio::task::spawn_blocking(move || {
        create_upload(
            &store,
            &project_id,
            &display_name,
            &media_type,
            byte_length,
            &sha256,
            idempotency_key.as_deref(),
        )
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

#[allow(clippy::too_many_arguments)]
fn create_upload(
    store: &Arc<V1Store>,
    project_id: &str,
    display_name: &str,
    media_type: &str,
    byte_length: u64,
    sha256: &str,
    idempotency_key: Option<&str>,
) -> Result<Value, ContractError> {
    // 机会式清扫（承接 GC 职责；无行孤儿 + 过期上传）。
    sweep_uploads(store);

    let digest = compute_input_digest(
        "uploads.create",
        &json!({
            "projectId": project_id,
            "displayName": display_name,
            "mediaType": media_type,
            "byteLength": byte_length,
            "sha256": sha256,
        }),
    );

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;

    // 项目存在性。
    let project: Option<String> = tx
        .query_row(
            "SELECT project_id FROM projects WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if project.is_none() {
        return Err(ContractError::new(
            CODE_NOT_FOUND,
            format!("project `{project_id}` not found"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    // 幂等：同 key 同内容（digest）→ 同 uploadId；异内容 → 冲突。
    if let Some(key) = idempotency_key {
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT input_digest, resource_json FROM idempotency_records
                 WHERE idempotency_key = ?1 AND method_id = 'uploads.create' AND request_state = 'accepted'",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((stored_digest, resource_raw)) = existing {
            if stored_digest != digest {
                return Err(ContractError::new(
                    CODE_IDEMPOTENCY_CONFLICT,
                    format!(
                        "idempotencyKey `{key}` already used for uploads.create with different input"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if let Some(upload_id) =
                serde_json::from_str::<Value>(&resource_raw)
                    .ok()
                    .and_then(|v| {
                        v.get("uploadId")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                    })
            {
                let row = fetch_upload_row(&tx, &upload_id)?
                    .ok_or_else(|| upload_not_found(&upload_id))?;
                // 惰性过期复核（同事务内 CAS）。
                let row = expire_row_if_past(store, &tx, &row)?;
                return Ok(json!({"upload": upload_to_json(&tx, store, &row)}));
            }
        }
    }

    // 并发上限（同项目在途上传计数）。
    let active: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM uploads WHERE project_id = ?1 AND state IN ('receiving', 'validating')",
            params![project_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if active >= MAX_CONCURRENT_UPLOADS {
        let mut err = ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!(
                "project `{project_id}` already has {active} concurrent uploads (limit {MAX_CONCURRENT_UPLOADS})"
            ),
            RetrySpec::backoff_read(Some(500)),
            EffectStatus::NotApplied,
        );
        err.details = Some(serde_json::Map::from_iter([(
            "maxConcurrentUploads".to_owned(),
            json!(MAX_CONCURRENT_UPLOADS),
        )]));
        return Err(err);
    }

    let upload_id = store.next_id();
    let revision = next_rev(&tx)?;
    let part_size = if byte_length == 0 {
        MAX_PART_BYTES
    } else {
        MAX_PART_BYTES.min(byte_length)
    };
    let now = now_rfc3339();
    let expires_at = (Utc::now() + chrono::Duration::seconds(UPLOAD_TTL_SECONDS)).to_rfc3339();
    tx.execute(
        "INSERT INTO uploads (upload_id, project_id, state, byte_length, part_size, total_sha256, media_type, display_name, artifact_id, revision, expires_at, created_at, updated_at)
         VALUES (?1, ?2, 'receiving', ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, ?10, ?11)",
        params![
            upload_id,
            project_id,
            byte_length as i64,
            part_size as i64,
            sha256,
            media_type,
            display_name,
            revision,
            expires_at,
            now,
            now,
        ],
    )
    .map_err(sql_err)?;

    if let Some(key) = idempotency_key {
        record_accepted_in_tx(
            &tx,
            key,
            "uploads.create",
            &digest,
            &json!({"uploadId": upload_id}),
            &json!({"upload": {"uploadId": upload_id}}),
        )?;
    }
    tx.commit().map_err(sql_err)?;

    // 目录创建（行先落，目录后建：孤儿回收按「无行且超龄」判定）。
    std::fs::create_dir_all(upload_dir(store, &upload_id)).map_err(io_unavailable)?;
    let row = UploadRow {
        upload_id: upload_id.clone(),
        project_id: project_id.to_owned(),
        state: "receiving".to_owned(),
        byte_length: byte_length as i64,
        part_size: part_size as i64,
        total_sha256: sha256.to_owned(),
        media_type: media_type.to_owned(),
        display_name: display_name.to_owned(),
        artifact_id: None,
        revision,
        expires_at,
    };
    cache_meta(&row);
    Ok(json!({"upload": upload_to_json(&conn, store, &row)}))
}

async fn uploads_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let upload_id = input["uploadId"].as_str().unwrap_or_default().to_owned();
    let upload = tokio::task::spawn_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let row =
            fetch_upload_row(&conn, &upload_id)?.ok_or_else(|| upload_not_found(&upload_id))?;
        let row = expire_row_if_past(&store, &conn, &row)?;
        Ok::<_, ContractError>(upload_to_json(&conn, &store, &row))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, json!({"upload": upload})))
}

async fn uploads_part(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let upload_id = input["uploadId"].as_str().unwrap_or_default().to_owned();
    let part_number = input["partNumber"].as_u64().unwrap_or(0);
    let sha256 = input["sha256"].as_str().unwrap_or_default().to_owned();
    let bytes = input["bytesBase64"].as_str().unwrap_or_default().to_owned();

    // 热路径：无 SQLite 写。meta 缓存命中时连读也省掉。
    let (state, byte_length, part_size) = {
        let cache = meta_cache().read().expect("upload meta cache");
        if let Some(meta) = cache.get(&upload_id) {
            (meta.state.clone(), meta.byte_length, meta.part_size)
        } else {
            drop(cache);
            let conn = store.conn.lock().expect("sqlite mutex");
            let row =
                fetch_upload_row(&conn, &upload_id)?.ok_or_else(|| upload_not_found(&upload_id))?;
            cache_meta(&row);
            (row.state, row.byte_length, row.part_size)
        }
    };
    if state != "receiving" {
        return Err(DispatchError::Application(ContractError::new(
            CODE_UPLOAD_CLOSED,
            format!("upload `{upload_id}` is `{state}`; parts are no longer accepted"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    let total = total_parts(byte_length, part_size);
    if part_number >= total {
        return Err(DispatchError::Application(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!(
                "partNumber {part_number} is out of range (upload declares {total} parts of {part_size} bytes)"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&bytes)
        .map_err(|e| {
            DispatchError::Application(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("bytesBase64 decode failed: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        })?;
    let expected_len = expected_part_len(byte_length, part_size, part_number) as usize;
    if decoded.len() != expected_len {
        return Err(DispatchError::Application(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!(
                "part {part_number} must be exactly {expected_len} bytes (got {})",
                decoded.len()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    let actual_sha = {
        let mut hasher = Sha256::new();
        hasher.update(&decoded);
        format!("{:x}", hasher.finalize())
    };
    if actual_sha != sha256 {
        return Err(DispatchError::Application(ContractError::new(
            CODE_CHECKSUM_MISMATCH,
            format!(
                "part {part_number} sha256 `{sha256}` does not match received bytes (`{actual_sha}`)"
            ),
            RetrySpec::same_key(None),
            EffectStatus::NotApplied,
        )));
    }

    let dir = upload_dir(&store, &upload_id);
    let outcome = tokio::task::spawn_blocking(move || {
        accept_part(&dir, part_number, expected_len, &decoded, &actual_sha)
    })
    .await
    .map_err(join_err)??;
    let (deduplicated, accepted_bytes) = outcome;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "uploadId": upload_id,
            "partNumber": part_number,
            "sha256": sha256,
            "acceptedBytes": accepted_bytes,
            "deduplicated": deduplicated,
        }),
    ))
}

/// 落盘一块：已存在同块同字节 → deduplicated；异字节 → PART_CONFLICT；否则
/// 原子写 part 文件 + append parts.log，各自 fsync 后才确认。
fn accept_part(
    dir: &PathBuf,
    part_number: u64,
    expected_len: usize,
    decoded: &[u8],
    actual_sha: &str,
) -> Result<(bool, usize), ContractError> {
    let target = part_path(dir, part_number);
    if let Ok(existing) = std::fs::read(&target) {
        if existing.len() == decoded.len()
            && existing.iter().zip(decoded.iter()).all(|(a, b)| a == b)
        {
            return Ok((true, existing.len()));
        }
        return Err(ContractError::new(
            CODE_PART_CONFLICT,
            format!(
                "part {part_number} already exists with different bytes; cancel the upload to restart"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    // 临时写 + rename（防止半写块被 complete 拼进产物）。
    let temp = dir.join(format!(".part-{part_number}.tmp"));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io_unavailable)?;
        file.write_all(decoded).map_err(io_unavailable)?;
        file.sync_all().map_err(io_unavailable)?;
    }
    std::fs::rename(&temp, &target).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        io_unavailable(err)
    })?;
    // append-only journal（确认面）。
    {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(parts_log_path(dir))
            .map_err(io_unavailable)?;
        let line = json!({
            "partNumber": part_number,
            "byteLength": expected_len,
            "sha256": actual_sha,
        });
        writeln!(log, "{}", line).map_err(io_unavailable)?;
        log.sync_all().map_err(io_unavailable)?;
    }
    Ok((false, decoded.len()))
}

async fn uploads_complete(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let upload_id = input["uploadId"].as_str().unwrap_or_default().to_owned();
    let expected_sha256 = input["expectedSha256"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DispatchError::Application(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "expectedSha256 must be a 64-character hex digest",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    let complete_digest = compute_input_digest(
        "uploads.complete",
        &json!({"uploadId": upload_id, "expectedSha256": expected_sha256}),
    );

    // 受理（异步）：校验存在性/状态/块齐全 → operation receipt；拼装与整
    // 文件摘要校验在后台任务（completionResult 经 uploads.get / operations
    // .get 查询；完成响应丢失重查得到同一产物）。
    let (operation_id, accepted_revision) = {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;

        if let Some(key) = idempotency_key.as_deref() {
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT input_digest, receipt_json FROM idempotency_records
                     WHERE idempotency_key = ?1 AND method_id = 'uploads.complete' AND request_state = 'accepted'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql_err)?;
            if let Some((stored_digest, receipt_raw)) = existing {
                if stored_digest != complete_digest {
                    return Err(DispatchError::Application(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for uploads.complete with different input"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )));
                }
                if let Ok(mut receipt) = serde_json::from_str::<Value>(&receipt_raw) {
                    if let Some(obj) = receipt.as_object_mut() {
                        obj.insert("deduplicated".into(), json!(true));
                    }
                    return Ok((ResponseOutcome::Accepted, receipt));
                }
            }
        }

        let row = fetch_upload_row(&tx, &upload_id)?.ok_or_else(|| upload_not_found(&upload_id))?;
        if matches!(row.state.as_str(), "cancelled" | "expired" | "failed") {
            return Err(DispatchError::Application(ContractError::new(
                CODE_UPLOAD_CLOSED,
                format!("upload `{upload_id}` is `{}`", row.state),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
        if row.state == "complete" {
            // 已完成：幂等重放同一结果形态（uploads.get 查询同一产物）。
            let operation_id = store.next_id();
            let revision =
                create_async_operation(&tx, "uploads.complete", &upload_id, &operation_id)?;
            let receipt = json!({
                "kind": "operation",
                "operationId": operation_id,
                "durable": true,
                "acceptedRevision": row.revision.clone(),
                "deduplicated": true,
                "state": "succeeded",
            });
            if let Some(key) = idempotency_key.as_deref() {
                record_accepted_in_tx(
                    &tx,
                    key,
                    "uploads.complete",
                    &complete_digest,
                    &json!({"uploadId": upload_id}),
                    &receipt,
                )?;
            }
            tx.commit().map_err(sql_err)?;
            return Ok((ResponseOutcome::Accepted, receipt));
        }

        // 块齐全性（磁盘为准）。
        let dir = upload_dir(&store, &upload_id);
        let parts = read_parts_log(&dir);
        let total = total_parts(row.byte_length, row.part_size);
        let mut received_numbers: Vec<u64> = parts.iter().map(|(n, _, _)| *n).collect();
        received_numbers.sort();
        received_numbers.dedup();
        let all_present = total == received_numbers.len() as u64
            && (0..total).all(|n| received_numbers.binary_search(&n).is_ok());
        if !all_present {
            return Err(DispatchError::Application(ContractError::new(
                CODE_MISSING_PARTS,
                format!(
                    "upload `{upload_id}` is missing parts (has {}/{total})",
                    received_numbers.len()
                ),
                RetrySpec::same_key(None),
                EffectStatus::NotApplied,
            )));
        }

        // 终态竞态门：receiving → validating 的 CAS。输了（被取消）→ UPLOAD_CLOSED。
        let updated = tx
            .execute(
                "UPDATE uploads SET state = 'validating', revision = ?1, updated_at = ?2
                 WHERE upload_id = ?3 AND state = 'receiving'",
                params![next_rev(&tx)?, now_rfc3339(), upload_id],
            )
            .map_err(sql_err)?;
        if updated != 1 {
            let refreshed =
                fetch_upload_row(&tx, &upload_id)?.ok_or_else(|| upload_not_found(&upload_id))?;
            return Err(DispatchError::Application(ContractError::new(
                CODE_UPLOAD_CLOSED,
                format!("upload `{upload_id}` is `{}`", refreshed.state),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }

        let operation_id = store.next_id();
        let revision = create_async_operation(&tx, "uploads.complete", &upload_id, &operation_id)?;
        let receipt = json!({
            "kind": "operation",
            "operationId": operation_id,
            "durable": true,
            "acceptedRevision": revision,
            "deduplicated": false,
            "state": "running",
        });
        if let Some(key) = idempotency_key.as_deref() {
            record_accepted_in_tx(
                &tx,
                key,
                "uploads.complete",
                &complete_digest,
                &json!({"uploadId": upload_id}),
                &receipt,
            )?;
        }
        tx.commit().map_err(sql_err)?;
        (operation_id, revision)
    };

    // 后台拼装 + 校验 + 发布（占 blocking 池，不挡其它请求）。所需值先
    // clone 进 blocking 闭包，外层 async 保留终态化要用的句柄。
    let driver_store = store.clone();
    let driver_operation_id = operation_id.clone();
    let inner_store = store.clone();
    let inner_upload_id = upload_id.clone();
    let inner_operation_id = operation_id.clone();
    let inner_sha = expected_sha256.clone();
    tokio::task::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            assemble_and_publish(
                &inner_store,
                &inner_upload_id,
                &inner_operation_id,
                &inner_sha,
            )
        })
        .await;
        if let Ok(Err((code, message))) = result {
            let _ = finish_async_operation(
                &driver_store,
                &driver_operation_id,
                "failed",
                None,
                Some((code.as_str(), message.as_str())),
            );
        }
    });

    Ok((
        ResponseOutcome::Accepted,
        json!({
            "kind": "operation",
            "operationId": operation_id,
            "durable": true,
            "acceptedRevision": accepted_revision,
            "deduplicated": false,
            "state": "running",
        }),
    ))
}

/// 拼装（按编号）→ 整文件 sha256 校验 → 不可变 artifact 发布 → 终态 CAS。
/// 失败以 (code, message) 返回（由调用方终态化异步操作）。
fn assemble_and_publish(
    store: &Arc<V1Store>,
    upload_id: &str,
    operation_id: &str,
    expected_sha256: &str,
) -> Result<(), (String, String)> {
    let fail = |code: &str, message: String| -> Result<(), (String, String)> {
        Err((code.to_owned(), message))
    };
    let unwrap_contract =
        |err: ContractError| -> (String, String) { (err.code.clone(), err.message.clone()) };

    let row = {
        let conn = store.conn.lock().expect("sqlite mutex");
        match fetch_upload_row(&conn, upload_id) {
            Ok(Some(row)) => row,
            _ => return fail("UPLOAD_CLOSED", format!("upload `{upload_id}` disappeared")),
        }
    };
    let dir = upload_dir(store, upload_id);
    let staging = uploads_root(store).join(format!(".assemble-{upload_id}"));

    let mut sha_mismatch = false;
    let artifact = (|| -> Result<Value, ContractError> {
        let parts = read_parts_log(&dir);
        let total = total_parts(row.byte_length, row.part_size);
        if (parts.len() as u64) != total {
            return Err(ContractError::new(
                CODE_MISSING_PARTS,
                format!(
                    "parts disappeared during assembly (has {}/{total})",
                    parts.len()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let mut hasher = Sha256::new();
        {
            let mut out = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&staging)
                .map_err(io_unavailable)?;
            for (number, _, _) in &parts {
                let bytes = std::fs::read(part_path(&dir, *number)).map_err(|_| {
                    ContractError::new(
                        CODE_MISSING_PARTS,
                        format!("part {number} bytes disappeared during assembly"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                })?;
                hasher.update(&bytes);
                out.write_all(&bytes).map_err(io_unavailable)?;
            }
            out.sync_all().map_err(io_unavailable)?;
        }
        let assembled_sha = format!("{:x}", hasher.finalize());
        if assembled_sha != expected_sha256 {
            sha_mismatch = true;
            return Err(ContractError::new(
                CODE_CHECKSUM_MISMATCH,
                format!(
                    "assembled sha256 `{assembled_sha}` does not match expectedSha256 `{expected_sha256}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let mut reader = File::open(&staging).map_err(io_unavailable)?;
        crate::public_v1::domains::artifacts::publish_artifact_from_reader(
            store,
            &row.project_id,
            &row.media_type,
            Some(&row.display_name),
            &mut reader,
            "upload",
        )
    })();

    let artifact = match artifact {
        Ok(value) => value,
        Err(err) => {
            let _ = std::fs::remove_file(&staging);
            if sha_mismatch {
                // 校验失败：upload 置 failed，临时字节释放。
                mark_upload_state(store, upload_id, "validating", "failed");
                let _ = std::fs::remove_dir_all(&dir);
                let _ = finish_async_operation(
                    store,
                    operation_id,
                    "failed",
                    None,
                    Some((err.code.as_str(), err.message.as_str())),
                );
                return Err((err.code, err.message));
            }
            // 其它失败（IO 等）：upload 回 receiving 允许重试 complete。
            mark_upload_state(store, upload_id, "validating", "receiving");
            return Err(unwrap_contract(err));
        }
    };

    // 终态 CAS：validating → complete（输给 cancel 则只清暂存）。
    let completed = {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(err) => return Err(unwrap_contract(sql_err(err))),
        };
        let artifact_id = artifact["artifactId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let revision = match next_rev(&tx) {
            Ok(revision) => revision,
            Err(err) => return Err(unwrap_contract(err)),
        };
        let updated = match tx.execute(
            "UPDATE uploads SET state = 'complete', artifact_id = ?1, revision = ?2, updated_at = ?3
             WHERE upload_id = ?4 AND state = 'validating'",
            params![artifact_id, revision, now_rfc3339(), upload_id],
        ) {
            Ok(updated) => updated,
            Err(err) => return Err(unwrap_contract(sql_err(err))),
        };
        if updated == 1 {
            // 分块审计记录（终态时一次写入，不占 part 热路径）。
            for (number, length, sha) in read_parts_log(&dir) {
                let _ = tx.execute(
                    "INSERT OR REPLACE INTO upload_parts (upload_id, part_number, byte_length, sha256)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![upload_id, number as i64, length as i64, sha],
                );
            }
            let _ = append_domain_event_in_tx(
                &tx,
                upload_id,
                "uploads.changed",
                "agentcpm-pd.upload.event/1",
                &json!({"uploadId": upload_id, "state": "complete", "artifactId": artifact_id}),
            );
            let _ = append_domain_event_in_tx(
                &tx,
                &artifact_id,
                "artifacts.created",
                "agentcpm-pd.artifact.event/1",
                &json!({"artifactId": artifact_id, "projectId": row.project_id, "origin": "upload"}),
            );
        }
        if let Err(err) = tx.commit() {
            return Err(unwrap_contract(sql_err(err)));
        }
        updated == 1
    };

    let _ = std::fs::remove_file(&staging);
    if completed {
        let _ = std::fs::remove_dir_all(&dir);
        let completion = {
            let conn = store.conn.lock().expect("sqlite mutex");
            fetch_upload_row(&conn, upload_id)
                .ok()
                .flatten()
                .map(|row| {
                    cache_meta(&row);
                    json!({
                        "upload": upload_to_json(&conn, store, &row),
                        "artifact": artifact,
                    })
                })
        };
        match completion {
            Some(completion) => {
                let _ = finish_async_operation(
                    store,
                    operation_id,
                    "succeeded",
                    Some(&completion),
                    None,
                );
                Ok(())
            }
            None => {
                let _ = finish_async_operation(
                    store,
                    operation_id,
                    "failed",
                    None,
                    Some(("UNAVAILABLE", "upload row vanished after completion")),
                );
                fail(
                    "UNAVAILABLE",
                    "upload row vanished after completion".to_owned(),
                )
            }
        }
    } else {
        // cancel 赢得终态：产物已发布但未挂到 upload（不可变、无引用），清暂存。
        fail(
            CODE_UPLOAD_CLOSED,
            "upload was cancelled during assembly".to_owned(),
        )
    }
}

/// 状态 CAS（from → to），成功后刷新 meta 缓存。
fn mark_upload_state(store: &V1Store, upload_id: &str, from_state: &str, to_state: &str) -> bool {
    let conn = store.conn.lock().expect("sqlite mutex");
    let revision = match next_rev(&conn) {
        Ok(revision) => revision,
        Err(_) => return false,
    };
    let updated = conn
        .execute(
            "UPDATE uploads SET state = ?4, revision = ?1, updated_at = ?2
             WHERE upload_id = ?3 AND state = ?5",
            params![revision, now_rfc3339(), upload_id, to_state, from_state],
        )
        .unwrap_or(0);
    if updated == 1 {
        if let Ok(Some(row)) = fetch_upload_row(&conn, upload_id) {
            cache_meta(&row);
        }
        true
    } else {
        false
    }
}

async fn uploads_cancel(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let upload_id = input["uploadId"].as_str().unwrap_or_default().to_owned();
    let outcome = tokio::task::spawn_blocking(move || cancel_upload(&store, &upload_id))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn cancel_upload(store: &Arc<V1Store>, upload_id: &str) -> Result<Value, ContractError> {
    let dir;
    {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let row = fetch_upload_row(&conn, upload_id)?.ok_or_else(|| upload_not_found(upload_id))?;
        match row.state.as_str() {
            "complete" => {
                // 已完成：产物不受取消影响，重复取消安全（#2275 no-op 语义）。
                return Ok(json!({
                    "uploadId": upload_id,
                    "state": "complete",
                    "temporaryBytesReleased": 0,
                    "artifactId": row.artifact_id,
                }));
            }
            "cancelled" => {
                return Ok(json!({
                    "uploadId": upload_id,
                    "state": "cancelled",
                    "temporaryBytesReleased": 0,
                }));
            }
            "expired" => {
                return Ok(json!({
                    "uploadId": upload_id,
                    "state": "expired",
                    "temporaryBytesReleased": 0,
                }));
            }
            _ => {}
        }
        // 门内 CAS：receiving/validating → cancelled（与 complete 的
        // receiving→validating→complete 单向竞态，只有一个终态赢家）。
        let tx = conn.transaction().map_err(sql_err)?;
        let updated = tx
            .execute(
                "UPDATE uploads SET state = 'cancelled', revision = ?1, updated_at = ?2
                 WHERE upload_id = ?3 AND state IN ('receiving', 'validating')",
                params![next_rev(&tx)?, now_rfc3339(), upload_id],
            )
            .map_err(sql_err)?;
        if updated != 1 {
            let refreshed =
                fetch_upload_row(&tx, upload_id)?.ok_or_else(|| upload_not_found(upload_id))?;
            return Ok(json!({
                "uploadId": upload_id,
                "state": refreshed.state,
                "temporaryBytesReleased": 0,
                "artifactId": refreshed.artifact_id,
            }));
        }
        let _ = append_domain_event_in_tx(
            &tx,
            upload_id,
            "uploads.changed",
            "agentcpm-pd.upload.event/1",
            &json!({"uploadId": upload_id, "state": "cancelled"}),
        );
        tx.commit().map_err(sql_err)?;
        let refreshed =
            fetch_upload_row(&conn, upload_id)?.ok_or_else(|| upload_not_found(upload_id))?;
        cache_meta(&refreshed);
        dir = upload_dir(store, upload_id);
    }

    // 释放本 upload 临时字节（只删本 uploadId 目录，已发布产物不受影响）。
    let mut released: u64 = 0;
    if dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    released += meta.len();
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(json!({
        "uploadId": upload_id,
        "state": "cancelled",
        "temporaryBytesReleased": released,
    }))
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("uploads worker task failed: {err}"),
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

    fn seed_project(rt: &Arc<PublicV1Runtime>, project_id: &str) {
        rt.store()
            .register_project(&json!({
                "projectId": project_id,
                "name": "T013 Uploads",
                "environmentId": "local",
                "root": {"instanceId": rt.store().meta().instance_id, "rootId": "root-t013", "relativePath": "ws"},
            }))
            .unwrap();
    }

    fn sha256_of(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn part_frame(upload_id: &str, number: u64, bytes: &[u8]) -> Value {
        json!({
            "uploadId": upload_id,
            "partNumber": number,
            "bytesBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
            "sha256": sha256_of(bytes),
        })
    }

    fn contract_code(err: &DispatchError) -> String {
        match err {
            DispatchError::Application(e) => e.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    #[tokio::test]
    async fn limits_report_effective_configuration() {
        let tmp = TempDir::new("uploads-limits");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        let (_, limits) = rt
            .dispatch("uploads.limits", json!({}), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("uploads.limits", &limits)
            .unwrap();
        assert_eq!(limits["maxPartBytes"], json!(512 * 1024));
        assert!(limits["maxArtifactBytes"].as_u64().unwrap() > 50 * 1024 * 1024);
        assert_eq!(limits["uploadTtlSeconds"], json!(900));
    }

    #[tokio::test]
    async fn out_of_order_upload_completes_with_immutable_artifact() {
        let tmp = TempDir::new("uploads-flow");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-up");

        let part_a = vec![1u8; 512 * 1024];
        let part_b = vec![2u8; 512 * 1024];
        let part_c = vec![3u8; 100];
        let mut whole = Vec::new();
        whole.extend_from_slice(&part_a);
        whole.extend_from_slice(&part_b);
        whole.extend_from_slice(&part_c);

        let (_, created) = rt
            .dispatch(
                json!({
                    "projectId": "proj-up",
                    "displayName": "big.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": whole.len(),
                    "sha256": sha256_of(&whole),
                }),
                Some("key-create-1"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("uploads.create", &created)
            .unwrap();
        let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();
        assert_eq!(created["upload"]["state"], json!("receiving"));
        assert_eq!(created["upload"]["partSize"], json!(512 * 1024));
        assert_eq!(created["upload"]["receivedParts"], json!([]));
        assert!(created["upload"]["artifactId"].is_null());

        // 同 key 同内容 → 同 uploadId。
        let (_, again) = rt
            .dispatch(
                json!({
                    "projectId": "proj-up",
                    "displayName": "big.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": whole.len(),
                    "sha256": sha256_of(&whole),
                }),
                Some("key-create-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(again["upload"]["uploadId"], json!(upload_id));
        // 同 key 异内容 → IDEMPOTENCY_CONFLICT。
        let err = rt
            .dispatch(
                json!({
                    "projectId": "proj-up",
                    "displayName": "big.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": 1,
                    "sha256": sha256_of(b"x"),
                }),
                Some("key-create-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "IDEMPOTENCY_CONFLICT");

        // 乱序提交：2、0、1。
        for (number, bytes) in [
            (2u64, part_c.as_slice()),
            (0, part_a.as_slice()),
            (1, part_b.as_slice()),
        ] {
            let (_, accepted) = rt
                .dispatch(
                    "uploads.part",
                    part_frame(&upload_id, number, bytes),
                    None,
                    None,
                )
                .await
                .unwrap();
            OperationRegistry::global()
                .validate_result("uploads.part", &accepted)
                .unwrap();
            assert_eq!(accepted["deduplicated"], json!(false));
        }
        // 同块同字节重传 → deduplicated（不重复占用）。
        let (_, dup) = rt
            .dispatch(
                "uploads.part",
                part_frame(&upload_id, 1, &part_b),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(dup["deduplicated"], json!(true));
        // 同块异字节 → PART_CONFLICT。
        let bad = vec![9u8; 512 * 1024];
        let err = rt
            .dispatch("uploads.part", part_frame(&upload_id, 1, &bad), None, None)
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "PART_CONFLICT");
        // 块摘要错误 → CHECKSUM_MISMATCH。
        let mut lying = part_frame(&upload_id, 0, &part_a);
        lying["sha256"] = json!(sha256_of(b"not-the-real-hash-at-all-aaaaaaaaaaaaaaaaaa"));
        let err = rt
            .dispatch("uploads.part", lying, None, None)
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "CHECKSUM_MISMATCH");

        // complete：receipt（accepted）+ 后台终态。
        let (_, receipt) = rt
            .dispatch(
                json!({"uploadId": upload_id, "expectedSha256": sha256_of(&whole)}),
                Some("key-complete-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], json!("operation"));
        assert_eq!(receipt["state"], json!("running"));
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        // 等待后台完成（uploads.get 轮询）。
        let mut final_upload = None;
        for _ in 0..100 {
            let (_, got) = rt
                .dispatch("uploads.get", json!({"uploadId": upload_id}), None, None)
                .await
                .unwrap();
            if got["upload"]["state"] == json!("complete") {
                final_upload = Some(got);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let final_upload = final_upload.expect("upload reaches complete");
        let artifact_id = final_upload["upload"]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            final_upload["upload"]["receivedParts"]
                .as_array()
                .unwrap()
                .len(),
            3
        );

        // 异步操作记录 completionResult{upload, artifact}。
        {
            let store = rt.store();
            let conn = store.conn.lock().expect("sqlite mutex");
            let (state, result_json): (String, Option<String>) = conn
                .query_row(
                    "SELECT state, result_json FROM async_operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, "succeeded");
            let completion: Value = serde_json::from_str(&result_json.unwrap()).unwrap();
            assert_eq!(completion["artifact"]["artifactId"], json!(artifact_id));
            assert_eq!(completion["upload"]["state"], json!("complete"));
        }

        // 产物可读且字节一致（分块循环）。
        let mut assembled = Vec::new();
        let mut offset = 0u64;
        loop {
            let (_, page) = rt
                .dispatch(
                    "artifacts.read",
                    json!({"artifactId": artifact_id, "offset": offset, "maxBytes": 512 * 1024}),
                    None,
                    None,
                )
                .await
                .unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(page["chunk"]["bytesBase64"].as_str().unwrap())
                .unwrap();
            assembled.extend_from_slice(&bytes);
            offset += bytes.len() as u64;
            if page["chunk"]["eof"] == json!(true) {
                break;
            }
        }
        assert_eq!(assembled, whole);
        assert_eq!(sha256_of(&assembled), sha256_of(&whole));

        // 完成后取消：产物保留，state=complete。
        let (_, cancelled) = rt
            .dispatch("uploads.cancel", json!({"uploadId": upload_id}), None, None)
            .await
            .unwrap();
        assert_eq!(cancelled["state"], json!("complete"));
        assert_eq!(cancelled["artifactId"], json!(artifact_id));
        // 完成后再提交块 → UPLOAD_CLOSED。
        let err = rt
            .dispatch(
                "uploads.part",
                part_frame(&upload_id, 0, &part_a),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "UPLOAD_CLOSED");
        // 临时目录已清。
        assert!(!upload_dir(rt.store(), &upload_id).exists());
    }

    #[tokio::test]
    async fn checksum_mismatch_fails_without_publishing() {
        let tmp = TempDir::new("uploads-checksum");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-bad");

        let payload = b"tiny payload".to_vec();
        let (_, created) = rt
            .dispatch(
                json!({
                    "projectId": "proj-bad",
                    "displayName": "tiny.txt",
                    "mediaType": "text/plain",
                    "byteLength": payload.len(),
                    "sha256": sha256_of(&payload),
                }),
                Some("key-bad-1"),
                None,
            )
            .await
            .unwrap();
        let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "uploads.part",
            part_frame(&upload_id, 0, &payload),
            None,
            None,
        )
        .await
        .unwrap();

        // 错误的整文件摘要 → 受理后终态 failed（不发布产物）。
        let wrong = sha256_of(b"different content entirely");
        let (_, receipt) = rt
            .dispatch(
                json!({"uploadId": upload_id, "expectedSha256": wrong}),
                Some("key-bad-complete"),
                None,
            )
            .await
            .unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let mut saw_failed = false;
        for _ in 0..100 {
            let (_, got) = rt
                .dispatch("uploads.get", json!({"uploadId": upload_id}), None, None)
                .await
                .unwrap();
            if got["upload"]["state"] == json!("failed") {
                saw_failed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            saw_failed,
            "checksum mismatch terminalizes the upload as failed"
        );
        {
            let store = rt.store();
            let conn = store.conn.lock().expect("sqlite mutex");
            let (state, code): (String, Option<String>) = conn
                .query_row(
                    "SELECT state, error_code FROM async_operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, "failed");
            assert_eq!(code.as_deref(), Some("CHECKSUM_MISMATCH"));
        }
        assert!(
            !upload_dir(rt.store(), &upload_id).exists(),
            "temp bytes released"
        );
    }

    #[tokio::test]
    async fn cancel_releases_only_this_upload() {
        let tmp = TempDir::new("uploads-cancel");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-cancel");

        let payload = vec![5u8; 1024];
        let (_, created) = rt
            .dispatch(
                json!({
                    "projectId": "proj-cancel",
                    "displayName": "a.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": payload.len(),
                    "sha256": sha256_of(&payload),
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "uploads.part",
            part_frame(&upload_id, 0, &payload),
            None,
            None,
        )
        .await
        .unwrap();

        // 另一个 upload（不受本次取消影响）。
        let (_, other) = rt
            .dispatch(
                json!({
                    "projectId": "proj-cancel",
                    "displayName": "b.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": payload.len(),
                    "sha256": sha256_of(&payload),
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let other_id = other["upload"]["uploadId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "uploads.part",
            part_frame(&other_id, 0, &payload),
            None,
            None,
        )
        .await
        .unwrap();

        let (_, cancelled) = rt
            .dispatch("uploads.cancel", json!({"uploadId": upload_id}), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("uploads.cancel", &cancelled)
            .unwrap();
        assert_eq!(cancelled["state"], json!("cancelled"));
        assert_eq!(cancelled["temporaryBytesReleased"], json!(1024));
        assert!(!upload_dir(rt.store(), &upload_id).exists());
        // 重复取消安全。
        let (_, again) = rt
            .dispatch("uploads.cancel", json!({"uploadId": upload_id}), None, None)
            .await
            .unwrap();
        assert_eq!(again["state"], json!("cancelled"));
        assert_eq!(again["temporaryBytesReleased"], json!(0));
        // 另一个 upload 的目录仍在。
        assert!(upload_dir(rt.store(), &other_id).exists());
        // 取消后再提交/complete → UPLOAD_CLOSED。
        let err = rt
            .dispatch(
                "uploads.part",
                part_frame(&upload_id, 0, &payload),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "UPLOAD_CLOSED");
        let err = rt
            .dispatch(
                json!({"uploadId": upload_id, "expectedSha256": sha256_of(&payload)}),
                Some("key-cancelled-complete"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "UPLOAD_CLOSED");
    }

    #[tokio::test]
    async fn missing_parts_and_over_limit_are_rejected() {
        let tmp = TempDir::new("uploads-guards");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-guard");

        let payload = vec![7u8; 2048];
        let (_, created) = rt
            .dispatch(
                json!({
                    "projectId": "proj-guard",
                    "displayName": "g.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": payload.len() * 3,
                    "sha256": sha256_of(&payload),
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "uploads.part",
            part_frame(&upload_id, 0, &payload),
            None,
            None,
        )
        .await
        .unwrap();
        // 只交 1/3 块 → MISSING_PARTS。
        let err = rt
            .dispatch(
                json!({"uploadId": upload_id, "expectedSha256": sha256_of(&payload)}),
                Some("key-missing-parts"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "MISSING_PARTS");

        // 超大 byteLength → RESOURCE_LIMIT（携带实际限制）。
        let err = rt
            .dispatch(
                json!({
                    "projectId": "proj-guard",
                    "displayName": "huge.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": MAX_ARTIFACT_BYTES + 1,
                    "sha256": sha256_of(&payload),
                }),
                Some("key-too-big"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RESOURCE_LIMIT");

        // 非法 hex sha → INVALID_ARGUMENT。
        let err = rt
            .dispatch(
                json!({
                    "projectId": "proj-guard",
                    "displayName": "x",
                    "mediaType": "text/plain",
                    "byteLength": 1,
                    "sha256": "zz",
                }),
                Some("key-bad-sha"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
    }
}
