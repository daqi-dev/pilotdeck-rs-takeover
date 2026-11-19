//! `data.*` — 旧数据根检查、一致性备份（t020，S4；capability a038）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `data.inspect` /
//! `data.create_backup` / `data.list_backup_entries` / `data.read_backup_artifact`
//! 四条；设计正本是 `docs/plan/008_t020-legacy-data-migration.md`。旧数据事实
//! 依据基线 cc7c4e6：`src/pilot/paths.ts`、`src/session/persistence/
//! JsonlSessionPersistence.ts`、`src/gateway/run/RunRegistry.ts`、
//! `src/cron/storage/CronPaths.ts`（008 §2 A1–A22）。
//!
//! 本文件是 data/migration 两域 + CLI local 操作共用的**扫描与备份核心**：
//! 域内注册 `data.create_backup`（async / idempotentSubmit），其余三个
//! `execution=local` 操作由 `agentcpm-pd-cli` 的 local_ops 经本模块的 `pub`
//! API 复用（lifecycle.rs 的 embedded-gateway 先例）。红线（AGENTS.md /
//! 008）：**不默认读真实 `~/.pilotdeck`**，源根一律由调用方显式给出；测试
//! 用隔离临时目录。
//!
//! ## 扫描核心（单一定义，多处消费）
//!
//! [`scan_source_root`] 产出根的 `identity`（路径绑定）/ `revision`（内容或
//! stat 派生）/ 活跃写者 / issues / 摘要。消费方：`data.inspect`（CLI）、
//! `data.create_backup`（写者校验 + 备份内容）、`migration.plan`（CLI，变更
//! 清单）、`migration.stage`（PLAN_STALE 复验）。活跃写者判定 = 008 §13 T3
//! 的联合检测：`runs/registry.json.lock` flock 探测（RunRegistry.ts:62-67）
//! + `cron/.store-migration.lock` 新鲜度（CronStoreMigration.ts:26，10min
//! 陈锁）+ 复扫 mtime 无变化；全部通过才置 `activeWriter = null`。
//!
//! ## 备份落盘格式（008 §4 第 2/3 步；路径属实现层）
//!
//! ```text
//! <destination>/              # 显式新目录；已存在 → DESTINATION_EXISTS
//!   manifest.json            # 恰好 list_backup_entries.manifest 的合同字段
//!   entries.json             # {"formatVersion": ".../backup-entries/1",
//!                            #  "entries":[{entryId,kind,bytes,checksum}]}
//!   data/<sha256>            # 内容寻址的条目字节（0600）
//! ```
//!
//! `manifest.checksum` = `entries.json` 字节的 sha256（可复算的整体指纹，
//! 同时是 completionResult.checksum）；条目 `checksum` = 内容 sha256（即
//! `data/` 文件名）。凭据（`weixin-credentials.json`）在
//! `includeSecrets=false`（默认）时**不进备份**（a038「避免把凭据意外写
//! 入不受保护目标」）；`pilotdeck.yaml` 含 adapters 凭据键，默认导出时将
//! `adapters.feishu.appSecret` / `adapters.wecom.token` / `adapters.wecom.
//! extra` 的值置 null（缩进栈路径匹配，其余字节原样保留；008 §6 凭据行
//! 「迁移链路明文零接触」）。写入次序：临时目录 → 逐条目复算校验 → 原
//! 子 rename 发布（「先临时文件校验再发布；取消不留下看似完整备份」——
//! 中断只留 dot 前缀的 `.tmp` 目录）。
//!
//! ## 备份登记（artifactId → 位置）
//!
//! `data.list_backup_entries` / `data.read_backup_artifact` 是 local 操作，
//! 只拿 artifactId；登记落在 `<home>/v1/backups/index.json`（进程内互斥 +
//! 原子替换，唯一写者是 create_backup 的完成路径）。索引记录携带源根
//! `dataRoot` —— `migration.stage` 据此对源根再算一次 revision（PLAN_STALE
//! 复验的必要输入）。
//!
//! ## 域内 `data.create_backup`（async）
//!
//! 受理前扫描（写者校验 + FORMAT_UNSUPPORTED）；受理复验目标不存在 →
//! 后台 worker 重扫源根（写者再验，一致性快照语义）→ 写备份 → 登记 →
//! `backup.created` 事件 → completion（合同字段集，不增删）。

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::bootstrap::atomic_write_file;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::now_rfc3339;

