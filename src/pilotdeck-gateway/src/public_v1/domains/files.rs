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
//! （与 write_file 同构：门内重算，门外结论一律丢弃）。
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
use std::fs::{File, OpenOptions};
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
use rusqlite::{Connection, OptionalExtension, params};
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

#[cfg(unix)]
fn os_to_bytes(os: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    os.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_to_bytes(os: &std::ffi::OsStr) -> Vec<u8> {
    os.to_string_lossy().into_owned().into_bytes()
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
/// 根自身没有独立的叶子/父代，直接返回。
fn leaf_identity_path(project: &ProjectWorkspace, target: &Path) -> Result<PathBuf, ContractError> {
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

#[cfg(unix)]
fn stat_mtime_ns(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

#[cfg(not(unix))]
fn stat_mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn stat_ids(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn stat_ids(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn mtime_rfc3339(meta: &std::fs::Metadata) -> String {
    let (secs, nanos) = {
        let modified = meta.modified().ok();
        match modified {
            Some(time) => time
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
                .unwrap_or((0, 0)),
            None => (0, 0),
        }
    };
    chrono::DateTime::from_timestamp(secs, nanos)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

/// 可读/可写探测（faccessat EACCESS，作用于 canonical 路径的父目录句柄）。
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

/// 一个文件条目所需的全部观测。`project_path` 是调用方视角的 ProjectPath
/// JSON；`canonical` 必须已经过边界复核。
fn build_entry(
    project: &ProjectWorkspace,
    relative: &[u8],
    canonical: &Path,
) -> Result<Value, ContractError> {
    let meta = std::fs::symlink_metadata(canonical).map_err(io_not_found)?;
    let file_type = meta.file_type();
    let (kind, revision, byte_length, symlink_target) = if file_type.is_symlink() {
        let target = std::fs::read_link(canonical).map_err(io_not_found)?;
        let target_bytes = os_to_bytes(target.as_os_str());
        (
            "symlink",
            symlink_revision(&target_bytes),
            None,
            Some(byte_path_json(&target_bytes)),
        )
    } else if file_type.is_dir() {
        let (dev, ino) = stat_ids(&meta);
        let mut names: Vec<Vec<u8>> = Vec::new();
        for entry in std::fs::read_dir(canonical).map_err(io_not_found)? {
            let entry = entry.map_err(io_not_found)?;
            names.push(os_to_bytes(&entry.file_name()));
        }
        names.sort();
        (
            "directory",
            directory_revision(dev, ino, stat_mtime_ns(&meta), &names),
            None,
            None,
        )
    } else if file_type.is_file() {
        // 流式哈希（pinned 句柄，hash_pinned 同款）：files.write 的 artifact
        // 分支可把 uploads 允许的 2 GiB 产物写进工作区，此后的
        // stat/list/read/write 结果条目都经这里——整读进 Vec 会按文件大小
        // 分配，N 个并发再各自乘一份。摘要对同一原始字节流计算，`frev-`
        // 输出与整读实现逐字节一致（read_at / open_file_at 均无规范化）。
        let (content_sha, len) = hash_file_canonical(project, canonical)?;
        ("file", file_revision(&content_sha, len), Some(len), None)
    } else {
        (
            "other",
            format!("orev-{}-{}", meta.len(), stat_mtime_ns(&meta)),
            None,
            None,
        )
    };

    let parent = canonical.parent().map(Path::to_path_buf);
    let name = canonical
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_default();
    // faccessat 探测失败（父目录打不开等）保守报不可读写。
    let (readable, writable) = match parent.as_deref().and_then(|parent| File::open(parent).ok()) {
        Some(dir) => (
            access_flags(&dir, &name, false),
            access_flags(&dir, &name, true),
        ),
        None => (false, false),
    };

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
    if file_type.is_file() || file_type.is_dir() || file_type.is_symlink() {
        entry["modifiedAt"] = json!(mtime_rfc3339(&meta));
    }
    Ok(entry)
}

/// 经 pinned 工作区句柄 NOFOLLOW 打开 canonical 文件（read_range 数据面同款
/// 原语）。canonical 已在边界内，walk 不跟随任何 raced-in 链接。作为流式源
/// 供 recovery 备份与 revision 哈希消费——不再提供整读进 Vec 的形态。
fn open_file_canonical(
    project: &ProjectWorkspace,
    canonical: &Path,
) -> Result<File, ContractError> {
    let workspace_dir =
        crate::project_files::open_directory_beneath(&project.root, &project.workspace)
            .map_err(io_not_found)?;
    let relative = canonical
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("file outside the workspace anchor"))?;
    crate::project_files::open_file_at(&workspace_dir, relative).map_err(io_not_found)
}

/// pinned NOFOLLOW 打开后流式哈希，返回 (sha256, 字节数)。与原
/// `read_file_canonical` + `sha256_hex` 整读对同一原始字节产出同一摘要，
/// `frev-` revision 不变；常量内存（256 KiB 缓冲），无文件大小封顶需求。
fn hash_file_canonical(
    project: &ProjectWorkspace,
    canonical: &Path,
) -> Result<(String, u64), ContractError> {
    let mut file = open_file_canonical(project, canonical)?;
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

fn io_not_found(err: std::io::Error) -> ContractError {
    match err.kind() {
        std::io::ErrorKind::NotFound => ContractError::new(
            CODE_NOT_FOUND,
            format!("path not found: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        std::io::ErrorKind::PermissionDenied => ContractError::new(
            CODE_PATH_ESCAPE,
            format!("path refused (link or permission): {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        // NOFOLLOW 叶子打开撞上链接（含复核后竞态换入的链接）与符号链接
        // 循环：一律按拒绝处理，不退避重试（fail closed）。
        _ if err.raw_os_error() == Some(libc::ELOOP) => ContractError::new(
            CODE_PATH_ESCAPE,
            format!("path refused (symlink loop or NOFOLLOW leaf): {err}"),
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

fn io_disk_or_unknown(err: std::io::Error, staged: bool) -> ContractError {
    if err.raw_os_error() == Some(libc::ENOSPC) {
        ContractError::new(
            CODE_DISK_FULL,
            format!("disk full: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
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
    let placeholder_cursor = format!("tmp-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
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
/// `run_id=NULL` 的快照并消费台账行。run 归属在登记时读取（`PublicV1Runtime`
/// 的活跃写归属单元——写发生在 run 驱动期间且尚未终态清除 → 归属该
/// run；否则无归属走 A6b 即时快照）。登记失败上抛——落盘已成功，纪律与
/// [`append_file_event`] 相同（不吞错：漏记 = 该次改动从恢复面静默消
/// 失，违背 a021「不伪报完整成功」的精神）。
fn append_file_event_with_capture(
    store: &V1Store,
    rt: &PublicV1Runtime,
    payload: &Value,
    captures: &[CapturedFileChange],
) -> Result<(), ContractError> {
    let project_id = payload
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let run_id = rt.active_write_run(project_id);
    let mut conn = store.conn.lock().expect("sqlite mutex");
    if captures.is_empty() {
        append_file_event_in_tx(&conn, project_id, "files.changed", payload)?;
        return Ok(());
    }
    let tx = conn.transaction().map_err(sql_err)?;
    insert_capture_ledger(&tx, project_id, run_id.as_deref(), captures)?;
    append_file_event_in_tx(&tx, project_id, "files.changed", payload)?;
    if run_id.is_none() {
        capture_unowned_entries(store, &tx, project_id)?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(())
}

/// A5（snap-wire §5）driver 写指令的域内路径：离线 driver 的
/// `[pd-driver:write=<relpath>@<字面量>]` 经此走到与 `files.write` 完全相同
/// 的生产链路——同一 [`write_file`]（absent 前提，即纯 create；目标已存在
/// 或父目录缺失 → 上抛，由 driver 转 run 失败）+ 同一
/// [`append_file_event_with_capture`] 捕获台账登记。写归属不在此设置：
/// worker 在驱动 run 前已按项目设置活跃写归属（`set_active_write_run`），
/// 台账行自动带 run_id，run 终态后由边界聚合消费为一条 run 归属快照。这
/// 不是合同面（测试适配器扩展，007 §7.2 定位）。
pub(crate) fn driver_write_file(
    store: &Arc<V1Store>,
    rt: &PublicV1Runtime,
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
    let (_outcome, captures) = write_file(
        store,
        project_id,
        relative,
        &WriteContent::Inline(content.to_vec()),
        &json!({"state": "absent"}),
    )?;
    append_file_event_with_capture(store, rt, &event_payload, &captures)
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
    let project = resolve_project_list(store, project_id)?;
    let target = request_target(&project, relative)?;
    let canonical = std::fs::canonicalize(&target).map_err(io_not_found)?;
    require_within(&project.workspace, &canonical)?;
    let meta = std::fs::symlink_metadata(&canonical).map_err(io_not_found)?;
    if !meta.is_dir() {
        return Err(ContractError::new(
            CODE_NOT_A_DIRECTORY,
            format!("`{}` is not a directory", canonical.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    // 目录 revision（子项名参与），作为本次列表快照身份。
    let (dev, ino) = stat_ids(&meta);
    let mut child_names: Vec<Vec<u8>> = Vec::new();
    for entry in std::fs::read_dir(&canonical).map_err(io_not_found)? {
        let entry = entry.map_err(io_not_found)?;
        child_names.push(os_to_bytes(&entry.file_name()));
    }
    child_names.sort();
    let dir_revision = directory_revision(dev, ino, stat_mtime_ns(&meta), &child_names);

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

    // 收集（depth 层内）按字典序排列的条目。
    let mut collected: Vec<(Vec<u8>, PathBuf)> = Vec::new();
    let mut queue: Vec<(PathBuf, Vec<u8>, usize)> = vec![(canonical.clone(), relative.to_vec(), 0)];
    while let Some((dir, dir_rel, level)) = queue.pop() {
        let mut entries: Vec<(Vec<u8>, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(io_not_found)? {
            let entry = entry.map_err(io_not_found)?;
            let name = os_to_bytes(&entry.file_name());
            if !include_hidden && name.starts_with(b".") {
                continue;
            }
            let path = entry.path();
            entries.push((name, path));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, path) in entries {
            let child_rel = concat_relative(&dir_rel, &name);
            collected.push((child_rel.clone(), path.clone()));
            if level + 1 < depth
                && let Ok(meta) = std::fs::symlink_metadata(&path)
                && meta.is_dir()
            {
                queue.push((path, child_rel, level + 1));
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
    let end = (offset + limit).min(collected.len());
    let mut entries = Vec::with_capacity(end - offset);
    for (rel, canonical_path) in &collected[offset..end] {
        entries.push(build_entry(&project, rel, canonical_path)?);
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
        build_entry(&project, &relative, &leaf)
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
/// （`open_file_at`），不再按 canonical 路径 open/hash——路径级 open 会跟随
/// 复核之后竞态换入的符号链接，读出授权根之外的字节且复核重算的是同一链
/// 接目标、不会发现。
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
    let meta = std::fs::symlink_metadata(&canonical).map_err(io_not_found)?;
    if !meta.is_file() {
        return Err(ContractError::new(
            CODE_NOT_A_FILE,
            format!("`{}` is not a regular file", canonical.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let total = meta.len();
    if offset > total {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("offset {offset} is beyond the file length {total}; this is not an empty file"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    // pinned 数据面：工作区句柄 + canonical 相对的 NOFOLLOW 打开。
    let workspace_dir =
        crate::project_files::open_directory_beneath(&project.root, &project.workspace)
            .map_err(io_not_found)?;
    let relative_to_file = canonical
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("file outside the workspace anchor"))?;
    let mut file = crate::project_files::open_file_at(&workspace_dir, relative_to_file)
        .map_err(io_not_found)?;
    // 读前 stat 指纹（句柄 fstat，不经路径），用于读后复核。
    let before = handle_fingerprint(&file)?;
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
    // 读后复核：句柄 fstat 变化（原地改写）或路径 lstat 变化（换入新 inode
    // 或链接）则经 pinned 句柄重开重哈希，仍不同即冲突。
    let after_handle = handle_fingerprint(&file)?;
    let after_path = read_fingerprint(&canonical)?;
    if before != after_handle || before != after_path {
        let recheck = hash_pinned(&workspace_dir, relative_to_file)?;
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
    let entry = build_entry(&project, relative, &canonical)?;
    Ok((chunk, entry))
}

#[derive(PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    mtime_ns: i64,
    ino: u64,
}

fn read_fingerprint(path: &Path) -> Result<Fingerprint, ContractError> {
    let meta = std::fs::symlink_metadata(path).map_err(io_not_found)?;
    Ok(Fingerprint {
        len: meta.len(),
        mtime_ns: stat_mtime_ns(&meta),
        ino: stat_ids(&meta).1,
    })
}

fn hash_file(path: &Path) -> Result<String, ContractError> {
    let mut file = File::open(path).map_err(io_not_found)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_unavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 句柄 fstat 指纹（pinned 复核用）：不经路径，叶路径被竞态替换不影响该
/// 指纹所描述的 inode。
fn handle_fingerprint(file: &File) -> Result<Fingerprint, ContractError> {
    let meta = file.metadata().map_err(io_unavailable)?;
    Ok(Fingerprint {
        len: meta.len(),
        mtime_ns: stat_mtime_ns(&meta),
        ino: stat_ids(&meta).1,
    })
}

/// 经 pinned 工作区句柄 NOFOLLOW 重开并整读哈希（读后复核用；禁止按
/// canonical 路径 open/hash）。竞态换入的叶子链接在这里以 ELOOP 拒绝。
fn hash_pinned(workspace_dir: &File, relative: &Path) -> Result<String, ContractError> {
    let mut file =
        crate::project_files::open_file_at(workspace_dir, relative).map_err(io_not_found)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_unavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
        // artifactId 存在性先验（未知 → NOT_FOUND，字节未发布 → ARTIFACT_NOT_READY）。
        crate::public_v1::domains::artifacts::artifact_json(&store, artifact_id)
            .map_err(DispatchError::Application)?;
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
    let (outcome, captures) = tokio::task::spawn_blocking(move || {
        write_file(
            &blocking_store,
            &project_id,
            &relative,
            &write_content,
            &precondition,
        )
    })
    .await
    .map_err(join_err)??;

    // files.changed 事件 + 捕获条目登记（写入已确认后；A1/A6b）。
    append_file_event_with_capture(&store, &rt, &event_payload, &captures)
        .map_err(DispatchError::Application)?;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 暂存完整字节 → 复验身份 → fsync → rename 原子替换（合同 postcondition：
/// 「暂存完整字节后原子替换；失败不报告成功」）。替换既有内容前先把原字节
/// 备份为 recovery artifact（ops.json binaryArtifactBindings: 单文件原字节
/// 备份）。成功后返回捕获条目（A1，snap-wire §5）供调用方登记台账。
fn write_file(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    content: &WriteContent,
    precondition: &Value,
) -> Result<(Value, Vec<CapturedFileChange>), ContractError> {
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
    let parent_meta = std::fs::symlink_metadata(&parent_canonical).map_err(io_not_found)?;
    if !parent_meta.is_dir() {
        return Err(ContractError::new(
            CODE_NOT_A_DIRECTORY,
            format!("`{}` is not a directory", parent_canonical.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

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

    // 现状观测 + precondition 判定。
    let existing = match std::fs::symlink_metadata(parent_canonical.join(&leaf)) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(io_not_found(err)),
    };
    let canonical = parent_canonical.join(&leaf);
    let (previous_revision, recovery_artifact_id, current_revision) = match (&existing, state) {
        (Some(meta), "revision") if meta.is_file() => {
            let content_sha = hash_file(&canonical)?;
            let current = file_revision(&content_sha, meta.len());
            let expected = expected.as_deref().unwrap_or_default();
            if expected != current {
                return Err(revision_conflict(expected, &current));
            }
            // 原字节备份 → recovery artifact（pinned NOFOLLOW 打开 + 流式发
            // 布：uploads 允许 2 GiB 产物写进工作区，替换前的整读备份会按文
            // 件大小分配内存，N 个并发再各自乘一份）。
            let mut original = open_file_canonical(&project, &canonical)?;
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
        (Some(_meta), "revision") => {
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
            // &str 不可穷尽匹配：未知 state 且目标存在 → 明确拒绝。
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

    // 暂存写入（同目录隐藏临时文件 → fsync → 复验 → rename）。
    let temp_name = format!(".agentcpm-pd-write-{}", store.next_id());
    let temp_path = parent_canonical.join(&temp_name);
    let staged_result = (|| -> Result<(), ContractError> {
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|err| io_disk_or_unknown(err, false))?;
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
    if let Err(err) = staged_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    // 提交前复验身份：precondition 判定与暂存之间目标被外部改动 → 拒绝。
    if state == "revision" {
        if let Ok(meta) = std::fs::symlink_metadata(&canonical)
            && meta.is_file()
        {
            let recheck = hash_file(&canonical)?;
            if recheck_sha_changed(&current_revision, &recheck, meta.len()) {
                let _ = std::fs::remove_file(&temp_path);
                return Err(revision_conflict(
                    &current_revision,
                    &file_revision(&recheck, meta.len()),
                ));
            }
        } else {
            let _ = std::fs::remove_file(&temp_path);
            return Err(revision_conflict(&current_revision, "<missing>"));
        }
    } else if std::fs::symlink_metadata(&canonical).is_ok() {
        // absent：暂存期间目标被并发创建 → 冲突，不覆盖。
        let _ = std::fs::remove_file(&temp_path);
        return Err(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_REVISION_CONFLICT,
            "target appeared after the absent precondition was checked",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let renamed = std::fs::rename(&temp_path, &canonical);
    if let Err(err) = renamed {
        let _ = std::fs::remove_file(&temp_path);
        return Err(io_disk_or_unknown(err, false));
    }
    // 父目录 fsync（rename 持久化，尽力而为）。std::fs::File 的 sync_all
    // 在 Unix 上就是目录描述符上的 fsync(2)，不需要 unsafe 的 fd 借用。
    if let Ok(dir) = File::open(&parent_canonical) {
        let _ = dir.sync_all();
    }

    // A1（snap-wire §5）：after 内容产物化——条目模型的 artifactId 语义是
    // **动作完成后**的路径内容。artifact 输入直接复用输入 artifactId；
    // inline 字节发布 origin="snapshot-capture" 产物（recovery artifact
    // 是 before 字节，不能充当）。rename 已生效，此时失败 → RESULT_UNKNOWN
    // （与结果观测同纪律）。
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
            .map_err(|err| {
                ContractError::new(
                    agentcpm_pd_protocol::errors::CODE_RESULT_UNKNOWN,
                    format!("write applied but snapshot capture failed: {}", err.message),
                    RetrySpec::same_key(Some(250)),
                    EffectStatus::Unknown,
                )
            })?;
            Some(
                artifact["artifactId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        }
    };

    // 结果条目（rename 后的观测；此时失败 → RESULT_UNKNOWN）。
    let entry = build_entry(&project, relative, &canonical).map_err(|err| {
        ContractError::new(
            agentcpm_pd_protocol::errors::CODE_RESULT_UNKNOWN,
            format!(
                "write applied but result verification failed: {}",
                err.message
            ),
            RetrySpec::same_key(Some(250)),
            EffectStatus::Unknown,
        )
    })?;
    // 捕获条目（A1）：before = precondition 观测的 revision（replace 有 /
    // create 无），after = rename 后的内容 revision。
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
    let mut result = json!({"entry": entry});
    if let Some(previous) = previous_revision {
        result["previousRevision"] = json!(previous);
    }
    if let Some(recovery) = recovery_artifact_id {
        result["recoveryArtifactId"] = json!(recovery);
    }
    Ok((result, capture))
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

    let _unused_blocking_store = store.clone();
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
        create_directory(&blocking_store, &project_id, &relative, create_parents)
    })
    .await
    .map_err(join_err)??;
    append_file_event(&store, &event_payload).map_err(DispatchError::Application)?;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn create_directory(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    create_parents: bool,
) -> Result<Value, ContractError> {
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

    // 已存在（文件或目录）→ ALREADY_EXISTS，不替换。
    if let Ok(_meta) = std::fs::symlink_metadata(&target) {
        return Err(ContractError::new(
            CODE_ALREADY_EXISTS,
            format!("`{}` already exists", target.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    // 父链解析：createParents=false 只允许直接父目录存在；true 用 NOFOLLOW
    // 逐级 mkdir（project_files::create_directories_at）。
    let workspace_dir =
        crate::project_files::open_directory_beneath(&project.root, &project.workspace)
            .map_err(io_not_found)?;
    let relative_to_workspace = target
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("target outside the workspace anchor"))?;

    let mut created: Vec<Vec<u8>> = Vec::new();
    let result = (|| -> std::io::Result<()> {
        // 记录实际新建的层级（供失败清理与 createdPaths）。
        let mut probe = PathBuf::new();
        for component in relative_to_workspace.components() {
            let Component::Normal(name) = component else {
                return Err(std::io::Error::from_raw_os_error(libc::EACCES));
            };
            probe = probe.join(name);
            if !probe_exits_at(&workspace_dir, &probe) {
                created.push(os_to_bytes(probe.as_os_str()));
            }
        }
        if create_parents {
            crate::project_files::create_directories_at(
                &workspace_dir,
                relative_to_workspace,
                rustix::fs::Mode::from_raw_mode(0o755),
            )
        } else {
            // 默认不递归：父级缺失时 mkdirat 落 ENOENT（io_not_found 映射
            // 为 NOT_FOUND，与「默认不递归创建父目录」一致）。
            let name = relative_to_workspace
                .file_name()
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EACCES))?;
            rustix::fs::mkdirat(&workspace_dir, name, rustix::fs::Mode::from_raw_mode(0o755))
                .map_err(std::io::Error::from)
        }
    })();

    if let Err(err) = result {
        // 失败清理：只回滚本次新建且仍为空的目录（合同验收）。
        for path in created.iter().rev() {
            let candidate = project.workspace.join(os_from_bytes(path));
            let _ = std::fs::remove_dir(&candidate);
        }
        // ENOENT（父级缺失，默认不递归）等确定性失败 → NOT_FOUND /
        // PATH_ESCAPE，效果已知为未应用；UNAVAILABLE 的退避重试与
        // EffectStatus::Unknown 只应留给瞬态故障。
        return Err(io_not_found(err));
    }

    let canonical = std::fs::canonicalize(&target).map_err(io_not_found)?;
    require_within(&project.workspace, &canonical)?;
    let entry = build_entry(&project, relative, &canonical)?;
    let created_paths: Vec<Value> = created
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

fn probe_exits_at(root: &File, relative: &Path) -> bool {
    use rustix::fs::{AtFlags, statat};
    statat(root, relative, AtFlags::SYMLINK_NOFOLLOW).is_ok()
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
    let (outcome, captures) = tokio::task::spawn_blocking(move || {
        move_path(
            &blocking_store,
            &project_id,
            &source_rel,
            &dest_rel,
            &expected_source_revision,
            &destination_precondition,
        )
    })
    .await
    .map_err(join_err)??;
    append_file_event_with_capture(&store, &rt, &event_payload, &captures)
        .map_err(DispatchError::Application)?;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn move_path(
    store: &Arc<V1Store>,
    project_id: &str,
    source_rel: &[u8],
    dest_rel: &[u8],
    expected_source_revision: &str,
    destination_precondition: &Value,
) -> Result<(Value, Vec<CapturedFileChange>), ContractError> {
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
    // source 的 canonical 解析只为取写门 key 与 rename 目标；revision 前提
    // 在门内重算（见下）。
    let source_canonical = std::fs::canonicalize(&source).map_err(io_not_found)?;
    require_within(&project.workspace, &source_canonical)?;

    // 目标父目录必须存在；目标本身按 precondition 判定。
    let dest_parent = dest.parent().map(Path::to_path_buf).ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "destination has no parent directory",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let dest_leaf = dest.file_name().map(ToOwned::to_owned).ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "destination has no leaf component",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let dest_parent_canonical = std::fs::canonicalize(&dest_parent).map_err(io_not_found)?;
    require_within(&project.workspace, &dest_parent_canonical)?;
    let dest_canonical = dest_parent_canonical.join(&dest_leaf);

    // 自移动拒绝：source 与 destination 解析到同一路径（`move a a`，或
    // alias→real 这类解析后同路径）时，写门按 canonical 路径取到同一把
    // `Arc<Mutex<()>>`，`std::sync::Mutex` 不可重入，二次加锁会死锁（该
    // blocking 线程永久占住，同路径后续 write/move/delete 全部挂住）。取门
    // 之前明确拒绝。
    if source_canonical == dest_canonical {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "source and destination resolve to the same path",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    // 原子 rename（同文件系统）。EXDEV → UNSUPPORTED_ATOMIC_MOVE。写门按
    // 字典序获取，避免 A→B 与 B→A 对向移动互相死锁。
    let mut gated = [source_canonical.clone(), dest_canonical.clone()];
    gated.sort();
    let _gate_first = acquire_write_gate(&gated[0]);
    let _gate_second = acquire_write_gate(&gated[1]);
    let _guard_first = _gate_first.lock().expect("write gate");
    let _guard_second = _gate_second.lock().expect("write gate");

    // source/dest 两条 revision 前提都在**两把门都持有之后**重算（与
    // delete/write_file 的门内重算不变量同构：门外结论一律丢弃）。门外校验
    // 的丢数据交错：T3 move(A→dest, destPrecondition.revision=R1) 在取门之
    // 前校验通过，T1 write(dest, precondition rev=R1) 拿到同一把门（同 key：
    // parent_canonical+leaf）把 dest 换成 R2 后释放，T3 拿到门后 rename 覆
    // 盖 R2——T1、T3 都报成功，R2 无 recovery artifact 被销毁。同一序列对
    // source 也成立：T3 校验 expectedSourceRevision 后 source 被改写，T3
    // 移走的是调用方从未见过的内容。
    let source_entry = build_entry(&project, source_rel, &source_canonical)?;
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
    match std::fs::symlink_metadata(&dest_canonical) {
        Ok(meta) => {
            let current = if meta.is_file() {
                let sha = hash_file(&dest_canonical)?;
                file_revision(&sha, meta.len())
            } else {
                build_entry(&project, dest_rel, &dest_canonical)?["revision"]
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
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
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

    std::fs::rename(&source_canonical, &dest_canonical).map_err(|err| {
        if err.raw_os_error() == Some(libc::EXDEV) {
            ContractError::new(
                CODE_UNSUPPORTED_ATOMIC_MOVE,
                format!(
                    "atomic move between `{}` and `{}` is unsupported (different filesystems)",
                    source_canonical.display(),
                    dest_canonical.display()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        } else {
            io_unavailable(err)
        }
    })?;

    let entry = build_entry(&project, dest_rel, &dest_canonical)?;

    // A1（snap-wire §5）：move = src delete + dst create 双条目（同事务登
    // 记）。after 内容产物：源是普通文件时从移动后的位置流式发布
    // origin="snapshot-capture" 产物；目录/符号链接等无内容产物（条目模型的
    // artifactId 是 after **内容**，恢复面对无产物条目不伪装可恢复）。
    // rename 已生效，此时失败上抛（EffectStatus 语义与现行 post-rename
    // 失败一致）。
    let after_artifact_id = if source_kind == "file" {
        let mut moved = open_file_canonical(&project, &dest_canonical)?;
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
        Some(
            artifact["artifactId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
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
            before_revision: None,
            after_revision: entry["revision"].as_str().map(ToOwned::to_owned),
            action: "create",
            artifact_id: after_artifact_id,
        },
    ];
    Ok((
        json!({
            "entry": entry,
            "previousPath": {
                "projectId": project_id,
                "relativePath": byte_path_json(source_rel),
            },
        }),
        captures,
    ))
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
    let (outcome, captures) = tokio::task::spawn_blocking(move || {
        delete_path(
            &blocking_store,
            &project_id,
            &relative,
            &expected_revision,
            recursive,
        )
    })
    .await
    .map_err(join_err)??;
    append_file_event_with_capture(&store, &rt, &event_payload, &captures)
        .map_err(DispatchError::Application)?;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn delete_path(
    store: &Arc<V1Store>,
    project_id: &str,
    relative: &[u8],
    expected_revision: &str,
    recursive: bool,
) -> Result<(Value, Vec<CapturedFileChange>), ContractError> {
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
    // 对象是请求路径的叶子（与 stat/list 同源）：mid-path 链接由 canonical
    // 父目录复核不越工作区；叶子是符号链接时删除链接本身，不触碰目标（合
    // 同验收：禁止删除项目根或逃逸链接目标——先 canonicalize 整条路径会把
    // `alias->inside.txt` 解析成目标，删掉目标并留下悬空链接）。
    let no_parent = || {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "target path has no parent directory",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let parent = target.parent().ok_or_else(no_parent)?;
    let leaf_name = target.file_name().ok_or_else(no_parent)?;
    let parent_canonical = std::fs::canonicalize(parent).map_err(io_not_found)?;
    require_within(&project.workspace, &parent_canonical)?;
    let leaf = parent_canonical.join(leaf_name);

    // 写门（与 files.write 同 key：canonical 父目录 + 叶名）。revision 前
    // 提在**门内**重算（与 write_file 同构）：门外先校验的实现里，delete 读
    // 到 R、校验通过之后、拿门之前，同路径 write 可把内容换成 R'，delete 拿
    // 到门后 unlink 掉 R'——删掉调用方从未见过的 revision，recovery 备份的
    // 也不是调用方预期的字节。
    let _gate = acquire_write_gate(&leaf);
    let _guard = _gate.lock().expect("write gate");

    let entry = build_entry(&project, relative, &leaf)?;
    let current_revision = entry["revision"].as_str().unwrap_or_default().to_owned();
    if expected_revision != current_revision {
        // revision 不匹配 → 零删除。
        return Err(revision_conflict(expected_revision, &current_revision));
    }
    let kind = entry["kind"].as_str().unwrap_or("other").to_owned();

    let workspace_dir =
        crate::project_files::open_directory_beneath(&project.root, &project.workspace)
            .map_err(io_not_found)?;
    let relative_to_leaf = leaf
        .strip_prefix(&project.workspace)
        .map_err(|_| io_escape("target outside the workspace anchor"))?;

    let mut recovery_artifact_id: Option<String> = None;
    match kind.as_str() {
        "file" => {
            // 单文件：原字节备份（pinned NOFOLLOW 打开 + 流式发布——2 GiB 产
            // 物整读进 Vec 会按文件大小分配内存）→ recovery artifact，再
            // NOFOLLOW unlink。
            let mut original = crate::project_files::open_file_at(&workspace_dir, relative_to_leaf)
                .map_err(io_not_found)?;
            let recovery = crate::public_v1::domains::artifacts::publish_artifact_from_reader(
                store,
                project_id,
                "application/octet-stream",
                Some(&format!("recovery:{}", String::from_utf8_lossy(relative))),
                &mut original,
                "recovery",
            )?;
            recovery_artifact_id = recovery["artifactId"].as_str().map(ToOwned::to_owned);
            crate::project_files::unlink_at(&workspace_dir, relative_to_leaf)
                .map_err(io_unavailable)?;
        }
        "directory" => {
            if !recursive {
                let empty = std::fs::read_dir(&leaf)
                    .map(|mut iter| iter.next().is_none())
                    .unwrap_or(false);
                if !empty {
                    return Err(ContractError::new(
                        CODE_DIRECTORY_NOT_EMPTY,
                        format!("`{}` is not empty (use recursive)", leaf.display()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                // 空目录必须真的删除：只判空不落 rmdir 的实现会对空目录返回
                // deletedPath 成功而目录仍在——调用方与 recovery/事件都认为
                // 已删除，下一次 list/stat 仍可见。rmdir 不递归：判空之后恰
                // 有并发条目落入则 ENOTEMPTY 失败（零删除），不会吞掉。
                std::fs::remove_dir(&leaf).map_err(|err| {
                    if err.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                        ContractError::new(
                            CODE_DIRECTORY_NOT_EMPTY,
                            format!("`{}` is not empty (use recursive)", leaf.display()),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        )
                    } else {
                        io_unavailable(err)
                    }
                })?;
            } else {
                // 递归：先登记清单（恢复信息），失败时报告已影响项（PARTIAL_FAILURE）。
                let mut manifest: Vec<Value> = Vec::new();
                collect_manifest(&leaf, &leaf, &mut manifest)?;
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
                    crate::project_files::remove_tree_at(&workspace_dir, relative_to_leaf, &leaf);
                if let Err(err) = removed {
                    // 报告已影响项：重新枚举仍存在的路径差集。
                    let mut remaining: Vec<String> = Vec::new();
                    let _ = collect_remaining(&leaf, &leaf, &mut remaining);
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
            // 符号链接 / FIFO 等：仅移除叶子本身（NOFOLLOW unlink，不触碰
            // 目标——无论链接目标在根内还是根外）。
            crate::project_files::unlink_at(&workspace_dir, relative_to_leaf)
                .map_err(io_unavailable)?;
        }
    }

    // A1（snap-wire §5）：delete 单条 {path, before_revision,
    // action:"delete"}（after 无内容）。目录/符号链接同样只登记顶层路径
    // 条目（不逐文件展开——递归清单在 recovery manifest 里，恢复面只消费
    // 内容产物）。
    let captures = vec![CapturedFileChange {
        path: relative.to_vec(),
        before_revision: Some(current_revision),
        after_revision: None,
        action: "delete",
        artifact_id: None,
    }];
    let mut result = json!({
        "deletedPath": {
            "projectId": project_id,
            "relativePath": byte_path_json(relative),
        },
    });
    if let Some(recovery) = recovery_artifact_id {
        result["recoveryArtifactId"] = json!(recovery);
    }
    Ok((result, captures))
}

/// 递归收集删除清单（相对 canonical 的 posix 相对路径 + 长度 + sha256）。
fn collect_manifest(root: &Path, dir: &Path, out: &mut Vec<Value>) -> Result<(), ContractError> {
    for entry in std::fs::read_dir(dir).map_err(io_not_found)? {
        let entry = entry.map_err(io_not_found)?;
        let path = entry.path();
        let meta = entry.metadata().map_err(io_not_found)?;
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if meta.is_file() {
            let sha = hash_file(&path)?;
            out.push(json!({
                "path": rel.to_string_lossy(),
                "byteLength": meta.len(),
                "sha256": sha,
            }));
        } else if meta.is_dir() {
            collect_manifest(root, &path, out)?;
        }
    }
    Ok(())
}

fn collect_remaining(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), ContractError> {
    for entry in std::fs::read_dir(dir).map_err(io_not_found)? {
        let entry = entry.map_err(io_not_found)?;
        let path = entry.path();
        if entry.file_type().map_err(io_not_found)?.is_dir() {
            collect_remaining(root, &path, out)?;
        }
        out.push(
            path.strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
        );
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
}
