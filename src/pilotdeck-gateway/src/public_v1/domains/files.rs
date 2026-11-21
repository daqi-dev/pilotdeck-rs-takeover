//! `files.*` — 项目文件读取与受限写入（t013，S3）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `files.*` 条目（行 31389–32940）。
//! TS 基线（cc7c4e6 `ui/server/routes/projects.js` + `utils/pathSafety.js`）是
//! 语义参考；防符号链接竞态的底层原语复用旧 Rust 线 `project_files.rs`
//! （open_beneath / read_at / overwrite_at / unlink_at / remove_tree_at /
//! open_directory_beneath，A4-FILE-TOCTOU-001 加固，仅 crate 内可达）。
//!
//! 路径解析（`ProjectPath{projectId, relativePath}` → 文件系统）：
//! `projects.root_json{rootId, relativePath}` → `authorized_roots` 表（t012 合
//! 同面；本域登记同形 DDL 并只消费）→ canonical 授权根 → 工作区目录
//! （`<authorized_root>/<project relativePath>`，lexical 归一 + canonical 复验
//! 不得越根）→ 调用方 `relativePath` 再 lexical 归一（绝对路径/越界 →
//! PATH_ESCAPE）。读取经 canonicalize 后做边界复核，写入经「暂存完整字节 →
//! 复验 revision → fsync → rename」原子替换；叶子访问用 NOFOLLOW 原语，链接
//! 逃逸拒绝（合同验收：不通过符号链接访问授权根之外）。
//!
//! 对象身份（stat/list/delete 共识）：操作对象是请求路径的**叶子**——叶子
//! 是符号链接时身份就是链接本身（`lrev-`，与 list 同源；合同 files.stat 验收
//! 「符号链接自身与其解析目标状态可区分」）。mid-path 链接经 canonical 父
//! 目录复核不越工作区（越界 → PATH_ESCAPE）。delete 对链接只删链接本身，
//! 不触碰目标（合同 files.delete 验收：禁止删除项目根或逃逸链接目标）。
//! 读数据面（files.read）例外：跟随根内链接读目标字节，但字节与哈希全部经
//! pinned 工作区句柄 NOFOLLOW 打开（按 canonical 路径 open/hash 会跟随竞态
//! 换入的链接）。delete/move 的 revision 前提与删除/移动动作同在写门内
//! （与 write_file 同构：门内重算，门外结论一律丢弃）。捕获台账登记（快照
//! 链 A1/A6b，dsh r3 P1#1）同样在写门内：落盘→登记→事件→即时聚合一个
//! 门临界区完成，台账 seq 与文件系统变更时刻同序；归属由调用方显式传入
//! （P1#3：driver 路径 Some(run_id)，CLI 请求 None）。
//!
//! `revision` 是内容相关身份：文件 = `sha256(len || bytes)`（同大小同 mtime
//! 字节改变必须产生不同 revision，ops.json:31628 验收），目录 = `sha256(dev ||
//! ino || mtime_ns || 排序子项名)`，符号链接 = `sha256(target 字节)`。大文件每
//! 次 stat/read 全量哈希是登记在案的实现成本（brief §7 风险 4）。
//!
//! `authorized_roots` 行由宿主声明：t012 的 `directories.*`（default 根惰性
//! bootstrap）或 [`V1Store::declare_authorized_root`]；e2e 走 `directories
//! create`（见 `tests/e2e_v1_files_uploads.rs`）。

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use base64::Engine;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::snapshots::{
    CapturedFileChange, capture_unowned_entries, insert_capture_ledger,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

// Domain-registered DDL, executed by `V1Store::init_schema` (S3 substrate
// rule: domains add tables through `reg.schema` only). The
// `authorized_roots` table's owner is t012 (directories.*); this domain
// registers the shared draft shape so its consumption code works before
// t012's merged version lands — `CREATE TABLE IF NOT EXISTS` keeps both
// registrations compatible.
// `authorized_roots` 表归 t012（`domains/workspace_dirs.rs`）所有并登记 DDL
// （含 `source` 溯源列）。本域**只消费**（rootId → canonical 绝对路径），不
// 注册该表的 DDL——两份 `IF NOT EXISTS` DDL 竞争会以先注册者的形状建表，
// 本域早前草案形状缺 `source` 列会让 t012 的溯源 UPDATE 失败。

/// Contract error codes that live only in the `files.*` error tables (they are
/// not part of the protocol crate's registered common codes).
pub(crate) const CODE_PATH_ESCAPE: &str = "PATH_ESCAPE";
pub(crate) const CODE_NOT_A_DIRECTORY: &str = "NOT_A_DIRECTORY";
pub(crate) const CODE_NOT_A_FILE: &str = "NOT_A_FILE";
pub(crate) const CODE_VIRTUAL_PROJECT: &str = "VIRTUAL_PROJECT";
pub(crate) const CODE_DISK_FULL: &str = "DISK_FULL";
pub(crate) const CODE_PARTIAL_FAILURE: &str = "PARTIAL_FAILURE";
pub(crate) const CODE_DIRECTORY_NOT_EMPTY: &str = "DIRECTORY_NOT_EMPTY";
pub(crate) const CODE_CROSS_PROJECT_MOVE: &str = "CROSS_PROJECT_MOVE_UNSUPPORTED";
pub(crate) const CODE_UNSUPPORTED_ATOMIC_MOVE: &str = "UNSUPPORTED_ATOMIC_MOVE";

/// 单次 files.read 响应携带的字节上限。与 `uploads.limits.maxPartBytes`
/// 同值（512 KiB）：base64 膨胀 4/3 后仍在 1 MiB 帧预算内（ws.rs 帧上限，
/// 007 §6.1）。请求更大 maxBytes 时按此上限截断返回，`chunk.byteLength` /
/// `eof` 如实反映。
pub(crate) const FILE_CHUNK_MAX_BYTES: usize = 512 * 1024;

/// 列表分页默认/上限（PageInput limit 默认 100，与 store::parse_page 同钳制）。
const LIST_PAGE_LIMIT: usize = 500;

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("files.list", |rt, input, _key, _rev| {
        Box::pin(files_list(rt, input))
    });
    reg.handler("files.stat", |rt, input, _key, _rev| {
        Box::pin(files_stat(rt, input))
    });
    reg.handler("files.read", |rt, input, _key, _rev| {
        Box::pin(files_read(rt, input))
    });
    reg.handler("files.write", |rt, input, _key, _rev| {
        Box::pin(files_write(rt, input))
    });
    reg.handler("files.create_directory", |rt, input, _key, _rev| {
        Box::pin(files_create_directory(rt, input))
    });
    reg.handler("files.move", |rt, input, _key, _rev| {
        Box::pin(files_move(rt, input))
    });
    reg.handler("files.delete", |rt, input, _key, _rev| {
        Box::pin(files_delete(rt, input))
    });
}

// ---------------------------------------------------------------------------
// BytePath codec（files/uploads/documents 共用）
// ---------------------------------------------------------------------------

/// 解码合同 `BytePath{encoding, value}` 为原始路径字节。utf8 编码的值必须
/// 是合法 UTF-8（有损替换被禁止）；base64 解码失败或字节含 NUL 拒绝。
pub(crate) fn decode_byte_path(value: &Value) -> Result<Vec<u8>, ContractError> {
    let err = |msg: String| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            msg,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let encoding = value
        .get("encoding")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let raw = value
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let bytes = match encoding {
        "utf8" => raw.as_bytes().to_vec(),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(raw)
            .map_err(|e| err(format!("relativePath base64 decode failed: {e}")))?,
        other => {
            return Err(err(format!("unsupported BytePath encoding `{other}`")));
        }
    };
    if bytes.contains(&0) {
        return Err(err("path bytes must not contain NUL".to_owned()));
    }
    Ok(bytes)
}

/// 把原始路径字节编码回合同 `BytePath`：可无损 UTF-8 时用 utf8，否则 base64。
pub(crate) fn byte_path_json(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => json!({"encoding": "utf8", "value": text}),
        Err(_) => json!({
            "encoding": "base64",
            "value": base64::engine::general_purpose::STANDARD.encode(bytes)
        }),
    }
}

#[cfg(unix)]
pub(crate) fn os_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
pub(crate) fn os_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

// ---------------------------------------------------------------------------
// 路径代数：lexical 归一 + 边界
// ---------------------------------------------------------------------------

/// `root.join(rel)` 的 lexical 归一（Node path.resolve 语义，参考
/// metadata.rs::resolve_path）：拒绝绝对路径与越过 root 的 `..`。
pub(crate) fn lexical_join_under(root: &Path, rel: &[u8]) -> Result<PathBuf, ContractError> {
    let escape = || {
        ContractError::new(
            CODE_PATH_ESCAPE,
            format!(
                "relative path escapes the project workspace under `{}`",
                root.display()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let rel_os = os_from_bytes(rel);
    let mut out = root.to_path_buf();
    for component in Path::new(&rel_os).components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
                if !out.starts_with(root) {
                    return Err(escape());
                }
            }
            _ => return Err(escape()),
        }
    }
    if !out.starts_with(root) {
        return Err(escape());
    }
    Ok(out)
}