/// 基线 TS 数据根的识别格式版本（A1–A22 形态）。
pub const LEGACY_FORMAT_VERSION: &str = "agentcpm-pd.legacy-pilotdeck/1";
/// 备份制品自身的格式版本（manifest.formatVersion / completionResult）。
pub const BACKUP_FORMAT_VERSION: &str = "agentcpm-pd.backup/1";
/// `entries.json` 的内部格式标记。
const BACKUP_ENTRIES_FORMAT: &str = "agentcpm-pd.backup-entries/1";
/// 备份索引的内部格式标记。
const BACKUP_INDEX_FORMAT: &str = "agentcpm-pd.backup-index/1";
/// cron 迁移锁的陈锁阈值（CronStoreMigration.ts 的 10min）。
const CRON_LOCK_STALE_MS: u128 = 10 * 60 * 1000;
/// 活跃写者复扫窗口：两次 stat 之间的静默期。
const WRITER_RECHECK_PAUSE_MS: u64 = 250;
/// `pilotdeck.yaml` 中默认置空的凭据键（008 §2 A1 / §6 凭据行）。
const SECRET_YAML_KEYS: [&str; 3] = [
    "adapters.feishu.appSecret",
    "adapters.wecom.token",
    "adapters.wecom.extra",
];

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("data.create_backup", |rt, input, key, _rev| {
        Box::pin(data_create_backup(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 错误构造（本域局部）
// ---------------------------------------------------------------------------

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn io_unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message,
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn integrity_error(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "INTEGRITY_ERROR",
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn destination_exists(path: &str) -> ContractError {
    ContractError::new(
        "DESTINATION_EXISTS",
        format!("destination `{path}` already exists; backups never overwrite it"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn writer_active(detail: impl std::fmt::Display) -> ContractError {
    ContractError::new(
        "WRITER_ACTIVE",
        format!(
            "the source data root still has an active writer ({detail}); stop it before backing up"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("data backup worker task failed: {err}"),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// 扫描核心
// ---------------------------------------------------------------------------

/// 扫描深度（`data.inspect` 的 `depth` 入参）。
pub enum ScanDepth {
    /// 只做 stat 与锁探测；revision 由 (relpath, len, mtime) 派生。
    Metadata,
    /// 逐文件内容 sha256 + 转写行校验；revision 由内容派生。
    Full,
}

/// 一条扫描出的源文件。
pub struct ScannedFile {
    /// 相对源根的 POSIX 风格路径（`/` 分隔）。
    pub rel_path: String,
    /// 数据类标签（分类见 [`classify_entry`]）。
    pub kind: &'static str,
    pub bytes: u64,
    /// 内容 sha256（Full 深度）；符号链接为目标路径文本的 sha256。
    pub sha256: String,
    pub mtime_ms: i64,
}

/// inspect 合同的一条 issue。
pub struct ScanIssue {
    pub code: String,
    pub path: Option<String>,
    pub message: String,
    pub severity: &'static str,
}

/// 扫描摘要（inspect 合同 `summary`）。
pub struct SourceSummary {
    pub sessions: u64,
    pub artifacts: u64,
    pub bytes: u64,
}

/// 一次源根扫描的全部产物。
pub struct SourceScan {
    /// 路径绑定的根身份（`dident-<hex>`）：喂给 plan/stage 的 sourceIdentity。
    pub identity: String,
    /// 内容（或 stat）派生的版本（`drev-<hex>`）：PLAN_STALE 复验依据。
    pub revision: String,
    pub format_version: &'static str,
    /// `Some(信号)` 即存在活跃写者；`None` = 未检测到。
    pub active_writer: Option<String>,
    pub issues: Vec<ScanIssue>,
    pub summary: SourceSummary,
    /// 相对路径排序的文件清单（备份与 plan 消费）。
    pub files: Vec<ScannedFile>,
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn sha256_file(path: &Path) -> Result<String, ContractError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| io_unavailable(format!("open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| io_unavailable(format!("read {}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn system_time_ms() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i128)
        .unwrap_or(0)
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 数据类分类（008 §2 A1–A22 的路径形态）。`kind` 同时进备份条目。
pub fn classify_entry(rel_path: &str) -> &'static str {
    let first = rel_path.split('/').next().unwrap_or(rel_path);
    match first {
        "weixin-credentials.json" => return "credential",
        "pilotdeck.yaml" => return "config",
        "projects" => {
            let rest = rel_path.strip_prefix("projects/").unwrap_or(rel_path);
            if rest.ends_with("/.cwd") {
                return "project";
            }
            if let Some((_, chats)) = rest.split_once("/chats/") {
                if chats.contains("subagents/") {
                    return "subagent-transcript";
                }
                if chats.ends_with(".jsonl") && !chats.contains('/') {
                    return "session-transcript";
                }
            }
            return "project-file";
        }
        "runs" => {
            return if rel_path == "runs/registry.json" {
                "run-registry"
            } else {
                "run-file"
            };
        }
        "cron" => {
            return if rel_path.ends_with("/tasks.json") || rel_path == "cron/tasks.json" {
                "cron-task"
            } else {
                "cron-history"
            };
        }
        "memory" => return "memory",
        "skills" => return "skill",
        "plugins" => return "plugin",
        "always-on" => return "always-on",
        "gateway" | "browser_screenshots" | "runtime" => return "runtime",
        _ => {}
    }
    // 顶层其他 YAML 配置。
    if !rel_path.contains('/') && (rel_path.ends_with(".yaml") || rel_path.ends_with(".yml")) {
        return "config";
    }
    "file"
}

/// 根身份：路径绑定的 `dident-<hex>`。
fn root_identity(canonical_root: &str) -> String {
    format!("dident-{}", &sha256_hex(canonical_root.as_bytes())[..32])
}

/// 目标（新侧）根身份：路径 + 实例 id（local.json 的 instanceId，缺省空）。
/// `migration.plan` 与 `migration.commit` 的 targetIdentity 同源，两侧可核对。
pub fn target_identity(home: &Path) -> String {
    let canonical = std::fs::canonicalize(home)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| home.to_string_lossy().into_owned());
    let instance = std::fs::read_to_string(home.join("local.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v["instanceId"].as_str().map(ToOwned::to_owned))
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hasher.update(instance.as_bytes());
    format!("tident-{}", &format!("{:x}", hasher.finalize())[..32])
}

/// 规范化路径字符串（canonicalize；失败即根不存在/不可达）。
pub fn canonical_path_string(path: &Path) -> Result<String, ContractError> {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| io_unavailable(format!("canonicalize {}: {e}", path.display())))
}

/// 根是否呈现基线 PilotDeck home 的任一已知形态（A1–A22 标记）。
fn looks_like_legacy_home(root: &Path) -> bool {
    let markers = [
        "pilotdeck.yaml",
        "weixin-credentials.json",
        "projects",
        "runs",
        "cron",
        "memory",
        "skills",
        "plugins",
        "always-on",
    ];
    markers.iter().any(|m| root.join(m).exists())
}

/// flock 探测（unix）：文件可独占 → 写者已停；WouldBlock → 持有者活跃。
#[cfg(unix)]
fn probe_flock_free(path: &Path) -> Result<bool, ContractError> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| io_unavailable(format!("probe lock {}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

#[cfg(not(unix))]
fn probe_flock_free(_path: &Path) -> Result<bool, ContractError> {
    Ok(true)
}

/// 活跃写者联合检测（008 §13 T3 的判据；全部通过才 None）。
fn detect_active_writer(root: &Path) -> Option<String> {
    let run_lock = root.join("runs").join("registry.json.lock");
    if run_lock.exists() {
        match probe_flock_free(&run_lock) {
            Ok(false) => return Some("runs/registry.json.lock is held".to_owned()),
            Ok(true) => {}
            Err(error) => return Some(format!("runs/registry.json.lock: {}", error.message)),
        }
    }
    let cron_lock = root.join("cron").join(".store-migration.lock");
    if let Ok(meta) = std::fs::metadata(&cron_lock) {
        let age = (system_time_ms() - i128::from(mtime_ms(&meta))).max(0) as u128;
        if age <= CRON_LOCK_STALE_MS {
            return Some("cron/.store-migration.lock is fresh".to_owned());
        }
    }
    None
}

/// 一次遍历：收集文件与（可选的）内容哈希。
fn walk_files(
    root: &Path,
    prefix: &str,
    hash: bool,
    files: &mut Vec<ScannedFile>,
    issues: &mut Vec<ScanIssue>,
) -> Result<(), ContractError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(io_unavailable(format!(
                "read dir {}: {err}",
                root.display()
            )));
        }
    };
    let mut children: Vec<std::fs::DirEntry> = entries.filter_map(|entry| entry.ok()).collect();
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let meta = match child.metadata() {
            Ok(meta) => meta,
            Err(err) => {
                issues.push(ScanIssue {
                    code: "unreadable-entry".to_owned(),
                    path: Some(rel),
                    message: format!("stat failed: {err}"),
                    severity: "error",
                });
                continue;
            }
        };
        let file_type = meta.file_type();
        if file_type.is_dir() {
            walk_files(&child.path(), &rel, hash, files, issues)?;
            continue;
        }
        if file_type.is_symlink() {
            // 不跟随符号链接（不逃逸根）；以目标文本的哈希入账。
            let target = std::fs::read_link(child.path())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            files.push(ScannedFile {
                rel_path: rel,
                kind: "file",
                bytes: 0,
                sha256: sha256_hex(target.as_bytes()),
                mtime_ms: mtime_ms(&meta),
            });
            continue;
        }
        let kind = classify_entry(&rel);
        let sha = if hash {
            sha256_file(&child.path())?
        } else {
            String::new()
        };
        files.push(ScannedFile {
            rel_path: rel,
            kind,
            bytes: meta.len(),
            sha256: sha,
            mtime_ms: mtime_ms(&meta),
        });
    }
    Ok(())
}

/// Full 深度下逐转写做行级校验（保留原文件，只报告）。
fn audit_transcripts(root: &Path, files: &[ScannedFile], issues: &mut Vec<ScanIssue>) {
    for file in files {
        if file.kind != "session-transcript" && file.kind != "subagent-transcript" {
            continue;
        }
        let path = root.join(&file.rel_path);
        let Ok(raw) = std::fs::read(&path) else {
            issues.push(ScanIssue {
                code: "unreadable-entry".to_owned(),
                path: Some(file.rel_path.clone()),
                message: "transcript could not be read".to_owned(),
                severity: "error",
            });
            continue;
        };
        let text = String::from_utf8_lossy(&raw);
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Result<Value, _> = serde_json::from_str(line);
            match parsed {
                Ok(value) if value.is_object() => {
                    let has_type = value["type"].as_str().is_some_and(|t| !t.is_empty());
                    let has_session = value["sessionId"].as_str().is_some_and(|s| !s.is_empty());
                    if !has_type || !has_session {
                        issues.push(ScanIssue {
                            code: "corrupt-transcript".to_owned(),
                            path: Some(file.rel_path.clone()),
                            message: format!(
                                "line {}: missing `type`/`sessionId` envelope",
                                index + 1
                            ),
                            severity: "error",
                        });
                    }
                }
                _ => {
                    issues.push(ScanIssue {
                        code: "corrupt-transcript".to_owned(),
                        path: Some(file.rel_path.clone()),
                        message: format!("line {}: not a JSON object", index + 1),
                        severity: "error",
                    });
                }
            }
        }
    }
}

/// 扫描一个源数据根（008 §4 第 1 步；`data.inspect` 的全部语义）。
pub fn scan_source_root(root: &Path, depth: ScanDepth) -> Result<SourceScan, ContractError> {
    let meta = std::fs::metadata(root).map_err(|e| {
        ContractError::new(
            "FORMAT_UNSUPPORTED",
            format!("data root {}: {e}", root.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if !meta.is_dir() {
        return Err(ContractError::new(
            "FORMAT_UNSUPPORTED",
            "the data root is not a directory",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if !looks_like_legacy_home(root) {
        return Err(ContractError::new(
            "FORMAT_UNSUPPORTED",
            "no known PilotDeck data marker found in the data root",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let canonical = canonical_path_string(root)?;
    let full = matches!(depth, ScanDepth::Full);

    let mut issues: Vec<ScanIssue> = Vec::new();
    let mut files: Vec<ScannedFile> = Vec::new();
    walk_files(root, "", full, &mut files, &mut issues)?;
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    if full {
        audit_transcripts(root, &files, &mut issues);
    }
    // 未知形态的顶层散文件（不属于任何已知数据类）→ warning。
    for file in &files {
        if file.kind == "file" && !file.rel_path.contains('/') {
            issues.push(ScanIssue {
                code: "unknown-entry".to_owned(),
                path: Some(file.rel_path.clone()),
                message: "top-level file matches no known PilotDeck data class".to_owned(),
                severity: "warning",
            });
        }
    }

    // 活跃写者：锁联合检测 + mtime 复扫。
    let active_writer = detect_active_writer(root).or_else(|| {
        std::thread::sleep(std::time::Duration::from_millis(WRITER_RECHECK_PAUSE_MS));
        let mut second: Vec<ScannedFile> = Vec::new();
        let mut second_issues = Vec::new();
        if walk_files(root, "", false, &mut second, &mut second_issues).is_err() {
            return Some("source-mutating (rescan failed)".to_owned());
        }
        let stats_of = |f: &ScannedFile| (f.rel_path.clone(), f.bytes, f.mtime_ms);
        let before: Vec<_> = files.iter().map(stats_of).collect();
        let after: Vec<_> = second.iter().map(stats_of).collect();
        if before != after {
            Some("source-mutating (files changed between scan passes)".to_owned())
        } else {
            None
        }
    });

    // identity / revision。
    let identity = root_identity(&canonical);
    let mut hasher = Sha256::new();
    for file in &files {
        hasher.update(file.rel_path.as_bytes());
        hasher.update(&[0]);
        if full {
            hasher.update(file.sha256.as_bytes());
        } else {
            hasher.update(file.bytes.to_le_bytes());
            hasher.update(file.mtime_ms.to_le_bytes());
        }
        hasher.update(&[0]);
    }
    let revision = format!("drev-{}", &format!("{:x}", hasher.finalize())[..32]);

    let sessions = files
        .iter()
        .filter(|f| f.kind == "session-transcript")
        .count() as u64;
    let total = files.len() as u64;
    let summary = SourceSummary {
        sessions,
        artifacts: total - sessions,
        bytes: files.iter().map(|f| f.bytes).sum(),
    };

    Ok(SourceScan {
        identity,
        revision,
        format_version: LEGACY_FORMAT_VERSION,
        active_writer,
        issues,
        summary,
        files,
    })
}

/// 转写投影后的单条历史消息（`migration.plan` 的 history-entry 计数与
/// `migration.stage` 的 stg_history 行同源——单一实现保证 §7 check 1 的
/// 计数对账成立）。
pub struct ProjectedMessage {
    pub turn_id: String,
    pub role: String,
    /// history_entries 的 content 块（text 块投影）。
    pub content: Vec<Value>,
    pub created_at: String,
}

/// 一条转写文件的投影产物（会话字段 + 可转换消息）。
pub struct TranscriptProjection {
    pub session_key: Option<String>,
    pub title: Option<String>,
    pub tag: Option<String>,
    pub messages: Vec<ProjectedMessage>,
}

/// 解析一条转写（A6/A7：可转换 type → 消息行；其他 type 由
/// [`unsupported_projection`] 按类型计数，不在此处理）。坏行静默跳过
/// （inspect 的 issues 已报告，plan/stage 的计数同源一致）。
pub fn project_transcript(raw: &[u8]) -> TranscriptProjection {
    let text = String::from_utf8_lossy(raw).into_owned();
    let mut projection = TranscriptProjection {
        session_key: None,
        title: None,
        tag: None,
        messages: Vec::new(),
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if projection.session_key.is_none() {
            if let Some(key) = value["sessionId"].as_str() {
                if !key.is_empty() {
                    projection.session_key = Some(key.to_owned());
                }
            }
        }
        let kind = value["type"].as_str().unwrap_or_default().to_owned();
        match kind.as_str() {
            "session_metadata" => {
                if let Some(title) = value["title"].as_str() {
                    projection.title = Some(title.to_owned());
                }
                if let Some(tag) = value["tag"].as_str() {
                    projection.tag = Some(tag.to_owned());
                }
            }
            "accepted_input" => {
                if let Some(messages) = value["messages"].as_array() {
                    for message in messages {
                        push_projected(
                            &mut projection.messages,
                            value["turnId"].as_str().unwrap_or("legacy"),
                            message["role"].as_str().unwrap_or("user"),
                            &message["content"],
                            value["createdAt"].as_str().unwrap_or_default(),
                        );
                    }
                }
            }
            "assistant_message" | "durable_message" => {
                let message = &value["message"];
                push_projected(
                    &mut projection.messages,
                    value["turnId"].as_str().unwrap_or("legacy"),
                    message["role"].as_str().unwrap_or("assistant"),
                    &message["content"],
                    value["createdAt"].as_str().unwrap_or_default(),
                );
            }
            _ => {}
        }
    }
    projection
}

fn push_projected(
    out: &mut Vec<ProjectedMessage>,
    turn_id: &str,
    role: &str,
    content: &Value,
    created_at: &str,
) {
    // text 块投影（NodeSessionSearchPort.ts:219-241 的投影面）。
    let mut parts = Vec::new();
    match content {
        Value::String(text) => {
            if !text.is_empty() {
                parts.push(json!({"kind": "text", "text": text}));
            }
        }
        Value::Array(blocks) => {
            for block in blocks {
                if block["type"].as_str() == Some("text") {
                    if let Some(text) = block["text"].as_str() {
                        parts.push(json!({"kind": "text", "text": text}));
                    }
                }
            }
        }
        _ => {}
    }
    if !parts.is_empty() {
        out.push(ProjectedMessage {
            turn_id: turn_id.to_owned(),
            role: role.to_owned(),
            content: parts,
            created_at: created_at.to_owned(),
        });
    }
}

/// 转写条目中**不可转换且不搬运**的类型集合（§6 历史消息行的投影面之外）。
pub const UNCONVERTIBLE_TRANSCRIPT_TYPES: [&str; 10] = [
    "model_stream_event",
    "context_snapshot",
    "agent_instructions",
    "subagent_*",
    "plan_todo_*",
    "goal_changed",
    "compaction_*",
    "question_*",
    "permission_*",
    "file_snapshot_recorded",
];

/// unsupported_projection 清单（`migration.plan` 的 unsupportedEntries 与
/// `migration.stage`/`verify` 记录/复算共用同一函数——同输入同输出）。
/// 逐条列出不可转换且不搬运的条目：转写条目类型与 content 块类型（按类
/// 型计数）、always-on 与运行期杂项文件（逐文件）。
pub fn unsupported_projection(root: &Path, items: &[(String, String)]) -> Vec<String> {
    let mut unsupported: BTreeMap<String, u64> = BTreeMap::new();
    let mut per_file: Vec<String> = Vec::new();
    for (rel_path, kind) in items {
        match kind.as_str() {
            "session-transcript" | "subagent-transcript" => {
                let Ok(raw) = std::fs::read(root.join(rel_path)) else {
                    continue;
                };
                let text = String::from_utf8_lossy(&raw).into_owned();
                for line in text.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    let entry_type = value["type"].as_str().unwrap_or_default().to_owned();
                    if !matches!(
                        entry_type.as_str(),
                        "accepted_input"
                            | "assistant_message"
                            | "durable_message"
                            | "session_metadata"
                    ) {
                        *unsupported
                            .entry(format!("transcript-entry:{entry_type}"))
                            .or_insert(0) += 1;
                    }
                    let content = value
                        .get("messages")
                        .and_then(Value::as_array)
                        .map(|messages| {
                            messages
                                .iter()
                                .map(|message| message["content"].clone())
                                .collect::<Vec<_>>()
                        })
                        .or_else(|| {
                            value
                                .get("message")
                                .map(|message| vec![message["content"].clone()])
                        });
                    if let Some(contents) = content {
                        for item in contents {
                            if let Value::Array(blocks) = &item {
                                for block in blocks {
                                    if block["type"].as_str().is_some_and(|t| t != "text") {
                                        *unsupported
                                            .entry(format!(
                                                "content-block:{}",
                                                block["type"].as_str().unwrap_or("?")
                                            ))
                                            .or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "always-on" => per_file.push(format!("always-on:{rel_path}")),
            "runtime" => per_file.push(format!("runtime:{rel_path}")),
            _ => {}
        }
    }
    let mut list: Vec<String> = unsupported
        .into_iter()
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect();
    list.extend(per_file);
    list.sort();
    list
}

/// inspect 合同的结果 JSON（CLI local 操作直接复用）。
pub fn inspect_result_json(scan: &SourceScan) -> Value {
    json!({
        "identity": scan.identity,
        "formatVersion": scan.format_version,
        "activeWriter": scan.active_writer.clone(),
        "issues": scan
            .issues
            .iter()
            .map(|issue| {
                let mut value = json!({
                    "code": issue.code,
                    "message": issue.message,
                    "severity": issue.severity,
                });
                if let Some(path) = &issue.path {
                    value["path"] = json!(path);
                }
                value
            })
            .collect::<Vec<_>>(),
        "summary": {
            "sessions": scan.summary.sessions,
            "artifacts": scan.summary.artifacts,
            "bytes": scan.summary.bytes,
        },
    })
}

// ---------------------------------------------------------------------------
// 备份落盘
// ---------------------------------------------------------------------------

/// 备份条目（`entries.json` 行 = `list_backup_entries` 的 items 项）。
#[derive(Clone, Serialize, Deserialize)]
pub struct BackupEntryRecord {
    pub entry_id: String,
    pub kind: String,
    pub bytes: u64,
    pub checksum: String,
}

/// 备份登记（索引行 = manifest 字段 + dataRoot/destination）。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRecord {
    pub artifact_id: String,
    pub data_root: String,
    pub destination: String,
    pub format_version: String,
    pub created_at: String,
    pub source_identity: String,
    pub source_revision: String,
    pub checksum: String,
    pub includes_secrets: bool,
    pub entry_count: u64,
}

/// 备份索引路径：`<home>/v1/backups/index.json`。
pub fn backup_index_path(home: &Path) -> PathBuf {
    home.join("v1").join("backups").join("index.json")
}

fn backup_registry_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 读取整个备份索引（缺文件 = 空）。
pub fn read_backup_index(home: &Path) -> Result<Vec<BackupRecord>, ContractError> {
    let path = backup_index_path(home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(io_unavailable(format!("read {}: {err}", path.display())));
        }
    };
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|e| integrity_error(format!("backup index is corrupt: {e}")))?;
    parsed["backups"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|record| {
            serde_json::from_value(record.clone())
                .map_err(|e| integrity_error(format!("backup index row is corrupt: {e}")))
        })
        .collect()
}

/// artifactId → 登记行。
pub fn lookup_backup(
    home: &Path,
    artifact_id: &str,
) -> Result<Option<BackupRecord>, ContractError> {
    Ok(read_backup_index(home)?
        .into_iter()
        .find(|record| record.artifact_id == artifact_id))
}

/// 追加一条登记（进程内互斥 + 原子替换；唯一写者是 create_backup 完成路径）。
pub fn register_backup(home: &Path, record: &BackupRecord) -> Result<(), ContractError> {
    let _guard = backup_registry_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut records = read_backup_index(home)?;
    records.retain(|existing| existing.artifact_id != record.artifact_id);
    records.push(record.clone());
    let index = json!({
        "formatVersion": BACKUP_INDEX_FORMAT,
        "backups": records,
    });
    atomic_write_file(
        &backup_index_path(home),
        serde_json::to_string(&index).unwrap_or_default().as_bytes(),
        false,
    )
}

/// YAML 凭据置空副本：缩进栈路径匹配 `SECRET_YAML_KEYS`，命中行的值替换为
/// `null`，其余字节（含注释）原样保留。无法按行解析（非 UTF-8 或无冒号
/// 结构）时返回 Err —— 调用方拒绝导出该配置文件，绝不把明文凭据写出。
fn redact_yaml_secrets(raw: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(raw).map_err(|e| format!("not UTF-8: {e}"))?;
    let mut out = String::with_capacity(text.len());
    // (缩进, 键) 栈；逐行维护当前完整路径。
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line_no = index + 1;
        let body = raw_line.trim_end();
        let trimmed = body.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(raw_line);
            out.push('\n');
            continue;
        }
        let indent = body.len() - trimmed.len();
        if trimmed.starts_with("- ") || trimmed == "-" {
            return Err(format!("line {line_no}: sequence syntax is not supported"));
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("line {line_no}: not a `key:` line"));
        };
        let key = key.trim();
        if key.is_empty() || key.contains(' ') {
            return Err(format!("line {line_no}: unparsable key"));
        }
        while let Some((top_indent, _)) = stack.last() {
            if indent <= *top_indent {
                stack.pop();
            } else {
                break;
            }
        }
        let mut path = stack
            .iter()
            .map(|(_, segment)| segment.clone())
            .collect::<Vec<_>>();
        path.push(key.to_owned());
        let dotted = path.join(".");
        if SECRET_YAML_KEYS.contains(&dotted.as_str()) && !value.trim().is_empty() {
            out.push_str(&format!("{}{}: null\n", " ".repeat(indent), key));
        } else {
            out.push_str(raw_line);
            out.push('\n');
        }
        if value.trim().is_empty() {
            stack.push((indent, key.to_owned()));
        }
    }
    Ok(out.into_bytes())
}

/// 写一个备份：临时目录 → 校验 → 原子 rename。返回 (checksum, entryCount)。
pub fn write_backup(
    root: &Path,
    scan: &SourceScan,
    artifact_id: &str,
    destination: &Path,
    includes_secrets: bool,
) -> Result<(String, u64), ContractError> {
    if destination.exists() {
        return Err(destination_exists(&destination.to_string_lossy()));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("destination has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| io_unavailable(format!("mkdir {}: {e}", parent.display())))?;
    let tmp = parent.join(format!(".{}.backup-tmp", artifact_id));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .map_err(|e| io_unavailable(format!("clean stale tmp {}: {e}", tmp.display())))?;
    }
    std::fs::create_dir_all(tmp.join("data"))
        .map_err(|e| io_unavailable(format!("mkdir data: {e}")))?;

    let result = write_backup_into(&tmp, root, scan, artifact_id, includes_secrets);
    match result {
        Ok(outcome) => {
            // 原子发布：rename 目录到不存在的新目标（rename 到已存在的非空
            // 目录本会失败；再显式查一次把空目录情形也挡住）。
            if destination.exists() {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(destination_exists(&destination.to_string_lossy()));
            }
            std::fs::rename(&tmp, destination).map_err(|err| {
                let _ = std::fs::remove_dir_all(&tmp);
                io_unavailable(format!(
                    "publish backup to {}: {err}",
                    destination.display()
                ))
            })?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(error)
        }
    }
}

fn write_backup_into(
    tmp: &Path,
    root: &Path,
    scan: &SourceScan,
    artifact_id: &str,
    includes_secrets: bool,
) -> Result<(String, u64), ContractError> {
    let data_dir = tmp.join("data");
    let mut entries: Vec<BackupEntryRecord> = Vec::new();
    for file in &scan.files {
        if !includes_secrets && file.kind == "credential" {
            continue; // 默认不导出凭据（a038 / 008 §6 凭据行）。
        }
        let source_path = root.join(&file.rel_path);
        let bytes = std::fs::read(&source_path)
            .map_err(|e| io_unavailable(format!("read {}: {e}", source_path.display())))?;
        let effective = if !includes_secrets && file.kind == "config" {
            redact_yaml_secrets(&bytes).map_err(|reason| {
                integrity_error(format!(
                    "config file `{}` could not be redacted: {}",
                    file.rel_path, reason
                ))
            })?
        } else {
            bytes
        };
        let checksum = sha256_hex(&effective);
        let target = data_dir.join(&checksum);
        if !target.exists() {
            atomic_write_file(&target, &effective, true).map_err(|error| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("write backup entry `{}`: {}", file.rel_path, error.message),
                    RetrySpec::backoff_read(Some(250)),
                    EffectStatus::Unknown,
                )
            })?;
        }
        entries.push(BackupEntryRecord {
            entry_id: file.rel_path.clone(),
            kind: file.kind.to_owned(),
            bytes: effective.len() as u64,
            checksum,
        });
    }
    entries.sort_by(|a, b| a.entry_id.cmp(&b.entry_id));
    let entries_doc = json!({
        "formatVersion": BACKUP_ENTRIES_FORMAT,
        "entries": entries,
    });
    let entries_bytes = serde_json::to_vec(&entries_doc)
        .map_err(|e| io_unavailable(format!("serialize entries: {e}")))?;
    let checksum = sha256_hex(&entries_bytes);
    atomic_write_file(&tmp.join("entries.json"), &entries_bytes, false).map_err(|error| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write entries.json: {}", error.message),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;

    // 发布前复算校验（「先临时文件校验再发布」）：entries.json 整体哈希 +
    // 每条目内容重读重算。
    let reread = std::fs::read(tmp.join("entries.json"))
        .map_err(|e| io_unavailable(format!("reread entries.json: {e}")))?;
    if sha256_hex(&reread) != checksum {
        return Err(integrity_error(
            "backup entries.json changed under the write (checksum mismatch)",
        ));
    }
    for entry in &entries {
        let bytes = std::fs::read(data_dir.join(&entry.checksum))
            .map_err(|e| io_unavailable(format!("reread entry `{}`: {e}", entry.entry_id)))?;
        if sha256_hex(&bytes) != entry.checksum {
            return Err(integrity_error(format!(
                "backup entry `{}` failed its self-verification checksum",
                entry.entry_id
            )));
        }
    }

    let manifest = json!({
        "artifactId": artifact_id,
        "formatVersion": BACKUP_FORMAT_VERSION,
        "createdAt": now_rfc3339(),
        "sourceIdentity": scan.identity,
        "sourceRevision": scan.revision,
        "checksum": checksum,
        "includesSecrets": includes_secrets,
        "entryCount": entries.len(),
    });
    let manifest_bytes = serde_json::to_vec(&manifest)
        .map_err(|e| io_unavailable(format!("serialize manifest: {e}")))?;
    atomic_write_file(&tmp.join("manifest.json"), &manifest_bytes, false).map_err(|error| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write manifest.json: {}", error.message),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?;
    Ok((checksum, entries.len() as u64))
}

/// 读取一个备份的 manifest 与条目清单（校验 manifest.checksum）。
pub fn load_backup_at(
    destination: &Path,
) -> Result<(Value, Vec<BackupEntryRecord>), ContractError> {
    let manifest_path = destination.join("manifest.json");
    let manifest_raw = std::fs::read_to_string(&manifest_path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            integrity_error(format!(
                "backup at {} has no manifest.json",
                destination.display()
            ))
        } else {
            io_unavailable(format!("read {}: {err}", manifest_path.display()))
        }
    })?;
    let manifest: Value = serde_json::from_str(&manifest_raw)
        .map_err(|e| integrity_error(format!("backup manifest is corrupt: {e}")))?;
    let entries_path = destination.join("entries.json");
    let entries_raw = std::fs::read(&entries_path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            integrity_error(format!(
                "backup at {} has no entries.json",
                destination.display()
            ))
        } else {
            io_unavailable(format!("read {}: {err}", entries_path.display()))
        }
    })?;
    let actual = sha256_hex(&entries_raw);
    let expected = manifest["checksum"].as_str().unwrap_or_default();
    if expected.is_empty() || actual != expected {
        return Err(integrity_error(format!(
            "backup checksum mismatch (manifest `{expected}`, computed `{actual}`)"
        )));
    }
    let entries_doc: Value = serde_json::from_slice(&entries_raw)
        .map_err(|e| integrity_error(format!("backup entries index is corrupt: {e}")))?;
    entries_doc["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|entry| {
            serde_json::from_value(entry.clone())
                .map_err(|e| integrity_error(format!("backup entry row is corrupt: {e}")))
        })
        .collect()
}

/// 读取一个条目的字节（按条目 checksum 复算校验）。
pub fn backup_entry_bytes(
    destination: &Path,
    entry: &BackupEntryRecord,
) -> Result<Vec<u8>, ContractError> {
    let path = destination.join("data").join(&entry.checksum);
    let bytes = std::fs::read(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            integrity_error(format!(
                "backup entry `{}` content is missing",
                entry.entry_id
            ))
        } else {
            io_unavailable(format!("read {}: {err}", path.display()))
        }
    })?;
    let actual = sha256_hex(&bytes);
    if actual != entry.checksum {
        return Err(integrity_error(format!(
            "backup entry `{}` checksum mismatch (expected `{}`, computed `{actual}`)",
            entry.entry_id, entry.checksum
        )));
    }
    Ok(bytes)
}

/// 按登记行加载备份（list/read 两个 local 操作的入口）。
pub fn load_registered_backup(
    home: &Path,
    artifact_id: &str,
) -> Result<(BackupRecord, Value, Vec<BackupEntryRecord>), ContractError> {
    let record = lookup_backup(home, artifact_id)?.ok_or_else(|| {
        ContractError::new(
            CODE_NOT_FOUND,
            format!("backup artifact `{artifact_id}` is not registered in this home"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let destination = PathBuf::from(&record.destination);
    let (manifest, entries) = load_backup_at(&destination)?;
    if manifest["artifactId"].as_str() != Some(record.artifact_id.as_str()) {
        return Err(integrity_error(format!(
            "backup at {} does not match artifact `{}`",
            record.destination, record.artifact_id
        )));
    }
    Ok((record, manifest, entries))
}

// ---------------------------------------------------------------------------
// 域：data.create_backup（async / idempotentSubmit）
// ---------------------------------------------------------------------------

fn data_backup_db(home: &Path) -> Result<Arc<DomainDb>, ContractError> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let db = Arc::new(DomainDb::open(home, &[DDL_DOMAIN_OPERATIONS])?);
    registry.insert(home.to_path_buf(), Arc::downgrade(&db));
    Ok(db)
}

async fn data_create_backup(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = data_backup_db(rt.store().home()).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let data_root_raw = input["dataRoot"].as_str().unwrap_or_default().to_owned();
    let destination_raw = input["destination"].as_str().unwrap_or_default().to_owned();
    let includes_secrets = input["includeSecrets"].as_bool().unwrap_or(false);
    if data_root_raw.is_empty() || destination_raw.is_empty() {
        return Err(invalid("dataRoot and destination are required").into());
    }
    let data_root = PathBuf::from(&data_root_raw);
    let destination = PathBuf::from(&destination_raw);

    // 受理前扫描（FORMAT_UNSUPPORTED / WRITER_ACTIVE / 目标已存在）。
    let pre_scan = tokio::task::spawn_blocking({
        let data_root = data_root.clone();
        move || scan_source_root(&data_root, ScanDepth::Full)
    })
    .await
    .map_err(join_err)?
    .map_err(DispatchError::Application)?;
    if let Some(writer) = &pre_scan.active_writer {
        return Err(writer_active(writer).into());
    }
    if destination.exists() {
        return Err(destination_exists(&destination_raw).into());
    }

    let input_for_accept = input.clone();
    let destination_for_validate = destination.clone();
    let operation = accept_domain_operation(
        &db,
        "data.create_backup",
        &input_for_accept,
        key.as_deref(),
        || format!("op-{}", store.next_id()),
        move || {
            // 廉价复验：目标仍不存在（写者重扫在 worker 内做一致性快照）。
            if destination_for_validate.exists() {
                return Err(destination_exists(&destination_raw));
            }
            Ok(())
        },
    )?;
    let operation_id = operation.operation_id.clone();

    let db_for_worker = db.clone();
    let home_for_worker = rt.store().home().to_path_buf();
    let artifact_id = store.next_id();
    tokio::task::spawn_blocking(move || {
        let _ = mark_domain_operation_running(&db_for_worker, &operation_id, "");
        let outcome = execute_create_backup(
            &home_for_worker,
            &data_root,
            &destination,
            &artifact_id,
            includes_secrets,
        );
        match outcome {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &db_for_worker,
                    &operation_id,
                    "",
                    "succeeded",
                    &completion,
                );
            }
            Err(error) => {
                let _ = finish_domain_operation(
                    &db_for_worker,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"errorCode": error.code, "message": error.message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// worker：重扫（一致性快照）→ 备份 → 登记 → `backup.created` 事件。
fn execute_create_backup(
    home: &Path,
    data_root: &Path,
    destination: &Path,
    artifact_id: &str,
    includes_secrets: bool,
) -> Result<Value, ContractError> {
    let scan = scan_source_root(data_root, ScanDepth::Full)?;
    if let Some(writer) = &scan.active_writer {
        return Err(writer_active(writer));
    }
    let (checksum, entry_count) =
        write_backup(data_root, &scan, artifact_id, destination, includes_secrets)?;
    let destination_canonical = canonical_path_string(destination)?;
    let record = BackupRecord {
        artifact_id: artifact_id.to_owned(),
        data_root: canonical_path_string(data_root)?,
        destination: destination_canonical.clone(),
        format_version: BACKUP_FORMAT_VERSION.to_owned(),
        created_at: now_rfc3339(),
        source_identity: scan.identity.clone(),
        source_revision: scan.revision.clone(),
        checksum: checksum.clone(),
        includes_secrets,
        entry_count,
    };
    register_backup(home, &record)?;
    // 事件（合同 events：backup.created）。
    let db = data_backup_db(home)?;
    let _ = db.append_event(
        artifact_id,
        "",
        "backup.created",
        &json!({
            "artifactId": artifact_id,
            "destination": destination_canonical,
            "checksum": checksum,
            "entryCount": entry_count,
            "includesSecrets": includes_secrets,
        }),
    );
    Ok(json!({
        "artifactId": artifact_id,
        "destination": destination_canonical,
        "formatVersion": BACKUP_FORMAT_VERSION,
        "checksum": checksum,
        "sourceRevision": scan.revision,
        "includesSecrets": includes_secrets,
        "entryCount": entry_count,
    }))
}

// ---------------------------------------------------------------------------
// 域内测试共享挂具（migration.rs 的域内测试同用；#[cfg(test)] 收缩面）
// ---------------------------------------------------------------------------

/// 构造一个基线形态的旧数据根（A1–A11 子集 + cron/always-on 由调用方追加）。
#[cfg(test)]
pub(super) fn seed_legacy_root(tmp: &std::path::Path) -> std::path::PathBuf {
    let root = tmp.join("legacy-home");
    std::fs::create_dir_all(root.join("projects").join("demo-proj").join("chats")).unwrap();
    std::fs::create_dir_all(root.join("runs")).unwrap();
    std::fs::write(
        root.join("pilotdeck.yaml"),
        "adapters:\n  feishu:\n    enabled: false\n    appId: cli-x\n    appSecret: super-secret\n",
    )
    .unwrap();
    std::fs::write(
        root.join("weixin-credentials.json"),
        r#"{"baseUrl":"https://x","botToken":"tok","accountId":"a1"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("projects").join("demo-proj").join(".cwd"),
        "/tmp/demo",
    )
    .unwrap();
    let transcript = concat!(
        r#"{"type":"session_metadata","sessionId":"tui:project=/tmp/demo:default","turnId":"t0","sequence":1,"createdAt":"2026-11-01T00:00:00Z","title":"Demo","tag":"old"}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"tui:project=/tmp/demo:default","turnId":"t1","sequence":2,"createdAt":"2026-11-01T00:00:01Z","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"tui:project=/tmp/demo:default","turnId":"t1","sequence":3,"createdAt":"2026-11-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
        "\n",
    );
    std::fs::write(
        root.join("projects")
            .join("demo-proj")
            .join("chats")
            .join("tui-project-demo-default.jsonl"),
        transcript,
    )
    .unwrap();
    std::fs::write(
            root.join("runs").join("registry.json"),
            r#"{"schemaVersion":1,"runs":{"11111111-1111-1111-1111-111111111111":{"sessionKey":"tui:project=/tmp/demo:default","state":"completed","revision":"r1","acceptedAt":"2026-11-01T00:00:00Z","updatedAt":"2026-11-01T00:00:05Z"}}}"#,
        )
        .unwrap();
    root
}

/// 断言一次 dispatch 以应用错误失败，返回该错误（migration.rs 同用）。
#[cfg(test)]
pub(super) async fn dispatch_err(
    rt: &Arc<PublicV1Runtime>,
    method: &str,
    input: Value,
    key: Option<&str>,
) -> ContractError {
    match rt.dispatch(method, input, key, None).await {
        Err(DispatchError::Application(e)) => e,
        other => panic!("`{method}` expected application error, got {other:?}"),
    }
}

/// 轮询 `operations.get` 直到终态，返回 operation JSON（migration.rs 同用）。
#[cfg(test)]
pub(super) async fn wait_domain_operation(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_, got) = rt
            .dispatch(
                "operations.get",
                json!({"operationId": operation_id}),
                None,
                None,
            )
            .await
            .expect("operations.get");
        let operation = &got["operation"];
        let state = operation["state"].as_str().unwrap_or_default();
        if state == "succeeded" || state == "failed" {
            return operation.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "operation `{operation_id}` never became terminal"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::{InstanceMeta, V1Store};
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

    #[test]
    fn scan_classifies_and_detects_writers() {
        let tmp = TempDir::new("data-scan");
        let root = seed_legacy_root(&tmp.0);
        let scan = scan_source_root(&root, ScanDepth::Full).unwrap();
        assert_eq!(scan.format_version, LEGACY_FORMAT_VERSION);
        assert_eq!(scan.summary.sessions, 1);
        assert!(scan.active_writer.is_none(), "no writer expected");
        assert!(
            scan.issues.is_empty(),
            "seeded root is clean: {:?}",
            scan.issues
                .iter()
                .map(|i| i.code.clone())
                .collect::<Vec<_>>()
        );
        assert!(scan.identity.starts_with("dident-"));
        assert!(scan.revision.starts_with("drev-"));

        // 内容变化 → revision 变化。
        std::fs::write(root.join("runs").join("registry.json"), "{}").unwrap();
        let rescan = scan_source_root(&root, ScanDepth::Full).unwrap();
        assert_ne!(scan.revision, rescan.revision);

        // 未知根形态 → FORMAT_UNSUPPORTED。
        let empty = tmp.0.join("empty-root");
        std::fs::create_dir_all(&empty).unwrap();
        let err = scan_source_root(&empty, ScanDepth::Metadata).unwrap_err();
        assert_eq!(err.code, "FORMAT_UNSUPPORTED");

        // 持锁 → active_writer 非空。
        let lock_path = root.join("runs").join("registry.json.lock");
        std::fs::write(&lock_path, b"held").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .unwrap();
            file.lock().unwrap();
            let held = scan_source_root(&root, ScanDepth::Metadata).unwrap();
            assert!(held.active_writer.is_some());
            file.unlock().unwrap();
        }
        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn yaml_secret_redaction_preserves_the_rest() {
        let raw = b"adapters:\n  feishu:\n    enabled: false\n    appId: cli-x\n    appSecret: super-secret\n  wecom:\n    token: plain-token\n";
        let redacted = redact_yaml_secrets(raw).unwrap();
        let text = String::from_utf8_lossy(&redacted).into_owned();
        assert!(!text.contains("super-secret"), "{text}");
        assert!(!text.contains("plain-token"), "{text}");
        assert!(text.contains("appId: cli-x"), "{text}");
        assert!(text.contains("appSecret: null"), "{text}");
        assert!(text.contains("token: null"), "{text}");
    }

    #[tokio::test]
    async fn create_backup_roundtrip_index_and_secret_exclusion() {
        let tmp = TempDir::new("data-backup");
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        let root = seed_legacy_root(&tmp.0);
        let destination = tmp.0.join("backups").join("b1");

        let input = json!({
            "dataRoot": root.to_string_lossy(),
            "destination": destination.to_string_lossy(),
        });
        let (_, receipt) = rt
            .dispatch("data.create_backup", input.clone(), Some("backup-1"), None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("data.create_backup", &receipt)
            .unwrap_or_else(|e| panic!("receipt schema: {e:?}"));
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = wait_domain_operation(&rt, &operation_id).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let completion = &operation["result"];
        assert_eq!(completion["includesSecrets"], json!(false));
        assert!(completion["entryCount"].as_u64().unwrap() >= 4);
        assert!(completion["sourceRevision"].as_str().is_some());
        let artifact_id = completion["artifactId"].as_str().unwrap().to_owned();

        // 索引可查；凭据未进备份；pilotdeck.yaml 为置空副本。
        let (record, manifest, entries) = load_registered_backup(&home, &artifact_id).unwrap();
        assert_eq!(manifest["checksum"], completion["checksum"]);
        assert!(!entries.iter().any(|e| e.kind == "credential"));
        let config = entries
            .iter()
            .find(|e| e.kind == "config")
            .expect("config entry");
        let bytes = backup_entry_bytes(Path::new(&record.destination), config).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("super-secret"), "secret redacted: {text}");
        assert!(text.contains("appSecret: null"));

        // 同幂等键重放 → 同一受理（deduplicated）。
        let (_, replay) = rt
            .dispatch("data.create_backup", input, Some("backup-1"), None)
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], json!(true));

        // 同名目标（不同键）→ DESTINATION_EXISTS。
        let err = dispatch_err(
            &rt,
            "data.create_backup",
            json!({
                "dataRoot": root.to_string_lossy(),
                "destination": destination.to_string_lossy(),
            }),
            Some("backup-2"),
        )
        .await;
        assert_eq!(err.code, "DESTINATION_EXISTS");

        // 未知根 → FORMAT_UNSUPPORTED。
        let err = dispatch_err(
            &rt,
            "data.create_backup",
            json!({
                "dataRoot": tmp.0.join("nope").to_string_lossy(),
                "destination": tmp.0.join("backups").join("b2").to_string_lossy(),
            }),
            Some("backup-3"),
        )
        .await;
        assert_eq!(err.code, "FORMAT_UNSUPPORTED");
    }
}
