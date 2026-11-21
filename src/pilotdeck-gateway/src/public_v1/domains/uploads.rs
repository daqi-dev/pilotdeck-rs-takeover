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
//! （`part-<N>` 文件 + append-only `parts.log`，各 fsync 后才 ack）。同一
//! upload 的落盘/日志追加/目录清除由 per-upload 进程内互斥串行化（并发同
//! 号块 dedup/PART_CONFLICT 二选一，日志行一次 `write_all` 原子追加，dedup
//! 命中补写崩溃窗口缺失的日志行）。SQLite 只在状态迁移（create/complete/
//! cancel/过期）时写。断点续传以磁盘为准。
//!
//! uploads.complete 是 `execution=async`：受理即返回公共 operation receipt
//! （业务结果在 completionResult{upload, artifact}，经 `uploads.get` /
//! operations.get 查询；完成响应丢失重查得到同一产物）。异步操作记录落在
//! `async_operations` 表（collections/t009 `operations.get` 的数据面，形状
//! 对齐其合同的 Operation 对象）。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_LIMIT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use base64::Engine;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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

fn part_path(dir: &Path, part_number: u64) -> PathBuf {
    dir.join(format!("part-{part_number}"))
}

fn parts_log_path(dir: &Path) -> PathBuf {
    dir.join("parts.log")
}

#[derive(Clone)]
struct UploadRow {
    upload_id: String,
    project_id: String,
    state: String,
    byte_length: i64,
    part_size: i64,
    #[allow(dead_code)] // whole-file digest retained for verification seams
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
        (byte_length as u64).div_ceil(part_size as u64)
    }
}

fn expected_part_len(byte_length: i64, part_size: i64, part_number: u64) -> u64 {
    let total = byte_length.max(0) as u64;
    let size = part_size.max(1) as u64;
    let remaining = total.saturating_sub(part_number * size);
    remaining.min(size)
}

/// 读取 parts.log（磁盘为准的确认块清单），按 partNumber 排序。
fn read_parts_log(dir: &Path) -> Vec<(u64, u64, String)> {
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

fn fetch_upload_audit_parts(conn: &Connection, upload_id: &str) -> Vec<(u64, u64, String)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT part_number, byte_length, sha256 FROM upload_parts
         WHERE upload_id = ?1 ORDER BY part_number ASC",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![upload_id], |r| {
        Ok((
            r.get::<_, i64>(0)? as u64,
            r.get::<_, i64>(1)? as u64,
            r.get::<_, String>(2)?,
        ))
    })
    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    .unwrap_or_default()
}

fn upload_to_json_with_parts(row: &UploadRow, parts: Vec<(u64, u64, String)>) -> Value {
    let received: Vec<Value> = parts
        .into_iter()
        .map(|(number, length, sha)| {
            json!({"partNumber": number, "byteLength": length, "sha256": sha})
        })
        .collect();
    // 合同里 artifactId 是可选 string（上传完成前不存在）：None 时省略
    // 字段而不是序列化成 null——null 过不了结果 schema 校验。
    let mut upload = json!({
        "uploadId": row.upload_id,
        "projectId": row.project_id,
        "state": row.state,
        "byteLength": row.byte_length.max(0),
        "receivedParts": received,
        "partSize": row.part_size.max(1),
        "expiresAt": row.expires_at,
        "revision": row.revision,
    });
    if let Some(artifact_id) = &row.artifact_id {
        upload["artifactId"] = json!(artifact_id);
    }
    upload
}

// ---------------------------------------------------------------------------
// 热路径 meta 缓存（uploads.part 不进 SQLite；P2 前提）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct UploadMeta {
    state: String,
    byte_length: i64,
    part_size: i64,
    expires_at: Option<DateTime<Utc>>,
}

fn meta_cache() -> &'static RwLock<HashMap<String, UploadMeta>> {
    static CACHE: OnceLock<RwLock<HashMap<String, UploadMeta>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// per-upload 互斥（accept_part / cancel / sweep 共用）
// ---------------------------------------------------------------------------