/// canonical 路径必须仍在 `root` 之下（符号链接解析后复核；越界 → PATH_ESCAPE）。
pub(crate) fn require_within(root: &Path, candidate: &Path) -> Result<(), ContractError> {
    if candidate.starts_with(root) {
        Ok(())
    } else {
        Err(ContractError::new(
            CODE_PATH_ESCAPE,
            format!(
                "resolved path `{}` escapes the authorized root `{}`",
                candidate.display(),
                root.display()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
    }
}

// ---------------------------------------------------------------------------
// 项目根解析（projectId → 授权根 + 工作区）
// ---------------------------------------------------------------------------

pub(crate) struct ProjectWorkspace {
    pub project_id: String,
    /// canonical 授权根（authorized_roots.absolute_path，声明时 canonical 化）。
    pub root: PathBuf,
    /// canonical 工作区目录（project 的 root.relativePath 解析结果）。
    pub workspace: PathBuf,
}

/// 项目无法解析为真实工作区的两类原因。`Virtual` 只在合同错误表列有
/// VIRTUAL_PROJECT 的操作（files.list）映射为该码；其余操作按 NOT_FOUND
/// 报告（虚拟项目里文件本就不存在，也与「不将 home 当项目根」一致）。
#[derive(Debug)]
pub(crate) enum ProjectResolveError {
    NotFound(String),
    Virtual(String),
    Contract(ContractError),
}

impl From<ProjectResolveError> for ContractError {
    /// 缺省映射：虚拟项目（无宿主声明根）对 files.stat/read/write/… 报
    /// NOT_FOUND——VIRTUAL_PROJECT 只在 files.list 的合同错误表里；列表专属
    /// 的 VIRTUAL_PROJECT 映射在 [`resolve_project_list`]。
    fn from(value: ProjectResolveError) -> Self {
        match value {
            ProjectResolveError::NotFound(msg) => ContractError::new(
                CODE_NOT_FOUND,
                msg,
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ),
            ProjectResolveError::Virtual(msg) => ContractError::new(
                CODE_NOT_FOUND,
                format!("virtual project has no file workspace: {msg}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ),
            ProjectResolveError::Contract(err) => err,
        }
    }
}

pub(crate) fn resolve_project(
    store: &V1Store,
    project_id: &str,
) -> Result<ProjectWorkspace, ProjectResolveError> {
    let root_json: Option<String> = {
        let conn = store.conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT root_json FROM projects WHERE project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| ProjectResolveError::Contract(sql_err(e)))?
    };
    let Some(root_json) = root_json else {
        return Err(ProjectResolveError::NotFound(format!(
            "project `{project_id}` not found"
        )));
    };
    let root: Value = serde_json::from_str(&root_json).map_err(|e| {
        ProjectResolveError::Contract(sql_err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::io::Error::other(e.to_string()),
        ))))
    })?;
    let root_id = root
        .get("rootId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let workspace_rel = root
        .get("relativePath")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // authorized_roots 消费（ServerPath 合同语义：rootId → canonical 绝对路径）。
    let declared: Option<String> = {
        let conn = store.conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT absolute_path FROM authorized_roots WHERE root_id = ?1",
            params![root_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| ProjectResolveError::Contract(sql_err(e)))?
    };
    let Some(declared) = declared else {
        return Err(ProjectResolveError::Virtual(format!(
            "project `{project_id}` has no host-declared authorized root `{root_id}` \
             (virtual project); the instance home is never used as a project root"
        )));
    };
    let declared_path = PathBuf::from(&declared);
    // 防根替换（借鉴 upload/mod.rs plain_root）：声明记录必须是普通目录且
    // canonical 结果与声明一致，符号链接化的根拒绝。
    let plain = std::fs::symlink_metadata(&declared_path)
        .map_err(|_| {
            ProjectResolveError::Contract(ContractError::new(
                CODE_UNAVAILABLE,
                format!("authorized root `{root_id}` is missing on disk"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::NotApplied,
            ))
        })
        .map(|m| m.is_dir())?;
    if !plain {
        return Err(ProjectResolveError::Contract(ContractError::new(
            CODE_UNAVAILABLE,
            format!("authorized root `{root_id}` is not a plain directory"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    let root_abs = std::fs::canonicalize(&declared_path).map_err(|_| {
        ProjectResolveError::Contract(ContractError::new(
            CODE_UNAVAILABLE,
            format!("authorized root `{root_id}` cannot be canonicalized"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::NotApplied,
        ))
    })?;
    // 声明可能非 canonical（t012 的 default bootstrap 存 home 相对拼接的
    // 路径；macOS 临时目录的祖先本就含符号链接）。以 canonicalize 结果为
    // 权威根——防的是「根本身被换成链接」（上面的普通目录检查已拒），
    // 祖先路径形态不在防御面（与旧线 upload/mod.rs plain_root 同界）。

    let workspace = if workspace_rel.is_empty() {
        root_abs.clone()
    } else {
        let joined = lexical_join_under(&root_abs, workspace_rel.as_bytes()).map_err(|err| {
            ProjectResolveError::Contract(ContractError::new(
                CODE_PATH_ESCAPE,
                format!(
                    "project `{project_id}` workspace escapes its authorized root: {}",
                    err.message
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        })?;
        let canonical = std::fs::canonicalize(&joined).map_err(|_| {
            ProjectResolveError::NotFound(format!(
                "project `{project_id}` workspace `{}` not found",
                joined.display()
            ))
        })?;
        require_within(&root_abs, &canonical).map_err(ProjectResolveError::Contract)?;
        canonical
    };

    Ok(ProjectWorkspace {
        project_id: project_id.to_owned(),
        root: root_abs,
        workspace,
    })
}

/// 请求目标：工作区 + 调用方相对路径的 lexical 归一（尚未 canonical 化）。
pub(crate) fn request_target(
    project: &ProjectWorkspace,
    relative: &[u8],
) -> Result<PathBuf, ContractError> {
    lexical_join_under(&project.workspace, relative)
}

/// 请求路径叶子的对象路径（NOFOLLOW）：叶子本身不解析——是符号链接时身份
/// 就是链接（与 files.list 的 `entry.path()` + `symlink_metadata` 同源，合同
/// files.stat 验收「符号链接自身与其解析目标状态可区分」）；mid-path 链接
/// 由 canonical 父目录解析并复核不越工作区（越界 → PATH_ESCAPE）。工作区
/// 根自身没有独立的叶子/父代，直接返回。写门 key 与 pinned 读取均以此为
/// 对象身份（[`with_write_gates`] / [`open_file_canonical`]）。
pub(crate) fn leaf_identity_path(
    project: &ProjectWorkspace,
    target: &Path,
) -> Result<PathBuf, ContractError> {
    if target == project.workspace {
        return Ok(project.workspace.clone());
    }
    let no_parent = || {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "target path has no parent directory",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let parent = target.parent().ok_or_else(no_parent)?;
    let leaf = target.file_name().ok_or_else(no_parent)?;
    let parent_canonical = std::fs::canonicalize(parent).map_err(io_not_found)?;
    require_within(&project.workspace, &parent_canonical)?;
    Ok(parent_canonical.join(leaf))
}

/// 文件内容 revision（documents 等域计算源 revision 用）。
pub(crate) fn file_revision_of(content_sha: &str, len: u64) -> String {
    file_revision(content_sha, len)
}

// ---------------------------------------------------------------------------
// revision / entry 构建
// ---------------------------------------------------------------------------

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 文件内容身份：`sha256("file" || len_le || bytes)`。长度参与防
/// 同哈希前缀歧义；内容全量参与保证「同大小同 mtime 异字节」可分辨。
fn file_revision(content_sha: &str, len: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"file:");
    hasher.update(len.to_le_bytes());
    hasher.update(content_sha.as_bytes());
    format!("frev-{:x}", hasher.finalize())
}

fn symlink_revision(target: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"link:");
    hasher.update(target);
    format!("lrev-{:x}", hasher.finalize())
}

fn directory_revision(dev: u64, ino: u64, mtime_ns: i64, names: &[Vec<u8>]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dir:");
    hasher.update(dev.to_le_bytes());
    hasher.update(ino.to_le_bytes());
    hasher.update(mtime_ns.to_le_bytes());
    for name in names {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name);
    }
    format!("drev-{:x}", hasher.finalize())
}

#[cfg(all(test, unix))]
fn stat_mtime_ns(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

#[cfg(all(test, not(unix)))]
fn stat_mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// 可读/可写探测（faccessat EACCESS，作用于 pinned 父目录句柄）。
#[cfg(unix)]
fn access_flags(parent: &File, name: &std::ffi::OsStr, write: bool) -> bool {
    use rustix::fs::{Access, AtFlags, accessat};
    let flags = if write {
        Access::WRITE_OK
    } else {
        Access::READ_OK
    };
    accessat(parent, name, flags, AtFlags::EACCESS).is_ok()
}

#[cfg(not(unix))]
fn access_flags(_parent: &File, _name: &std::ffi::OsStr, _write: bool) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Pinned-fd NOFOLLOW 原语（files + 跨模块 documents/history_archive/Batch 5 共用）
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn dir_search_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    #[cfg(target_os = "linux")]
    let access = OFlags::PATH;
    #[cfg(target_vendor = "apple")]
    let access = OFlags::from_bits_retain(libc::O_SEARCH as u32);
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    let access = OFlags::RDONLY;
    access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

/// 在已固定的目录描述符下以 `O_NOFOLLOW | O_DIRECTORY` 打开子目录（不跟随符号链接）。
/// 若组件实际为符号链接（含 macOS 上报 `ENOTDIR` 的场景），统一转为 `ELOOP`。
#[cfg(unix)]
pub(crate) fn open_dir_nofollow(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<File> {
    use rustix::fs::{AtFlags, FileType, Mode, openat, statat};
    match openat(parent, name, dir_search_flags(), Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(err) => {
            if matches!(err, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
                && let Ok(stat) = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                && FileType::from_raw_mode(stat.st_mode) == FileType::Symlink
            {
                return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
            }
            Err(err.into())
        }
    }
}

/// 在已固定的目录描述符下打开可读目录句柄（`O_RDONLY | O_DIRECTORY | O_NOFOLLOW`），
/// 供 `Dir::read_from` 与目录 `sync_all` 使用；`"."` 允许直接升级当前已固定句柄。
#[cfg(unix)]
pub(crate) fn open_dir_readable_nofollow(
    parent: &File,
    name: &std::ffi::OsStr,
) -> std::io::Result<File> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if name != std::ffi::OsStr::new(".") {
        flags |= OFlags::NOFOLLOW;
    }
    match openat(parent, name, flags, Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(err) => {
            if matches!(err, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
                && let Ok(stat) = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                && FileType::from_raw_mode(stat.st_mode) == FileType::Symlink
            {
                return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
            }
            Err(err.into())
        }
    }
}

/// 沿已固定目录句柄 `root` 逐级以 `O_NOFOLLOW | O_DIRECTORY` 走进 `relative`。
#[cfg(unix)]
pub(crate) fn walk_dir_beneath(root: &File, relative: &Path) -> Result<File, ContractError> {
    walk_dir_beneath_mapped(root, relative, io_not_found)
}

#[cfg(unix)]
fn walk_dir_beneath_for_dir_op(root: &File, relative: &Path) -> Result<File, ContractError> {
    walk_dir_beneath_mapped(root, relative, io_dir_not_found)
}

#[cfg(unix)]
fn walk_dir_beneath_mapped(
    root: &File,
    relative: &Path,
    map_err: fn(std::io::Error) -> ContractError,
) -> Result<File, ContractError> {
    let mut current = root.try_clone().map_err(map_err)?;
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                current = open_dir_nofollow(&current, name).map_err(map_err)?;
            }
            _ => {
                return Err(io_escape("relative path escapes pinned directory"));
            }
        }
    }
    Ok(current)
}

/// 打开并固定项目工作区目录句柄（通过 `project_files::open_directory_beneath`）。
pub(crate) fn pin_workspace_dir(project: &ProjectWorkspace) -> Result<File, ContractError> {
    crate::project_files::open_directory_beneath(&project.root, &project.workspace)
        .map_err(io_not_found)
}

/// 从已固定的 `root_dir` 走至 `relative_to_leaf` 的父目录并固定返回 `(parent_fd, leaf_name)`。
#[cfg(unix)]
pub(crate) fn pin_parent_dir(
    root_dir: &File,
    relative_to_leaf: &Path,
) -> Result<(File, std::ffi::OsString), ContractError> {
    let leaf = relative_to_leaf.file_name().ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "target path has no leaf component",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let parent_rel = relative_to_leaf.parent().unwrap_or_else(|| Path::new(""));
    let parent_fd = walk_dir_beneath(root_dir, parent_rel)?;
    Ok((parent_fd, leaf.to_os_string()))
}

/// 在已固定父目录句柄上以 `O_RDONLY | O_NOFOLLOW | O_NONBLOCK` 打开普通文件，
/// 并经 `fstat` 核实类型为 `RegularFile`（拒绝竞态换入的符号链接、FIFO 或目录）。
#[cfg(unix)]
pub(crate) fn open_regular_file_at(
    parent: &File,
    name: &std::ffi::OsStr,
) -> Result<File, ContractError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, statat};
    let fd = match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(err) => {
            if matches!(err, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
                && let Ok(stat) = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                && FileType::from_raw_mode(stat.st_mode) == FileType::Symlink
            {
                return Err(io_escape("target is a symlink (NOFOLLOW refused)"));
            }
            if err == rustix::io::Errno::ISDIR {
                return Err(ContractError::new(
                    CODE_NOT_A_FILE,
                    "target is a directory, not a regular file",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            return Err(io_not_found(err));
        }
    };
    let file = File::from(fd);
    let stat = fstat(&file).map_err(|e| io_unavailable(e.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(ContractError::new(
            CODE_NOT_A_FILE,
            "target is not a regular file",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(file)
}

/// 在已固定父目录句柄上流式读取有上限的普通文件字节（防止 FIFO 挂起、链接逃逸与 OOM）。
#[cfg(unix)]
pub(crate) fn read_file_bounded_at(
    parent: &File,
    name: &std::ffi::OsStr,
    max_bytes: u64,
) -> Result<Vec<u8>, ContractError> {
    let mut file = open_regular_file_at(parent, name)?;
    let meta = file.metadata().map_err(io_unavailable)?;
    if meta.len() > max_bytes {
        return Err(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_RESOURCE_LIMIT,
            format!("file exceeds maximum size limit of {max_bytes} bytes"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let mut bytes = Vec::with_capacity(meta.len().min(max_bytes) as usize);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_unavailable)?;
    if bytes.len() as u64 > max_bytes {
        return Err(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_RESOURCE_LIMIT,
            format!("file exceeds maximum size limit of {max_bytes} bytes"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(bytes)
}

/// 在已固定父目录句柄上流式哈希普通文件，返回 `(sha256_hex, byte_length)`。
#[cfg(unix)]
pub(crate) fn hash_regular_file_at(
    parent: &File,
    name: &std::ffi::OsStr,
) -> Result<(String, u64), ContractError> {
    let mut file = open_regular_file_at(parent, name)?;
    hash_open_file(&mut file)
}

fn hash_open_file(file: &mut File) -> Result<(String, u64), ContractError> {
    let mut hasher = Sha256::new();
    let mut length: u64 = 0;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_unavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        length += read as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), length))
}

/// 在已固定父目录句柄下以 `O_CREAT | O_EXCL | O_NOFOLLOW | O_NONBLOCK` 原子写文件并 `renameat`。
#[cfg(unix)]
pub(crate) fn atomic_write_at(
    parent: &File,
    name: &std::ffi::OsStr,
    bytes: &[u8],
    mode: u32,
) -> Result<(), ContractError> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
    let temp_name = std::ffi::OsString::from(format!(".agentcpm-tmp-{}", uuid::Uuid::new_v4()));
    struct UnlinkGuard<'a> {
        parent: &'a File,
        name: &'a std::ffi::OsStr,
        armed: bool,
    }
    impl Drop for UnlinkGuard<'_> {
        fn drop(&mut self) {
            if self.armed {
                let _ = unlinkat(self.parent, self.name, AtFlags::empty());
            }
        }
    }
    let mut guard = UnlinkGuard {
        parent,
        name: &temp_name,
        armed: false,
    };
    let fd = openat(
        parent,
        &temp_name,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::from_raw_mode(mode),
    )
    .map_err(|e| io_disk_or_unknown(e.into(), false))?;
    guard.armed = true;
    let mut file = File::from(fd);
    file.write_all(bytes)
        .map_err(|e| io_disk_or_unknown(e, false))?;
    file.sync_all().map_err(|e| io_disk_or_unknown(e, false))?;
    drop(file);
    renameat(parent, &temp_name, parent, name).map_err(|e| io_disk_or_unknown(e.into(), false))?;
    guard.armed = false;
    if let Ok(rd) = open_dir_readable_nofollow(parent, std::ffi::OsStr::new(".")) {
        let _ = rd.sync_all();
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) type CreatedDirEntry = (File, std::ffi::OsString);
#[cfg(unix)]
pub(crate) type CreatedDirChain = Vec<CreatedDirEntry>;
#[cfg(unix)]
pub(crate) type DirChainError = Box<(ContractError, CreatedDirChain)>;

/// 沿已固定 `root` 逐级以 `O_NOFOLLOW | O_DIRECTORY` 建目录，仅记录本请求实际
/// `mkdirat` 成功的 `(parent_fd, name)` 供精确回滚。
#[cfg(unix)]
pub(crate) fn create_dir_chain_at(
    root: &File,
    relative: &Path,
    mode: u32,
    fail_if_leaf_exists: bool,
) -> Result<CreatedDirChain, DirChainError> {
    let mut created = Vec::new();
    match create_dir_chain_at_inner(root, relative, mode, fail_if_leaf_exists, &mut created) {
        Ok(()) => Ok(created),
        Err(err) => Err(Box::new((err, created))),
    }
}

#[cfg(unix)]
fn create_dir_chain_at_inner(
    root: &File,
    relative: &Path,
    mode: u32,
    fail_if_leaf_exists: bool,
    created: &mut CreatedDirChain,
) -> Result<(), ContractError> {
    use rustix::fs::{AtFlags, Mode, mkdirat, statat};
    let components: Vec<std::ffi::OsString> = relative
        .components()
        .filter_map(|c| match c {
            Component::CurDir => None,
            Component::Normal(name) => Some(name.to_os_string()),
            _ => Some(std::ffi::OsString::new()),
        })
        .collect();
    if components.iter().any(|c| c.is_empty()) {
        return Err(io_escape("relative path contains forbidden component"));
    }
    let mut current = root.try_clone().map_err(io_dir_not_found)?;
    let total = components.len();
    for (idx, name) in components.into_iter().enumerate() {
        let is_last = idx + 1 == total;
        match open_dir_nofollow(&current, &name) {
            Ok(next) => {
                if is_last && fail_if_leaf_exists {
                    return Err(ContractError::new(
                        CODE_ALREADY_EXISTS,
                        format!("`{}` already exists", name.to_string_lossy()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                current = next;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match mkdirat(&current, &name, Mode::from_raw_mode(mode)) {
                    Ok(()) => {
                        if let Ok(parent_clone) = current.try_clone() {
                            created.push((parent_clone, name.clone()));
                        }
                    }
                    Err(rustix::io::Errno::EXIST) => {
                        if is_last && fail_if_leaf_exists {
                            return Err(ContractError::new(
                                CODE_ALREADY_EXISTS,
                                format!("`{}` already exists", name.to_string_lossy()),
                                RetrySpec::never(),
                                EffectStatus::NotApplied,
                            ));
                        }
                    }
                    Err(mkdir_err) => {
                        return Err(io_dir_not_found(mkdir_err));
                    }
                }
                current = open_dir_nofollow(&current, &name).map_err(io_dir_not_found)?;
            }
            Err(err) => {
                if is_last
                    && fail_if_leaf_exists
                    && statat(&current, &name, AtFlags::SYMLINK_NOFOLLOW).is_ok()
                {
                    return Err(ContractError::new(
                        CODE_ALREADY_EXISTS,
                        format!("`{}` already exists", name.to_string_lossy()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                return Err(io_dir_not_found(err));
            }
        }
    }
    Ok(())
}

/// 基于 pinned parent_fd 按逆序安全回滚本请求新建且仍为空的目录（绝不跟随符号链接）。
#[cfg(unix)]
pub(crate) fn rollback_created_dirs(created: &[CreatedDirEntry]) {
    use rustix::fs::{AtFlags, unlinkat};
    for (parent_fd, name) in created.iter().rev() {
        let _ = unlinkat(parent_fd, name, AtFlags::REMOVEDIR);
    }
}

#[cfg(unix)]
fn stat_mtime_ns_rustix(stat: &rustix::fs::Stat) -> i64 {
    i64::from(stat.st_mtime) * 1_000_000_000 + (stat.st_mtime_nsec as i64)
}

#[cfg(unix)]
fn stat_rfc3339_rustix(stat: &rustix::fs::Stat) -> String {
    chrono::DateTime::from_timestamp(i64::from(stat.st_mtime), stat.st_mtime_nsec as u32)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

/// 基于 pinned `parent_fd + leaf_name` 观测文件条目（绝不通过路径字符串重开祖先）。
#[cfg(unix)]
fn build_entry_at(
    project: &ProjectWorkspace,
    relative: &[u8],
    parent_fd: &File,
    leaf_name: &std::ffi::OsStr,
) -> Result<Value, ContractError> {
    use rustix::fs::{AtFlags, Dir, FileType, readlinkat, statat};
    let stat = statat(parent_fd, leaf_name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_not_found)?;
    let ftype = FileType::from_raw_mode(stat.st_mode);
    let (kind, revision, byte_length, symlink_target) = match ftype {
        FileType::Symlink => {
            let target = readlinkat(parent_fd, leaf_name, Vec::new()).map_err(io_not_found)?;
            let target_bytes = target.as_bytes().to_vec();
            (
                "symlink",
                symlink_revision(&target_bytes),
                None,
                Some(byte_path_json(&target_bytes)),
            )
        }
        FileType::Directory => {
            let dir = open_dir_readable_nofollow(parent_fd, leaf_name).map_err(io_not_found)?;
            let mut names: Vec<Vec<u8>> = Vec::new();
            for entry in Dir::read_from(&dir).map_err(io_not_found)? {
                let entry = entry.map_err(io_not_found)?;
                let b = entry.file_name().to_bytes();
                if b == b"." || b == b".." {
                    continue;
                }
                names.push(b.to_vec());
            }
            names.sort();
            (
                "directory",
                directory_revision(
                    stat.st_dev as u64,
                    stat.st_ino as u64,
                    stat_mtime_ns_rustix(&stat),
                    &names,
                ),
                None,
                None,
            )
        }
        FileType::RegularFile => {
            let (content_sha, len) = hash_regular_file_at(parent_fd, leaf_name)?;
            ("file", file_revision(&content_sha, len), Some(len), None)
        }
        _ => (
            "other",
            format!("orev-{}-{}", stat.st_size, stat_mtime_ns_rustix(&stat)),
            None,
            None,
        ),
    };
    let readable = access_flags(parent_fd, leaf_name, false);
    let writable = access_flags(parent_fd, leaf_name, true);
    let mut entry = json!({
        "path": {
            "projectId": project.project_id,
            "relativePath": byte_path_json(relative),
        },
        "kind": kind,
        "revision": revision,
        "readable": readable,
        "writable": writable,
    });
    if let Some(len) = byte_length {
        entry["byteLength"] = json!(len);
    }
    if let Some(target) = symlink_target {
        entry["symlinkTarget"] = target;
    }
    if matches!(
        ftype,
        FileType::RegularFile | FileType::Directory | FileType::Symlink
    ) {
        entry["modifiedAt"] = json!(stat_rfc3339_rustix(&stat));
    }
    Ok(entry)
}

#[cfg(unix)]
fn build_entry_root(
    project: &ProjectWorkspace,
    relative: &[u8],
    workspace_dir: &File,
) -> Result<Value, ContractError> {
    use rustix::fs::{Dir, fstat};
    let dir = open_dir_readable_nofollow(workspace_dir, std::ffi::OsStr::new("."))
        .map_err(io_not_found)?;
    let stat = fstat(&dir).map_err(io_not_found)?;
    let mut names: Vec<Vec<u8>> = Vec::new();
    for entry in Dir::read_from(&dir).map_err(io_not_found)? {
        let entry = entry.map_err(io_not_found)?;
        let b = entry.file_name().to_bytes();
        if b == b"." || b == b".." {
            continue;
        }
        names.push(b.to_vec());
    }
    names.sort();
    let revision = directory_revision(
        stat.st_dev as u64,
        stat.st_ino as u64,
        stat_mtime_ns_rustix(&stat),
        &names,
    );
    let dot = std::ffi::OsStr::new(".");
    Ok(json!({
        "path": {
            "projectId": project.project_id,
            "relativePath": byte_path_json(relative),
        },
        "kind": "directory",
        "revision": revision,
        "readable": access_flags(&dir, dot, false),
        "writable": access_flags(&dir, dot, true),
        "modifiedAt": stat_rfc3339_rustix(&stat),
    }))
}

/// 经 pinned 工作区句柄 NOFOLLOW 打开 canonical 文件（read_range 数据面同款
/// 原语）。canonical 已在边界内，walk 不跟随任何 raced-in 链接；同时经 `fstat`
/// 确认目标为普通文件（防竞态 FIFO 挂起）。
pub(crate) fn open_file_canonical(
    project: &ProjectWorkspace,
    canonical: &Path,
) -> Result<File, ContractError> {
    let workspace_dir =
        crate::project_files::open_directory_beneath(&project.root, &project.workspace)
            .map_err(io_not_found)?;
    let relative = canonical
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("file outside the workspace anchor"))?;
    let file =
        crate::project_files::open_file_at(&workspace_dir, relative).map_err(io_not_found)?;
    let meta = file.metadata().map_err(io_unavailable)?;
    if !meta.is_file() {
        return Err(ContractError::new(
            CODE_NOT_A_FILE,
            "target is not a regular file",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(file)
}

fn is_loop_or_nofollow_err(err: &std::io::Error) -> bool {
    crate::project_files::nofollow_loop(err) || err.raw_os_error() == Some(libc::ELOOP)
}

fn io_not_found(err: impl Into<std::io::Error>) -> ContractError {
    let err = err.into();
    if is_loop_or_nofollow_err(&err) {
        return ContractError::new(
            CODE_PATH_ESCAPE,
            format!("path refused (symlink loop or NOFOLLOW leaf): {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
    }
    match err.kind() {
        std::io::ErrorKind::NotFound => ContractError::new(
            CODE_NOT_FOUND,
            format!("path not found: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        std::io::ErrorKind::PermissionDenied => ContractError::new(
            agentcpm_pd_protocol::errors::CODE_FORBIDDEN,
            format!("permission denied: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        _ => io_unavailable(err),
    }
}

fn io_dir_not_found(err: impl Into<std::io::Error>) -> ContractError {
    let err = err.into();
    if is_loop_or_nofollow_err(&err) {
        return ContractError::new(
            CODE_PATH_ESCAPE,
            format!("path refused (symlink loop or NOFOLLOW leaf): {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
    }
    match err.kind() {
        std::io::ErrorKind::NotADirectory => ContractError::new(
            CODE_NOT_A_DIRECTORY,
            format!("path component is not a directory: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        std::io::ErrorKind::NotFound => ContractError::new(
            CODE_NOT_FOUND,
            format!("path not found: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        std::io::ErrorKind::PermissionDenied => ContractError::new(
            agentcpm_pd_protocol::errors::CODE_FORBIDDEN,
            format!("permission denied: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        _ => io_unavailable(err),
    }
}

fn io_escape(message: &str) -> ContractError {
    ContractError::new(
        CODE_PATH_ESCAPE,
        message.to_owned(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn io_unavailable(err: std::io::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("filesystem error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn result_unknown(context: &str, err: ContractError) -> ContractError {
    ContractError::new(
        agentcpm_pd_protocol::errors::CODE_RESULT_UNKNOWN,
        format!("{context}: {}", err.message),
        RetrySpec::same_key(Some(250)),
        EffectStatus::Unknown,
    )
}

fn io_disk_or_unknown(err: std::io::Error, staged: bool) -> ContractError {
    if err.raw_os_error() == Some(libc::ENOSPC) {
        ContractError::new(
            CODE_DISK_FULL,
            format!("disk full: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    } else if is_loop_or_nofollow_err(&err) {
        io_escape(&format!("path refused during write (NOFOLLOW): {err}"))
    } else if staged {
        // 原子替换之后失败：目标可能已换新，效果不明（合同 RESULT_UNKNOWN）。
        ContractError::new(
            agentcpm_pd_protocol::errors::CODE_RESULT_UNKNOWN,
            format!("post-rename verification failed: {err}"),
            RetrySpec::same_key(Some(250)),
            EffectStatus::Unknown,
        )
    } else {
        io_unavailable(err)
    }
}

// ---------------------------------------------------------------------------
// 写串行化：同 canonical 目标路径一次只有一个在途写
// ---------------------------------------------------------------------------

fn write_gates() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static GATES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn acquire_write_gate(canonical: &Path) -> Arc<Mutex<()>> {
    let mut gates = write_gates().lock().expect("write gates");
    gates
        .entry(canonical.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 按字典序一次性取齐多个写门并持门运行 `body`（dsh r3 P1#2：
/// `snapshots.restore` 的提交段与 [`move_path`] 的双门同构——全部路径
/// 的门先取齐再逐项执行，避免持一门等另一门的死锁；`body` 返回即全部
/// 释放）。调用方传 canonical 叶身份路径（与 write/move/delete 的门 key
/// 同源，如 [`leaf_identity_path`] 的产出）。sqlite conn 锁只允许在门内
/// 获取（现有写路径均为「写门→conn」序，此 helper 不引入 conn→门 的
/// 反序）。
pub(crate) fn with_write_gates<R>(
    canonical: &[PathBuf],
    body: impl FnOnce() -> Result<R, ContractError>,
) -> Result<R, ContractError> {
    let mut gated = canonical.to_vec();
    gated.sort();
    gated.dedup();
    let gates: Vec<Arc<Mutex<()>>> = gated.iter().map(|p| acquire_write_gate(p)).collect();
    let mut guards = Vec::with_capacity(gates.len());
    for gate in &gates {
        guards.push(gate.lock().expect("write gate"));
    }
    body()
}

// ---------------------------------------------------------------------------
// 事件（镜像 store.rs::append_event_in_tx 的两步 cursor 赋值）
// ---------------------------------------------------------------------------

/// 追加一条 `files.changed` 事件。resource_id（projectId）从 payload 里取：
/// 写路径的调用点在 spawn_blocking **之前**构造 payload（json! 只借用不
/// move），闭包可以独占 project_id/relative——事件不再在闭包后访问任何
/// 被移动的值。
fn append_file_event(store: &V1Store, payload: &Value) -> Result<(), ContractError> {
    let project_id = payload
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let conn = store.conn.lock().expect("sqlite mutex");
    append_file_event_in_tx(&conn, project_id, "files.changed", payload)
}

fn append_file_event_in_tx(
    conn: &Connection,
    project_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    let placeholder_cursor = format!("tmp-{}", uuid::Uuid::new_v4());
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', ?3, 'agentcpm-pd.file.event/1', ?4, ?5, ?6)",
        params![
            placeholder_cursor,
            project_id,
            event_type,
            payload_str,
            payload_str,
            now_rfc3339(),
        ],
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

fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

/// 追加 `files.changed` 事件并登记捕获条目（A1，snap-wire §5）：台账行与
/// 事件同事务落库；条目全部无 run 归属时（A6b）同事务内立即聚合为一条
/// `run_id=NULL` 的快照并消费台账行。**归属由调用方显式传入**（dsh r3
/// P1#3）：driver 路径传 `Some(run_id)`，普通 CLI `files.*` 请求恒传
/// `None`——同项目并发两个 run 时各归各的台账行，run 期间的 CLI 直写也不
/// 会误归 run（旧的进程内 project→run 单键 map 会让后 set 覆盖前 set）。
/// 只在写门守护内调用（P1#1：登记时点与文件系统变更同在门临界区）。
/// 登记失败上抛——落盘已成功，纪律与 [`append_file_event`] 相同（不吞
/// 错：漏记 = 该次改动从恢复面静默消失，违背 a021「不伪报完整成功」的
/// 精神）。
fn append_file_event_with_capture(
    store: &V1Store,
    attribution: Option<&str>,
    payload: &Value,
    captures: &[CapturedFileChange],
) -> Result<(), ContractError> {
    let project_id = payload
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut conn = store.conn.lock().expect("sqlite mutex");
    if captures.is_empty() {
        append_file_event_in_tx(&conn, project_id, "files.changed", payload)?;
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    insert_capture_ledger(&tx, project_id, attribution, captures)?;
    append_file_event_in_tx(&tx, project_id, "files.changed", payload)?;
    if attribution.is_none() {
        capture_unowned_entries(store, &tx, project_id)?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(())
}

/// A5（snap-wire §5）driver 写指令的域内路径：离线 driver 的
/// `[pd-driver:write=<relpath>@<字面量>]` 经此走到与 `files.write` 完全相同
/// 的生产链路——同一 [`write_file`]（absent 前提，即纯 create；目标已存在
/// 或父目录缺失 → 上抛，由 driver 转 run 失败）+ 同一登记路径。**归属显式
/// 传 `run_id`**（P1#3）：调用方（driver 的 `ctx.run`）持有 run 身份，
/// 台账行落 run 归属，run 终态后由边界聚合消费为一条 run 归属快照。这
/// 不是合同面（测试适配器扩展，007 §7.2 定位）。
pub(crate) fn driver_write_file(
    store: &Arc<V1Store>,
    run_id: &str,
    project_id: &str,
    relative: &[u8],
    content: &[u8],
) -> Result<(), ContractError> {
    let event_payload = json!({
        "projectId": project_id,
        "path": {
            "projectId": project_id,
            "relativePath": byte_path_json(relative),
        },
        "change": "write",
    });
    write_file(
        store,
        project_id,
        relative,
        &WriteContent::Inline(content.to_vec()),
        &json!({"state": "absent"}),
        Some(run_id),
        &event_payload,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// handlers：只读三操作
// ---------------------------------------------------------------------------

fn parse_project_path(input: &Value) -> Result<(String, Vec<u8>), ContractError> {
    let project_id = input
        .get("path")
        .and_then(|p| p.get("projectId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let relative = match input.get("path").and_then(|p| p.get("relativePath")) {
        Some(byte_path) => decode_byte_path(byte_path)?,
        None => Vec::new(),
    };
    Ok((project_id, relative))
}

async fn files_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let (project_id, relative) = parse_project_path(&input)?;
    let depth = input
        .get("depth")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(64) as usize;
    let include_hidden = input
        .get("includeHidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let limit = input
        .get("page")
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, LIST_PAGE_LIMIT as u64) as usize;
    let cursor_raw = input
        .get("page")
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let outcome = tokio::task::spawn_blocking(move || {
        list_directory(
            &store,
            &project_id,
            &relative,
            depth,
            include_hidden,
            limit,
            cursor_raw,
        )
    })
    .await
    .map_err(join_err)?
    .map_err(DispatchError::Application)?;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 列表游标：`<offset>:<目录 revision>`。分页期间目录 revision 变化则拒绝
/// 该游标（合同验收：目录变更使 cursor 失效或明确重新分页）。
fn parse_list_cursor(cursor: &str) -> Result<(usize, String), ContractError> {
    let bad = || {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("invalid files.list cursor `{cursor}`"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let (offset, revision) = cursor.split_once(':').ok_or_else(bad)?;
    Ok((
        offset.parse::<usize>().map_err(|_| bad())?,
        revision.to_owned(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn list_directory(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    depth: usize,
    include_hidden: bool,
    limit: usize,
    cursor_raw: Option<String>,
) -> Result<Value, ContractError> {
    use rustix::fs::{AtFlags, Dir, FileType, fstat, statat};

    let project = resolve_project_list(store, project_id)?;
    let target = request_target(&project, relative)?;
    let canonical = std::fs::canonicalize(&target).map_err(io_dir_not_found)?;
    require_within(&project.workspace, &canonical)?;

    let root_search_fd = crate::project_files::open_directory_beneath(&project.root, &canonical)
        .map_err(io_dir_not_found)?;
    let root_dir_fd = open_dir_readable_nofollow(&root_search_fd, std::ffi::OsStr::new("."))
        .map_err(io_dir_not_found)?;
    let root_stat = fstat(&root_dir_fd).map_err(io_dir_not_found)?;

    // 目录 revision（子项名参与），作为本次列表快照身份。
    let mut child_names: Vec<Vec<u8>> = Vec::new();
    for entry in Dir::read_from(&root_dir_fd).map_err(io_dir_not_found)? {
        let entry = entry.map_err(io_dir_not_found)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        child_names.push(bytes.to_vec());
    }
    child_names.sort();
    let dir_revision = directory_revision(
        root_stat.st_dev as u64,
        root_stat.st_ino as u64,
        stat_mtime_ns_rustix(&root_stat),
        &child_names,
    );

    let mut offset = 0usize;
    if let Some(cursor) = cursor_raw.as_deref() {
        let (cursor_offset, cursor_revision) = parse_list_cursor(cursor)?;
        if cursor_revision != dir_revision {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "directory changed during pagination; restart from the first page",
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        offset = cursor_offset;
    }

    // 收集（depth 层内）按字典序排列的条目；全程基于 pinned root_dir_fd + O_NOFOLLOW 遍历。
    let mut collected: Vec<(Vec<u8>, PathBuf)> = Vec::new();
    let mut queue: Vec<(PathBuf, Vec<u8>, usize)> = vec![(PathBuf::new(), relative.to_vec(), 0)];
    while let Some((rel_from_root, dir_rel, level)) = queue.pop() {
        let dir_fd = if rel_from_root.as_os_str().is_empty() {
            root_dir_fd.try_clone().map_err(io_dir_not_found)?
        } else {
            let search_fd = walk_dir_beneath_for_dir_op(&root_dir_fd, &rel_from_root)?;
            open_dir_readable_nofollow(&search_fd, std::ffi::OsStr::new("."))
                .map_err(io_dir_not_found)?
        };
        let mut entries: Vec<(Vec<u8>, bool)> = Vec::new();
        for entry in Dir::read_from(&dir_fd).map_err(io_dir_not_found)? {
            let entry = entry.map_err(io_dir_not_found)?;
            let name = entry.file_name().to_bytes().to_vec();
            if name == b"." || name == b".." {
                continue;
            }
            if !include_hidden && name.starts_with(b".") {
                continue;
            }
            let is_dir = if level + 1 < depth {
                match entry.file_type() {
                    FileType::Directory => true,
                    FileType::Unknown => {
                        statat(&dir_fd, os_from_bytes(&name), AtFlags::SYMLINK_NOFOLLOW)
                            .map(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory)
                            .unwrap_or(false)
                    }
                    _ => false,
                }
            } else {
                false
            };
            entries.push((name, is_dir));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, is_dir) in entries {
            let child_rel = concat_relative(&dir_rel, &name);
            let child_rel_from_root = rel_from_root.join(os_from_bytes(&name));
            collected.push((child_rel.clone(), child_rel_from_root.clone()));
            if is_dir && level + 1 < depth {
                queue.push((child_rel_from_root, child_rel, level + 1));
            }
        }
    }
    // 需要确定性的整体顺序：逐层收集已经按字典序，但 BFS 层间需重排。
    collected.sort_by(|a, b| a.0.cmp(&b.0));

    if offset > collected.len() {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "pagination cursor beyond end of directory",
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    let end = offset.saturating_add(limit).min(collected.len());
    let mut entries = Vec::with_capacity(end - offset);
    for (rel, rel_from_root) in &collected[offset..end] {
        let (parent_fd, leaf_name) = pin_parent_dir(&root_dir_fd, rel_from_root)?;
        entries.push(build_entry_at(&project, rel, &parent_fd, &leaf_name)?);
    }
    let complete = end >= collected.len();
    let next_cursor = if complete {
        Value::Null
    } else {
        Value::String(format!("{end}:{dir_revision}"))
    };
    let mut page = json!({
        "entries": entries,
        "revision": dir_revision,
    });
    // The contract's nextCursor is a non-nullable optional string: the
    // completion page omits the key entirely instead of sending null.
    if !next_cursor.is_null() {
        page["nextCursor"] = next_cursor;
    }
    Ok(page)
}

fn concat_relative(base: &[u8], name: &[u8]) -> Vec<u8> {
    if base.is_empty() {
        name.to_vec()
    } else {
        let mut out = base.to_vec();
        out.push(b'/');
        out.extend_from_slice(name);
        out
    }
}

/// files.list 专属解析：虚拟项目 → VIRTUAL_PROJECT（合同错误表列出该码，
/// 验收「不将 home 当项目根」）。其余操作经 `From<ProjectResolveError>` 的
/// 缺省映射报 NOT_FOUND。
fn resolve_project_list(
    store: &Arc<V1Store>,
    project_id: &str,
) -> Result<ProjectWorkspace, ContractError> {
    match resolve_project(store, project_id) {
        Ok(project) => Ok(project),
        Err(ProjectResolveError::Virtual(msg)) => Err(ContractError::new(
            CODE_VIRTUAL_PROJECT,
            msg,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        Err(other) => Err(other.into()),
    }
}

async fn files_stat(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let (project_id, relative) = parse_project_path(&input)?;
    let entry = tokio::task::spawn_blocking(move || {
        let project = resolve_project(&store, &project_id).map_err(ContractError::from)?;
        let target = request_target(&project, &relative)?;
        // 叶子身份（NOFOLLOW）：先 canonicalize 会把符号链接消解成目标的
        // frev-/drev-，与 files.list 对同一路径上报的 kind=symlink/lrev-
        // 失配，消费方按 list 结果再调 delete/move 必然 revision 失配。
        let leaf = leaf_identity_path(&project, &target)?;
        let workspace_dir = pin_workspace_dir(&project)?;
        if leaf == project.workspace {
            build_entry_root(&project, &relative, &workspace_dir)
        } else {
            let rel_to_leaf = leaf
                .strip_prefix(&project.workspace)
                .map_err(|_| io_escape("target outside the workspace anchor"))?;
            let (parent_fd, leaf_name) = pin_parent_dir(&workspace_dir, rel_to_leaf)?;
            build_entry_at(&project, &relative, &parent_fd, &leaf_name)
        }
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, json!({"entry": entry})))
}

async fn files_read(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let (project_id, relative) = parse_project_path(&input)?;
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let max_bytes = input
        .get("maxBytes")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(FILE_CHUNK_MAX_BYTES as u64) as usize;
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let (chunk, entry) = tokio::task::spawn_blocking(move || {
        read_range(
            &store,
            &project_id,
            &relative,
            offset,
            max_bytes,
            expected_revision,
        )
    })
    .await
    .map_err(join_err)??;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"chunk": chunk, "entry": entry}),
    ))
}

/// 单遍顺序读：完整哈希（revision）+ 捕获请求窗口。窗口外的完整哈希保证
/// expectedRevision 校验对整个文件成立；窗口读完后边界 stat 若变化则重哈
/// 希复核（中途变更 → REVISION_CONFLICT，不拼接两版本）。
///
/// 数据面全部走 pinned 句柄（A4-FILE-TOCTOU-001 同款原语）：canonicalize
/// 复核在界内之后，读取与复核重哈希都从 pinned 工作区目录 NOFOLLOW 打开
/// （`open_regular_file_at`），不再按 canonical 路径 open/hash/stat。
fn read_range(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    offset: u64,
    max_bytes: usize,
    expected_revision: Option<String>,
) -> Result<(Value, Value), ContractError> {
    let project = resolve_project(store, project_id).map_err(ContractError::from)?;
    let target = request_target(&project, relative)?;
    let canonical = std::fs::canonicalize(&target).map_err(io_not_found)?;
    require_within(&project.workspace, &canonical)?;

    // pinned 数据面：工作区句柄 + canonical 相对的 NOFOLLOW 打开。
    let workspace_dir = pin_workspace_dir(&project)?;
    let relative_to_file = canonical
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("file outside the workspace anchor"))?;
    if relative_to_file.as_os_str().is_empty() {
        return Err(ContractError::new(
            CODE_NOT_A_FILE,
            format!("`{}` is not a regular file", canonical.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let (parent_fd, leaf_name) = pin_parent_dir(&workspace_dir, relative_to_file)?;
    let mut file = open_regular_file_at(&parent_fd, &leaf_name)?;
    // 读前 stat 指纹（句柄 fstat，不经路径），用于读后复核。
    let before = handle_fingerprint(&file)?;
    let total = before.len;
    if offset > total {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("offset {offset} is beyond the file length {total}; this is not an empty file"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let mut hasher = Sha256::new();
    let mut window: Vec<u8> = Vec::with_capacity(max_bytes.min(FILE_CHUNK_MAX_BYTES));
    let window_start = offset as usize;
    let window_end = window_start.saturating_add(max_bytes);
    let mut position: usize = 0;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_unavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        let block_start = position;
        let block_end = position + read;
        if block_end > window_start && block_start < window_end {
            let from = window_start.saturating_sub(block_start).min(read);
            let to = (window_end - block_start).min(read);
            window.extend_from_slice(&buffer[from..to]);
        }
        position = block_end;
    }
    let content_sha = format!("{:x}", hasher.finalize());
    let revision = file_revision(&content_sha, position as u64);
    if let Some(expected) = expected_revision.as_deref()
        && expected != revision
    {
        return Err(revision_conflict(expected, &revision));
    }
    // 读后复核：句柄 fstat 变化（原地改写）或 pinned 父目录 statat 变化（换入新
    // inode 或链接）则经 pinned 句柄重开重哈希，仍不同即冲突。
    let after_handle = handle_fingerprint(&file)?;
    let after_path = stat_fingerprint_at(&parent_fd, &leaf_name)?;
    if before != after_handle || before != after_path {
        let (recheck, _) = hash_regular_file_at(&parent_fd, &leaf_name)?;
        if recheck != content_sha {
            return Err(revision_conflict(&revision, &recheck));
        }
    }

    let byte_length = window.len();
    let eof = offset as usize + byte_length >= position;
    let chunk = json!({
        "bytesBase64": base64::engine::general_purpose::STANDARD.encode(&window),
        "offset": offset,
        "byteLength": byte_length,
        "totalByteLength": position,
        "revision": revision,
        "eof": eof,
    });
    let entry = build_entry_at(&project, relative, &parent_fd, &leaf_name)?;
    Ok((chunk, entry))
}

#[derive(PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    mtime_ns: i64,
    ino: u64,
}

fn stat_fingerprint_at(
    parent_fd: &File,
    leaf_name: &std::ffi::OsStr,
) -> Result<Fingerprint, ContractError> {
    use rustix::fs::{AtFlags, statat};
    let stat = statat(parent_fd, leaf_name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_not_found)?;
    Ok(Fingerprint {
        len: stat.st_size as u64,
        mtime_ns: stat_mtime_ns_rustix(&stat),
        ino: stat.st_ino as u64,
    })
}

/// 句柄 fstat 指纹（pinned 复核用）：不经路径，叶路径被竞态替换不影响该
/// 指纹所描述的 inode。
fn handle_fingerprint(file: &File) -> Result<Fingerprint, ContractError> {
    let stat = rustix::fs::fstat(file).map_err(|e| io_unavailable(e.into()))?;
    Ok(Fingerprint {
        len: stat.st_size as u64,
        mtime_ns: stat_mtime_ns_rustix(&stat),
        ino: stat.st_ino as u64,
    })
}

fn revision_conflict(expected: &str, actual: &str) -> ContractError {
    ContractError::new(
        agentcpm_pd_protocol::errors::CODE_REVISION_CONFLICT,
        format!("file revision `{expected}` does not match current `{actual}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("files worker task failed: {err}"),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// handlers：写四操作
// ---------------------------------------------------------------------------

/// files.write 的内容来源：内联字节 XOR artifact 引用（schema oneOf 保证
/// 互斥）。artifact 引用走流式拷出，不整载内存。
enum WriteContent {
    Inline(Vec<u8>),
    Artifact(String),
}

async fn files_write(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let (project_id, relative) = parse_project_path(&input)?;
    let precondition = input.get("precondition").cloned().unwrap_or(Value::Null);
    let content = input.get("content").cloned().unwrap_or(Value::Null);
    let write_content = if let Some(artifact_id) = content.get("artifactId").and_then(Value::as_str)
    {
        WriteContent::Artifact(artifact_id.to_owned())
    } else {
        let encoded = content
            .get("bytesBase64")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| {
                DispatchError::Application(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!("content bytesBase64 decode failed: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ))
            })?;
        WriteContent::Inline(bytes)
    };

    // 事件 payload 先建（json! 只借用）；闭包随后独占 project_id/relative。
    let event_payload = json!({
        "projectId": project_id,
        "path": {
            "projectId": project_id,
            "relativePath": byte_path_json(&relative),
        },
        "change": "write",
    });
    let blocking_store = store.clone();
    // P1#1（dsh r3）：落盘、捕获台账登记（含 A6b 即时聚合）与 files.changed
    // 事件全部在写门临界区内一次完成——台账 seq 因此与文件系统变更时刻同
    // 序，并发同路径操作不会把时间序倒置进恢复面。CLI 请求无 run 归属
    // （P1#3）：恒传 None，A6b 即时快照。
    let outcome = tokio::task::spawn_blocking(move || {
        if let WriteContent::Artifact(ref artifact_id) = write_content {
            // artifactId 存在性先验（未知 → NOT_FOUND，字节未发布 → ARTIFACT_NOT_READY）。
            crate::public_v1::domains::artifacts::artifact_json(&blocking_store, artifact_id)?;
        }
        write_file(
            &blocking_store,
            &project_id,
            &relative,
            &write_content,
            &precondition,
            None,
            &event_payload,
        )
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 暂存完整字节 → 复验身份 → fsync → rename 原子替换（合同 postcondition：
/// 「暂存完整字节后原子替换；失败不报告成功」）。替换既有内容前先把原字节
/// 备份为 recovery artifact（ops.json binaryArtifactBindings: 单文件原字节
/// 备份）。捕获条目（A1，snap-wire §5）与 `files.changed` 事件在**写门内**
/// 登记（P1#1），归属由 `attribution` 显式给出（P1#3）。
#[allow(clippy::too_many_arguments)]
fn write_file(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    content: &WriteContent,
    precondition: &Value,
    attribution: Option<&str>,
    event_payload: &Value,
) -> Result<Value, ContractError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, renameat, statat, unlinkat};

    let project = resolve_project(store, project_id).map_err(ContractError::from)?;
    let target = request_target(&project, relative)?;
    if target == project.workspace {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "refusing to overwrite the project workspace root",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let parent = target.parent().map(Path::to_path_buf).ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "target path has no parent directory",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let leaf = target.file_name().map(ToOwned::to_owned).ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "target path has no leaf component",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    // 父目录必须已存在且在边界内（不隐式创建父目录）。
    let parent_canonical = std::fs::canonicalize(&parent).map_err(io_not_found)?;
    require_within(&project.workspace, &parent_canonical)?;

    let state = precondition
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = precondition
        .get("revision")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let _gate = acquire_write_gate(&parent_canonical.join(&leaf));
    let _guard = _gate.lock().expect("write gate");

    // 固定父目录句柄（O_NOFOLLOW | O_DIRECTORY）：此后所有 stat/open/temp/rename
    // 全部基于该描述符执行，杜绝祖先路径被竞态替换为符号链接的 TOCTOU 越界。
    let parent_fd = crate::project_files::open_directory_beneath(&project.root, &parent_canonical)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotADirectory
                && !crate::project_files::nofollow_loop(&err)
            {
                ContractError::new(
                    CODE_NOT_A_DIRECTORY,
                    format!("`{}` is not a directory", parent_canonical.display()),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            } else {
                io_not_found(err)
            }
        })?;

    // 现状观测 + precondition 判定（SYMLINK_NOFOLLOW）。
    let existing = match statat(&parent_fd, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Some(stat),
        Err(rustix::io::Errno::NOENT) => None,
        Err(err) => return Err(io_not_found(err)),
    };
    let (previous_revision, recovery_artifact_id, current_revision) = match (&existing, state) {
        (Some(stat), "revision")
            if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile =>
        {
            let (content_sha, len) = hash_regular_file_at(&parent_fd, &leaf)?;
            let current = file_revision(&content_sha, len);
            let expected = expected.as_deref().unwrap_or_default();
            if expected != current {
                return Err(revision_conflict(expected, &current));
            }
            // 原字节备份 → recovery artifact（pinned NOFOLLOW + NONBLOCK 打开 + 流式发布）。
            let mut original = open_regular_file_at(&parent_fd, &leaf)?;
            let recovery = crate::public_v1::domains::artifacts::publish_artifact_from_reader(
                store,
                project_id,
                "application/octet-stream",
                Some(&format!("recovery:{}", String::from_utf8_lossy(relative))),
                &mut original,
                "recovery",
            )?;
            (
                Some(current.clone()),
                Some(
                    recovery["artifactId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ),
                current,
            )
        }
        (Some(_), "revision") => {
            // 目标存在但不是普通文件：revision 语义不可满足。
            return Err(ContractError::new(
                agentcpm_pd_protocol::errors::CODE_REVISION_CONFLICT,
                "target exists but is not a regular file; revision precondition unsatisfiable",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        (Some(_), "absent") => {
            return Err(ContractError::new(
                agentcpm_pd_protocol::errors::CODE_REVISION_CONFLICT,
                "target already exists but precondition requires absent",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        (Some(_), other) => {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("precondition state `{other}` requires an existing file"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        (None, "absent") => (None, None, String::new()),
        (None, other) => {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("precondition state `{other}` requires an existing file"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    };

    // 暂存写入（同目录隐藏临时文件 O_CREAT|O_EXCL|O_NOFOLLOW|O_NONBLOCK → fsync → 复验 → renameat）。
    let temp_name = std::ffi::OsString::from(format!(".agentcpm-pd-write-{}", store.next_id()));
    struct TempGuard<'a> {
        parent: &'a File,
        name: &'a std::ffi::OsStr,
        armed: bool,
    }
    impl Drop for TempGuard<'_> {
        fn drop(&mut self) {
            if self.armed {
                let _ = unlinkat(self.parent, self.name, AtFlags::empty());
            }
        }
    }
    let mut temp_guard = TempGuard {
        parent: &parent_fd,
        name: &temp_name,
        armed: false,
    };
    let staged_result = (|| -> Result<(), ContractError> {
        let fd = openat(
            &parent_fd,
            &temp_name,
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o666),
        )
        .map_err(|err| io_disk_or_unknown(err.into(), false))?;
        temp_guard.armed = true;
        let mut temp = File::from(fd);
        match content {
            WriteContent::Inline(bytes) => {
                temp.write_all(bytes)
                    .map_err(|err| io_disk_or_unknown(err, false))?;
            }
            WriteContent::Artifact(artifact_id) => {
                crate::public_v1::domains::artifacts::copy_artifact_into(
                    store,
                    artifact_id,
                    &mut temp,
                )?;
            }
        }
        temp.sync_all()
            .map_err(|err| io_disk_or_unknown(err, false))?;
        drop(temp);
        Ok(())
    })();
    staged_result?;

    // 提交前复验身份：precondition 判定与暂存之间目标被外部改动或换成链接 → 拒绝。
    if state == "revision" {
        match statat(&parent_fd, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                let (recheck, recheck_len) = hash_regular_file_at(&parent_fd, &leaf)?;
                if recheck_sha_changed(&current_revision, &recheck, recheck_len) {
                    return Err(revision_conflict(
                        &current_revision,
                        &file_revision(&recheck, recheck_len),
                    ));
                }
            }
            _ => {
                return Err(revision_conflict(&current_revision, "<missing>"));
            }
        }
    } else if statat(&parent_fd, &leaf, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        // absent：暂存期间目标被并发创建 → 冲突，不覆盖。
        return Err(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_REVISION_CONFLICT,
            "target appeared after the absent precondition was checked",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    renameat(&parent_fd, &temp_name, &parent_fd, &leaf)
        .map_err(|err| io_disk_or_unknown(err.into(), false))?;
    temp_guard.armed = false;
    // 父目录 fsync（rename 持久化，尽力而为）。
    if let Ok(dir) = open_dir_readable_nofollow(&parent_fd, std::ffi::OsStr::new(".")) {
        let _ = dir.sync_all();
    }

    // A1（snap-wire §5）：after 内容产物化——条目模型的 artifactId 语义是
    // **动作完成后**的路径内容。rename 已生效，此后失败 → RESULT_UNKNOWN。
    let after_artifact_id = match content {
        WriteContent::Artifact(artifact_id) => Some(artifact_id.clone()),
        WriteContent::Inline(bytes) => {
            let artifact = crate::public_v1::domains::artifacts::publish_artifact(
                store,
                project_id,
                "application/octet-stream",
                Some(&format!(
                    "snapshot-capture:{}",
                    String::from_utf8_lossy(relative)
                )),
                bytes,
                "snapshot-capture",
            )
            .map_err(|err| result_unknown("write applied but snapshot capture failed", err))?;
            Some(
                artifact["artifactId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        }
    };

    // 结果条目（rename 后的观测；此时失败 → RESULT_UNKNOWN）。
    let entry = build_entry_at(&project, relative, &parent_fd, &leaf)
        .map_err(|err| result_unknown("write applied but result verification failed", err))?;
    let capture = vec![CapturedFileChange {
        path: relative.to_vec(),
        before_revision: previous_revision.clone(),
        after_revision: entry["revision"].as_str().map(ToOwned::to_owned),
        action: if previous_revision.is_some() {
            "replace"
        } else {
            "create"
        },
        artifact_id: after_artifact_id,
    }];
    append_file_event_with_capture(store, attribution, event_payload, &capture)
        .map_err(|err| result_unknown("write applied but event/capture recording failed", err))?;
    let mut result = json!({"entry": entry});
    if let Some(previous) = previous_revision {
        result["previousRevision"] = json!(previous);
    }
    if let Some(recovery) = recovery_artifact_id {
        result["recoveryArtifactId"] = json!(recovery);
    }
    Ok(result)
}

fn recheck_sha_changed(current_revision: &str, recheck_sha: &str, len: u64) -> bool {
    current_revision != file_revision(recheck_sha, len)
}

async fn files_create_directory(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let (project_id, relative) = parse_project_path(&input)?;
    let create_parents = input
        .get("createParents")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 事件 payload 先建（json! 只借用）；闭包随后独占 project_id/relative。
    let event_payload = json!({
        "projectId": project_id,
        "path": {
            "projectId": project_id,
            "relativePath": byte_path_json(&relative),
        },
        "change": "create_directory",
    });
    let blocking_store = store.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let out = create_directory(&blocking_store, &project_id, &relative, create_parents)?;
        append_file_event(&blocking_store, &event_payload)?;
        Ok::<_, ContractError>(out)
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn create_directory(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    create_parents: bool,
) -> Result<Value, ContractError> {
    use rustix::fs::{AtFlags, Mode, mkdirat, statat};

    let project = resolve_project(store, project_id).map_err(ContractError::from)?;
    if relative.is_empty() {
        return Err(ContractError::new(
            CODE_ALREADY_EXISTS,
            "the project workspace root already exists",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let target = request_target(&project, relative)?;
    if target == project.workspace {
        return Err(ContractError::new(
            CODE_ALREADY_EXISTS,
            "the project workspace root already exists",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let workspace_dir = pin_workspace_dir(&project)?;
    let relative_to_workspace = target
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("target outside the workspace anchor"))?;

    let leaf_name = relative_to_workspace
        .file_name()
        .ok_or_else(|| io_escape("target path has no leaf component"))?
        .to_os_string();
    let parent_rel = relative_to_workspace
        .parent()
        .unwrap_or_else(|| Path::new(""));

    let (parent_fd, created_rel_paths): (File, Vec<Vec<u8>>) = if create_parents {
        match create_dir_chain_at(&workspace_dir, relative_to_workspace, 0o755, true) {
            Ok(created) => {
                // 重新经 pinned workspace_dir 固定父目录句柄（零路径重开），确保 build_entry_at 安全。
                let parent_fd = walk_dir_beneath_for_dir_op(&workspace_dir, parent_rel)?;
                // 仅报告本请求实际 mkdirat 成功的相对路径（由浅到深）。
                let mut rels = Vec::with_capacity(created.len());
                let mut accum = PathBuf::new();
                let mut created_names = created.into_iter().map(|(_, n)| n).peekable();
                for component in relative_to_workspace.components() {
                    if let Component::Normal(part) = component {
                        accum.push(part);
                        if created_names.peek().is_some_and(|n| n.as_os_str() == part) {
                            created_names.next();
                            #[cfg(unix)]
                            {
                                use std::os::unix::ffi::OsStrExt;
                                rels.push(accum.as_os_str().as_bytes().to_vec());
                            }
                            #[cfg(not(unix))]
                            {
                                rels.push(accum.to_string_lossy().into_owned().into_bytes());
                            }
                        }
                    }
                }
                (parent_fd, rels)
            }
            Err(boxed) => {
                let (err, created) = *boxed;
                rollback_created_dirs(&created);
                return Err(err);
            }
        }
    } else {
        // createParents=false：先经 walk_dir_beneath_for_dir_op 以 O_NOFOLLOW|O_DIRECTORY
        // 逐级打开父目录（缺父 → NOT_FOUND，遇符号链接 → PATH_ESCAPE，非目录 → NOT_A_DIRECTORY），
        // 再在该父目录 fd 上检查存在性并执行 mkdirat(parent_fd, leaf)。
        let parent_fd = walk_dir_beneath_for_dir_op(&workspace_dir, parent_rel)?;
        if statat(&parent_fd, &leaf_name, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
            return Err(ContractError::new(
                CODE_ALREADY_EXISTS,
                format!("`{}` already exists", target.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        mkdirat(&parent_fd, &leaf_name, Mode::from_raw_mode(0o755)).map_err(|err| {
            if err == rustix::io::Errno::EXIST {
                ContractError::new(
                    CODE_ALREADY_EXISTS,
                    format!("`{}` already exists", target.display()),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            } else {
                io_dir_not_found(err)
            }
        })?;
        #[cfg(unix)]
        let rel_bytes = {
            use std::os::unix::ffi::OsStrExt;
            relative_to_workspace.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let rel_bytes = relative_to_workspace
            .to_string_lossy()
            .into_owned()
            .into_bytes();
        (parent_fd, vec![rel_bytes])
    };

    let entry = build_entry_at(&project, relative, &parent_fd, &leaf_name)?;
    let created_paths: Vec<Value> = created_rel_paths
        .iter()
        .map(|rel| {
            json!({
                "projectId": project_id,
                "relativePath": byte_path_json(rel),
            })
        })
        .collect();
    Ok(json!({"entry": entry, "createdPaths": created_paths}))
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(name) => out.push(name),
        }
    }
    Some(out)
}

async fn files_move(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let source = input.get("source").cloned().unwrap_or(Value::Null);
    let destination = input.get("destination").cloned().unwrap_or(Value::Null);
    let expected_source_revision = input
        .get("expectedSourceRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let destination_precondition = input
        .get("destinationPrecondition")
        .cloned()
        .unwrap_or(Value::Null);

    let (source_project, source_rel) = parse_project_path(&json!({"path": source}))?;
    let (dest_project, dest_rel) = parse_project_path(&json!({"path": destination}))?;
    if source_project != dest_project {
        return Err(DispatchError::Application(ContractError::new(
            CODE_CROSS_PROJECT_MOVE,
            format!(
                "moving between projects `{source_project}` and `{dest_project}` is unsupported"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }

    // 事件 payload 先建（json! 只借用）；闭包随后独占 project_id 和两个
    // 相对路径。事件按源项目记账（resource_id）。
    let event_payload = json!({
        "projectId": source_project,
        "path": {
            "projectId": dest_project,
            "relativePath": byte_path_json(&dest_rel),
        },
        "previousPath": {
            "projectId": source_project,
            "relativePath": byte_path_json(&source_rel),
        },
        "change": "move",
    });
    let project_id = source_project.clone();
    let blocking_store = store.clone();
    // P1#1（dsh r3）：rename、捕获登记（含 A6b）与事件全在双写门临界区内
    // 一次完成（与 write_file 同纪律）；CLI 请求无 run 归属（P1#3）。
    let outcome = tokio::task::spawn_blocking(move || {
        move_path(
            &blocking_store,
            &project_id,
            &source_rel,
            &dest_rel,
            &expected_source_revision,
            &destination_precondition,
            None,
            &event_payload,
        )
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

#[cfg(unix)]
fn source_symlink_resolves_to_dest(
    source_parent_fd: &File,
    source_leaf: &std::ffi::OsStr,
    source_parent_rel: &Path,
    dest_rel_path: &Path,
) -> bool {
    use rustix::fs::{AtFlags, FileType, readlinkat, statat};
    let Ok(stat) = statat(source_parent_fd, source_leaf, AtFlags::SYMLINK_NOFOLLOW) else {
        return false;
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Symlink {
        return false;
    }
    let Ok(target) = readlinkat(source_parent_fd, source_leaf, Vec::new()) else {
        return false;
    };
    let os_target = os_from_bytes(target.as_bytes());
    let target_path = Path::new(os_target.as_os_str());
    if target_path.is_absolute() {
        return false;
    }
    let joined = source_parent_rel.join(target_path);
    lexical_normalize(&joined).is_some_and(|norm| norm == dest_rel_path)
}

#[cfg(not(unix))]
fn source_symlink_resolves_to_dest(
    _source_parent_fd: &File,
    _source_leaf: &std::ffi::OsStr,
    _source_parent_rel: &Path,
    _dest_rel_path: &Path,
) -> bool {
    false
}

#[allow(clippy::too_many_arguments)]
fn move_path(
    store: &Arc<V1Store>,
    project_id: &str,
    source_rel: &[u8],
    dest_rel: &[u8],
    expected_source_revision: &str,
    destination_precondition: &Value,
    attribution: Option<&str>,
    event_payload: &Value,
) -> Result<Value, ContractError> {
    use rustix::fs::{AtFlags, FileType, renameat, statat};
    let project = resolve_project(store, project_id).map_err(ContractError::from)?;
    let source = request_target(&project, source_rel)?;
    let dest = request_target(&project, dest_rel)?;
    if source == project.workspace || dest == project.workspace {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "refusing to move the project workspace root",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let source_leaf_path = leaf_identity_path(&project, &source)?;
    let dest_leaf_path = leaf_identity_path(&project, &dest)?;

    let source_rel_path = source_leaf_path
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("source outside the workspace anchor"))?
        .to_path_buf();
    let dest_rel_path = dest_leaf_path
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("destination outside the workspace anchor"))?
        .to_path_buf();

    let workspace_dir = pin_workspace_dir(&project)?;
    let (source_parent_fd, source_leaf) = pin_parent_dir(&workspace_dir, &source_rel_path)?;
    let (dest_parent_fd, dest_leaf) = pin_parent_dir(&workspace_dir, &dest_rel_path)?;

    let source_gate_key = source_leaf_path;
    let dest_gate_key = dest_leaf_path;
    let source_parent_rel = source_rel_path.parent().unwrap_or_else(|| Path::new(""));

    // 自移动拒绝（同路径或叶符号链接自身经相对目标指向 destination）：
    // 写门按父目录+叶名取到同一把 `Arc<Mutex<()>>`，`std::sync::Mutex`
    // 不可重入，取门之前明确拒绝（同步兼容 self_move 测试与 P1#8 链接移动）。
    if source_gate_key == dest_gate_key
        || source_symlink_resolves_to_dest(
            &source_parent_fd,
            &source_leaf,
            source_parent_rel,
            &dest_rel_path,
        )
    {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "source and destination resolve to the same path",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    // 原子 rename（同文件系统）。EXDEV → UNSUPPORTED_ATOMIC_MOVE。写门按
    // 字典序获取，避免 A→B 与 B→A 对向移动互相死锁。
    let mut gated = [source_gate_key, dest_gate_key];
    gated.sort();
    let _gate_first = acquire_write_gate(&gated[0]);
    let _gate_second = acquire_write_gate(&gated[1]);
    let _guard_first = _gate_first.lock().expect("write gate");
    let _guard_second = _gate_second.lock().expect("write gate");

    // source/dest 两条 revision 前提都在**两把门都持有之后**以 pinned parent_fd
    // 重算（与 write/delete 同纪律；支持移动符号链接自身及悬空/跨根链接）。
    let source_entry = build_entry_at(&project, source_rel, &source_parent_fd, &source_leaf)?;
    let source_kind = source_entry["kind"].as_str().unwrap_or("other").to_owned();
    let source_revision = source_entry["revision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if expected_source_revision != source_revision {
        return Err(revision_conflict(
            expected_source_revision,
            &source_revision,
        ));
    }

    let dest_state = destination_precondition
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut dest_before_revision: Option<String> = None;
    let mut dest_recovery_artifact_id: Option<String> = None;
    match statat(&dest_parent_fd, &dest_leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let ftype = FileType::from_raw_mode(stat.st_mode);
            let current = if ftype == FileType::RegularFile {
                let (sha, len) = hash_regular_file_at(&dest_parent_fd, &dest_leaf)?;
                file_revision(&sha, len)
            } else {
                build_entry_at(&project, dest_rel, &dest_parent_fd, &dest_leaf)?["revision"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            };
            match dest_state {
                "absent" => {
                    return Err(ContractError::new(
                        agentcpm_pd_protocol::errors::CODE_REVISION_CONFLICT,
                        "destination exists but the precondition requires absent",
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                "revision" => {
                    let expected = destination_precondition
                        .get("revision")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if expected != current {
                        return Err(revision_conflict(expected, &current));
                    }
                }
                other => {
                    return Err(ContractError::new(
                        CODE_INVALID_ARGUMENT,
                        format!("invalid destinationPrecondition state `{other}`"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
            }
            dest_before_revision = Some(current);
            if ftype == FileType::RegularFile {
                let mut original = open_regular_file_at(&dest_parent_fd, &dest_leaf)?;
                let recovery = crate::public_v1::domains::artifacts::publish_artifact_from_reader(
                    store,
                    project_id,
                    "application/octet-stream",
                    Some(&format!("recovery:{}", String::from_utf8_lossy(dest_rel))),
                    &mut original,
                    "recovery",
                )?;
                dest_recovery_artifact_id = Some(
                    recovery["artifactId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
        Err(rustix::io::Errno::NOENT) => {
            if dest_state != "absent" {
                return Err(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "destination does not exist but the precondition expects a revision",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
        Err(err) => return Err(io_not_found(err)),
    }

    renameat(&source_parent_fd, &source_leaf, &dest_parent_fd, &dest_leaf).map_err(|err| {
        if err == rustix::io::Errno::XDEV {
            ContractError::new(
                CODE_UNSUPPORTED_ATOMIC_MOVE,
                format!(
                    "atomic move between `{}` and `{}` is unsupported (different filesystems)",
                    source.display(),
                    dest.display()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        } else {
            io_unavailable(err.into())
        }
    })?;
    if let Ok(rd) = open_dir_readable_nofollow(&dest_parent_fd, std::ffi::OsStr::new(".")) {
        let _ = rd.sync_all();
    }
    if source_parent_rel != dest_rel_path.parent().unwrap_or_else(|| Path::new(""))
        && let Ok(rd) = open_dir_readable_nofollow(&source_parent_fd, std::ffi::OsStr::new("."))
    {
        let _ = rd.sync_all();
    }

    let entry = build_entry_at(&project, dest_rel, &dest_parent_fd, &dest_leaf)
        .map_err(|err| result_unknown("move applied but result verification failed", err))?;

    let after_artifact_id = if source_kind == "file" {
        let published = (|| -> Result<Option<String>, ContractError> {
            let mut moved = open_regular_file_at(&dest_parent_fd, &dest_leaf)?;
            let artifact = crate::public_v1::domains::artifacts::publish_artifact_from_reader(
                store,
                project_id,
                "application/octet-stream",
                Some(&format!(
                    "snapshot-capture:{}",
                    String::from_utf8_lossy(dest_rel)
                )),
                &mut moved,
                "snapshot-capture",
            )?;
            Ok(Some(
                artifact["artifactId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ))
        })();
        published.map_err(|err| result_unknown("move applied but snapshot capture failed", err))?
    } else {
        None
    };

    let captures = vec![
        CapturedFileChange {
            path: source_rel.to_vec(),
            before_revision: Some(source_revision),
            after_revision: None,
            action: "delete",
            artifact_id: None,
        },
        CapturedFileChange {
            path: dest_rel.to_vec(),
            before_revision: dest_before_revision.clone(),
            after_revision: entry["revision"].as_str().map(ToOwned::to_owned),
            action: if dest_before_revision.is_some() {
                "replace"
            } else {
                "create"
            },
            artifact_id: after_artifact_id,
        },
    ];
    append_file_event_with_capture(store, attribution, event_payload, &captures)
        .map_err(|err| result_unknown("move applied but event/capture recording failed", err))?;
    let _ = dest_recovery_artifact_id;
    Ok(json!({
        "entry": entry,
        "previousPath": {
            "projectId": project_id,
            "relativePath": byte_path_json(source_rel),
        },
    }))
}

async fn files_delete(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let (project_id, relative) = parse_project_path(&input)?;
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let recursive = input
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 事件 payload 先建（json! 只借用）；闭包随后独占 project_id/relative。
    let event_payload = json!({
        "projectId": project_id,
        "path": {
            "projectId": project_id,
            "relativePath": byte_path_json(&relative),
        },
        "change": "delete",
    });
    let blocking_store = store.clone();
    // P1#1（dsh r3）：删除、捕获登记（含 A6b）与事件全在写门临界区内一次
    // 完成；CLI 请求无 run 归属（P1#3）。
    let outcome = tokio::task::spawn_blocking(move || {
        delete_path(
            &blocking_store,
            &project_id,
            &relative,
            &expected_revision,
            recursive,
            None,
            &event_payload,
        )
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

#[allow(clippy::too_many_arguments)]
fn delete_path(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    expected_revision: &str,
    recursive: bool,
    attribution: Option<&str>,
    event_payload: &Value,
) -> Result<Value, ContractError> {
    use rustix::fs::{AtFlags, Dir, unlinkat};
    let project = resolve_project(store, project_id).map_err(ContractError::from)?;
    if relative.is_empty() {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "refusing to delete the project workspace root",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let target = request_target(&project, relative)?;
    if target == project.workspace {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "refusing to delete the project workspace root",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let leaf = leaf_identity_path(&project, &target)?;
    let relative_to_workspace = leaf
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("target outside the workspace anchor"))?;
    let workspace_dir = pin_workspace_dir(&project)?;
    let (parent_fd, leaf_name) = pin_parent_dir(&workspace_dir, relative_to_workspace)?;

    // 写门（与 files.write 同 key：工作区锚定路径）。revision 前提在**门内**
    // 通过 pinned parent_fd 重算。
    let _gate = acquire_write_gate(&leaf);
    let _guard = _gate.lock().expect("write gate");

    let entry = build_entry_at(&project, relative, &parent_fd, &leaf_name)?;
    let current_revision = entry["revision"].as_str().unwrap_or_default().to_owned();
    if expected_revision != current_revision {
        return Err(revision_conflict(expected_revision, &current_revision));
    }
    let kind = entry["kind"].as_str().unwrap_or("other").to_owned();

    let mut recovery_artifact_id: Option<String> = None;
    match kind.as_str() {
        "file" => {
            let mut original = open_regular_file_at(&parent_fd, &leaf_name)?;
            let recovery = crate::public_v1::domains::artifacts::publish_artifact_from_reader(
                store,
                project_id,
                "application/octet-stream",
                Some(&format!("recovery:{}", String::from_utf8_lossy(relative))),
                &mut original,
                "recovery",
            )?;
            recovery_artifact_id = recovery["artifactId"].as_str().map(ToOwned::to_owned);
            unlinkat(&parent_fd, &leaf_name, AtFlags::empty())
                .map_err(|e| io_unavailable(e.into()))?;
        }
        "directory" => {
            if !recursive {
                let dir =
                    open_dir_readable_nofollow(&parent_fd, &leaf_name).map_err(io_not_found)?;
                let mut empty = true;
                for entry in Dir::read_from(&dir).map_err(io_not_found)? {
                    let entry = entry.map_err(io_not_found)?;
                    let b = entry.file_name().to_bytes();
                    if b != b"." && b != b".." {
                        empty = false;
                        break;
                    }
                }
                drop(dir);
                if !empty {
                    return Err(ContractError::new(
                        CODE_DIRECTORY_NOT_EMPTY,
                        format!("`{}` is not empty (use recursive)", leaf.display()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                unlinkat(&parent_fd, &leaf_name, AtFlags::REMOVEDIR).map_err(|err| {
                    if matches!(err, rustix::io::Errno::NOTEMPTY | rustix::io::Errno::EXIST) {
                        ContractError::new(
                            CODE_DIRECTORY_NOT_EMPTY,
                            format!("`{}` is not empty (use recursive)", leaf.display()),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        )
                    } else {
                        io_unavailable(err.into())
                    }
                })?;
            } else {
                let mut manifest: Vec<Value> = Vec::new();
                collect_manifest_at(&parent_fd, &leaf_name, Path::new(""), &mut manifest)?;
                let manifest_bytes =
                    serde_json::to_vec(&json!({"kind": "delete-manifest", "entries": manifest}))
                        .unwrap_or_default();
                let recovery = crate::public_v1::domains::artifacts::publish_artifact(
                    store,
                    project_id,
                    "application/json",
                    Some(&format!(
                        "recovery-manifest:{}",
                        String::from_utf8_lossy(relative)
                    )),
                    &manifest_bytes,
                    "recovery",
                )?;
                recovery_artifact_id = recovery["artifactId"].as_str().map(ToOwned::to_owned);
                let removed =
                    crate::project_files::remove_tree_at(&parent_fd, Path::new(&leaf_name), &leaf);
                if let Err(err) = removed {
                    let mut remaining: Vec<String> = Vec::new();
                    let _ =
                        collect_remaining_at(&parent_fd, &leaf_name, Path::new(""), &mut remaining);
                    let mut details = serde_json::Map::new();
                    details.insert(
                        "deletedPath".to_owned(),
                        json!(String::from_utf8_lossy(relative)),
                    );
                    details.insert("remaining".to_owned(), json!(remaining));
                    let mut err = ContractError::new(
                        CODE_PARTIAL_FAILURE,
                        format!("recursive delete failed partway: {err}"),
                        RetrySpec::after_reconcile(),
                        EffectStatus::Unknown,
                    );
                    err.details = Some(details);
                    return Err(err);
                }
            }
        }
        _ => {
            unlinkat(&parent_fd, &leaf_name, AtFlags::empty())
                .map_err(|e| io_unavailable(e.into()))?;
        }
    }
    if let Ok(rd) = open_dir_readable_nofollow(&parent_fd, std::ffi::OsStr::new(".")) {
        let _ = rd.sync_all();
    }

    let captures = vec![CapturedFileChange {
        path: relative.to_vec(),
        before_revision: Some(current_revision),
        after_revision: None,
        action: "delete",
        artifact_id: None,
    }];
    append_file_event_with_capture(store, attribution, event_payload, &captures)
        .map_err(|err| result_unknown("delete applied but event/capture recording failed", err))?;
    let mut result = json!({
        "deletedPath": {
            "projectId": project_id,
            "relativePath": byte_path_json(relative),
        },
    });
    if let Some(recovery) = recovery_artifact_id {
        result["recoveryArtifactId"] = json!(recovery);
    }
    Ok(result)
}

/// 递归收集删除清单（pinned NOFOLLOW：不跟随符号链接，仅哈希普通文件并下行实目录）。
#[cfg(unix)]
fn collect_manifest_at(
    parent_fd: &File,
    dir_name: &std::ffi::OsStr,
    prefix: &Path,
    out: &mut Vec<Value>,
) -> Result<(), ContractError> {
    use rustix::fs::{AtFlags, Dir, FileType, statat};
    let dir = open_dir_readable_nofollow(parent_fd, dir_name).map_err(io_not_found)?;
    for entry in Dir::read_from(&dir).map_err(io_not_found)? {
        let entry = entry.map_err(io_not_found)?;
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let child_os = os_from_bytes(name_bytes);
        let rel = if prefix.as_os_str().is_empty() {
            PathBuf::from(&child_os)
        } else {
            prefix.join(&child_os)
        };
        let stat = statat(&dir, &child_os, AtFlags::SYMLINK_NOFOLLOW).map_err(io_not_found)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {
                let (sha, len) = hash_regular_file_at(&dir, &child_os)?;
                out.push(json!({
                    "path": rel.to_string_lossy(),
                    "byteLength": len,
                    "sha256": sha,
                }));
            }
            FileType::Directory => {
                collect_manifest_at(&dir, &child_os, &rel, out)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(unix)]
fn collect_remaining_at(
    parent_fd: &File,
    dir_name: &std::ffi::OsStr,
    prefix: &Path,
    out: &mut Vec<String>,
) -> Result<(), ContractError> {
    use rustix::fs::{AtFlags, Dir, FileType, statat};
    let dir = open_dir_readable_nofollow(parent_fd, dir_name).map_err(io_not_found)?;
    for entry in Dir::read_from(&dir).map_err(io_not_found)? {
        let entry = entry.map_err(io_not_found)?;
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let child_os = os_from_bytes(name_bytes);
        let rel = if prefix.as_os_str().is_empty() {
            PathBuf::from(&child_os)
        } else {
            prefix.join(&child_os)
        };
        let stat = statat(&dir, &child_os, AtFlags::SYMLINK_NOFOLLOW).map_err(io_not_found)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            collect_remaining_at(&dir, &child_os, &rel, out)?;
        }
        out.push(rel.to_string_lossy().into_owned());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// impl V1Store：authorized_roots 读取面（消费侧；声明面归 t012）
// ---------------------------------------------------------------------------

impl V1Store {
    /// 授权根查询（t012 声明面写入前的消费 API）。返回 (instance_id, absolute_path)。
    #[allow(dead_code)] // superseded by decode_server_path; kept for domain callers
    pub(crate) fn lookup_authorized_root(
        &self,
        root_id: &str,
    ) -> Result<Option<(String, String)>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT instance_id, absolute_path FROM authorized_roots WHERE root_id = ?1",
                params![root_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        Ok(row)
    }

    /// 宿主声明一个授权根（写入 `authorized_roots`）：canonical 化、必须是
    /// 普通目录（拒绝符号链接化的根），幂等 upsert。这是 t012 的公开声明
    /// 操作（workspaces.prepare / directories.*）落地前的宿主侧声明入口，
    /// 也是 e2e 的播种路径；t012 合并后其操作 handler 应改走同一存储面。
    pub fn declare_authorized_root(
        &self,
        root_id: &str,
        absolute_path: &Path,
    ) -> Result<(), ContractError> {
        if root_id.is_empty() {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "rootId must not be empty",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let plain = std::fs::symlink_metadata(absolute_path).map_err(|_| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!(
                    "authorized root path `{}` does not exist",
                    absolute_path.display()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        if !plain.is_dir() {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!(
                    "authorized root `{}` must be a plain directory (not a symlink)",
                    absolute_path.display()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let canonical = std::fs::canonicalize(absolute_path).map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!(
                    "authorized root `{}` cannot be canonicalized: {err}",
                    absolute_path.display()
                ),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::NotApplied,
            )
        })?;
        let conn = self.conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(root_id) DO UPDATE SET
                instance_id = excluded.instance_id,
                absolute_path = excluded.absolute_path",
            params![
                root_id,
                self.meta().instance_id,
                canonical.display().to_string(),
                now_rfc3339()
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests：合同正反例（真实 store + dispatch 路径）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;

    fn fresh_home(tag: &str) -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        (tmp, root_dir)
    }

    fn open_runtime(root_dir: &std::path::Path) -> (Arc<PublicV1Runtime>, String) {
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
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    /// 播种一个带真实授权根的项目：authorized_roots 直连播种（t012 声明面
    /// 落地前的测试手段），workspace 目录真实存在于根下。
    fn seed_project(
        rt: &Arc<PublicV1Runtime>,
        root_dir: &std::path::Path,
        project_id: &str,
        workspace: &str,
    ) -> PathBuf {
        let store = rt.store();
        let declared_root = root_dir.parent().unwrap().join("authorized");
        std::fs::create_dir_all(&declared_root).unwrap();
        let workspace_path = declared_root.join(workspace);
        std::fs::create_dir_all(&workspace_path).unwrap();
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-t013', ?1, ?2, ?3)",
                params![
                    store.meta().instance_id,
                    declared_root.display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        rt.store()
            .register_project(&json!({
                "projectId": project_id,
                "name": "T013 Files",
                "environmentId": "local",
                "root": {"instanceId": store.meta().instance_id, "rootId": "root-t013", "relativePath": workspace},
            }))
            .unwrap();
        workspace_path
    }

    fn path_input(project_id: &str, relative: &str) -> Value {
        json!({
            "path": {
                "projectId": project_id,
                "relativePath": {"encoding": "utf8", "value": relative},
            }
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
    async fn stat_revision_is_content_addressed_not_mtime() {
        let (_tmp, root) = fresh_home("files-stat-rev");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-files", "ws");
        let file = workspace.join("data.txt");
        std::fs::write(&file, b"hello t013").unwrap();

        let (_, first) = rt
            .dispatch(
                "files.stat",
                path_input("proj-files", "data.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.stat", &first)
            .unwrap();
        let rev1 = first["entry"]["revision"].as_str().unwrap().to_owned();

        // 同长度同 mtime 字节改变 → revision 必须变化（验收 ops.json:31628）。
        let mtime_ns = stat_mtime_ns(&std::fs::metadata(&file).unwrap());
        std::fs::write(&file, b"HELLO t013").unwrap();
        {
            let stamp = rustix::fs::Timespec {
                tv_sec: mtime_ns / 1_000_000_000,
                tv_nsec: (mtime_ns % 1_000_000_000) as _,
            };
            let times = rustix::fs::Timestamps {
                last_access: stamp,
                last_modification: stamp,
            };
            rustix::fs::utimensat(rustix::fs::CWD, &file, &times, rustix::fs::AtFlags::empty())
                .unwrap();
        }

        let (_, second) = rt
            .dispatch(
                "files.stat",
                path_input("proj-files", "data.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let rev2 = second["entry"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(
            rev1, rev2,
            "same size+mtime with different bytes must change revision"
        );

        // 同内容重复 stat → revision 稳定。
        let (_, third) = rt
            .dispatch(
                "files.stat",
                path_input("proj-files", "data.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(third["entry"]["revision"], json!(rev2));
    }

    #[tokio::test]
    async fn read_chunks_and_revision_conflict_on_stale_expected() {
        let (_tmp, root) = fresh_home("files-read");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-read", "ws");
        std::fs::write(workspace.join("blob.bin"), vec![7u8; 1024]).unwrap();

        let (_, page) = rt
            .dispatch(
                "files.read",
                json!({
                    "path": {"projectId": "proj-read", "relativePath": {"encoding": "utf8", "value": "blob.bin"}},
                    "offset": 0,
                    "maxBytes": 600,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.read", &page)
            .unwrap();
        assert_eq!(page["chunk"]["byteLength"], json!(600));
        assert_eq!(page["chunk"]["totalByteLength"], json!(1024));
        assert_eq!(page["chunk"]["eof"], json!(false));
        let revision = page["chunk"]["revision"].as_str().unwrap().to_owned();

        // 过期 expectedRevision → REVISION_CONFLICT（不静默覆盖读取）。
        std::fs::write(workspace.join("blob.bin"), vec![9u8; 1024]).unwrap();
        let err = rt
            .dispatch(
                "files.read",
                json!({
                    "path": {"projectId": "proj-read", "relativePath": {"encoding": "utf8", "value": "blob.bin"}},
                    "offset": 0,
                    "maxBytes": 10,
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");

        // offset 越界不得伪装空文件。
        let err = rt
            .dispatch(
                "files.read",
                json!({
                    "path": {"projectId": "proj-read", "relativePath": {"encoding": "utf8", "value": "blob.bin"}},
                    "offset": 4096,
                    "maxBytes": 10,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn write_preconditions_recovery_and_conflict() {
        let (_tmp, root) = fresh_home("files-write");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-write", "ws");
        let file = workspace.join("note.md");
        std::fs::write(&file, b"original").unwrap();

        let (_, stat) = rt
            .dispatch(
                "files.stat",
                path_input("proj-write", "note.md"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = stat["entry"]["revision"].as_str().unwrap().to_owned();

        // absent 但已存在 → 冲突，无部分内容。
        let err = rt
            .dispatch(
                "files.write",
                json!({
                    "path": {"projectId": "proj-write", "relativePath": {"encoding": "utf8", "value": "note.md"}},
                    "content": {"bytesBase64": base64::engine::general_purpose::STANDARD.encode(b"new")},
                    "precondition": {"state": "absent"},
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");
        assert_eq!(std::fs::read(&file).unwrap(), b"original");

        // 正确 revision → 成功 + previousRevision + recoveryArtifactId。
        let (_, written) = rt
            .dispatch(
                "files.write",
                json!({
                    "path": {"projectId": "proj-write", "relativePath": {"encoding": "utf8", "value": "note.md"}},
                    "content": {"bytesBase64": base64::engine::general_purpose::STANDARD.encode(b"replaced")},
                    "precondition": {"state": "revision", "revision": revision},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.write", &written)
            .unwrap();
        assert_eq!(written["previousRevision"], json!(revision));
        let recovery = written["recoveryArtifactId"].as_str().unwrap().to_owned();
        assert!(!recovery.is_empty());
        assert_eq!(std::fs::read(&file).unwrap(), b"replaced");
        // 原字节可从 recovery artifact 读回。
        let (_, recovered) = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": recovery, "maxBytes": 1024}),
                None,
                None,
            )
            .await
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(recovered["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, b"original");
    }

    #[tokio::test]
    async fn concurrent_same_path_writes_have_exactly_one_winner() {
        let (_tmp, root) = fresh_home("files-race");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-race", "ws");
        let file = workspace.join("race.txt");
        std::fs::write(&file, b"base").unwrap();
        let (_, stat) = rt
            .dispatch(
                "files.stat",
                path_input("proj-race", "race.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = stat["entry"]["revision"].as_str().unwrap().to_owned();

        let mk = |payload: &[u8]| {
            json!({
                "path": {"projectId": "proj-race", "relativePath": {"encoding": "utf8", "value": "race.txt"}},
                "content": {"bytesBase64": base64::engine::general_purpose::STANDARD.encode(payload)},
                "precondition": {"state": "revision", "revision": revision},
            })
        };
        let (a, b) = tokio::join!(
            rt.dispatch("files.write", mk(b"writer-a"), None, None),
            rt.dispatch("files.write", mk(b"writer-b"), None, None),
        );
        let results = [a, b];
        let succeeded = results.iter().filter(|r| r.is_ok()).count();
        let conflicted = results
            .iter()
            .filter(|r| matches!(r, Err(e) if contract_code(e) == "REVISION_CONFLICT"))
            .count();
        assert_eq!(succeeded, 1, "exactly one writer wins: {results:?}");
        assert_eq!(conflicted, 1, "the other gets a conflict");
        let content = std::fs::read(&file).unwrap();
        assert!(
            content == b"writer-a" || content == b"writer-b",
            "no partial content"
        );
    }

    #[tokio::test]
    async fn path_escape_and_symlink_escape_are_rejected() {
        let (_tmp, root) = fresh_home("files-escape");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-escape", "ws");
        let outside = workspace.parent().unwrap().join("outside-secret.txt");
        std::fs::write(&outside, b"secret").unwrap();
        std::fs::write(workspace.join("inside.txt"), b"ok").unwrap();

        // .. 穿越。
        for relative in ["../outside-secret.txt", "a/../../outside-secret.txt"] {
            let err = rt.dispatch("files.read", json!({
                "path": {"projectId": "proj-escape", "relativePath": {"encoding": "utf8", "value": relative}},
                "maxBytes": 16,
            }), None, None).await.unwrap_err();
            assert_eq!(contract_code(&err), "PATH_ESCAPE", "relative={relative}");
        }
        // 越界符号链接。
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workspace.join("leak")).unwrap();
        let err = rt.dispatch("files.read", json!({
            "path": {"projectId": "proj-escape", "relativePath": {"encoding": "utf8", "value": "leak"}},
            "maxBytes": 16,
        }), None, None).await.unwrap_err();
        assert_eq!(contract_code(&err), "PATH_ESCAPE");
        // 根内普通链接 + 根内目标是允许的（canonicalize 仍在边界内）。
        #[cfg(unix)]
        std::os::unix::fs::symlink("inside.txt", workspace.join("alias")).unwrap();
        let (_, ok) = rt.dispatch("files.read", json!({
            "path": {"projectId": "proj-escape", "relativePath": {"encoding": "utf8", "value": "alias"}},
            "maxBytes": 16,
        }), None, None).await.unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(ok["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, b"ok");
    }

    #[tokio::test]
    async fn virtual_project_and_list_pagination() {
        let (_tmp, root) = fresh_home("files-list");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-list", "ws");
        // 未声明授权根的项目 → files.list 报 VIRTUAL_PROJECT（不把 home 当根）。
        rt.store()
            .register_project(&json!({
                "projectId": "proj-general",
                "name": "General",
                "environmentId": "local",
                "root": {"instanceId": rt.store().meta().instance_id, "rootId": "undeclared", "relativePath": "ws"},
            }))
            .unwrap();
        let err = rt
            .dispatch("files.list", path_input("proj-general", ""), None, None)
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "VIRTUAL_PROJECT");

        for i in 0..5 {
            std::fs::write(workspace.join(format!("f{i}.txt")), format!("content-{i}")).unwrap();
        }
        let page = rt
            .dispatch(
                "files.list",
                json_list_input("proj-list", "", 2, None),
                None,
                None,
            )
            .await;
        let (_, page) = page.unwrap();
        OperationRegistry::global()
            .validate_result("files.list", &page)
            .unwrap();
        assert_eq!(page["entries"].as_array().unwrap().len(), 2);
        let cursor = page["nextCursor"].as_str().unwrap().to_owned();

        let (_, page2) = rt
            .dispatch(
                "files.list",
                json_list_input("proj-list", "", 2, Some(&cursor)),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page2["entries"].as_array().unwrap().len(), 2);

        // 目录变更后旧游标失效。
        std::fs::write(workspace.join("f9.txt"), "new").unwrap();
        let err = rt
            .dispatch(
                "files.list",
                json_list_input("proj-list", "", 2, Some(&cursor)),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
        // 隐藏文件默认不出现。
        std::fs::write(workspace.join(".hidden"), "h").unwrap();
        let (_, page3) = rt
            .dispatch(
                "files.list",
                json_list_input("proj-list", "", 100, None),
                None,
                None,
            )
            .await
            .unwrap();
        let names: Vec<&str> = page3["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["path"]["relativePath"]["value"].as_str())
            .collect();
        assert!(!names.contains(&".hidden"));
    }

    fn json_list_input(
        project_id: &str,
        relative: &str,
        limit: u64,
        cursor: Option<&str>,
    ) -> Value {
        // 合同 workspace__PageInput：cursor 是 string（不可为 null），且
        // 非必填——没有游标时省略字段，而不是发 null。
        let mut page = json!({"limit": limit});
        if let Some(cursor) = cursor {
            page["cursor"] = json!(cursor);
        }
        json!({
            "path": {"projectId": project_id, "relativePath": {"encoding": "utf8", "value": relative}},
            "page": page,
        })
    }

    #[tokio::test]
    async fn create_directory_move_delete_roundtrip() {
        let (_tmp, root) = fresh_home("files-cmd");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-cmd", "ws");

        // 默认不递归建父目录。files.create_directory 是 idempotentSubmit（合
        // 同 cli-v1-operations.json），dispatch 必须带调用方 idempotencyKey
        // （缺失 → -32086）。
        let err = rt
            .dispatch(
                "files.create_directory",
                path_input("proj-cmd", "a/b/c"),
                Some("key-mkdir-no-parents"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
        assert!(
            !workspace.join("c").exists(),
            "createParents=false failure must not leave orphan `<workspace>/c`"
        );
        assert!(!workspace.join("a").exists());
        // createParents 递归。
        let (_, made) = rt
            .dispatch(
                "files.create_directory",
                json!({
                    "path": {"projectId": "proj-cmd", "relativePath": {"encoding": "utf8", "value": "a/b/c"}},
                    "createParents": true,
                }),
                Some("key-mkdir-parents"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.create_directory", &made)
            .unwrap();
        assert_eq!(made["createdPaths"].as_array().unwrap().len(), 3);
        assert!(workspace.join("a/b/c").is_dir());
        // 已存在 → ALREADY_EXISTS。
        let err = rt
            .dispatch(
                "files.create_directory",
                json!({
                    "path": {"projectId": "proj-cmd", "relativePath": {"encoding": "utf8", "value": "a"}},
                    "createParents": true,
                }),
                Some("key-mkdir-exists"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "ALREADY_EXISTS");

        // move：目标必须 absent。
        std::fs::write(workspace.join("a/b/c/file.txt"), b"moved").unwrap();
        let (_, stat) = rt
            .dispatch(
                "files.stat",
                path_input("proj-cmd", "a/b/c/file.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = stat["entry"]["revision"].as_str().unwrap().to_owned();
        let (_, moved) = rt
            .dispatch(
                "files.move",
                json!({
                    "source": {"projectId": "proj-cmd", "relativePath": {"encoding": "utf8", "value": "a/b/c/file.txt"}},
                    "destination": {"projectId": "proj-cmd", "relativePath": {"encoding": "utf8", "value": "a/renamed.txt"}},
                    "expectedSourceRevision": revision,
                    "destinationPrecondition": {"state": "absent"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.move", &moved)
            .unwrap();
        assert!(!workspace.join("a/b/c/file.txt").exists());
        assert_eq!(
            std::fs::read(workspace.join("a/renamed.txt")).unwrap(),
            b"moved"
        );

        // delete：revision 匹配 + recovery。
        let (_, stat2) = rt
            .dispatch(
                "files.stat",
                path_input("proj-cmd", "a/renamed.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision2 = stat2["entry"]["revision"].as_str().unwrap().to_owned();
        let (_, deleted) = rt
            .dispatch(
                "files.delete",
                json!({
                    "path": {"projectId": "proj-cmd", "relativePath": {"encoding": "utf8", "value": "a/renamed.txt"}},
                    "expectedRevision": revision2,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.delete", &deleted)
            .unwrap();
        assert!(!workspace.join("a/renamed.txt").exists());

        // 非空目录无 recursive → DIRECTORY_NOT_EMPTY。先 stat 拿目录真实
        // revision（目录 revision 由内容派生）：delete 先校验 revision 再判
        // 空，占位 revision 只会得到 REVISION_CONFLICT，到不了判空分支。
        let (_, dir_stat) = rt
            .dispatch("files.stat", path_input("proj-cmd", "a"), None, None)
            .await
            .unwrap();
        let dir_revision = dir_stat["entry"]["revision"].as_str().unwrap().to_owned();
        let err = rt
            .dispatch(
                "files.delete",
                json!({
                    "path": {"projectId": "proj-cmd", "relativePath": {"encoding": "utf8", "value": "a"}},
                    "expectedRevision": dir_revision,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "DIRECTORY_NOT_EMPTY");

        // 项目根删除拒绝。
        let err = rt
            .dispatch(
                "files.delete",
                json!({
                    "path": {"projectId": "proj-cmd", "relativePath": {"encoding": "utf8", "value": ""}},
                    "expectedRevision": "x",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn non_utf8_names_round_trip_losslessly() {
        let (_tmp, root) = fresh_home("files-nonutf8");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-bytes", "ws");
        let raw_name = b"caf\xe9.txt"; // latin-1 é，非 UTF-8。
        let file = workspace.join(os_from_bytes(raw_name));
        std::fs::write(&file, b"non-utf8 name").unwrap();

        let relative = base64::engine::general_purpose::STANDARD.encode(raw_name);
        let (_, stat) = rt
            .dispatch(
                "files.stat",
                json!({
                    "path": {
                        "projectId": "proj-bytes",
                        "relativePath": {"encoding": "base64", "value": relative},
                    }
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(stat["entry"]["kind"], json!("file"));
        assert_eq!(
            stat["entry"]["path"]["relativePath"]["encoding"],
            json!("base64")
        );
        let round = base64::engine::general_purpose::STANDARD
            .decode(
                stat["entry"]["path"]["relativePath"]["value"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(round, raw_name);
    }

    /// 合同 files.stat 验收（ops.json:31801-31804）：「符号链接自身与其解析
    /// 目标状态可区分」。stat 不得先 canonicalize 把链接消解成目标身份——
    /// 那会让同一路径在 stat（frev-/drev-）与 list（kind=symlink/lrev-）出
    /// 现两套 revision 语义，消费方按 list 结果再调 delete/move 必然失配。
    #[cfg(unix)]
    #[tokio::test]
    async fn stat_and_list_agree_on_symlink_identity() {
        let (_tmp, root) = fresh_home("files-symlink-stat");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-link", "ws");
        std::fs::write(workspace.join("inside.txt"), b"ok").unwrap();
        std::fs::create_dir(workspace.join("subdir")).unwrap();
        std::os::unix::fs::symlink("inside.txt", workspace.join("alias")).unwrap();
        std::os::unix::fs::symlink("subdir", workspace.join("dirlink")).unwrap();
        std::os::unix::fs::symlink("dangling-nowhere", workspace.join("dangling")).unwrap();

        // list 对 alias 上报 symlink/lrev-；stat 同一路径必须给出同一身份。
        let (_, listed) = rt
            .dispatch(
                "files.list",
                json_list_input("proj-link", "", 100, None),
                None,
                None,
            )
            .await
            .unwrap();
        let alias_entry = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"]["relativePath"]["value"] == json!("alias"))
            .cloned()
            .expect("alias listed");
        assert_eq!(alias_entry["kind"], json!("symlink"));
        let list_revision = alias_entry["revision"].as_str().unwrap().to_owned();

        let (_, stat_alias) = rt
            .dispatch("files.stat", path_input("proj-link", "alias"), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.stat", &stat_alias)
            .unwrap();
        assert_eq!(
            stat_alias["entry"]["kind"],
            json!("symlink"),
            "stat must not canonicalize the link identity away"
        );
        assert_eq!(
            stat_alias["entry"]["revision"],
            json!(list_revision),
            "stat and list must agree on the same path"
        );
        assert_eq!(
            stat_alias["entry"]["symlinkTarget"]["value"],
            json!("inside.txt")
        );

        // 链接与其解析目标可区分。
        let (_, stat_target) = rt
            .dispatch(
                "files.stat",
                path_input("proj-link", "inside.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(stat_target["entry"]["kind"], json!("file"));
        assert_ne!(
            stat_target["entry"]["revision"],
            stat_alias["entry"]["revision"]
        );

        // 指向目录的链接身份是链接，不是目录；悬空链接同样是链接（list 也
        // 会列出它）。
        for name in ["dirlink", "dangling"] {
            let (_, stat_link) = rt
                .dispatch("files.stat", path_input("proj-link", name), None, None)
                .await
                .unwrap();
            assert_eq!(stat_link["entry"]["kind"], json!("symlink"), "{name}");
        }

        // 逃逸链接：stat 报链接身份（不解析目标即不越界访问）；read 仍拒绝。
        let outside = workspace.parent().unwrap().join("outside-secret.txt");
        std::fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("leak")).unwrap();
        let (_, stat_leak) = rt
            .dispatch("files.stat", path_input("proj-link", "leak"), None, None)
            .await
            .unwrap();
        assert_eq!(stat_leak["entry"]["kind"], json!("symlink"));
        assert_eq!(
            stat_leak["entry"]["symlinkTarget"]["value"],
            json!(outside.display().to_string())
        );
        let err = rt
            .dispatch(
                "files.read",
                json!({
                    "path": {"projectId": "proj-link", "relativePath": {"encoding": "utf8", "value": "leak"}},
                    "maxBytes": 16,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "PATH_ESCAPE");

        // mid-path 链接越界（父目录解析出工作区）→ PATH_ESCAPE。
        let outside_dir = workspace.parent().unwrap().join("outside-dir");
        std::fs::create_dir(&outside_dir).unwrap();
        std::os::unix::fs::symlink(&outside_dir, workspace.join("outdir")).unwrap();
        let err = rt
            .dispatch(
                "files.stat",
                path_input("proj-link", "outdir/x.txt"),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "PATH_ESCAPE");
    }

    /// 合同 files.delete 验收（ops.json:33001-33003）：「禁止删除项目根或逃
    /// 逸链接目标」；leaf-as-object：删除符号链接只删链接本身。list → delete
    /// 串联（消费方真实流程：拿 list 的 lrev- 去 delete）。
    #[cfg(unix)]
    #[tokio::test]
    async fn delete_symlink_removes_the_link_not_the_target() {
        let (_tmp, root) = fresh_home("files-symlink-delete");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-dlink", "ws");
        std::fs::write(workspace.join("inside.txt"), b"survivor").unwrap();
        std::os::unix::fs::symlink("inside.txt", workspace.join("alias")).unwrap();

        // 正例：list 的 lrev- → delete(alias) 成功；目标仍在，链接消失。
        let (_, listed) = rt
            .dispatch(
                "files.list",
                json_list_input("proj-dlink", "", 100, None),
                None,
                None,
            )
            .await
            .unwrap();
        let alias_revision = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"]["relativePath"]["value"] == json!("alias"))
            .and_then(|e| e["revision"].as_str())
            .map(ToOwned::to_owned)
            .expect("alias listed");
        assert!(alias_revision.starts_with("lrev-"));

        let (_, deleted) = rt
            .dispatch(
                "files.delete",
                json!({
                    "path": {"projectId": "proj-dlink", "relativePath": {"encoding": "utf8", "value": "alias"}},
                    "expectedRevision": alias_revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.delete", &deleted)
            .unwrap();
        assert!(!workspace.join("alias").exists(), "the link itself is gone");
        assert_eq!(
            std::fs::read(workspace.join("inside.txt")).unwrap(),
            b"survivor",
            "the link target survives"
        );

        // 负控：revision 不匹配（拿目标的 frev- 去删链接）→ 零删除。
        std::os::unix::fs::symlink("inside.txt", workspace.join("alias2")).unwrap();
        let (_, stat_target) = rt
            .dispatch(
                "files.stat",
                path_input("proj-dlink", "inside.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let target_revision = stat_target["entry"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let err = rt
            .dispatch(
                "files.delete",
                json!({
                    "path": {"projectId": "proj-dlink", "relativePath": {"encoding": "utf8", "value": "alias2"}},
                    "expectedRevision": target_revision,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");
        assert!(workspace.join("alias2").symlink_metadata().is_ok());
        assert!(workspace.join("inside.txt").exists());

        // 逃逸链接：删除链接本身；根外目标不动。
        let outside = workspace.parent().unwrap().join("outside-target.txt");
        std::fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("leak")).unwrap();
        let (_, stat_leak) = rt
            .dispatch("files.stat", path_input("proj-dlink", "leak"), None, None)
            .await
            .unwrap();
        let leak_revision = stat_leak["entry"]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "files.delete",
            json!({
                "path": {"projectId": "proj-dlink", "relativePath": {"encoding": "utf8", "value": "leak"}},
                "expectedRevision": leak_revision,
            }),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            !workspace.join("leak").exists(),
            "escaping link removed itself"
        );
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"outside",
            "the outside target must never be deleted"
        );
    }

    /// delete 的 revision 前提在写门**内**重算（与 write_file 同构）：并发
    /// delete vs 同路径 write 只允许一个赢家。门外校验的实现里，delete 读
    /// 到 R 校验通过之后、拿门之前，write 把内容换成 R'，delete 拿门后删掉
    /// R'——两个都成功，删掉了调用方从未见过的 revision（丢数据）。
    #[tokio::test]
    async fn concurrent_delete_and_write_have_exactly_one_winner() {
        let (_tmp, root) = fresh_home("files-del-race");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-delrace", "ws");
        let file = workspace.join("race.txt");
        std::fs::write(&file, b"base").unwrap();
        let (_, stat) = rt
            .dispatch(
                "files.stat",
                path_input("proj-delrace", "race.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = stat["entry"]["revision"].as_str().unwrap().to_owned();

        let (deleted, written) = tokio::join!(
            rt.dispatch(
                "files.delete",
                json!({
                    "path": {"projectId": "proj-delrace", "relativePath": {"encoding": "utf8", "value": "race.txt"}},
                    "expectedRevision": revision,
                }),
                None,
                None,
            ),
            rt.dispatch(
                "files.write",
                json!({
                    "path": {"projectId": "proj-delrace", "relativePath": {"encoding": "utf8", "value": "race.txt"}},
                    "content": {"bytesBase64": base64::engine::general_purpose::STANDARD.encode(b"writer")},
                    "precondition": {"state": "revision", "revision": revision},
                }),
                None,
                None,
            ),
        );
        let results = [deleted, written];
        let succeeded = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            succeeded, 1,
            "exactly one of delete/write wins: {results:?}"
        );
        // 终态二选一：文件被删（write 冲突），或文件是 writer（delete 冲突）。
        if file.exists() {
            assert_eq!(std::fs::read(&file).unwrap(), b"writer");
        }
    }

    /// move 的 dest 方向门内重算（dsh r2 评审 P1）：门外校验的交错是 T3
    /// move(A→dest, destPrecondition.revision=R1) 校验通过后等门，T1
    /// write(dest, precondition rev=R1) 拿到同一把门（同 key：
    /// parent_canonical+leaf）把 dest 换成 R2 后释放，T3 拿到门后 rename
    /// 覆盖 R2——T1、T3 都报成功，R2 无 recovery artifact 被销毁。门内重算
    /// 后只剩一个赢家。
    #[tokio::test]
    async fn concurrent_move_and_write_on_destination_have_exactly_one_winner() {
        let (_tmp, root) = fresh_home("files-move-dest-race");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-moverace", "ws");
        std::fs::write(workspace.join("source.txt"), b"source").unwrap();
        std::fs::write(workspace.join("dest.txt"), b"dest").unwrap();

        let (_, stat_source) = rt
            .dispatch(
                "files.stat",
                path_input("proj-moverace", "source.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let source_revision = stat_source["entry"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, stat_dest) = rt
            .dispatch(
                "files.stat",
                path_input("proj-moverace", "dest.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let dest_revision = stat_dest["entry"]["revision"].as_str().unwrap().to_owned();

        let (moved, written) = tokio::join!(
            rt.dispatch(
                "files.move",
                json!({
                    "source": {"projectId": "proj-moverace", "relativePath": {"encoding": "utf8", "value": "source.txt"}},
                    "destination": {"projectId": "proj-moverace", "relativePath": {"encoding": "utf8", "value": "dest.txt"}},
                    "expectedSourceRevision": source_revision,
                    "destinationPrecondition": {"state": "revision", "revision": dest_revision},
                }),
                None,
                None,
            ),
            rt.dispatch(
                "files.write",
                json!({
                    "path": {"projectId": "proj-moverace", "relativePath": {"encoding": "utf8", "value": "dest.txt"}},
                    "content": {"bytesBase64": base64::engine::general_purpose::STANDARD.encode(b"writer")},
                    "precondition": {"state": "revision", "revision": dest_revision},
                }),
                None,
                None,
            ),
        );
        let results = [moved, written];
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "exactly one of move/write-on-dest wins: {results:?}"
        );
        let failed = results
            .into_iter()
            .find(|r| r.is_err())
            .expect("one failure")
            .unwrap_err();
        assert_eq!(contract_code(&failed), "REVISION_CONFLICT");
        // 终态二选一：dest 是被移入的 source 内容（write 冲突），或 dest 是
        // writer 字节（move 冲突）；R2 不可能被无备份覆盖。
        let dest_content = std::fs::read(workspace.join("dest.txt")).unwrap();
        assert!(
            dest_content == b"source" || dest_content == b"writer",
            "dest content must be exactly one winner's payload: {dest_content:?}"
        );
        if dest_content == b"source" {
            assert!(!workspace.join("source.txt").exists());
        }
    }

    /// move 的 source 方向门内重算（同上交错的镜像）：T3 校验
    /// expectedSourceRevision 后 source 被并发 write 改写，门外校验的实现会
    /// 移走调用方从未见过的内容；门内重算后只剩一个赢家。
    #[tokio::test]
    async fn concurrent_move_and_write_on_source_have_exactly_one_winner() {
        let (_tmp, root) = fresh_home("files-move-src-race");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-movesrc", "ws");
        std::fs::write(workspace.join("source.txt"), b"source").unwrap();

        let (_, stat_source) = rt
            .dispatch(
                "files.stat",
                path_input("proj-movesrc", "source.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let source_revision = stat_source["entry"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        let (moved, written) = tokio::join!(
            rt.dispatch(
                "files.move",
                json!({
                    "source": {"projectId": "proj-movesrc", "relativePath": {"encoding": "utf8", "value": "source.txt"}},
                    "destination": {"projectId": "proj-movesrc", "relativePath": {"encoding": "utf8", "value": "renamed.txt"}},
                    "expectedSourceRevision": source_revision,
                    "destinationPrecondition": {"state": "absent"},
                }),
                None,
                None,
            ),
            rt.dispatch(
                "files.write",
                json!({
                    "path": {"projectId": "proj-movesrc", "relativePath": {"encoding": "utf8", "value": "source.txt"}},
                    "content": {"bytesBase64": base64::engine::general_purpose::STANDARD.encode(b"writer")},
                    "precondition": {"state": "revision", "revision": source_revision},
                }),
                None,
                None,
            ),
        );
        let results = [moved, written];
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "exactly one of move/write-on-source wins: {results:?}"
        );
        // 终态二选一：source 被移走（renamed.txt = 原字节，write 落空门后
        // 发现目标缺失而失败），或 source = writer（move 门内 revision 失配
        // 冲突，renamed.txt 不存在）。
        if workspace.join("source.txt").exists() {
            assert_eq!(
                std::fs::read(workspace.join("source.txt")).unwrap(),
                b"writer"
            );
            assert!(!workspace.join("renamed.txt").exists());
        } else {
            assert_eq!(
                std::fs::read(workspace.join("renamed.txt")).unwrap(),
                b"source"
            );
        }
    }

    /// 非递归删除空目录必须真的删除（dsh r2 评审 P1）：只判空不落 rmdir 的
    /// 实现会返回 deletedPath 成功而目录仍在——调用方与 recovery/事件都认为
    /// 已删除，下一次 list/stat 仍可见。
    #[tokio::test]
    async fn delete_empty_directory_non_recursively_removes_it() {
        let (_tmp, root) = fresh_home("files-emptydir-del");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-emptydir", "ws");
        std::fs::create_dir(workspace.join("emptydir")).unwrap();

        let (_, stat) = rt
            .dispatch(
                "files.stat",
                path_input("proj-emptydir", "emptydir"),
                None,
                None,
            )
            .await
            .unwrap();
        let dir_revision = stat["entry"]["revision"].as_str().unwrap().to_owned();
        assert!(dir_revision.starts_with("drev-"));

        let (_, deleted) = rt
            .dispatch(
                "files.delete",
                json!({
                    "path": {"projectId": "proj-emptydir", "relativePath": {"encoding": "utf8", "value": "emptydir"}},
                    "expectedRevision": dir_revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.delete", &deleted)
            .unwrap();
        assert!(!workspace.join("emptydir").exists());

        // 删除成功后 stat 必须是 NOT_FOUND，不是目录再次可见。
        let err = rt
            .dispatch(
                "files.stat",
                path_input("proj-emptydir", "emptydir"),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }

    /// 自移动（source 与 destination 解析到同一路径）必须在取写门前拒绝：
    /// 同一 `Arc<Mutex<()>>` 二次加锁（std Mutex 不可重入）会让该 blocking
    /// 线程永久挂住，同路径后续 write/move/delete 全部挂住。
    #[tokio::test]
    async fn self_move_is_rejected_without_deadlocking() {
        let (_tmp, root) = fresh_home("files-selfmove");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-self", "ws");
        std::fs::write(workspace.join("hello.txt"), b"self").unwrap();
        let (_, stat) = rt
            .dispatch(
                "files.stat",
                path_input("proj-self", "hello.txt"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = stat["entry"]["revision"].as_str().unwrap().to_owned();

        let move_input = |source: &str, dest: &str| {
            json!({
                "source": {"projectId": "proj-self", "relativePath": {"encoding": "utf8", "value": source}},
                "destination": {"projectId": "proj-self", "relativePath": {"encoding": "utf8", "value": dest}},
                "expectedSourceRevision": revision,
                "destinationPrecondition": {"state": "revision", "revision": revision},
            })
        };

        // move a a（两份前提都匹配才会走到取门，即原死锁路径）。
        let moved = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            rt.dispatch(
                "files.move",
                move_input("hello.txt", "hello.txt"),
                None,
                None,
            ),
        )
        .await
        .expect("same-path move must not deadlock");
        let err = moved.unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
        assert_eq!(std::fs::read(workspace.join("hello.txt")).unwrap(), b"self");

        // alias → real（canonical 解析后同路径）同样拒绝。
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("hello.txt", workspace.join("alias")).unwrap();
            let moved = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                rt.dispatch("files.move", move_input("alias", "hello.txt"), None, None),
            )
            .await
            .expect("alias->real move must not deadlock");
            let err = moved.unwrap_err();
            assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
            assert!(workspace.join("alias").symlink_metadata().is_ok());
            assert_eq!(std::fs::read(workspace.join("hello.txt")).unwrap(), b"self");
        }

        // 死锁未被触发、门未被挂死的证据：同路径的后续操作正常完成。
        let (_, after) = rt
            .dispatch(
                "files.stat",
                path_input("proj-self", "hello.txt"),
                None,
                None,
            )
            .await
            .expect("gateway stays responsive after rejected self-move");
        assert_eq!(after["entry"]["revision"], json!(revision));
    }

    #[tokio::test]
    async fn create_directory_no_parents_and_symlink_rollback_safety() {
        let (_tmp, root) = fresh_home("files-mkdir-p0");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-mkdir", "ws");

        // 1) 父目录不存在时 createParents=false 报 NOT_FOUND 且工作区根下零残留。
        let err = rt
            .dispatch(
                "files.create_directory",
                path_input("proj-mkdir", "missing/leafdir"),
                Some("key-missing-parent"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
        assert!(!workspace.join("leafdir").exists());
        assert!(!workspace.join("missing").exists());

        // 2) 已存在的非空/空子目录下 createParents=false 必须建在子目录下而非根目录。
        std::fs::create_dir(workspace.join("sub")).unwrap();
        let (_, created) = rt
            .dispatch(
                "files.create_directory",
                path_input("proj-mkdir", "sub/newdir"),
                Some("key-existing-parent"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("files.create_directory", &created)
            .unwrap();
        assert!(workspace.join("sub/newdir").is_dir());
        assert!(
            !workspace.join("newdir").exists(),
            "child dir must not be created in workspace root"
        );

        // 3) 非目录中间段报 NOT_A_DIRECTORY 且工作区根下零残留。
        std::fs::write(workspace.join("sub/blocker"), b"file").unwrap();
        let err = rt
            .dispatch(
                "files.create_directory",
                path_input("proj-mkdir", "sub/blocker/orphan_leaf"),
                Some("key-not-a-dir"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_A_DIRECTORY");
        assert!(!workspace.join("orphan_leaf").exists());

        // 4) 逃逸符号链接中间段（无论 createParents true/false）一律 PATH_ESCAPE，
        //    且不跟随符号链接探测或 rmdir 根外目录。
        #[cfg(unix)]
        {
            let outside_dir = workspace.parent().unwrap().join("outside-target-dir");
            std::fs::create_dir_all(outside_dir.join("preexisting")).unwrap();
            std::os::unix::fs::symlink(&outside_dir, workspace.join("outdir")).unwrap();

            let err = rt
                .dispatch(
                    "files.create_directory",
                    json!({
                        "path": {"projectId": "proj-mkdir", "relativePath": {"encoding": "utf8", "value": "outdir/preexisting/nested"}},
                        "createParents": true,
                    }),
                    Some("key-symlink-parents"),
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code(&err), "PATH_ESCAPE");
            assert!(
                outside_dir.join("preexisting").is_dir(),
                "rollback must never touch or delete existing outside directories"
            );
            assert!(!outside_dir.join("preexisting/nested").exists());

            let err = rt
                .dispatch(
                    "files.create_directory",
                    path_input("proj-mkdir", "outdir/direct_leaf"),
                    Some("key-symlink-noparent"),
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code(&err), "PATH_ESCAPE");
            assert!(!outside_dir.join("direct_leaf").exists());
            assert!(!workspace.join("direct_leaf").exists());

            // 验证 create_dir_chain_at + rollback_created_dirs 基于 pinned fd 精准回滚自身创建的目录。
            let project_mk = resolve_project(rt.store(), "proj-mkdir").unwrap();
            let ws_fd = pin_workspace_dir(&project_mk).unwrap();
            let created = create_dir_chain_at(&ws_fd, Path::new("rb1/rb2"), 0o755, true).unwrap();
            assert_eq!(created.len(), 2);
            assert!(workspace.join("rb1/rb2").is_dir());
            rollback_created_dirs(&created);
            assert!(!workspace.join("rb1").exists());
        }
    }

    #[tokio::test]
    async fn io_not_found_and_fifo_fail_closed() {
        let (_tmp, root) = fresh_home("files-nofollow-fifo");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-nf", "ws");
        std::fs::write(workspace.join("real.txt"), b"hello").unwrap();

        // P1#7: ELOOP / project_files::NoFollowLoop 一律映射 PATH_ESCAPE（不重试）。
        let raw_eloop = std::io::Error::from_raw_os_error(libc::ELOOP);
        let mapped_raw = io_not_found(raw_eloop);
        assert_eq!(mapped_raw.code, CODE_PATH_ESCAPE);
        assert_eq!(mapped_raw.effect, EffectStatus::NotApplied);

        let perm_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "eacces");
        assert_eq!(
            io_not_found(perm_err).code,
            agentcpm_pd_protocol::errors::CODE_FORBIDDEN
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("real.txt", workspace.join("alias")).unwrap();
            let project = resolve_project(rt.store(), "proj-nf").unwrap();
            let ws_fd = pin_workspace_dir(&project).unwrap();
            let pf_err =
                crate::project_files::open_file_at(&ws_fd, Path::new("alias")).unwrap_err();
            assert!(crate::project_files::nofollow_loop(&pf_err));
            assert!(pf_err.raw_os_error().is_none());
            let mapped = io_not_found(pf_err);
            assert_eq!(mapped.code, CODE_PATH_ESCAPE);
            assert_eq!(mapped.effect, EffectStatus::NotApplied);

            // P0#2: FIFO 必须 via O_NONBLOCK + fstat 立即拒为 NOT_A_FILE，绝不阻塞线程。
            let fifo_path = workspace.join("pipe.fifo");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("mkfifo");
            assert!(status.success(), "mkfifo failed");
            let fifo_err =
                open_regular_file_at(&ws_fd, std::ffi::OsStr::new("pipe.fifo")).unwrap_err();
            assert_eq!(fifo_err.code, CODE_NOT_A_FILE);
            let hash_err =
                hash_regular_file_at(&ws_fd, std::ffi::OsStr::new("pipe.fifo")).unwrap_err();
            assert_eq!(hash_err.code, CODE_NOT_A_FILE);
            let read_err = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                rt.dispatch("files.read", path_input("proj-nf", "pipe.fifo"), None, None),
            )
            .await
            .expect("FIFO files.read must not block")
            .unwrap_err();
            assert_eq!(contract_code(&read_err), "NOT_A_FILE");
        }
    }

    #[tokio::test]
    async fn move_symlink_preserves_link_identity_and_handles_dangling() {
        let (_tmp, root) = fresh_home("files-move-symlink");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-msym", "ws");
        std::fs::write(workspace.join("inside.txt"), b"target-bytes").unwrap();

        #[cfg(unix)]
        {
            let outside = workspace.parent().unwrap().join("outside-sym.txt");
            std::fs::write(&outside, b"outside-secret").unwrap();
            std::os::unix::fs::symlink("inside.txt", workspace.join("alias")).unwrap();
            std::os::unix::fs::symlink("nowhere.txt", workspace.join("dangling")).unwrap();
            std::os::unix::fs::symlink(&outside, workspace.join("leak")).unwrap();

            for (src, dst) in [
                ("alias", "alias_moved"),
                ("dangling", "dangling_moved"),
                ("leak", "leak_moved"),
            ] {
                let (_, st) = rt
                    .dispatch("files.stat", path_input("proj-msym", src), None, None)
                    .await
                    .unwrap();
                assert_eq!(st["entry"]["kind"], json!("symlink"), "{src}");
                let lrev = st["entry"]["revision"].as_str().unwrap().to_owned();
                assert!(lrev.starts_with("lrev-"), "{src}");

                let (_, moved) = rt
                    .dispatch(
                        "files.move",
                        json!({
                            "source": {"projectId": "proj-msym", "relativePath": {"encoding": "utf8", "value": src}},
                            "destination": {"projectId": "proj-msym", "relativePath": {"encoding": "utf8", "value": dst}},
                            "expectedSourceRevision": lrev,
                            "destinationPrecondition": {"state": "absent"},
                        }),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                OperationRegistry::global()
                    .validate_result("files.move", &moved)
                    .unwrap();
                assert_eq!(moved["entry"]["kind"], json!("symlink"), "{dst}");
                assert_eq!(moved["entry"]["revision"], json!(lrev), "{dst}");
                assert!(
                    workspace.join(src).symlink_metadata().is_err(),
                    "old symlink `{src}` gone"
                );
                assert!(
                    workspace
                        .join(dst)
                        .symlink_metadata()
                        .unwrap()
                        .file_type()
                        .is_symlink(),
                    "moved `{dst}` remains a symlink"
                );
            }
            assert_eq!(
                std::fs::read(workspace.join("inside.txt")).unwrap(),
                b"target-bytes"
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"outside-secret");

            // 误用目标文件的 frev- 尝试移动符号链接必须 REVISION_CONFLICT。
            let (_, target_stat) = rt
                .dispatch(
                    "files.stat",
                    path_input("proj-msym", "inside.txt"),
                    None,
                    None,
                )
                .await
                .unwrap();
            let target_frev = target_stat["entry"]["revision"]
                .as_str()
                .unwrap()
                .to_owned();
            let err = rt
                .dispatch(
                    "files.move",
                    json!({
                        "source": {"projectId": "proj-msym", "relativePath": {"encoding": "utf8", "value": "alias_moved"}},
                        "destination": {"projectId": "proj-msym", "relativePath": {"encoding": "utf8", "value": "alias_again"}},
                        "expectedSourceRevision": target_frev,
                        "destinationPrecondition": {"state": "absent"},
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code(&err), "REVISION_CONFLICT");
        }
    }
}