/// 每个 upload 目录一把进程内互斥：串行化同一 upload 的分块落盘、parts.log
/// 追加、拼装读取与目录清除。
///
/// 锁序不变量：SQLite 连接锁与 `upload_lock` **完全不嵌套**（DB 事务拿完
/// 状态释放 `store.conn` 之后再取 `upload_lock` 做磁盘 I/O；`meta_cache` 只
/// 在叶子短持），从而杜绝 DB↔upload 锁反转死锁，也不持 DB 锁做磁盘 I/O。
fn upload_locks() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn acquire_upload_lock(dir: &Path) -> Arc<Mutex<()>> {
    let mut locks = upload_locks().lock().expect("upload locks");
    locks
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn parse_expires_at(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn cache_meta(row: &UploadRow) {
    meta_cache().write().expect("upload meta cache").insert(
        row.upload_id.clone(),
        UploadMeta {
            state: row.state.clone(),
            byte_length: row.byte_length,
            part_size: row.part_size,
            expires_at: parse_expires_at(&row.expires_at),
        },
    );
}

fn is_expired(row: &UploadRow) -> bool {
    parse_expires_at(&row.expires_at).is_some_and(|expires| Utc::now() > expires)
}

fn staging_path(store: &V1Store, upload_id: &str) -> PathBuf {
    uploads_root(store).join(format!(".assemble-{upload_id}"))
}

/// 在已持 `upload_lock(&dir)` 外或内部删除本 upload 临时目录及 `.assemble-*` 暂存文件
/// （绝不持有 SQLite 连接锁时调用）。
fn cleanup_upload_artifacts(store: &V1Store, upload_id: &str) {
    let dir = upload_dir(store, upload_id);
    let staging = staging_path(store, upload_id);
    let upload_lock = acquire_upload_lock(&dir);
    let _guard = upload_lock.lock().expect("upload lock");
    let _ = std::fs::remove_file(&staging);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 过期惰性 CAS（仅做 SQLite 迁移 + 刷新 `meta_cache`；返回 `(row, expired_now)`，
/// 磁盘清理由调用方释放 DB 锁后调用 `cleanup_upload_artifacts` 完成）。
fn expire_row_in_tx(
    conn: &Connection,
    row: &UploadRow,
) -> Result<(UploadRow, bool), ContractError> {
    if !is_expired(row) || (row.state != "receiving" && row.state != "validating") {
        return Ok((row.clone(), false));
    }
    let updated = conn
        .execute(
            "UPDATE uploads SET state = 'expired', revision = ?1, updated_at = ?2
             WHERE upload_id = ?3 AND state = ?4",
            params![next_rev(conn)?, now_rfc3339(), row.upload_id, row.state],
        )
        .map_err(sql_err)?;
    let refreshed =
        fetch_upload_row(conn, &row.upload_id)?.ok_or_else(|| upload_not_found(&row.upload_id))?;
    cache_meta(&refreshed);
    Ok((refreshed, updated == 1))
}

/// 启动崩溃恢复（供 `store.rs::sync_meta_and_recover` 在启动恢复事务中调用）：
/// 1. 全部非终态 `async_operations`（queued/running）标为 `interrupted`（`GATEWAY_RESTART`）；
/// 2. 滞留 `validating` 的 upload 回退为 `receiving`（或超期转 `expired`）并清内存 cache；
/// 3. 清理 `<home>/v1/uploads/.assemble-*` 残留大暂存文件，消除 2 GiB 磁盘泄漏。
pub(crate) fn recover_orphaned_uploads_and_async_ops(
    home: &Path,
    conn: &Connection,
    now: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "UPDATE async_operations
         SET state = 'interrupted',
             error_code = 'GATEWAY_RESTART',
             error_message = 'gateway restarted while operation was running',
             updated_at = ?1
         WHERE state IN ('queued', 'running')",
        params![now],
    )
    .map_err(sql_err)?;

    let validating_ids: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT upload_id, expires_at FROM uploads WHERE state = 'validating'")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        rows
    };
    for (upload_id, expires_at) in validating_ids {
        let next = next_rev(conn)?;
        let expired = parse_expires_at(&expires_at).is_some_and(|exp| Utc::now() > exp);
        let new_state = if expired { "expired" } else { "receiving" };
        conn.execute(
            "UPDATE uploads SET state = ?1, revision = ?2, updated_at = ?3
             WHERE upload_id = ?4 AND state = 'validating'",
            params![new_state, next, now, upload_id],
        )
        .map_err(sql_err)?;
        if expired {
            let dir = home.join("v1").join("uploads").join(&upload_id);
            let _ = std::fs::remove_dir_all(&dir);
        }
        meta_cache()
            .write()
            .expect("upload meta cache")
            .remove(&upload_id);
    }

    let root = home.join("v1").join("uploads");
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".assemble-") {
                let path = entry.path();
                if std::fs::symlink_metadata(&path)
                    .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    Ok(())
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

pub(crate) fn finish_async_operation_in_tx(
    conn: &Connection,
    operation_id: &str,
    state: &str,
    completion_result: Option<&Value>,
    error: Option<(&str, &str)>,
) -> Result<(), ContractError> {
    let result_str = completion_result.map(|v| serde_json::to_string(v).unwrap_or_default());
    conn.execute(
        "UPDATE async_operations
         SET state = ?1, result_json = ?2, error_code = ?3, error_message = ?4, updated_at = ?5
         WHERE operation_id = ?6 AND state IN ('queued', 'running')",
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
    finish_async_operation_in_tx(&conn, operation_id, state, completion_result, error)
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

fn valid_upload_id(upload_id: &str) -> bool {
    !upload_id.is_empty()
        && upload_id != "."
        && upload_id != ".."
        && !upload_id.contains('/')
        && !upload_id.contains('\\')
        && !upload_id.contains('\0')
}

// ---------------------------------------------------------------------------
// GC（语义移植 upload/mod.rs cleanup_expired / reclaim_orphan）
// ---------------------------------------------------------------------------

/// 机会式清扫（SQLite 状态更新与磁盘清理彻底分离，绝不持 DB 锁做磁盘 I/O）：
/// 1. 过期未完成上传 → Immediate tx CAS 到 expired（含缺目录行），释放锁后再删目录与 `.assemble-*`；
/// 2. cancelled/failed/expired 行的残留目录 → 删除；
/// 3. 无行孤儿目录或非在途 `.assemble-*` 暂存碎片 → 回收（防 2 GiB 永久泄漏）。
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

    // 1) 将全部已过期行在单个 Immediate 事务里 CAS 到 expired（不持锁做磁盘 I/O）。
    let expired_candidates: Vec<UploadRow> = rows
        .iter()
        .filter(|row| (row.state == "receiving" || row.state == "validating") && is_expired(row))
        .cloned()
        .collect();
    let mut expired_ids: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, UploadRow> = rows
        .into_iter()
        .map(|row| (row.upload_id.clone(), row))
        .collect();
    if !expired_candidates.is_empty() {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        if let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            for row in expired_candidates {
                if let Ok((refreshed, changed)) = expire_row_in_tx(&tx, &row) {
                    if changed {
                        expired_ids.push(refreshed.upload_id.clone());
                    }
                    by_id.insert(refreshed.upload_id.clone(), refreshed);
                }
            }
            let _ = tx.commit();
        }
    }
    for uid in expired_ids {
        cleanup_upload_artifacts(store, &uid);
    }

    // 2) 不持 DB 锁扫描磁盘：清理终态目录、TTL 孤儿目录、及失效 `.assemble-*` 暂存文件。
    let Ok(entries) = std::fs::read_dir(uploads_root(store)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path);
        if let Some(uid) = name.strip_prefix(".assemble-") {
            let is_plain_file = meta
                .as_ref()
                .map(|m| m.is_file() && !m.file_type().is_symlink())
                .unwrap_or(false);
            if !is_plain_file {
                continue;
            }
            let should_remove = match by_id.get(uid) {
                Some(row) => row.state != "validating" || is_expired(row),
                None => meta
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|mtime| mtime.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > UPLOAD_TTL_SECONDS as u64),
            };
            if should_remove {
                let dir = upload_dir(store, uid);
                let lock = acquire_upload_lock(&dir);
                let _guard = lock.lock().expect("upload lock");
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        let is_plain_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        if !is_plain_dir {
            continue;
        }
        match by_id.get(&name) {
            Some(row) => {
                if matches!(row.state.as_str(), "cancelled" | "failed" | "expired") {
                    cleanup_upload_artifacts(store, &name);
                }
            }
            None => {
                let old = meta
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|mtime| mtime.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > UPLOAD_TTL_SECONDS as u64);
                if old {
                    cleanup_upload_artifacts(store, &name);
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
    // 机会式清扫（承接 GC 职责；无行孤儿 + 过期上传 + 残留 .assemble-*）。
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

    let replay_hit: Option<(UploadRow, bool, Vec<(u64, u64, String)>)>;
    let new_row: UploadRow;
    {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

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
                    let (row, expired_now) = expire_row_in_tx(&tx, &row)?;
                    let audit_parts = fetch_upload_audit_parts(&tx, &row.upload_id);
                    tx.commit().map_err(sql_err)?;
                    replay_hit = Some((row, expired_now, audit_parts));
                    drop(conn);
                    let (row, expired_now, audit_parts) = replay_hit.unwrap();
                    let parts = if expired_now {
                        cleanup_upload_artifacts(store, &row.upload_id);
                        Vec::new()
                    } else {
                        let dir = upload_dir(store, &row.upload_id);
                        let lock = acquire_upload_lock(&dir);
                        let _guard = lock.lock().expect("upload lock");
                        let log_parts = read_parts_log(&dir);
                        if log_parts.is_empty() && !dir.is_dir() {
                            audit_parts
                        } else {
                            log_parts
                        }
                    };
                    return Ok(json!({"upload": upload_to_json_with_parts(&row, parts)}));
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
        new_row = UploadRow {
            upload_id,
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
    }

    // 目录创建在释放 DB 锁后进行（行先落，目录后建：孤儿回收按「无行且超龄」判定）。
    std::fs::create_dir_all(upload_dir(store, &new_row.upload_id)).map_err(io_unavailable)?;
    cache_meta(&new_row);
    Ok(json!({"upload": upload_to_json_with_parts(&new_row, Vec::new())}))
}

async fn uploads_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let upload_id = input["uploadId"].as_str().unwrap_or_default().to_owned();
    if !valid_upload_id(&upload_id) {
        return Err(DispatchError::Application(upload_not_found(&upload_id)));
    }
    let upload = tokio::task::spawn_blocking(move || {
        let (row, expired_now, audit_parts) = {
            let mut conn = store.conn.lock().expect("sqlite mutex");
            let row =
                fetch_upload_row(&conn, &upload_id)?.ok_or_else(|| upload_not_found(&upload_id))?;
            let (row, expired_now) =
                if is_expired(&row) && (row.state == "receiving" || row.state == "validating") {
                    let tx = conn
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .map_err(sql_err)?;
                    let (row, expired_now) = expire_row_in_tx(&tx, &row)?;
                    tx.commit().map_err(sql_err)?;
                    (row, expired_now)
                } else {
                    (row, false)
                };
            let audit_parts = fetch_upload_audit_parts(&conn, &row.upload_id);
            (row, expired_now, audit_parts)
        };
        let parts = if expired_now {
            cleanup_upload_artifacts(&store, &row.upload_id);
            Vec::new()
        } else {
            let dir = upload_dir(&store, &row.upload_id);
            let lock = acquire_upload_lock(&dir);
            let _guard = lock.lock().expect("upload lock");
            let log_parts = read_parts_log(&dir);
            if log_parts.is_empty() && !dir.is_dir() {
                audit_parts
            } else {
                log_parts
            }
        };
        Ok::<_, ContractError>(upload_to_json_with_parts(&row, parts))
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
    if !valid_upload_id(&upload_id) {
        return Err(DispatchError::Application(upload_not_found(&upload_id)));
    }
    let part_number = input["partNumber"].as_u64().unwrap_or(0);
    let sha256 = input["sha256"].as_str().unwrap_or_default().to_owned();
    let bytes = input["bytesBase64"].as_str().unwrap_or_default().to_owned();

    // 热路径读 meta_cache；仅在 cache miss 或过期转态时进 spawn_blocking 查库，不阻塞 Tokio worker。
    let cached = {
        meta_cache()
            .read()
            .expect("upload meta cache")
            .get(&upload_id)
            .cloned()
    };
    let meta = match cached {
        Some(meta) => meta,
        None => {
            let store_db = store.clone();
            let id_db = upload_id.clone();
            tokio::task::spawn_blocking(move || {
                let conn = store_db.conn.lock().expect("sqlite mutex");
                let row =
                    fetch_upload_row(&conn, &id_db)?.ok_or_else(|| upload_not_found(&id_db))?;
                cache_meta(&row);
                Ok::<_, ContractError>(UploadMeta {
                    state: row.state,
                    byte_length: row.byte_length,
                    part_size: row.part_size,
                    expires_at: parse_expires_at(&row.expires_at),
                })
            })
            .await
            .map_err(join_err)??
        }
    };

    if meta.state != "receiving" {
        return Err(DispatchError::Application(ContractError::new(
            CODE_UPLOAD_CLOSED,
            format!(
                "upload `{upload_id}` is `{}`; parts are no longer accepted",
                meta.state
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    if meta.expires_at.is_some_and(|exp| Utc::now() > exp) {
        let store_exp = store.clone();
        let id_exp = upload_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let expired_now = {
                let mut conn = store_exp.conn.lock().expect("sqlite mutex");
                if let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate)
                    && let Ok(Some(row)) = fetch_upload_row(&tx, &id_exp)
                    && let Ok((_, changed)) = expire_row_in_tx(&tx, &row)
                {
                    let _ = tx.commit();
                    changed
                } else {
                    false
                }
            };
            if expired_now {
                cleanup_upload_artifacts(&store_exp, &id_exp);
            }
        })
        .await;
        return Err(DispatchError::Application(ContractError::new(
            CODE_UPLOAD_CLOSED,
            format!("upload `{upload_id}` has expired"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }

    let total = total_parts(meta.byte_length, meta.part_size);
    if part_number >= total {
        return Err(DispatchError::Application(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!(
                "partNumber {part_number} is out of range (upload declares {total} parts of {} bytes)",
                meta.part_size
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
    let expected_len = expected_part_len(meta.byte_length, meta.part_size, part_number) as usize;
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
    let id_for_lock = upload_id.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        // per-upload 互斥 + 锁内状态/过期复核（阻断 complete/cancel 并发滑过门写盘）。
        let upload_lock = acquire_upload_lock(&dir);
        let _guard = upload_lock.lock().expect("upload lock");
        if let Some(latest) = meta_cache()
            .read()
            .expect("upload meta cache")
            .get(&id_for_lock)
        {
            if latest.state != "receiving" {
                return Err(ContractError::new(
                    CODE_UPLOAD_CLOSED,
                    format!(
                        "upload `{id_for_lock}` is `{}`; parts are no longer accepted",
                        latest.state
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if latest.expires_at.is_some_and(|exp| Utc::now() > exp) {
                return Err(ContractError::new(
                    CODE_UPLOAD_CLOSED,
                    format!("upload `{id_for_lock}` has expired"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
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
/// 原子写 part 文件 + append parts.log，各自 fsync 后才确认。调用方必须已
/// 持有该 upload 的 per-upload 锁（同号块串行化、日志行不交错）。
fn accept_part(
    dir: &Path,
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
            // dedup 命中但日志缺行（rename 成功后、append 之前进程被杀的
            // 崩溃窗口）：补写确认行。不补的话 complete 永远 MISSING_PARTS，
            // 而重传只会再次命中 dedup 提前返回，没有任何修复路径。
            let confirmed = read_parts_log(dir)
                .iter()
                .any(|(number, _, _)| *number == part_number);
            if !confirmed {
                append_parts_log_line(dir, part_number, expected_len, actual_sha)?;
            }
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
    append_parts_log_line(dir, part_number, expected_len, actual_sha)?;
    Ok((false, decoded.len()))
}

/// 追加单行确认记录：先格式化成一条字节串再一次 `write_all`——O_APPEND
/// 只保证单次 write 原子，`writeln!` 经 write_fmt 拆成多次 write，两个不同
/// 号块并发追加可在 token 边界拼出非法 JSON 行（`read_parts_log` 对坏行
/// continue 丢弃 → 永久 MISSING_PARTS）。
fn append_parts_log_line(
    dir: &Path,
    part_number: u64,
    byte_length: usize,
    sha: &str,
) -> Result<(), ContractError> {
    let line = json!({
        "partNumber": part_number,
        "byteLength": byte_length,
        "sha256": sha,
    });
    let mut bytes = serde_json::to_vec(&line)
        .map_err(|e| io_unavailable(std::io::Error::other(e.to_string())))?;
    bytes.push(b'\n');
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(parts_log_path(dir))
        .map_err(io_unavailable)?;
    log.write_all(&bytes).map_err(io_unavailable)?;
    log.sync_all().map_err(io_unavailable)?;
    Ok(())
}

async fn uploads_complete(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let upload_id = input["uploadId"].as_str().unwrap_or_default().to_owned();
    if !valid_upload_id(&upload_id) {
        return Err(DispatchError::Application(upload_not_found(&upload_id)));
    }
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

    enum CompleteOutcome {
        ImmediateReceipt(Value),
        Started {
            operation_id: String,
            receipt: Value,
        },
    }

    let store_intake = store.clone();
    let upload_id_intake = upload_id.clone();
    let expected_sha_intake = expected_sha256.clone();
    let idemp_intake = idempotency_key.clone();
    let complete_outcome = tokio::task::spawn_blocking(move || -> Result<CompleteOutcome, ContractError> {
        let dir = upload_dir(&store_intake, &upload_id_intake);

        // 阶段 1：短持 DB 事务检查幂等、存在性、过期与已完成重放（不持 DB 锁做磁盘读）。
        let total = {
            let mut conn = store_intake.conn.lock().expect("sqlite mutex");
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;
            if let Some(key) = idemp_intake.as_deref() {
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
                        return Err(ContractError::new(
                            CODE_IDEMPOTENCY_CONFLICT,
                            format!(
                                "idempotencyKey `{key}` already used for uploads.complete with different input"
                            ),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    if let Ok(mut receipt) = serde_json::from_str::<Value>(&receipt_raw) {
                        if let Some(obj) = receipt.as_object_mut() {
                            obj.insert("deduplicated".into(), json!(true));
                        }
                        tx.commit().map_err(sql_err)?;
                        return Ok(CompleteOutcome::ImmediateReceipt(receipt));
                    }
                }
            }

            let row = fetch_upload_row(&tx, &upload_id_intake)?
                .ok_or_else(|| upload_not_found(&upload_id_intake))?;
            let (_row, expired_now) = expire_row_in_tx(&tx, &row)?;
            if expired_now {
                tx.commit().map_err(sql_err)?;
                drop(conn);
                cleanup_upload_artifacts(&store_intake, &upload_id_intake);
                return Err(ContractError::new(
                    CODE_UPLOAD_CLOSED,
                    format!("upload `{upload_id_intake}` has expired"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if matches!(row.state.as_str(), "cancelled" | "expired" | "failed") {
                return Err(ContractError::new(
                    CODE_UPLOAD_CLOSED,
                    format!("upload `{upload_id_intake}` is `{}`", row.state),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if row.state == "complete" {
                if expected_sha_intake != row.total_sha256 {
                    return Err(ContractError::new(
                        CODE_CHECKSUM_MISMATCH,
                        format!(
                            "expectedSha256 `{expected_sha_intake}` does not match completed upload sha256 `{}`",
                            row.total_sha256
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                let artifact_id = row.artifact_id.as_deref().ok_or_else(|| {
                    ContractError::new(
                        CODE_UNAVAILABLE,
                        format!("completed upload `{upload_id_intake}` is missing artifactId"),
                        RetrySpec::backoff_read(Some(250)),
                        EffectStatus::Unknown,
                    )
                })?;
                let artifact =
                    crate::public_v1::domains::artifacts::artifact_json_in_tx(&tx, artifact_id)?;
                let audit_parts = fetch_upload_audit_parts(&tx, &upload_id_intake);
                let completion = json!({
                    "upload": upload_to_json_with_parts(&row, audit_parts),
                    "artifact": artifact,
                });
                let operation_id = store_intake.next_id();
                let revision =
                    create_async_operation(&tx, "uploads.complete", &upload_id_intake, &operation_id)?;
                finish_async_operation_in_tx(&tx, &operation_id, "succeeded", Some(&completion), None)?;
                let receipt = json!({
                    "kind": "operation",
                    "operationId": operation_id,
                    "durable": true,
                    "acceptedRevision": revision,
                    "deduplicated": true,
                    "state": "succeeded",
                });
                if let Some(key) = idemp_intake.as_deref() {
                    record_accepted_in_tx(
                        &tx,
                        key,
                        "uploads.complete",
                        &complete_digest,
                        &json!({"uploadId": upload_id_intake}),
                        &receipt,
                    )?;
                }
                tx.commit().map_err(sql_err)?;
                return Ok(CompleteOutcome::ImmediateReceipt(receipt));
            }
            total_parts(row.byte_length, row.part_size)
        };

        // 阶段 2：在 DB 锁外持 upload_lock 校验磁盘块齐全性（阻塞池上执行）。
        let parts = {
            let lock = acquire_upload_lock(&dir);
            let _guard = lock.lock().expect("upload lock");
            read_parts_log(&dir)
        };
        let mut received_numbers: Vec<u64> = parts.iter().map(|(n, _, _)| *n).collect();
        received_numbers.sort();
        received_numbers.dedup();
        let all_present = total == received_numbers.len() as u64
            && (0..total).all(|n| received_numbers.binary_search(&n).is_ok());
        if !all_present {
            return Err(ContractError::new(
                CODE_MISSING_PARTS,
                format!(
                    "upload `{upload_id_intake}` is missing parts (has {}/{total})",
                    received_numbers.len()
                ),
                RetrySpec::same_key(None),
                EffectStatus::NotApplied,
            ));
        }

        // 阶段 3：Immediate 事务 CAS receiving → validating + 立即刷新 meta_cache。
        let mut conn = store_intake.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        if let Some(key) = idemp_intake.as_deref() {
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
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for uploads.complete with different input"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if let Ok(mut receipt) = serde_json::from_str::<Value>(&receipt_raw) {
                    if let Some(obj) = receipt.as_object_mut() {
                        obj.insert("deduplicated".into(), json!(true));
                    }
                    tx.commit().map_err(sql_err)?;
                    return Ok(CompleteOutcome::ImmediateReceipt(receipt));
                }
            }
        }
        let row = fetch_upload_row(&tx, &upload_id_intake)?
            .ok_or_else(|| upload_not_found(&upload_id_intake))?;
        let (_row, expired_now) = expire_row_in_tx(&tx, &row)?;
        if expired_now {
            tx.commit().map_err(sql_err)?;
            drop(conn);
            cleanup_upload_artifacts(&store_intake, &upload_id_intake);
            return Err(ContractError::new(
                CODE_UPLOAD_CLOSED,
                format!("upload `{upload_id_intake}` has expired"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let updated = tx
            .execute(
                "UPDATE uploads SET state = 'validating', revision = ?1, updated_at = ?2
                 WHERE upload_id = ?3 AND state = 'receiving'",
                params![next_rev(&tx)?, now_rfc3339(), upload_id_intake],
            )
            .map_err(sql_err)?;
        if updated != 1 {
            let refreshed = fetch_upload_row(&tx, &upload_id_intake)?
                .ok_or_else(|| upload_not_found(&upload_id_intake))?;
            cache_meta(&refreshed);
            return Err(ContractError::new(
                CODE_UPLOAD_CLOSED,
                format!("upload `{upload_id_intake}` is `{}`", refreshed.state),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let refreshed = fetch_upload_row(&tx, &upload_id_intake)?
            .ok_or_else(|| upload_not_found(&upload_id_intake))?;
        // P1#11：CAS 到 validating 后立刻刷新 meta_cache，阻止并发 uploads.part 继续写入。
        cache_meta(&refreshed);

        let operation_id = store_intake.next_id();
        let revision =
            create_async_operation(&tx, "uploads.complete", &upload_id_intake, &operation_id)?;
        let receipt = json!({
            "kind": "operation",
            "operationId": operation_id,
            "durable": true,
            "acceptedRevision": revision,
            "deduplicated": false,
            "state": "running",
        });
        if let Some(key) = idemp_intake.as_deref() {
            record_accepted_in_tx(
                &tx,
                key,
                "uploads.complete",
                &complete_digest,
                &json!({"uploadId": upload_id_intake}),
                &receipt,
            )?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(CompleteOutcome::Started {
            operation_id,
            receipt,
        })
    })
    .await
    .map_err(join_err)??;

    let (operation_id, receipt) = match complete_outcome {
        CompleteOutcome::ImmediateReceipt(receipt) => {
            return Ok((ResponseOutcome::Accepted, receipt));
        }
        CompleteOutcome::Started {
            operation_id,
            receipt,
        } => (operation_id, receipt),
    };

    // 后台拼装 + 校验 + 发布；同时处理 Err 与 JoinError（绝不静默丢弃 panic）。
    let driver_store = store.clone();
    let driver_operation_id = operation_id.clone();
    let driver_upload_id = upload_id.clone();
    let inner_store = store.clone();
    let inner_upload_id = upload_id;
    let inner_operation_id = operation_id;
    let inner_sha = expected_sha256;
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
        match result {
            Ok(Ok(())) => {}
            Ok(Err((code, message))) => {
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = finish_async_operation(
                        &driver_store,
                        &driver_operation_id,
                        "failed",
                        None,
                        Some((code.as_str(), message.as_str())),
                    );
                })
                .await;
            }
            Err(join_err) => {
                let message = format!("upload assembly worker panicked: {join_err}");
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = mark_upload_state(
                        &driver_store,
                        &driver_upload_id,
                        "validating",
                        "receiving",
                    );
                    let _ = std::fs::remove_file(staging_path(&driver_store, &driver_upload_id));
                    let _ = finish_async_operation(
                        &driver_store,
                        &driver_operation_id,
                        "failed",
                        None,
                        Some((CODE_UNAVAILABLE, message.as_str())),
                    );
                })
                .await;
            }
        }
    });

    Ok((ResponseOutcome::Accepted, receipt))
}

/// 拼装（在 `upload_lock` 下读 part 与写 `.assemble-*`，随后放锁发布产物，最后 CAS `validating -> complete`）。
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
    let staging = staging_path(store, upload_id);

    // 1. 持 per-upload 锁读取 parts.log/part-N、哈希并写入 `.assemble-*`（释放锁后再调 artifacts/SQLite）。
    let mut sha_mismatch = false;
    let stage_result: Result<Vec<(u64, u64, String)>, ContractError> = (|| {
        let upload_lock = acquire_upload_lock(&dir);
        let _guard = upload_lock.lock().expect("upload lock");

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
        Ok(parts)
    })();

    let parts = match stage_result {
        Ok(parts) => parts,
        Err(err) => {
            if sha_mismatch {
                mark_upload_state(store, upload_id, "validating", "failed");
                cleanup_upload_artifacts(store, upload_id);
                return Err(unwrap_contract(err));
            }
            let _ = std::fs::remove_file(&staging);
            mark_upload_state(store, upload_id, "validating", "receiving");
            return Err(unwrap_contract(err));
        }
    };

    // 2. 流式发布不可变产物（已释放 upload_lock，只用只读 .assemble-* 暂存文件）。
    let publish_res = (|| -> Result<Value, ContractError> {
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
    let artifact = match publish_res {
        Ok(value) => value,
        Err(err) => {
            let _ = std::fs::remove_file(&staging);
            mark_upload_state(store, upload_id, "validating", "receiving");
            return Err(unwrap_contract(err));
        }
    };

    // 3. 终态 CAS：validating → complete（使用内存中 parts，不持 DB 锁读盘；同事务写 async_operations）。
    let completed = {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
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
            for (number, length, sha) in &parts {
                let _ = tx.execute(
                    "INSERT OR REPLACE INTO upload_parts (upload_id, part_number, byte_length, sha256)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![upload_id, *number as i64, *length as i64, sha],
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
            if let Ok(Some(refreshed)) = fetch_upload_row(&tx, upload_id) {
                let completion = json!({
                    "upload": upload_to_json_with_parts(&refreshed, parts.clone()),
                    "artifact": artifact,
                });
                let _ = finish_async_operation_in_tx(
                    &tx,
                    operation_id,
                    "succeeded",
                    Some(&completion),
                    None,
                );
                cache_meta(&refreshed);
            } else {
                let _ = finish_async_operation_in_tx(
                    &tx,
                    operation_id,
                    "failed",
                    None,
                    Some(("UNAVAILABLE", "upload row vanished after completion")),
                );
            }
        }
        if let Err(err) = tx.commit() {
            return Err(unwrap_contract(sql_err(err)));
        }
        updated == 1
    };

    // 4. 释放 DB 锁后在 upload_lock 下清 `.assemble-*` 与临时分块目录。
    cleanup_upload_artifacts(store, upload_id);
    if completed {
        Ok(())
    } else {
        fail(
            CODE_UPLOAD_CLOSED,
            "upload was cancelled during assembly".to_owned(),
        )
    }
}

/// 状态 CAS（from → to），成功后刷新 meta 缓存。
fn mark_upload_state(store: &V1Store, upload_id: &str, from_state: &str, to_state: &str) -> bool {
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) else {
        return false;
    };
    let Ok(revision) = next_rev(&tx) else {
        return false;
    };
    let updated = tx
        .execute(
            "UPDATE uploads SET state = ?4, revision = ?1, updated_at = ?2
             WHERE upload_id = ?3 AND state = ?5",
            params![revision, now_rfc3339(), upload_id, to_state, from_state],
        )
        .unwrap_or(0);
    if updated == 1 {
        if let Ok(Some(row)) = fetch_upload_row(&tx, upload_id) {
            cache_meta(&row);
        }
        let _ = tx.commit();
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
    if !valid_upload_id(&upload_id) {
        return Err(DispatchError::Application(upload_not_found(&upload_id)));
    }
    let outcome = tokio::task::spawn_blocking(move || cancel_upload(&store, &upload_id))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn cancel_upload(store: &Arc<V1Store>, upload_id: &str) -> Result<Value, ContractError> {
    let dir;
    let staging;
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
        // 门内 CAS：receiving/validating → cancelled（Immediate tx 独占写锁）。
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
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
            cache_meta(&refreshed);
            let mut lost_race = json!({
                "uploadId": upload_id,
                "state": refreshed.state,
                "temporaryBytesReleased": 0,
            });
            if let Some(artifact_id) = &refreshed.artifact_id {
                lost_race["artifactId"] = json!(artifact_id);
            }
            return Ok(lost_race);
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
        staging = staging_path(store, upload_id);
    }

    let mut released: u64 = 0;
    {
        let upload_lock = acquire_upload_lock(&dir);
        let _guard = upload_lock.lock().expect("upload lock");
        if staging.is_file() {
            let _ = std::fs::remove_file(&staging);
        }
        if dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_name() == "parts.log" {
                        continue;
                    }
                    if let Ok(meta) = entry.metadata() {
                        released += meta.len();
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
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
                "uploads.create",
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
                "uploads.create",
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
                "uploads.create",
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

        // 乱序提交：2、0、1。uploads.part 是 idempotentSubmit（合同
        // cli-v1-operations.json），dispatch 必须带调用方 idempotencyKey（缺
        // 失 → -32086）；每块一个 key。
        for (number, bytes) in [
            (2u64, part_c.as_slice()),
            (0, part_a.as_slice()),
            (1, part_b.as_slice()),
        ] {
            let (_, accepted) = rt
                .dispatch(
                    "uploads.part",
                    part_frame(&upload_id, number, bytes),
                    Some(&format!("key-part-{number}")),
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
                Some("key-part-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(dup["deduplicated"], json!(true));
        // 同块异字节 → PART_CONFLICT。
        let bad = vec![9u8; 512 * 1024];
        let err = rt
            .dispatch(
                "uploads.part",
                part_frame(&upload_id, 1, &bad),
                Some("key-part-bad"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "PART_CONFLICT");
        // 块摘要错误 → CHECKSUM_MISMATCH。
        let mut lying = part_frame(&upload_id, 0, &part_a);
        lying["sha256"] = json!(sha256_of(b"not-the-real-hash-at-all-aaaaaaaaaaaaaaaaaa"));
        let err = rt
            .dispatch("uploads.part", lying, Some("key-part-lying"), None)
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "CHECKSUM_MISMATCH");

        // complete：receipt（accepted）+ 后台终态。
        let (_, receipt) = rt
            .dispatch(
                "uploads.complete",
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

        // 异步操作记录 completionResult{upload, artifact}。uploads.get 报告
        // complete 与操作行落 succeeded 是完成体两步先后写——按操作自身的
        // 异步可见性轮询到终态再断言（同套件 wait_operation 语义），避免把
        // 中间态当失败。
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let (state, result_json) = loop {
                // 锁的作用域止于块尾：guard 不跨 await（clippy
                // await_holding_lock）。
                let row: Option<(String, Option<String>)> = {
                    let conn = rt.store().conn.lock().expect("sqlite mutex");
                    conn.query_row(
                        "SELECT state, result_json FROM async_operations WHERE operation_id = ?1",
                        params![operation_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .unwrap()
                };
                if let Some((state, result_json)) = row
                    && matches!(state.as_str(), "succeeded" | "failed")
                {
                    break (state, result_json);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "completion operation `{operation_id}` never became terminal"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            };
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

        // 完成后取消：产物保留，state=complete。uploads.cancel 同为
        // idempotentSubmit，dispatch 也必须带 key。
        let (_, cancelled) = rt
            .dispatch(
                "uploads.cancel",
                json!({"uploadId": upload_id}),
                Some("key-cancel-after-complete"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancelled["state"], json!("complete"));
        assert_eq!(cancelled["artifactId"], json!(artifact_id));
        // 完成后再提交块 → UPLOAD_CLOSED。
        let err = rt
            .dispatch(
                "uploads.part",
                part_frame(&upload_id, 0, &part_a),
                Some("key-part-after-complete"),
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
                "uploads.create",
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
            Some("key-bad-part-0"),
            None,
        )
        .await
        .unwrap();

        // 错误的整文件摘要 → 受理后终态 failed（不发布产物）。
        let wrong = sha256_of(b"different content entirely");
        let (_, receipt) = rt
            .dispatch(
                "uploads.complete",
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
                "uploads.create",
                json!({
                    "projectId": "proj-cancel",
                    "displayName": "a.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": payload.len(),
                    "sha256": sha256_of(&payload),
                }),
                Some("key-cancel-create-a"),
                None,
            )
            .await
            .unwrap();
        let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "uploads.part",
            part_frame(&upload_id, 0, &payload),
            Some("key-cancel-part-a-0"),
            None,
        )
        .await
        .unwrap();

        // 另一个 upload（不受本次取消影响）。
        let (_, other) = rt
            .dispatch(
                "uploads.create",
                json!({
                    "projectId": "proj-cancel",
                    "displayName": "b.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": payload.len(),
                    "sha256": sha256_of(&payload),
                }),
                Some("key-cancel-create-b"),
                None,
            )
            .await
            .unwrap();
        let other_id = other["upload"]["uploadId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "uploads.part",
            part_frame(&other_id, 0, &payload),
            Some("key-cancel-part-b-0"),
            None,
        )
        .await
        .unwrap();

        let (_, cancelled) = rt
            .dispatch(
                "uploads.cancel",
                json!({"uploadId": upload_id}),
                Some("key-cancel-cancel"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("uploads.cancel", &cancelled)
            .unwrap();
        assert_eq!(cancelled["state"], json!("cancelled"));
        assert_eq!(cancelled["temporaryBytesReleased"], json!(1024));
        assert!(!upload_dir(rt.store(), &upload_id).exists());
        // 重复取消安全（同 key 重试语义：结果幂等）。
        let (_, again) = rt
            .dispatch(
                "uploads.cancel",
                json!({"uploadId": upload_id}),
                Some("key-cancel-cancel"),
                None,
            )
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
                Some("key-cancel-part-after-cancel"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "UPLOAD_CLOSED");
        let err = rt
            .dispatch(
                "uploads.complete",
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

        // partSize 由服务端定为 512 KiB：声明 3 块、只交第 0 块，才能走到
        // complete 的 MISSING_PARTS（块字节必须与声明长度精确匹配）。
        let payload = vec![7u8; 512 * 1024];
        let (_, created) = rt
            .dispatch(
                "uploads.create",
                json!({
                    "projectId": "proj-guard",
                    "displayName": "g.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": payload.len() * 3,
                    "sha256": sha256_of(&payload),
                }),
                Some("key-guard-create"),
                None,
            )
            .await
            .unwrap();
        let upload_id = created["upload"]["uploadId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "uploads.part",
            part_frame(&upload_id, 0, &payload),
            Some("key-guard-part-0"),
            None,
        )
        .await
        .unwrap();
        // 只交 1/3 块 → MISSING_PARTS。
        let err = rt
            .dispatch(
                "uploads.complete",
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
                "uploads.create",
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
                "uploads.create",
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

    /// parts.log 逐行严格解析：交错/拼接出的坏行会让 `read_parts_log` 静默
    /// continue（→ 永久 MISSING_PARTS），必须在这里炸出来而不是被吞掉。
    fn parts_log_lines(rt: &Arc<PublicV1Runtime>, upload_id: &str) -> Vec<Value> {
        let log = upload_dir(rt.store(), upload_id).join("parts.log");
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<Value>(line).expect("well-formed parts.log line"))
            .collect()
    }

    async fn create_upload_for(
        rt: &Arc<PublicV1Runtime>,
        project_id: &str,
        key: &str,
        whole: &[u8],
    ) -> String {
        let (_, created) = rt
            .dispatch(
                "uploads.create",
                json!({
                    "projectId": project_id,
                    "displayName": "concurrent.bin",
                    "mediaType": "application/octet-stream",
                    "byteLength": whole.len(),
                    "sha256": sha256_of(whole),
                }),
                Some(key),
                None,
            )
            .await
            .unwrap();
        created["upload"]["uploadId"].as_str().unwrap().to_owned()
    }

    /// P1（并发 #6）：accept_part 无串行化时——并发不同号块的 `writeln!` 经
    /// write_fmt 拆成多次 write，可在 token 边界拼出坏行；并发同号块撞同名
    /// `.part-N.tmp`（EEXIST → UNAVAILABLE 而非 dedup）；并发同号异字节不会
    /// 恰好产生一个 PART_CONFLICT。per-upload 互斥 + 单条 write_all 后三者都
    /// 有确定结果。
    #[tokio::test]
    async fn concurrent_parts_are_serialized_per_upload() {
        let tmp = TempDir::new("uploads-race");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-race");

        let part0 = vec![1u8; 512 * 1024];
        let part1 = vec![2u8; 100];
        let mut whole = Vec::new();
        whole.extend_from_slice(&part0);
        whole.extend_from_slice(&part1);

        // (a) 并发不同号块：两行日志各自完整（严格解析），随后 complete 成功。
        let upload_a = create_upload_for(&rt, "proj-race", "key-race-a", &whole).await;
        let (a0, a1) = tokio::join!(
            rt.dispatch(
                "uploads.part",
                part_frame(&upload_a, 0, &part0),
                Some("key-race-a-p0"),
                None,
            ),
            rt.dispatch(
                "uploads.part",
                part_frame(&upload_a, 1, &part1),
                Some("key-race-a-p1"),
                None,
            ),
        );
        let (_, accepted0) = a0.expect("concurrent distinct parts both accepted");
        let (_, accepted1) = a1.expect("concurrent distinct parts both accepted");
        assert_eq!(accepted0["deduplicated"], json!(false));
        assert_eq!(accepted1["deduplicated"], json!(false));
        let lines = parts_log_lines(&rt, &upload_a);
        let mut numbers: Vec<u64> = lines
            .iter()
            .map(|l| l["partNumber"].as_u64().unwrap())
            .collect();
        numbers.sort();
        assert_eq!(numbers, vec![0, 1], "one well-formed line per part");
        let dir = upload_dir(rt.store(), &upload_a);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left: {leftovers:?}");
        // 端到端：拼装按日志进行，坏行会导致永久 MISSING_PARTS。
        rt.dispatch(
            "uploads.complete",
            json!({"uploadId": upload_a, "expectedSha256": sha256_of(&whole)}),
            Some("key-race-a-complete"),
            None,
        )
        .await
        .unwrap();
        let mut completed = false;
        for _ in 0..100 {
            let (_, got) = rt
                .dispatch("uploads.get", json!({"uploadId": upload_a}), None, None)
                .await
                .unwrap();
            if got["upload"]["state"] == json!("complete") {
                completed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(completed, "concurrent parts assemble into the artifact");

        // (b) 并发同号同字节：两个都成功，恰一个真正落盘（dedup 旗帜互补），
        // 日志只有一行。
        let upload_b = create_upload_for(&rt, "proj-race", "key-race-b", &whole).await;
        let (b0, b1) = tokio::join!(
            rt.dispatch(
                "uploads.part",
                part_frame(&upload_b, 0, &part0),
                Some("key-race-b-p0-first"),
                None,
            ),
            rt.dispatch(
                "uploads.part",
                part_frame(&upload_b, 0, &part0),
                Some("key-race-b-p0-second"),
                None,
            ),
        );
        let (_, first) = b0.expect("same-part same-bytes: one wins the write");
        let (_, second) = b1.expect("same-part same-bytes: the other dedups");
        let mut flags = [
            first["deduplicated"].as_bool().unwrap(),
            second["deduplicated"].as_bool().unwrap(),
        ];
        flags.sort();
        assert_eq!(flags, [false, true], "exactly one real write");
        let lines = parts_log_lines(&rt, &upload_b);
        assert_eq!(lines.len(), 1, "no duplicate confirmation lines");
        assert_eq!(lines[0]["partNumber"], json!(0));

        // (c) 并发同号异字节：恰一个成功、另一个 PART_CONFLICT（而不是
        // 临时文件 EEXIST → UNAVAILABLE）。
        let upload_c = create_upload_for(&rt, "proj-race", "key-race-c", &whole).await;
        let variant_a = vec![7u8; 100];
        let variant_b = vec![9u8; 100];
        let (c0, c1) = tokio::join!(
            rt.dispatch(
                "uploads.part",
                part_frame(&upload_c, 1, &variant_a),
                Some("key-race-c-p1-a"),
                None,
            ),
            rt.dispatch(
                "uploads.part",
                part_frame(&upload_c, 1, &variant_b),
                Some("key-race-c-p1-b"),
                None,
            ),
        );
        let outcomes = [c0, c1];
        let ok = outcomes.iter().filter(|r| r.is_ok()).count();
        let conflicts = outcomes
            .iter()
            .filter(|r| matches!(r, Err(e) if contract_code(e) == "PART_CONFLICT"))
            .count();
        assert_eq!(ok, 1, "exactly one variant wins: {outcomes:?}");
        assert_eq!(conflicts, 1, "the other is a clean PART_CONFLICT");
        let lines = parts_log_lines(&rt, &upload_c);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["partNumber"], json!(1));
    }

    /// P1（并发 #6 的第三面）：rename(part-N) 与日志追加之间崩溃留下的
    /// 「块已落盘、日志无行」没有修复路径——重传命中 dedup 提前返回却不补
    /// 行，complete 永远 MISSING_PARTS。dedup 命中时必须补写日志行。
    #[tokio::test]
    async fn dedup_backfills_missing_log_line() {
        let tmp = TempDir::new("uploads-backfill");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-backfill");

        let payload = vec![5u8; 2048];
        let upload_id = create_upload_for(&rt, "proj-backfill", "key-backfill", &payload).await;

        // 崩溃窗口模拟：part-0 已落盘（rename 完成）、parts.log 行缺失。
        std::fs::write(upload_dir(rt.store(), &upload_id).join("part-0"), &payload).unwrap();
        let (_, got) = rt
            .dispatch("uploads.get", json!({"uploadId": upload_id}), None, None)
            .await
            .unwrap();
        assert_eq!(
            got["upload"]["receivedParts"],
            json!([]),
            "the journal (not the part file) is the confirmation surface"
        );
        // 此状态下 complete 只能 MISSING_PARTS。
        let err = rt
            .dispatch(
                "uploads.complete",
                json!({"uploadId": upload_id, "expectedSha256": sha256_of(&payload)}),
                Some("key-backfill-complete-early"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "MISSING_PARTS");

        // 重传同块同字节 → dedup 命中 + 日志补行。
        let (_, retransmit) = rt
            .dispatch(
                "uploads.part",
                part_frame(&upload_id, 0, &payload),
                Some("key-backfill-retransmit"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(retransmit["deduplicated"], json!(true));
        let lines = parts_log_lines(&rt, &upload_id);
        assert_eq!(lines.len(), 1, "the confirmation line is backfilled");
        assert_eq!(lines[0]["partNumber"], json!(0));

        // 补行后 complete 成功（修复路径闭环）。
        rt.dispatch(
            "uploads.complete",
            json!({"uploadId": upload_id, "expectedSha256": sha256_of(&payload)}),
            Some("key-backfill-complete"),
            None,
        )
        .await
        .unwrap();
        let mut completed = false;
        for _ in 0..100 {
            let (_, got) = rt
                .dispatch("uploads.get", json!({"uploadId": upload_id}), None, None)
                .await
                .unwrap();
            if got["upload"]["state"] == json!("complete") {
                completed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(completed, "backfilled journal lets complete succeed");
    }

    #[tokio::test]
    async fn validating_cache_expired_part_replay_and_expired_create_commit() {
        let tmp = TempDir::new("uploads-p1-11");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-u11");

        // 1) uploads.complete 完成后，以新 idempotencyKey 再调 uploads.complete：
        //    必须立即落成 async_operations state=succeeded + 完整 completionResult{upload, artifact}
        //    （不可永远停留在 running + result_json=NULL）；错 sha 报 CHECKSUM_MISMATCH。
        let payload = vec![7u8; 1024];
        let sha = sha256_of(&payload);
        let upload_id = create_upload_for(&rt, "proj-u11", "key-u11-create", &payload).await;
        rt.dispatch(
            "uploads.part",
            part_frame(&upload_id, 0, &payload),
            Some("key-u11-part0"),
            None,
        )
        .await
        .unwrap();
        let (_, complete_receipt) = rt
            .dispatch(
                "uploads.complete",
                json!({"uploadId": upload_id, "expectedSha256": sha}),
                Some("key-u11-complete1"),
                None,
            )
            .await
            .unwrap();
        let first_op_id = complete_receipt["operationId"].as_str().unwrap().to_owned();
        let mut complete_done = false;
        for _ in 0..100 {
            let (_, got) = rt
                .dispatch("uploads.get", json!({"uploadId": upload_id}), None, None)
                .await
                .unwrap();
            if got["upload"]["state"] == json!("complete") {
                complete_done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(complete_done);

        let wrong_sha = "f".repeat(64);
        let wrong_err = rt
            .dispatch(
                "uploads.complete",
                json!({"uploadId": upload_id, "expectedSha256": wrong_sha}),
                Some("key-u11-complete-wrong-sha"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&wrong_err), "CHECKSUM_MISMATCH");

        let (_, replay_receipt) = rt
            .dispatch(
                "uploads.complete",
                json!({"uploadId": upload_id, "expectedSha256": sha}),
                Some("key-u11-complete2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay_receipt["state"], json!("succeeded"));
        let replay_op_id = replay_receipt["operationId"].as_str().unwrap();
        assert_ne!(replay_op_id, first_op_id);
        let (_, op_got) = rt
            .dispatch(
                "operations.get",
                json!({"operationId": replay_op_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(op_got["operation"]["state"], json!("succeeded"));
        let completion = &op_got["operation"]["result"];
        assert_eq!(completion["upload"]["uploadId"], json!(upload_id));
        assert_eq!(completion["upload"]["state"], json!("complete"));
        assert_eq!(completion["artifact"]["sha256"], json!(sha));

        // 2) 已过期的 upload 不得继续被 uploads.part 接受，且重放 uploads.create 必须持久 commit expired 并释放配额。
        let exp_payload = vec![9u8; 512];
        let exp_id = create_upload_for(&rt, "proj-u11", "key-u11-exp-create", &exp_payload).await;
        let past_ts = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        {
            let conn = rt.store().conn.lock().unwrap();
            conn.execute(
                "UPDATE uploads SET expires_at = ?1 WHERE upload_id = ?2",
                params![past_ts, exp_id],
            )
            .unwrap();
            if let Some(meta) = meta_cache().write().unwrap().get_mut(&exp_id) {
                meta.expires_at = parse_expires_at(&past_ts);
            }
        }
        let part_err = rt
            .dispatch(
                "uploads.part",
                part_frame(&exp_id, 0, &exp_payload),
                Some("key-u11-part-expired"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&part_err), "UPLOAD_CLOSED");

        // 重置为 receiving+past_ts 专门测 uploads.create 幂等重放必定 commit expired（不回滚事务）。
        let exp_id2 = create_upload_for(&rt, "proj-u11", "key-u11-exp-replay", &exp_payload).await;
        {
            let conn = rt.store().conn.lock().unwrap();
            conn.execute(
                "UPDATE uploads SET expires_at = ?1 WHERE upload_id = ?2",
                params![past_ts, exp_id2],
            )
            .unwrap();
        }
        let (_, replayed_create) = rt
            .dispatch(
                "uploads.create",
                json!({
                    "projectId": "proj-u11",
                    "displayName": "tiny.txt",
                    "mediaType": "application/octet-stream",
                    "byteLength": exp_payload.len(),
                    "sha256": sha256_of(&exp_payload),
                }),
                Some("key-u11-exp-replay"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replayed_create["upload"]["state"], json!("expired"));
        // 验证 SQLite 中真正持久化为 expired，而不是仅删目录+更新内存缓存却回滚事务。
        let db_state: String = {
            let conn = rt.store().conn.lock().unwrap();
            conn.query_row(
                "SELECT state FROM uploads WHERE upload_id = ?1",
                params![exp_id2],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(db_state, "expired");
    }

    #[tokio::test]
    async fn sweep_cancel_and_startup_recovery_clean_assemble_staging_and_validating() {
        let tmp = TempDir::new("uploads-p1-12");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        seed_project(&rt, "proj-u12");

        let payload = vec![3u8; 512];
        let u1 = create_upload_for(&rt, "proj-u12", "key-u12-1", &payload).await;
        let staging1 = staging_path(rt.store(), &u1);
        std::fs::create_dir_all(uploads_root(rt.store())).unwrap();
        std::fs::write(&staging1, b"stale staging bytes").unwrap();
        assert!(staging1.exists());

        // cancel 必须顺带删掉 .assemble-<uploadId>。
        rt.dispatch(
            "uploads.cancel",
            json!({"uploadId": u1}),
            Some("key-u12-cancel"),
            None,
        )
        .await
        .unwrap();
        assert!(
            !staging1.exists(),
            "cancel must delete .assemble-* staging file"
        );

        // sweep_uploads 必须清理非 validating 或无行残留的 .assemble-* 暂存文件。
        let orphan_staging = staging_path(rt.store(), "u-cancelled-leftover");
        std::fs::write(&orphan_staging, b"orphan").unwrap();
        {
            let conn = rt.store().conn.lock().unwrap();
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO uploads (upload_id, project_id, state, byte_length, part_size, total_sha256, media_type, display_name, artifact_id, revision, expires_at, created_at, updated_at)
                 VALUES ('u-cancelled-leftover', 'proj-u12', 'cancelled', 6, 6, ?1, 'application/octet-stream', 'x', NULL, 'rev-1', ?2, ?2, ?2)",
                params![sha256_of(b"orphan"), now],
            )
            .unwrap();
        }
        sweep_uploads(rt.store());
        assert!(
            !orphan_staging.exists(),
            "sweep must remove cancelled .assemble-* file"
        );

        // recover_orphaned_uploads_and_async_ops：非终态 async_operations -> interrupted +
        // validating upload -> receiving + 清全部 .assemble-*。
        let u2 = create_upload_for(&rt, "proj-u12", "key-u12-2", &payload).await;
        let staging2 = staging_path(rt.store(), &u2);
        std::fs::write(&staging2, b"crash leftover").unwrap();
        {
            let mut conn = rt.store().conn.lock().unwrap();
            conn.execute(
                "UPDATE uploads SET state = 'validating' WHERE upload_id = ?1",
                params![u2],
            )
            .unwrap();
            create_async_operation(&conn, "uploads.complete", &u2, "op-crash-1").unwrap();
            let tx = conn.transaction().unwrap();
            recover_orphaned_uploads_and_async_ops(rt.store().home(), &tx, &now_rfc3339()).unwrap();
            tx.commit().unwrap();
        }
        assert!(!staging2.exists(), "recovery must delete .assemble-*");
        let (op_state, err_code): (String, Option<String>) = {
            let conn = rt.store().conn.lock().unwrap();
            conn.query_row(
                "SELECT state, error_code FROM async_operations WHERE operation_id = 'op-crash-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(op_state, "interrupted");
        assert_eq!(err_code.as_deref(), Some("GATEWAY_RESTART"));
        let u2_state: String = {
            let conn = rt.store().conn.lock().unwrap();
            conn.query_row(
                "SELECT state FROM uploads WHERE upload_id = ?1",
                params![u2],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(u2_state, "receiving");
    }
}
