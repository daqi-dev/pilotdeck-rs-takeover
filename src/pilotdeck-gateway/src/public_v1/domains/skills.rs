//! `skills.*` — 技能目录 CRUD、校验、扫描与两段式导入
//! （extensions-management，S3；capability a031）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `skills.list/read/put/
//! remove/validate/scan/plan_import/apply_import` 条目。TS 基线
//! （cc7c4e6 `SkillManager.ts`）是语义参考：scope 三层（user=global /
//! project / 对应存储目录）、`validate` 的 SKILL.md frontmatter 与尺寸
//! 规则（缺 SKILL.md、frontmatter 缺 name/description、>500 文件、
//! >50MB 总量、>10MB 单文件、风险扩展名警告）、`scan` 的父目录/
//! > 子目录发现、`migrateSkills.ts` 的 legacy 导入（conflictMode
//! > skip/overwrite/rename，rename 目标 `<slug>-imported[-N]`，slug 规则
//! > `^[a-zA-Z0-9][A-Za-z0-9._-]{0,99}$` 且不含 `..`）。合同把 TS 的
//! > skill_import（单段复制）重设计为两段式 plan/apply（t018 模式：
//! > ChangePlan + 过期拒绝 + revision CAS），本域照合同落。
//!
//! ## v1 薄实现裁决
//!
//! * 旧线 `src/skills/`（约 6k 行）与 `runtime/mcp` 一样是进程装配件
//!   （ICU 排序、外部模块 host、npm 注册表搜索）；按简报只读参考，v1 落
//!   目录 CRUD + 结构校验 + 两段式导入。注册表源（`source=registry`、
//!   `origin=registry`）在本运行时没有配置注册表——**失败而不是空成功**
//!   （`UPSTREAM_ERROR`）；`origin=archive` 无解包装配 → `UNAVAILABLE`。
//!   二者登记为差异。
//! * frontmatter 解析用最小 YAML 子集（`---` 块内 `key: value` 行，
//!   允许成对引号），不做全量 YAML（基线行为由 name/description 必填与
//!   诊断锚定）；登记为差异。
//! * 存储：`<home>/v1/skills/{global,projects/<id>,sessions/<id>}/
//! <skillId>/`（域数据统一在 `v1/` 下，artifacts 域同惯例；不占用旧线
//!   `<home>/skills` 布局）。SQLite 登记行（`v1_skills`）承载 revision 与
//!   scope 聚合版本（`v1_skill_scopes`，plan/apply 的 CAS 基准）。
//! * `skills.put` 的 revision 是**单个技能**的修订（创建用 `absent`）；
//!   `remove` 返回 scope 聚合版本（资源已删）。`apply_import` 的
//!   `expectedRevision` CAS 的是目标 scope 的聚合版本。
//! * `applied` 恒为 `false`（「保存不宣称已装载」——v1 无装载代际），
//!   结果 `generation` 恒 `null`。
//! * 路径安全：relativePath 拒绝绝对路径与 `..`；读取解析后做符号链接
//!   越界检查（canonicalize 后必须在技能目录内）；`scan`/`plan_import`
//!   的 sourceRoot 在 project/session scope 必须落在项目工作区内
//!   （global scope 由调用方显式授权：绝对路径即可）。
//!
//! ## 两段式导入
//!
//! * `plan_import`（sync 只读）：发现源技能目录（源目录本身带 SKILL.md
//!   或其直接子目录），逐个校验，按 conflictPolicy/renameMap 计算
//!   `changes[]`（action: import / overwrite / skip / rename / reject），
//!   产出 `planId` + `planHash`（计划内容）+ `sourceHash`（源树内容）+
//!   `expectedRevision`（目标 scope 当前版本）。计划 10 分钟过期
//!   （memory/snapshots 域同款 TTL），存 `v1_skill_plans`。
//! * `apply_import`（async idempotentSubmit）：受理即复验（计划存在/
//!   未消费/未过期/planHash 匹配 → 否则 `PLAN_STALE`；expectedRevision
//!   → `REVISION_CONFLICT`），执行体里**一次性消费计划**（CAS UPDATE）
//!   并复算 sourceHash（变了 → `INTEGRITY_ERROR`，「损坏下载不标
//!   installed」），逐技能暂存→rename 原子发布，`skills.changed` 事件。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::{ProjectResolveError, resolve_project};
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 合同 error 表里 protocol 常量尚未覆盖的码。
const CODE_PATH_OUTSIDE_SCOPE: &str = "PATH_OUTSIDE_SCOPE";
const CODE_IO_ERROR: &str = "IO_ERROR";
const CODE_PLAN_STALE: &str = "PLAN_STALE";
const CODE_INTEGRITY_ERROR: &str = "INTEGRITY_ERROR";
const CODE_UPSTREAM_ERROR: &str = "UPSTREAM_ERROR";

/// 从未配置过的 scope 的聚合版本哨兵（合同 expectedRevision：创建用
/// `absent`）。
const REVISION_ABSENT: &str = "absent";
/// 两段式计划过期窗口（memory/snapshots 域同款）。
const PLAN_TTL_MINUTES: i64 = 10;
/// TS `SkillManager` 的尺寸纪律。
const MAX_FILE_COUNT: usize = 500;
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const LARGE_FILE_BYTES: u64 = 1024 * 1024;
const RISKY_EXTS: [&str; 10] = [
    ".sh", ".bash", ".zsh", ".fish", ".exe", ".bat", ".cmd", ".dll", ".so", ".dylib",
];
/// 扫描 / 源发现的条目上限（防病态输入的无限遍历）。
const WALK_ENTRY_CAP: usize = 8192;

const DDL_SKILLS: &str = "CREATE TABLE IF NOT EXISTS v1_skills (
    skill_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    project_id TEXT,
    session_id TEXT,
    name TEXT NOT NULL,
    origin TEXT NOT NULL,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (scope_key, skill_id)
)";
const DDL_SKILL_SCOPES: &str = "CREATE TABLE IF NOT EXISTS v1_skill_scopes (
    scope_key TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const DDL_SKILL_PLANS: &str = "CREATE TABLE IF NOT EXISTS v1_skill_plans (
    plan_id TEXT PRIMARY KEY,
    plan_hash TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open',
    expires_at TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_skills", DDL_SKILLS);
    reg.schema("v1_skill_scopes", DDL_SKILL_SCOPES);
    reg.schema("v1_skill_plans", DDL_SKILL_PLANS);

    reg.handler("skills.list", |rt, input, _key, _rev| {
        Box::pin(skills_list(rt, input))
    });
    reg.handler("skills.read", |rt, input, _key, _rev| {
        Box::pin(skills_read(rt, input))
    });
    reg.handler("skills.put", |rt, input, _key, _rev| {
        Box::pin(skills_put(rt, input))
    });
    reg.handler("skills.remove", |rt, input, _key, _rev| {
        Box::pin(skills_remove(rt, input))
    });
    reg.handler("skills.validate", |rt, input, _key, _rev| {
        Box::pin(skills_validate(rt, input))
    });
    reg.handler("skills.scan", |rt, input, _key, _rev| {
        Box::pin(skills_scan(rt, input))
    });
    reg.handler("skills.plan_import", |rt, input, _key, _rev| {
        Box::pin(skills_plan_import(rt, input))
    });
    reg.handler("skills.apply_import", |rt, input, key, _rev| {
        Box::pin(skills_apply_import(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 错误辅助
// ---------------------------------------------------------------------------

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("skills expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn path_outside_scope(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_PATH_OUTSIDE_SCOPE,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn io_error(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_IO_ERROR,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("skills worker task failed: {err}"),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// 作用域（与 mcp.rs 同形的合同 scope；域内独立持有避免跨域耦合）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkillScope {
    kind: &'static str,
    project_id: Option<String>,
    session_id: Option<String>,
}

impl SkillScope {
    fn key(&self) -> String {
        match (self.kind, &self.project_id, &self.session_id) {
            ("project", Some(p), _) => format!("project:{p}"),
            ("session", _, Some(s)) => format!("session:{s}"),
            _ => "global".to_owned(),
        }
    }

    /// origin 标签：条目所在层。
    fn origin(&self) -> &'static str {
        self.kind
    }

    /// 继承链（底→顶）：global → project → session。
    fn chain(&self) -> Vec<Self> {
        match self.kind {
            "global" => vec![Self::global()],
            "project" => vec![
                Self::global(),
                Self {
                    kind: "project",
                    project_id: self.project_id.clone(),
                    session_id: None,
                },
            ],
            _ => vec![
                Self::global(),
                Self {
                    kind: "project",
                    project_id: self.project_id.clone(),
                    session_id: None,
                },
                Self {
                    kind: "session",
                    project_id: self.project_id.clone(),
                    session_id: self.session_id.clone(),
                },
            ],
        }
    }

    fn global() -> Self {
        Self {
            kind: "global",
            project_id: None,
            session_id: None,
        }
    }
}

fn parse_scope(rt: &PublicV1Runtime, input: &Value) -> Result<SkillScope, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(invalid("input.scope is required"));
    };
    let id = scope.get("id").and_then(Value::as_str);
    match scope.get("kind").and_then(Value::as_str) {
        Some("global") => {
            if id.is_some() {
                return Err(invalid("scope.id must be null for kind=global"));
            }
            Ok(SkillScope::global())
        }
        Some("project") => {
            let Some(project_id) = id.filter(|value| !value.is_empty()) else {
                return Err(invalid("scope.id is required for kind=project"));
            };
            if rt.store().get_project(project_id).is_err() {
                return Err(invalid(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(SkillScope {
                kind: "project",
                project_id: Some(project_id.to_owned()),
                session_id: None,
            })
        }
        Some("session") => {
            let Some(session_id) = id.filter(|value| !value.is_empty()) else {
                return Err(invalid("scope.id is required for kind=session"));
            };
            let session = rt
                .store()
                .get_session(session_id)
                .map_err(|_| invalid(format!("scope session `{session_id}` does not exist")))?;
            let Some(project_id) = session["session"].get("projectId").and_then(Value::as_str)
            else {
                return Err(invalid(format!(
                    "scope session `{session_id}` has no project"
                )));
            };
            Ok(SkillScope {
                kind: "session",
                project_id: Some(project_id.to_owned()),
                session_id: Some(session_id.to_owned()),
            })
        }
        _ => Err(invalid("scope.kind must be global|project|session")),
    }
}

// ---------------------------------------------------------------------------
// 校验（TS SkillManager.validate 的 v1 投影 → 合同 Diagnostics）
// ---------------------------------------------------------------------------

fn diagnostic(code: &str, path: Option<&str>, message: impl Into<String>, severity: &str) -> Value {
    json!({
        "code": code,
        "path": path,
        "message": message.into(),
        "severity": severity,
    })
}

fn has_error(diagnostics: &[Value]) -> bool {
    diagnostics
        .iter()
        .any(|d| d.get("severity").and_then(Value::as_str) == Some("error"))
}

/// 最小 YAML frontmatter：`---` 起始块内的 `key: value` 行（值允许成对
/// 引号）。返回 (frontmatter, 是否有起始块)。
fn parse_frontmatter(content: &str) -> (HashMap<String, String>, bool) {
    let trimmed = content.trim_start_matches('\u{feff}');
    let mut map = HashMap::new();
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or_default().trim_end();
    if first != "---" {
        return (map, false);
    }
    let mut has_close = false;
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.trim() == "---" {
            has_close = true;
            break;
        }
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            let unquoted = if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                &value[1..value.len() - 1]
            } else {
                value
            };
            map.insert(key.trim().to_owned(), unquoted.to_owned());
        }
    }
    let _ = has_close;
    (map, true)
}

/// TS `requiredFrontmatter`（缺 SKILL.md 内容时由调用方先报 NO_SKILL_MD）。
fn validate_frontmatter(content: &str, path: &str) -> Vec<Value> {
    let mut diagnostics = Vec::new();
    if content.trim().is_empty() {
        diagnostics.push(diagnostic(
            "NO_SKILL_MD",
            Some(path),
            "SKILL.md is empty or missing.",
            "error",
        ));
        return diagnostics;
    }
    let (frontmatter, has_block) = parse_frontmatter(content);
    if !has_block {
        diagnostics.push(diagnostic(
            "FRONTMATTER_MISSING",
            Some(path),
            "SKILL.md does not start with a YAML frontmatter block.",
            "error",
        ));
        return diagnostics;
    }
    let filled = |key: &str| {
        frontmatter
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    };
    if filled("name").is_none() {
        diagnostics.push(diagnostic(
            "FRONTMATTER_MISSING_NAME",
            Some(path),
            "Frontmatter is missing required field: name.",
            "error",
        ));
    }
    match filled("description") {
        None => diagnostics.push(diagnostic(
            "FRONTMATTER_MISSING_DESCRIPTION",
            Some(path),
            "Frontmatter is missing required field: description.",
            "error",
        )),
        Some(description) => {
            let length = description.chars().count();
            if length < 20 {
                diagnostics.push(diagnostic(
                    "DESCRIPTION_SHORT",
                    Some(path),
                    format!(
                        "Description is short ({length} chars). Consider expanding for better \
                         discovery."
                    ),
                    "warning",
                ));
            }
            if length > 1024 {
                diagnostics.push(diagnostic(
                    "DESCRIPTION_LONG",
                    Some(path),
                    format!(
                        "Description is very long ({length} chars). Most slash-menu surfaces \
                         truncate this."
                    ),
                    "warning",
                ));
            }
        }
    }
    diagnostics
}

/// relativePath 安全检查（TS `unsafe_path`：拒绝 `..` 与绝对路径；另拒
/// 反斜杠分隔与空段）。
fn safe_relative_path(relative: &str) -> bool {
    if relative.is_empty() || relative.contains('\\') {
        return false;
    }
    let path = Path::new(relative);
    if !path.is_relative() {
        return false;
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return false,
        }
    }
    !relative
        .split('/')
        .any(|part| part.is_empty() || part == ".")
}

fn risky_extension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RISKY_EXTS.iter().any(|ext| lower.ends_with(ext))
}

/// 技能文件集的结构校验（`skills.validate` 的 files 形态与 `skills.put`
/// 的写前校验共用）。`sources`：`(relativePath, bytes)`。
fn validate_file_set(sources: &[(String, Vec<u8>)], skill_md_index: Option<usize>) -> Vec<Value> {
    let mut diagnostics = Vec::new();
    if skill_md_index.is_none() {
        diagnostics.push(diagnostic(
            "NO_SKILL_MD",
            None,
            "No SKILL.md at the root of the skill.",
            "error",
        ));
    }
    let mut total_bytes: u64 = 0;
    for (relative, bytes) in sources {
        if !safe_relative_path(relative) {
            diagnostics.push(diagnostic(
                "UNSAFE_PATH",
                Some(relative),
                format!("File path is unsafe: {relative}"),
                "error",
            ));
            continue;
        }
        total_bytes += bytes.len() as u64;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            diagnostics.push(diagnostic(
                "FILE_TOO_LARGE",
                Some(relative),
                format!(
                    "File exceeds {} bytes: {} ({} bytes)",
                    MAX_FILE_BYTES,
                    relative,
                    bytes.len()
                ),
                "error",
            ));
        } else if bytes.len() as u64 > LARGE_FILE_BYTES {
            diagnostics.push(diagnostic(
                "FILE_LARGE",
                Some(relative),
                format!("Large file: {relative}"),
                "warning",
            ));
        }
        if risky_extension(relative) {
            diagnostics.push(diagnostic(
                "RISKY_EXTENSION",
                Some(relative),
                format!("Executable-style file: {relative}"),
                "warning",
            ));
        }
    }
    if sources.len() > MAX_FILE_COUNT {
        diagnostics.push(diagnostic(
            "TOO_MANY_FILES",
            None,
            format!("Skill has more than {MAX_FILE_COUNT} files."),
            "error",
        ));
    }
    if total_bytes > MAX_TOTAL_BYTES {
        diagnostics.push(diagnostic(
            "TOTAL_TOO_LARGE",
            None,
            format!("Skill total size exceeds {MAX_TOTAL_BYTES} bytes ({total_bytes})."),
            "error",
        ));
    }
    if let Some(index) = skill_md_index {
        let content = String::from_utf8_lossy(&sources[index].1).into_owned();
        diagnostics.extend(validate_frontmatter(&content, "SKILL.md"));
    }
    diagnostics
}

/// 从磁盘读一个技能目录的文件集（受 entry cap 约束；跳过符号链接——
/// 越界的链接不进入内容）。
#[allow(clippy::type_complexity)]
fn read_skill_dir(dir: &Path) -> Result<(Vec<(String, Vec<u8>)>, Vec<Value>), ContractError> {
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();
    let mut stack = vec![(dir.to_owned(), String::new())];
    let mut visited = 0usize;
    while let Some((current, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|e| io_error(format!("read dir {}: {e}", current.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_error(format!("read entry: {e}")))?;
            visited += 1;
            if visited > WALK_ENTRY_CAP {
                return Err(io_error(format!(
                    "skill directory tree exceeded the limit of {WALK_ENTRY_CAP} entries"
                )));
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let path = current.join(&name);
            let kind = entry
                .file_type()
                .map_err(|e| io_error(format!("stat: {e}")))?;
            if kind.is_symlink() {
                diagnostics.push(diagnostic(
                    "CONTAINS_SYMLINK",
                    Some(&relative),
                    format!("Skill contains a symlink: {relative}"),
                    "warning",
                ));
                continue;
            }
            if kind.is_dir() {
                stack.push((path, relative));
                continue;
            }
            if kind.is_file() {
                let bytes = std::fs::read(&path)
                    .map_err(|e| io_error(format!("read {}: {e}", path.display())))?;
                files.push((relative, bytes));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((files, diagnostics))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// 存储路径与行
// ---------------------------------------------------------------------------

fn scope_dir(store: &V1Store, scope: &SkillScope) -> PathBuf {
    let root = store.home().join("v1").join("skills");
    match scope.kind {
        "project" => root
            .join("projects")
            .join(scope.project_id.as_deref().unwrap_or("")),
        "session" => root
            .join("sessions")
            .join(scope.session_id.as_deref().unwrap_or("")),
        _ => root.join("global"),
    }
}

fn skill_dir(store: &V1Store, scope: &SkillScope, skill_id: &str) -> PathBuf {
    scope_dir(store, scope).join(skill_id)
}

#[derive(Clone)]
struct StoredSkill {
    skill_id: String,
    scope: SkillScope,
    name: String,
    revision: String,
}

fn row_to_skill(row: &rusqlite::Row<'_>, scope: &SkillScope) -> rusqlite::Result<StoredSkill> {
    Ok(StoredSkill {
        skill_id: row.get(0)?,
        scope: scope.clone(),
        name: row.get(2)?,
        revision: row.get(3)?,
    })
}

fn skills_in_scope(
    conn: &Connection,
    scope: &SkillScope,
) -> Result<Vec<StoredSkill>, ContractError> {
    let mut stmt = conn
        .prepare(
            "SELECT skill_id, scope_key, name, revision FROM v1_skills
             WHERE scope_key = ?1 ORDER BY skill_id",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![scope.key()], |row| row_to_skill(row, scope))
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    Ok(rows)
}

/// 有效视图：内层覆盖同名（后写胜）。
fn effective_skills(
    conn: &Connection,
    scope: &SkillScope,
) -> Result<Vec<StoredSkill>, ContractError> {
    let mut merged: HashMap<String, StoredSkill> = HashMap::new();
    for layer in scope.chain() {
        for skill in skills_in_scope(conn, &layer)? {
            merged.insert(skill.skill_id.clone(), skill);
        }
    }
    let mut list: Vec<StoredSkill> = merged.into_values().collect();
    list.sort_by(|a, b| a.skill_id.cmp(&b.skill_id));
    Ok(list)
}

fn scope_revision(conn: &Connection, scope_key: &str) -> Result<String, ContractError> {
    let revision: Option<String> = conn
        .query_row(
            "SELECT revision FROM v1_skill_scopes WHERE scope_key = ?1",
            params![scope_key],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(revision.unwrap_or_else(|| REVISION_ABSENT.to_owned()))
}

fn snapshot_revision(conn: &Connection, scope: &SkillScope) -> Result<String, ContractError> {
    let mut parts = Vec::new();
    for layer in scope.chain() {
        parts.push(format!(
            "{}:{}",
            layer.kind,
            scope_revision(conn, &layer.key())?
        ));
    }
    Ok(parts.join("+"))
}

/// 共享 `meta.rev_counter` 的下一版本（policy/credentials 同款自有副本）。
fn next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let current: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .unwrap_or(1);
    let next = current + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("rev-{next:08}"))
}

fn bump_scope_revision_in_tx(
    conn: &Connection,
    scope: &SkillScope,
) -> Result<String, ContractError> {
    let revision = next_rev_in_tx(conn)?;
    conn.execute(
        "INSERT INTO v1_skill_scopes (scope_key, revision, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(scope_key) DO UPDATE SET revision = excluded.revision, updated_at = excluded.updated_at",
        params![scope.key(), revision, now_rfc3339()],
    )
        .map_err(sql_err)?;
    Ok(revision)
}

fn append_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let placeholder = format!("tmpskills-{nanos}-{seq}");
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', ?3, 'agentcpm-pd.event/1', ?4, ?4, ?5)",
        params![
            placeholder,
            resource_id,
            event_type,
            serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
            now_rfc3339(),
        ],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![format!("cur-{seq:012}"), seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// skills.list / skills.read
// ---------------------------------------------------------------------------

async fn skills_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let source = input
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("installed");
        if source == "registry" {
            // 「网络失败不显示空成功」：本运行时未配置注册表 → 失败。
            return Err(ContractError::new(
                CODE_UPSTREAM_ERROR,
                "no skills registry is configured in this runtime; registry search fails \
                 honestly instead of returning an empty success"
                    .to_owned(),
                RetrySpec::backoff_read(Some(500)),
                EffectStatus::NotApplied,
            ));
        }
        if source != "installed" {
            return Err(invalid(format!("unsupported source `{source}`")));
        }
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::to_lowercase);
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let after = parse_after_cursor(&input)?;

        let conn = store.conn.lock().expect("sqlite mutex");
        let effective = effective_skills(&conn, &scope)?;
        let mut items: Vec<Value> = Vec::new();
        for skill in &effective {
            if let Some(after) = &after
                && &skill.skill_id <= after
            {
                continue;
            }
            if let Some(query) = &query {
                let haystack = format!("{} {}", skill.skill_id, skill.name).to_lowercase();
                if !haystack.contains(query.as_str()) {
                    continue;
                }
            }
            // 真实状态：行存在但目录/SKILL.md 丢失 → 不假报可用。
            let (available, reason_code) = {
                let dir = skill_dir(&store, &skill.scope, &skill.skill_id);
                if dir.join("SKILL.md").is_file() {
                    (true, Value::Null)
                } else {
                    (false, json!("SKILL_FILES_MISSING"))
                }
            };
            items.push(json!({
                "skillId": skill.skill_id,
                "name": skill.name,
                "origin": skill.scope.origin(),
                "revision": skill.revision,
                "installed": true,
                "available": available,
                "reasonCode": reason_code,
            }));
            if items.len() > limit {
                break;
            }
        }
        let next_cursor = if items.len() > limit {
            items.pop();
            items
                .last()
                .and_then(|item| item["skillId"].as_str())
                .map(|last| format!("after:{last}"))
        } else {
            None
        };
        let snapshot = snapshot_revision(&conn, &scope)?;
        Ok(json!({
            "items": items,
            "nextCursor": next_cursor,
            "snapshotRevision": snapshot,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn parse_after_cursor(input: &Value) -> Result<Option<String>, ContractError> {
    match input.get("cursor").and_then(Value::as_str) {
        None => Ok(None),
        Some(cursor) => cursor
            .strip_prefix("after:")
            .filter(|rest| !rest.is_empty())
            .map(ToOwned::to_owned)
            .map(Some)
            .ok_or_else(|| {
                invalid(format!(
                    "malformed cursor `{cursor}` (expected after:<skillId>)"
                ))
            }),
    }
}

async fn skills_read(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let skill_id = input
            .get("skillId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`skillId` is required"))?
            .to_owned();
        let relative_path = input
            .get("relativePath")
            .and_then(Value::as_str)
            .unwrap_or("SKILL.md")
            .to_owned();

        let conn = store.conn.lock().expect("sqlite mutex");
        let skill = effective_skills(&conn, &scope)?
            .into_iter()
            .find(|skill| skill.skill_id == skill_id)
            .ok_or_else(|| not_found("skill", &skill_id))?;
        drop(conn);
        // 相对文件解析（绝对路径 / `..` / 符号链接越界 → PATH_OUTSIDE_SCOPE）。
        if !safe_relative_path(&relative_path) {
            return Err(path_outside_scope(format!(
                "relativePath `{relative_path}` escapes the skill root"
            )));
        }
        let dir = skill_dir(&store, &skill.scope, &skill_id);
        let target = dir.join(&relative_path);
        let canonical_dir = dir
            .canonicalize()
            .map_err(|_| not_found("skill", &skill_id))?;
        let canonical_target = target
            .canonicalize()
            .map_err(|_| not_found("skill file", &relative_path))?;
        if !canonical_target.starts_with(&canonical_dir) {
            return Err(path_outside_scope(format!(
                "relativePath `{relative_path}` resolves outside the skill root (symlink escape)"
            )));
        }
        let bytes = std::fs::read(&target).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                not_found("skill file", &relative_path)
            } else {
                io_error(format!("read {}: {e}", target.display()))
            }
        })?;
        let content = String::from_utf8(bytes.clone())
            .map_err(|_| invalid(format!("skill file `{relative_path}` is not valid UTF-8")))?;
        let content_hash = sha256_hex(&bytes);
        Ok(json!({
            "skill": {
                "skillId": skill.skill_id,
                "name": skill.name,
                "origin": skill.scope.origin(),
                "revision": skill.revision,
                "installed": true,
                "available": true,
                "reasonCode": Value::Null,
            },
            "relativePath": relative_path,
            "content": content,
            "contentHash": content_hash,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

// ---------------------------------------------------------------------------
// skills.put / skills.remove
// ---------------------------------------------------------------------------

/// 原子发布技能目录：暂存 → 备份旧目录 → 换入；返回 `(target, backup)` 以便
/// 调用方在事务提交后清理备份，或在批量事务失败时回滚目录。
fn publish_skill_dir_with_backup(
    store: &V1Store,
    scope: &SkillScope,
    skill_id: &str,
    sources: &[(String, Vec<u8>)],
) -> Result<(PathBuf, Option<PathBuf>), ContractError> {
    let scope_root = scope_dir(store, scope);
    std::fs::create_dir_all(&scope_root).map_err(|e| io_error(format!("create scope dir: {e}")))?;
    let target = scope_root.join(skill_id);
    let staging = scope_root.join(format!(".staging-{skill_id}-{}", store.next_id()));
    std::fs::create_dir_all(&staging).map_err(|e| io_error(format!("create staging dir: {e}")))?;
    let write_result = (|| -> Result<(), ContractError> {
        for (relative, bytes) in sources {
            let path = staging.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io_error(format!("create {}: {e}", parent.display())))?;
            }
            std::fs::write(&path, bytes)
                .map_err(|e| io_error(format!("write {}: {e}", path.display())))?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    // 换入（目标存在则先备份；失败回滚）。
    let backup = scope_root.join(format!(".old-{skill_id}-{}", store.next_id()));
    let had_target = target.exists();
    if had_target {
        std::fs::rename(&target, &backup)
            .map_err(|e| io_error(format!("back up skill dir: {e}")))?;
    }
    if let Err(error) = std::fs::rename(&staging, &target) {
        if had_target {
            let _ = std::fs::rename(&backup, &target);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(io_error(format!(
            "publish skill dir {}: {error}",
            target.display()
        )));
    }
    Ok((target, if had_target { Some(backup) } else { None }))
}

/// 原子发布技能目录：暂存 → 备份旧目录 → 换入 → 删备份；失败时回滚。
fn publish_skill_dir(
    store: &V1Store,
    scope: &SkillScope,
    skill_id: &str,
    sources: &[(String, Vec<u8>)],
) -> Result<(), ContractError> {
    let (_target, backup) = publish_skill_dir_with_backup(store, scope, skill_id, sources)?;
    if let Some(backup) = backup {
        let _ = std::fs::remove_dir_all(&backup);
    }
    Ok(())
}

fn rollback_published_dirs(rollbacks: Vec<(PathBuf, Option<PathBuf>)>) {
    for (target, backup) in rollbacks.into_iter().rev() {
        let _ = std::fs::remove_dir_all(&target);
        if let Some(backup) = backup {
            let _ = std::fs::rename(&backup, &target);
        }
    }
}

async fn skills_put(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let skill_id = input
            .get("skillId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`skillId` is required"))?
            .to_owned();
        if !valid_slug(&skill_id) {
            return Err(invalid(format!(
                "skillId `{skill_id}` is not a valid skill id \
                 (expected ^[A-Za-z0-9][A-Za-z0-9._-]{{0,99}}$ without `..`)"
            )));
        }
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let files = input
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| invalid("`files` is required"))?;
        let mut sources: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len());
        for file in &files {
            let relative = file
                .get("relativePath")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let content = file
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            sources.push((relative, content.as_bytes().to_vec()));
        }

        // 校验（多文件校验失败不留下半技能：先校验后写）。
        let skill_md_index = sources
            .iter()
            .position(|(relative, _)| relative == "SKILL.md");
        let diagnostics = validate_file_set(&sources, skill_md_index);
        if has_error(&diagnostics) {
            let mut error = invalid("skill files failed validation");
            let mut details = serde_json::Map::new();
            details.insert("diagnostics".to_owned(), json!(diagnostics));
            error.details = Some(details);
            return Err(error);
        }
        // 重复 relativePath 拒绝（半技能防线）。
        let mut seen: HashMap<&str, ()> = HashMap::new();
        for (relative, _) in &sources {
            if seen.insert(relative.as_str(), ()).is_some() {
                let mut error = invalid("skill files contain a duplicate relativePath");
                let mut details = serde_json::Map::new();
                details.insert(
                    "diagnostics".to_owned(),
                    json!([diagnostic(
                        "DUPLICATE_PATH",
                        Some(relative),
                        format!("duplicate relativePath `{relative}`"),
                        "error",
                    )]),
                );
                error.details = Some(details);
                return Err(error);
            }
        }

        // 单写者纪律：CAS 校验、目录发布与行登记在同一把 store 连接锁
        // 下完成（并发 put 串行化，不存在「目录已换而行未写」的窗口）。
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let current_revision = skills_in_scope(&conn, &scope)?
            .into_iter()
            .find(|skill| skill.skill_id == skill_id)
            .map(|skill| skill.revision)
            .unwrap_or_else(|| REVISION_ABSENT.to_owned());
        if expected != current_revision {
            return Err(revision_conflict(&expected, &current_revision));
        }

        // 发布（目录原子换入），再在同一事务里登记行 + 事件。
        let name = parse_frontmatter(&String::from_utf8_lossy(
            &sources[skill_md_index.expect("validated above")].1,
        ))
        .0
        .get("name")
        .cloned()
        .unwrap_or_else(|| skill_id.clone());
        publish_skill_dir(&store, &scope, &skill_id, &sources)?;
        let tx = conn.transaction().map_err(sql_err)?;
        let new_revision = next_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO v1_skills
                (skill_id, scope_key, scope_kind, project_id, session_id, name, origin,
                 revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT(scope_key, skill_id) DO UPDATE SET
                name = excluded.name,
                revision = excluded.revision,
                updated_at = excluded.updated_at",
            params![
                skill_id,
                scope.key(),
                scope.kind,
                scope.project_id,
                scope.session_id,
                name,
                scope.origin(),
                new_revision,
                now,
            ],
        )
        .map_err(sql_err)?;
        bump_scope_revision_in_tx(&tx, &scope)?;
        append_event_in_tx(
            &tx,
            &skill_id,
            "skills.changed",
            &json!({
                "scope": {"kind": scope.kind},
                "skillId": skill_id,
                "change": "put",
                "revision": new_revision,
            }),
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(json!({
            "revision": new_revision,
            // 保存不宣称已装载：v1 无装载代际。
            "applied": false,
            "generation": Value::Null,
            "skillId": skill_id,
            "validation": diagnostics,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

async fn skills_remove(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let skill_id = input
            .get("skillId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`skillId` is required"))?
            .to_owned();
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let mut conn = store.conn.lock().expect("sqlite mutex");
        let current_revision = skills_in_scope(&conn, &scope)?
            .into_iter()
            .find(|skill| skill.skill_id == skill_id)
            .map(|skill| skill.revision)
            .ok_or_else(|| not_found("skill", &skill_id))?;
        if expected != current_revision {
            return Err(revision_conflict(&expected, &current_revision));
        }

        let tx = conn.transaction().map_err(sql_err)?;
        let current_revision = skills_in_scope(&tx, &scope)?
            .into_iter()
            .find(|skill| skill.skill_id == skill_id)
            .map(|skill| skill.revision)
            .ok_or_else(|| not_found("skill", &skill_id))?;
        if expected != current_revision {
            return Err(revision_conflict(&expected, &current_revision));
        }
        tx.execute(
            "DELETE FROM v1_skills WHERE scope_key = ?1 AND skill_id = ?2",
            params![scope.key(), skill_id],
        )
        .map_err(sql_err)?;
        let scope_revision = bump_scope_revision_in_tx(&tx, &scope)?;
        append_event_in_tx(
            &tx,
            &skill_id,
            "skills.changed",
            &json!({
                "scope": {"kind": scope.kind},
                "skillId": skill_id,
                "change": "remove",
                "revision": scope_revision,
            }),
        )?;
        tx.commit().map_err(sql_err)?;
        // 目录删除在行删除之后：行已终态，残留目录在 list 里表现为
        // SKILL_FILES_MISSING（不假报可用），下一次 put 原子换入会清掉。
        let _ = std::fs::remove_dir_all(skill_dir(&store, &scope, &skill_id));
        // v1 无活动运行-技能绑定面：绑定集合恒空（报告登记）。
        Ok(json!({
            "revision": scope_revision,
            "applied": false,
            "generation": Value::Null,
            "retainedBindings": [],
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

// ---------------------------------------------------------------------------
// skills.validate / skills.scan
// ---------------------------------------------------------------------------

async fn skills_validate(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let skill_id = input.get("skillId").and_then(Value::as_str);
        let files = input.get("files").and_then(Value::as_array);
        // 「skillId与files恰选其一」。
        match (skill_id, files) {
            (Some(_), Some(_)) => {
                return Err(invalid(
                    "skillId and files are mutually exclusive; pass exactly one",
                ));
            }
            (None, None) => return Err(invalid("one of skillId or files is required")),
            _ => {}
        }
        if let Some(files) = files {
            let mut sources: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len());
            for file in files {
                let relative = file
                    .get("relativePath")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let content = file
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                sources.push((relative, content.as_bytes().to_vec()));
            }
            let skill_md_index = sources
                .iter()
                .position(|(relative, _)| relative == "SKILL.md");
            let diagnostics = validate_file_set(&sources, skill_md_index);
            return Ok(json!({
                "valid": !has_error(&diagnostics),
                "diagnostics": diagnostics,
            }));
        }
        let skill_id = skill_id.unwrap_or_default();
        let conn = store.conn.lock().expect("sqlite mutex");
        let skill = effective_skills(&conn, &scope)?
            .into_iter()
            .find(|skill| skill.skill_id == skill_id)
            .ok_or_else(|| not_found("skill", skill_id))?;
        drop(conn);
        let (files, mut diagnostics) = read_skill_dir(&skill_dir(&store, &skill.scope, skill_id))?;
        let skill_md_index = files
            .iter()
            .position(|(relative, _)| relative == "SKILL.md");
        diagnostics.extend(validate_file_set(&files, skill_md_index));
        Ok(json!({
            "valid": !has_error(&diagnostics),
            "diagnostics": diagnostics,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// sourceRoot 的 scope 锚定：project/session 必须落在项目工作区内；
/// global 由调用方显式授权（绝对路径）。
fn anchor_source_root(
    rt: &PublicV1Runtime,
    scope: &SkillScope,
    source_root: &str,
) -> Result<PathBuf, ContractError> {
    let path = Path::new(source_root);
    if !path.is_absolute() {
        return Err(invalid(format!(
            "sourceRoot `{source_root}` must be an absolute path"
        )));
    }
    let canonical = path.canonicalize().map_err(|e| {
        ContractError::new(
            CODE_NOT_FOUND,
            format!("sourceRoot `{source_root}` does not exist: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    match scope.kind {
        "global" => Ok(canonical),
        _ => {
            let project_id = scope.project_id.as_deref().unwrap_or_default();
            match resolve_project(rt.store(), project_id) {
                Ok(workspace) => {
                    let root = workspace
                        .workspace
                        .canonicalize()
                        .map_err(|e| io_error(format!("canonicalize workspace root: {e}")))?;
                    if canonical.starts_with(&root) {
                        Ok(canonical)
                    } else {
                        Err(path_outside_scope(format!(
                            "sourceRoot `{source_root}` is outside the authorized project workspace"
                        )))
                    }
                }
                Err(ProjectResolveError::NotFound(message)) => Err(invalid(message)),
                Err(ProjectResolveError::Contract(error)) => Err(error),
                Err(ProjectResolveError::Virtual(_)) => Err(invalid(format!(
                    "project `{project_id}` has no host workspace to anchor sourceRoot"
                ))),
            }
        }
    }
}

async fn skills_scan(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let source_root = input
            .get("sourceRoot")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`sourceRoot` is required"))?
            .to_owned();
        let recursive = input
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let root = anchor_source_root(&rt, &scope, &source_root)?;
        if !root.is_dir() {
            return Err(invalid(format!(
                "sourceRoot `{source_root}` is not a directory"
            )));
        }

        // 候选：源根本身（relativePath "."）与各子目录；recursive 时全
        // 子树（条目上限内）。不自动安装或覆盖（只读）。没有 SKILL.md
        // 的目录仍列出，valid=false + NO_SKILL_MD 诊断。
        let mut candidates: Vec<Value> = Vec::new();
        let mut visited = 0usize;
        let mut queue: Vec<(PathBuf, String, bool)> = vec![(root.clone(), String::new(), true)];
        while let Some((dir, prefix, is_root)) = queue.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) => return Err(io_error(format!("read dir {}: {e}", dir.display()))),
            };
            let mut names: Vec<(String, bool, bool)> = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| io_error(format!("read entry: {e}")))?;
                visited += 1;
                if visited > WALK_ENTRY_CAP {
                    return Err(io_error(format!(
                        "scan exceeded the limit of {WALK_ENTRY_CAP} entries"
                    )));
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let kind = entry
                    .file_type()
                    .map_err(|e| io_error(format!("stat: {e}")))?;
                names.push((name, kind.is_dir(), kind.is_symlink()));
            }
            names.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, is_dir, is_symlink) in names {
                if name.starts_with('.') {
                    continue;
                }
                let path = dir.join(&name);
                let relative = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                // 安全边界：禁止跟随目录 symlink，防止从工作区内链接越界读取外部目录。
                if is_dir && !is_symlink && (recursive || prefix.is_empty()) {
                    queue.push((path, relative, false));
                }
            }
            let (files, walk_diagnostics) = read_skill_dir(&dir)?;
            let skill_md_index = files
                .iter()
                .position(|(relative, _)| relative == "SKILL.md");
            let mut diagnostics = walk_diagnostics;
            diagnostics.extend(validate_file_set(&files, skill_md_index));
            let display_name = if is_root {
                ".".to_owned()
            } else {
                prefix.clone()
            };
            let name = skill_md_index
                .and_then(|index| {
                    parse_frontmatter(&String::from_utf8_lossy(&files[index].1))
                        .0
                        .get("name")
                        .cloned()
                })
                .unwrap_or_else(|| {
                    dir.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| ".".to_owned())
                });
            candidates.push(json!({
                "relativePath": display_name,
                "name": name,
                "valid": !has_error(&diagnostics),
                "diagnostics": diagnostics,
            }));
        }
        candidates.sort_by(|a, b| {
            a["relativePath"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["relativePath"].as_str().unwrap_or_default())
        });
        Ok(json!({"candidates": candidates}))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

// ---------------------------------------------------------------------------
// skills.plan_import / skills.apply_import（两段式）
// ---------------------------------------------------------------------------

/// TS `isValidSlug`。
fn valid_slug(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    if bytes.is_empty() || bytes.len() > 100 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphanumeric()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !slug.contains("..")
}

/// TS `discoverSkillDirs`：源根本身带 SKILL.md → 单技能；否则收集直接
/// 子目录中带 SKILL.md 的（按名排序，不跟随目录 symlink）。不存在 → None。
fn discover_skill_dirs(source_root: &Path) -> Result<Option<(Vec<PathBuf>, bool)>, ContractError> {
    if !source_root.exists() {
        return Ok(None);
    }
    if source_root
        .join("SKILL.md")
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_file())
    {
        return Ok(Some((vec![source_root.to_owned()], true)));
    }
    let entries = std::fs::read_dir(source_root)
        .map_err(|e| io_error(format!("read dir {}: {e}", source_root.display())))?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error(format!("read entry: {e}")))?;
        let path = source_root.join(entry.file_name());
        let is_dir = entry
            .file_type()
            .map(|kind| kind.is_dir() && !kind.is_symlink())
            .unwrap_or(false);
        let has_skill_md = path
            .join("SKILL.md")
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_file());
        if is_dir && has_skill_md {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(Some((dirs, false)))
}

/// 源树内容 hash：发现到的每个技能目录内全部文件（排序路径 + 内容
/// sha256）串接后整体 sha256。目录集合或任一文件变化 → hash 变化。
fn source_hash(source_root: &Path) -> Result<String, ContractError> {
    let (dirs, _) = discover_skill_dirs(source_root)?
        .ok_or_else(|| not_found("source root", &source_root.display().to_string()))?;
    let mut hasher = Sha256::new();
    for dir in &dirs {
        hasher.update(dir.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        let (files, _) = read_skill_dir(dir)?;
        for (relative, bytes) in &files {
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hasher.update(sha256_hex(bytes).into_bytes());
            hasher.update(b"\0");
        }
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

async fn skills_plan_import(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let origin = input
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match origin.as_str() {
            "directory" | "legacy" => {}
            "archive" => {
                return Err(ContractError::new(
                    CODE_UNAVAILABLE,
                    "archive import is not assembled in this runtime".to_owned(),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ))
            }
            "registry" => {
                return Err(ContractError::new(
                    CODE_UPSTREAM_ERROR,
                    "no skills registry is configured in this runtime".to_owned(),
                    RetrySpec::backoff_read(Some(500)),
                    EffectStatus::NotApplied,
                ))
            }
            other => return Err(invalid(format!("unsupported origin `{other}`"))),
        }
        let legacy_format = input.get("legacyFormat").and_then(Value::as_str);
        if origin == "legacy" && legacy_format.is_none() {
            return Err(invalid("legacyFormat is required for origin=legacy"));
        }
        let source_ref = input
            .get("sourceRef")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`sourceRef` is required"))?
            .to_owned();
        let conflict_policy = input
            .get("conflictPolicy")
            .and_then(Value::as_str)
            .unwrap_or("skip")
            .to_owned();
        if !["skip", "rename", "overwrite"].contains(&conflict_policy.as_str()) {
            return Err(invalid(format!(
                "unsupported conflictPolicy `{conflict_policy}`"
            )));
        }
        let rename_map: Vec<(String, String)> = input
            .get("renameMap")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|m| {
                        (
                            m.get("from").and_then(Value::as_str).unwrap_or_default().to_owned(),
                            m.get("to").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (from, to) in &rename_map {
            if from.is_empty() {
                return Err(invalid("renameMap entries require a non-empty `from`"));
            }
            if !valid_slug(to) {
                return Err(invalid(format!(
                    "renameMap target `{to}` is not a valid skill id"
                )));
            }
        }

        let source_root = anchor_source_root(&rt, &scope, &source_ref)?;
        if !source_root.exists() {
            return Err(not_found("source", &source_ref));
        }
        let (dirs, single_skill_root) = discover_skill_dirs(&source_root)?
            .ok_or_else(|| not_found("source", &source_ref))?;
        let source_hash = source_hash(&source_root)?;
        let expected_revision = {
            let conn = store.conn.lock().expect("sqlite mutex");
            scope_revision(&conn, &scope.key())?
        };

        // 逐技能计算变更。
        let conn = store.conn.lock().expect("sqlite mutex");
        let existing: Vec<String> = skills_in_scope(&conn, &scope)?
            .into_iter()
            .map(|skill| skill.skill_id)
            .collect();
        drop(conn);
        let mut changes: Vec<Value> = Vec::new();
        let mut diagnostics: Vec<Value> = Vec::new();
        let mut used_targets: Vec<String> = existing.clone();
        for dir in &dirs {
            let source_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !valid_slug(&source_name) {
                changes.push(json!({
                    "source": source_name,
                    "target": source_name,
                    "action": "reject",
                    "reason": format!("source folder name `{source_name}` is not a valid skill id"),
                }));
                continue;
            }
            // 源内容校验（不兼容内容拒绝）。
            let (files, walk_diagnostics) = read_skill_dir(dir)?;
            let skill_md_index = files
                .iter()
                .position(|(relative, _)| relative == "SKILL.md");
            let mut file_diagnostics = walk_diagnostics;
            file_diagnostics.extend(validate_file_set(&files, skill_md_index));
            diagnostics.extend(file_diagnostics.iter().filter(|d| {
                d.get("severity").and_then(Value::as_str) == Some("error")
            }).cloned());
            if has_error(&file_diagnostics) {
                changes.push(json!({
                    "source": source_name,
                    "target": source_name,
                    "action": "reject",
                    "reason": "source skill failed validation (see diagnostics)",
                }));
                continue;
            }
            // 目标解析：renameMap > conflictPolicy。
            let target = if let Some((_, to)) =
                rename_map.iter().find(|(from, _)| from == &source_name)
            {
                if used_targets.contains(to) {
                    changes.push(json!({
                        "source": source_name,
                        "target": to,
                        "action": "skip",
                        "reason": format!("renameMap target `{to}` already exists"),
                    }));
                    continue;
                }
                to.clone()
            } else if existing.contains(&source_name) {
                match conflict_policy.as_str() {
                    "skip" => {
                        changes.push(json!({
                            "source": source_name,
                            "target": source_name,
                            "action": "skip",
                            "reason": "destination skill already exists",
                        }));
                        continue;
                    }
                    "rename" => {
                        let mut counter = 2;
                        let mut candidate = format!("{source_name}-imported");
                        while used_targets.contains(&candidate) {
                            candidate = format!("{source_name}-imported-{counter}");
                            counter += 1;
                        }
                        candidate
                    }
                    _ => source_name.clone(),
                }
            } else {
                source_name.clone()
            };
            used_targets.push(target.clone());
            let action = if target != source_name {
                "rename"
            } else if existing.contains(&source_name) {
                "overwrite"
            } else {
                "import"
            };
            let reason = if target != source_name {
                format!("imported as {target} (conflict policy {conflict_policy})")
            } else if action == "overwrite" {
                "overwrites the existing destination skill".to_owned()
            } else {
                "imports a new skill".to_owned()
            };
            changes.push(json!({
                "source": source_name,
                "target": target,
                "action": action,
                "reason": reason,
            }));
        }

        // 计划登记（10 分钟过期）。
        let payload = json!({
            "scope": {"kind": scope.kind, "projectId": scope.project_id, "sessionId": scope.session_id},
            "origin": origin,
            "sourceRef": source_root.display().to_string(),
            "singleSkillRoot": single_skill_root,
            "legacyFormat": legacy_format,
            "conflictPolicy": conflict_policy,
            "changes": changes,
        });
        let plan_hash = sha256_hex(serde_json::to_string(&payload).unwrap_or_default().as_bytes());
        let plan_id = store.next_id();
        let expires_at = (chrono::Utc::now() + chrono::Duration::minutes(PLAN_TTL_MINUTES))
            .to_rfc3339();
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        tx.execute(
            "INSERT INTO v1_skill_plans
                (plan_id, plan_hash, source_hash, kind, state, expires_at, scope_json, payload_json, created_at)
             VALUES (?1, ?2, ?3, 'import', 'open', ?4, ?5, ?6, ?7)",
            params![
                plan_id,
                plan_hash,
                source_hash,
                expires_at,
                serde_json::to_string(&scope_json_of(&scope)).unwrap_or_default(),
                serde_json::to_string(&payload).unwrap_or_default(),
                now_rfc3339(),
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(json!({
            "planId": plan_id,
            "planHash": plan_hash,
            "sourceHash": source_hash,
            "expectedRevision": expected_revision,
            "changes": changes,
            "diagnostics": diagnostics,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn scope_json_of(scope: &SkillScope) -> Value {
    json!({
        "kind": scope.kind,
        "projectId": scope.project_id,
        "sessionId": scope.session_id,
    })
}

fn skills_domain_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
    let home = rt.store().home().to_path_buf();
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let db = Arc::new(DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?);
    registry.insert(home, Arc::downgrade(&db));
    Ok(db)
}

async fn skills_apply_import(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = skills_domain_db(&rt)?;
    let store = rt.store().clone();
    let operation = accept_domain_operation(
        &db,
        "skills.apply_import",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", store.next_id()),
        || {
            let conn = store.conn.lock().expect("sqlite mutex");
            let plan_id = input["planId"].as_str().unwrap_or_default();
            let plan_hash = input["planHash"].as_str().unwrap_or_default();
            let (stored_hash, _, state, expires_at, _) = read_plan_row(&conn, plan_id)?
                .ok_or_else(|| {
                    plan_stale(format!(
                        "plan `{plan_id}` does not exist; re-plan and retry"
                    ))
                })?;
            verify_plan(plan_id, plan_hash, &stored_hash, &state, &expires_at)
        },
    )?;
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    let operation_id = operation.operation_id.clone();

    let db_for_task = db.clone();
    let rt_for_task = rt.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&db_for_task, &operation_id, "");
        let completion = match tokio::task::spawn_blocking(move || {
            skills_apply_import_execute(&rt_for_task, &input)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err((
                "UNAVAILABLE".to_owned(),
                format!("skills apply task panicked: {error}"),
            )),
        };
        match completion {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &db_for_task,
                    &operation_id,
                    "",
                    "succeeded",
                    &completion,
                );
            }
            Err((code, message)) => {
                let _ = finish_domain_operation(
                    &db_for_task,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"code": code, "message": message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

fn plan_stale(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_PLAN_STALE,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn expires_before_now(expires_at: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(expires_at),
        chrono::DateTime::parse_from_rfc3339(&now_rfc3339()),
    ) {
        (Ok(expires), Ok(now)) => expires < now,
        // 解析失败（含缺失）按已过期处理：宁可拒绝旧计划也不放行。
        _ => true,
    }
}

/// apply 执行体：sourceHash 复算 → 单写者事务内一次性消费计划（CAS）→
/// revision CAS → 逐技能原子发布 + 行登记（失败回滚 DB 与目录）→ scope 版本 + `skills.changed`。
fn skills_apply_import_execute(
    rt: &Arc<PublicV1Runtime>,
    input: &Value,
) -> Result<Value, (String, String)> {
    let store = rt.store();
    let run = (|| -> Result<Value, ContractError> {
        let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
        let plan_hash = input["planHash"].as_str().unwrap_or_default().to_owned();
        let expected_revision = input["expectedRevision"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let (_, stored_source_hash, payload_json) = {
            let conn = store.conn.lock().expect("sqlite mutex");
            let (stored_hash, source_hash, state, expires_at, payload) =
                read_plan_row(&conn, &plan_id)?.ok_or_else(|| {
                    plan_stale(format!(
                        "plan `{plan_id}` does not exist; re-plan and retry"
                    ))
                })?;
            verify_plan(&plan_id, &plan_hash, &stored_hash, &state, &expires_at)?;
            (stored_hash, source_hash, payload)
        };
        let payload: Value = serde_json::from_str(&payload_json)
            .map_err(|e| invalid(format!("stored plan payload is corrupt: {e}")))?;
        let scope = scope_from_payload(&payload);
        let source_ref = payload
            .get("sourceRef")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let source_root = anchor_source_root(rt, &scope, source_ref).map_err(|error| {
            if error.code == CODE_NOT_FOUND {
                ContractError::new(
                    CODE_INTEGRITY_ERROR,
                    format!(
                        "source root vanished or became unreadable: {}",
                        error.message
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            } else {
                error
            }
        })?;

        // sourceHash 复算：源在计划后变化 → INTEGRITY_ERROR（不标
        // installed）。读文件不持锁。
        let recomputed = source_hash(&source_root).map_err(|error| {
            ContractError::new(
                CODE_INTEGRITY_ERROR,
                format!(
                    "source root vanished or became unreadable: {}",
                    error.message
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        if recomputed != stored_source_hash {
            return Err(ContractError::new(
                CODE_INTEGRITY_ERROR,
                format!(
                    "source content changed since the plan was created (planned \
                     `{stored_source_hash}`, now `{recomputed}`); re-plan and retry"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        // 先行预读待导入技能内容（避免持 SQLite 锁遍历磁盘）。
        struct PreparedImport<'a> {
            source: &'a str,
            target: &'a str,
            files: Vec<(String, Vec<u8>)>,
        }
        let changes = payload
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let single_skill_root = payload
            .get("singleSkillRoot")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut skipped: Vec<String> = Vec::new();
        let mut prepared: Vec<PreparedImport<'_>> = Vec::new();
        for change in &changes {
            let action = change
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let source = change
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let target = change
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if action == "skip" || action == "reject" {
                skipped.push(source.to_owned());
                continue;
            }
            if !valid_slug(target) {
                return Err(invalid(format!(
                    "planned target `{target}` is not a valid skill id"
                )));
            }
            let source_dir = if single_skill_root || source == "." {
                source_root.clone()
            } else {
                source_root.join(source)
            };
            let (files, _) = read_skill_dir(&source_dir)?;
            prepared.push(PreparedImport {
                source,
                target,
                files,
            });
        }

        // 单写者事务：消费计划、CAS、目录发布与行登记在同一事务内，任一步失败
        // 回滚已发布目录与 DB 写入（plan 保持 open）。
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let current = scope_revision(&tx, &scope.key())?;
        if expected_revision != current {
            return Err(revision_conflict(&expected_revision, &current));
        }
        let consumed = tx
            .execute(
                "UPDATE v1_skill_plans SET state = 'consumed' WHERE plan_id = ?1 AND state = 'open'",
                params![plan_id],
            )
            .map_err(sql_err)?;
        if consumed == 0 {
            return Err(plan_stale(format!(
                "plan `{plan_id}` was already consumed by a concurrent apply; re-plan and retry"
            )));
        }

        let mut imported: Vec<String> = Vec::new();
        let mut renamed: Vec<Value> = Vec::new();
        let mut rollbacks: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        let apply_result = (|| -> Result<String, ContractError> {
            for item in &prepared {
                let published =
                    publish_skill_dir_with_backup(store, &scope, item.target, &item.files)?;
                rollbacks.push(published);
                let name = item
                    .files
                    .iter()
                    .find(|(relative, _)| relative == "SKILL.md")
                    .and_then(|(_, bytes)| {
                        parse_frontmatter(&String::from_utf8_lossy(bytes))
                            .0
                            .get("name")
                            .cloned()
                    })
                    .unwrap_or_else(|| item.target.to_owned());
                let new_revision = next_rev_in_tx(&tx)?;
                tx.execute(
                    "INSERT INTO v1_skills
                        (skill_id, scope_key, scope_kind, project_id, session_id, name, origin,
                         revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
                     ON CONFLICT(scope_key, skill_id) DO UPDATE SET
                        name = excluded.name,
                        revision = excluded.revision,
                        updated_at = excluded.updated_at",
                    params![
                        item.target,
                        scope.key(),
                        scope.kind,
                        scope.project_id,
                        scope.session_id,
                        name,
                        scope.origin(),
                        new_revision,
                        now_rfc3339(),
                    ],
                )
                .map_err(sql_err)?;
                append_event_in_tx(
                    &tx,
                    item.target,
                    "skills.changed",
                    &json!({
                        "scope": {"kind": scope.kind},
                        "skillId": item.target,
                        "change": "import",
                        "planId": plan_id,
                        "revision": new_revision,
                    }),
                )?;
                if item.target != item.source {
                    renamed.push(json!({"from": item.source, "to": item.target}));
                }
                imported.push(item.target.to_owned());
            }
            bump_scope_revision_in_tx(&tx, &scope)
        })();
        let scope_revision = match apply_result {
            Ok(rev) => {
                if let Err(err) = tx.commit().map_err(sql_err) {
                    rollback_published_dirs(rollbacks);
                    return Err(err);
                }
                for (_, backup) in rollbacks {
                    if let Some(backup) = backup {
                        let _ = std::fs::remove_dir_all(&backup);
                    }
                }
                rev
            }
            Err(err) => {
                drop(tx);
                rollback_published_dirs(rollbacks);
                return Err(err);
            }
        };
        Ok(json!({
            "planId": plan_id,
            "importedSkillIds": imported,
            "skipped": skipped,
            "renamed": renamed,
            "revision": scope_revision,
            // v1 无装载代际。
            "generation": Value::Null,
        }))
    })();
    match run {
        Ok(value) => Ok(value),
        Err(error) => Err((error.code, error.message)),
    }
}

/// 计划行（`plan_hash, source_hash, state, expires_at, payload_json`）。
#[allow(clippy::type_complexity)]
fn read_plan_row(
    conn: &Connection,
    plan_id: &str,
) -> Result<Option<(String, String, String, String, String)>, ContractError> {
    conn.query_row(
        "SELECT plan_hash, source_hash, state, expires_at, payload_json
         FROM v1_skill_plans WHERE plan_id = ?1",
        params![plan_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional()
    .map_err(sql_err)
}

/// 计划存在性/状态/过期/hash 的公共复验。
fn verify_plan(
    plan_id: &str,
    submitted_hash: &str,
    stored_hash: &str,
    state: &str,
    expires_at: &str,
) -> Result<(), ContractError> {
    if state != "open" {
        return Err(plan_stale(format!(
            "plan `{plan_id}` is `{state}`; re-plan and retry"
        )));
    }
    if expires_before_now(expires_at) {
        return Err(plan_stale(format!(
            "plan `{plan_id}` expired at `{expires_at}`; re-plan and retry"
        )));
    }
    if stored_hash != submitted_hash {
        return Err(plan_stale(format!(
            "planHash `{submitted_hash}` does not match the stored plan `{stored_hash}`"
        )));
    }
    Ok(())
}

fn scope_from_payload(payload: &Value) -> SkillScope {
    let scope_json = payload.get("scope").cloned().unwrap_or(Value::Null);
    let kind = match scope_json.get("kind").and_then(Value::as_str) {
        Some("project") => "project",
        Some("session") => "session",
        _ => "global",
    };
    SkillScope {
        kind,
        project_id: scope_json
            .get("projectId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        session_id: scope_json
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

// ---------------------------------------------------------------------------
// 测试（合同正反例，走真实 dispatch 表）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use std::time::{Duration, Instant};

    fn fresh_home(tag: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        (tmp, root_dir)
    }

    fn open_runtime(root_dir: &Path) -> (Arc<PublicV1Runtime>, String) {
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

    async fn dispatch_ok(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> Value {
        let (_, result) = rt.dispatch(method, input, None, None).await.unwrap();
        OperationRegistry::global()
            .validate_result(method, &result)
            .unwrap_or_else(|e| panic!("`{method}` result failed schema validation: {e:?}"));
        result
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> ContractError {
        match rt.dispatch(method, input, Some("key-err"), None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    /// dispatch_err 的带 key 变体：idempotentSubmit 操作的负例需要每次独立的
    /// idempotencyKey——被拒请求也会在幂等账本登记 key，同键不同输入会先撞
    /// IDEMPOTENCY_CONFLICT，到不了用例本身要验的错误。
    async fn dispatch_err_keyed(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: &str,
    ) -> ContractError {
        match rt.dispatch(method, input, Some(key), None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    fn global_scope() -> Value {
        json!({"kind": "global"})
    }

    fn skill_md(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
    }

    fn put_input(skill_id: &str, files: Value) -> Value {
        json!({
            "scope": global_scope(),
            "skillId": skill_id,
            "files": files,
            "expectedRevision": REVISION_ABSENT,
        })
    }

    async fn put_skill(rt: &Arc<PublicV1Runtime>, skill_id: &str) -> Value {
        dispatch_ok(
            rt,
            "skills.put",
            put_input(
                skill_id,
                json!([
                    {"relativePath": "SKILL.md", "content": skill_md(skill_id, "A skill for the extensions test suite.")},
                    {"relativePath": "notes/usage.md", "content": "usage notes"},
                ]),
            ),
        )
        .await
    }

    async fn wait_operation(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, page) = rt
                .dispatch(
                    "operations.get",
                    json!({"operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            let operation = page["operation"].clone();
            let state = operation["state"].as_str().unwrap_or_default().to_owned();
            if matches!(
                state.as_str(),
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return operation;
            }
            assert!(
                Instant::now() < deadline,
                "operation `{operation_id}` never became terminal (state `{state}`)"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn validate_requires_exactly_one_of_skill_id_or_files() {
        let (_tmp, root) = fresh_home("skills-validate-shape");
        let (rt, _iid) = open_runtime(&root);

        // 两者都给 → INVALID_ARGUMENT。
        let error = dispatch_err(
            &rt,
            "skills.validate",
            json!({
                "scope": global_scope(),
                "skillId": "x",
                "files": [{"relativePath": "SKILL.md", "content": "c"}],
            }),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 都不给 → INVALID_ARGUMENT。
        let error = dispatch_err(&rt, "skills.validate", json!({"scope": global_scope()})).await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // files：缺 SKILL.md / 路径逃逸按位置诊断。
        let result = dispatch_ok(
            &rt,
            "skills.validate",
            json!({
                "scope": global_scope(),
                "files": [{"relativePath": "README.md", "content": "no skill md"}],
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(false));
        assert!(
            result["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == json!("NO_SKILL_MD"))
        );

        let result = dispatch_ok(
            &rt,
            "skills.validate",
            json!({
                "scope": global_scope(),
                "files": [
                    {"relativePath": "SKILL.md", "content": skill_md("s", "Valid description for the skill.")},
                    {"relativePath": "../escape.md", "content": "x"},
                ],
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(false));
        assert!(
            result["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == json!("UNSAFE_PATH"))
        );

        // 缺 frontmatter name。
        let result = dispatch_ok(
            &rt,
            "skills.validate",
            json!({
                "scope": global_scope(),
                "files": [{"relativePath": "SKILL.md", "content": "---\ndescription: no name here\n---\n"}],
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(false));
        assert!(
            result["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == json!("FRONTMATTER_MISSING_NAME"))
        );

        // 合法（带 description 短的 warning）。
        let result = dispatch_ok(
            &rt,
            "skills.validate",
            json!({
                "scope": global_scope(),
                "files": [{"relativePath": "SKILL.md", "content": skill_md("ok", "short")}],
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(true));
        assert!(
            result["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == json!("DESCRIPTION_SHORT"))
        );
    }

    #[tokio::test]
    async fn put_read_remove_roundtrip_with_cas() {
        let (_tmp, root) = fresh_home("skills-put");
        let (rt, _iid) = open_runtime(&root);

        // 创建：absent。
        let result = put_skill(&rt, "demo").await;
        assert_eq!(result["skillId"], json!("demo"));
        assert_eq!(result["applied"], json!(false));
        assert_eq!(result["generation"], Value::Null);
        assert!(result["validation"].as_array().is_some());
        let rev1 = result["revision"].as_str().unwrap().to_owned();

        // 重复创建 → REVISION_CONFLICT（过期写冲突锚）。
        let error = dispatch_err(&rt, "skills.put", put_input("demo", json!([
            {"relativePath": "SKILL.md", "content": skill_md("demo", "A skill for the extensions test suite.")}
        ])))
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        // 更新：用真实 revision。
        let result = rt
            .dispatch(
                "skills.put",
                json!({
                    "scope": global_scope(),
                    "skillId": "demo",
                    "files": [
                        {"relativePath": "SKILL.md", "content": skill_md("demo", "Updated description for the extensions suite.")},
                        {"relativePath": "notes/usage.md", "content": "usage v2"},
                    ],
                    "expectedRevision": rev1,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_ne!(result["revision"], json!(rev1));

        // read：内容 + hash（导出→导入可规范比较的锚）。
        let read = dispatch_ok(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "demo"}),
        )
        .await;
        assert!(
            read["content"]
                .as_str()
                .unwrap()
                .contains("Updated description")
        );
        assert_eq!(read["skill"]["skillId"], json!("demo"));
        assert_eq!(read["skill"]["origin"], json!("global"));
        assert_eq!(read["skill"]["installed"], json!(true));
        assert_eq!(
            read["contentHash"],
            json!(sha256_hex(
                skill_md("demo", "Updated description for the extensions suite.").as_bytes()
            )),
            "contentHash is the sha256 of the file bytes"
        );
        let read2 = dispatch_ok(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "demo", "relativePath": "notes/usage.md"}),
        )
        .await;
        assert_eq!(read2["content"], json!("usage v2"));
        assert_eq!(
            read2["contentHash"],
            json!(sha256_hex(b"usage v2")),
            "contentHash is the sha256 of the file bytes"
        );

        // 路径逃逸拒绝。
        let error = dispatch_err(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "demo", "relativePath": "../other/secret"}),
        )
        .await;
        assert_eq!(error.code, "PATH_OUTSIDE_SCOPE");
        let error = dispatch_err(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "demo", "relativePath": "/etc/passwd"}),
        )
        .await;
        assert_eq!(error.code, "PATH_OUTSIDE_SCOPE");

        // 符号链接越界拒绝（unix 符号链接面）。
        #[cfg(unix)]
        {
            let target_dir = skill_dir(rt.store(), &SkillScope::global(), "demo");
            std::os::unix::fs::symlink("/etc", target_dir.join("evil-link")).unwrap();
            let error = dispatch_err(
                &rt,
                "skills.read",
                json!({"scope": global_scope(), "skillId": "demo", "relativePath": "evil-link/passwd"}),
            )
            .await;
            assert_eq!(error.code, "PATH_OUTSIDE_SCOPE");
            let _ = std::fs::remove_file(target_dir.join("evil-link"));
        }

        // remove：CAS + 目录清理。
        let error = dispatch_err(
            &rt,
            "skills.remove",
            json!({
                "scope": global_scope(),
                "skillId": "demo",
                "expectedRevision": "bogus",
            }),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");
        let skill_rev = dispatch_ok(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "demo"}),
        )
        .await["skill"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let removed = dispatch_ok(
            &rt,
            "skills.remove",
            json!({
                "scope": global_scope(),
                "skillId": "demo",
                "expectedRevision": skill_rev,
            }),
        )
        .await;
        assert_eq!(removed["retainedBindings"], json!([]));
        let error = dispatch_err(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "demo"}),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
        assert!(!skill_dir(rt.store(), &SkillScope::global(), "demo").exists());
    }

    #[tokio::test]
    async fn put_rejects_half_skills_and_bad_paths() {
        let (_tmp, root) = fresh_home("skills-put-bad");
        let (rt, _iid) = open_runtime(&root);

        // 校验失败（缺 SKILL.md）→ 不落任何文件。
        let error = dispatch_err(
            &rt,
            "skills.put",
            put_input(
                "bad",
                json!([{"relativePath": "README.md", "content": "x"}]),
            ),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        assert!(!skill_dir(rt.store(), &SkillScope::global(), "bad").exists());

        // 非法 skillId。
        let error = dispatch_err(
            &rt,
            "skills.put",
            put_input(
                "../escape",
                json!([{"relativePath": "SKILL.md", "content": "c"}]),
            ),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 非法 relativePath（绝对路径）。
        let error = dispatch_err(
            &rt,
            "skills.put",
            put_input("abs", json!([
                {"relativePath": "SKILL.md", "content": skill_md("abs", "A skill for the extensions test suite.")},
                {"relativePath": "/etc/passwd", "content": "x"},
            ])),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        assert!(!skill_dir(rt.store(), &SkillScope::global(), "abs").exists());
    }

    #[tokio::test]
    async fn list_scope_resolution_and_availability() {
        let (_tmp, root) = fresh_home("skills-list");
        let (rt, _iid) = open_runtime(&root);
        let instance_id = rt.store().meta().instance_id.clone();
        rt.store()
            .register_project(&json!({
                "projectId": "proj-sk",
                "name": "SK",
                "environmentId": "local",
                "root": {"instanceId": instance_id, "rootId": "root-x", "relativePath": "ws"},
            }))
            .unwrap();
        rt.store()
            .create_session(&json!({"sessionId": "sess-sk", "projectId": "proj-sk"}))
            .unwrap();

        put_skill(&rt, "shared").await;
        // 项目层覆盖同名 + 项目专属。
        for skill_id in ["shared", "proj-only"] {
            rt.dispatch(
                "skills.put",
                json!({
                    "scope": {"kind": "project", "id": "proj-sk"},
                    "skillId": skill_id,
                    "files": [
                        {"relativePath": "SKILL.md", "content": skill_md(skill_id, "A skill for the extensions test suite.")}
                    ],
                    "expectedRevision": REVISION_ABSENT,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        }

        // 会话视图：三层合并（session 无自有 → project + global）。
        let listed = dispatch_ok(
            &rt,
            "skills.list",
            json!({"scope": {"kind": "session", "id": "sess-sk"}}),
        )
        .await;
        let items = listed["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 2, "shared + proj-only in the session view");
        let find = |id: &str| {
            items
                .iter()
                .find(|i| i["skillId"] == json!(id))
                .unwrap()
                .clone()
        };
        // 内层覆盖：shared 来自 project。
        assert_eq!(find("shared")["origin"], json!("project"));
        assert_eq!(find("proj-only")["origin"], json!("project"));
        assert_eq!(find("shared")["installed"], json!(true));
        assert_eq!(find("shared")["available"], json!(true));

        // 全局视图只有 global 的 shared。
        let listed = dispatch_ok(&rt, "skills.list", json!({"scope": global_scope()})).await;
        let items = listed["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["origin"], json!("global"));

        // query 过滤。
        let listed = dispatch_ok(
            &rt,
            "skills.list",
            json!({"scope": global_scope(), "query": "SHARED"}),
        )
        .await;
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);

        // 目录被外部删除 → available=false + reasonCode（不假报可用）。
        let shared_dir = skill_dir(rt.store(), &SkillScope::global(), "shared");
        let _ = std::fs::remove_dir_all(&shared_dir);
        let listed = dispatch_ok(&rt, "skills.list", json!({"scope": global_scope()})).await;
        let item = &listed["items"][0];
        assert_eq!(item["available"], json!(false));
        assert_eq!(item["reasonCode"], json!("SKILL_FILES_MISSING"));

        // registry 源：诚实失败（不显示空成功）。
        let error = dispatch_err(
            &rt,
            "skills.list",
            json!({"scope": global_scope(), "source": "registry"}),
        )
        .await;
        assert_eq!(error.code, "UPSTREAM_ERROR");
    }

    #[tokio::test]
    async fn scan_discovers_candidates_within_authorized_root() {
        let (_tmp, root) = fresh_home("skills-scan");
        let (rt, _iid) = open_runtime(&root);
        let source = root.parent().unwrap().join("scan-src");
        std::fs::create_dir_all(source.join("good")).unwrap();
        std::fs::write(
            source.join("good").join("SKILL.md"),
            skill_md("good", "A discovered skill for the scan test."),
        )
        .unwrap();
        std::fs::create_dir_all(source.join("bad")).unwrap();
        std::fs::write(source.join("bad").join("SKILL.md"), "no frontmatter here").unwrap();
        std::fs::create_dir_all(source.join("not-a-skill")).unwrap();

        let result = dispatch_ok(
            &rt,
            "skills.scan",
            json!({"scope": global_scope(), "sourceRoot": source.display().to_string()}),
        )
        .await;
        let candidates = result["candidates"].as_array().unwrap().clone();
        // 根（无 SKILL.md 的父目录本身也作为候选给出诊断）+ good + bad +
        // not-a-skill。
        assert!(candidates.len() >= 3, "scan lists every child folder");
        let find = |path: &str| {
            candidates
                .iter()
                .find(|c| c["relativePath"] == json!(path))
                .unwrap_or_else(|| panic!("candidate `{path}` missing"))
                .clone()
        };
        assert_eq!(find("good")["valid"], json!(true));
        assert_eq!(find("good")["name"], json!("good"));
        assert_eq!(find("bad")["valid"], json!(false));
        assert_eq!(find("not-a-skill")["valid"], json!(false));

        // 相对路径拒绝。
        let error = dispatch_err(
            &rt,
            "skills.scan",
            json!({"scope": global_scope(), "sourceRoot": "relative/path"}),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 不存在 → NOT_FOUND。
        let error = dispatch_err(
            &rt,
            "skills.scan",
            json!({"scope": global_scope(), "sourceRoot": "/definitely/not/here"}),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // project scope：授权根外 → PATH_OUTSIDE_SCOPE；根内放行。
        let instance_id = rt.store().meta().instance_id.clone();
        let authorized_root = root.parent().unwrap().join("authorized-skills");
        std::fs::create_dir_all(authorized_root.join("ws")).unwrap();
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-scan', ?1, ?2, ?3)",
                rusqlite::params![
                    instance_id,
                    authorized_root.display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        rt.store()
            .register_project(&json!({
                "projectId": "proj-scan",
                "name": "SC",
                "environmentId": "local",
                "root": {"instanceId": instance_id, "rootId": "root-scan", "relativePath": "ws"},
            }))
            .unwrap();
        let error = dispatch_err(
            &rt,
            "skills.scan",
            json!({
                "scope": {"kind": "project", "id": "proj-scan"},
                "sourceRoot": source.display().to_string(),
            }),
        )
        .await;
        assert_eq!(error.code, "PATH_OUTSIDE_SCOPE");

        // 授权根内：正常扫描。
        std::fs::create_dir_all(authorized_root.join("ws").join("inner")).unwrap();
        std::fs::write(
            authorized_root.join("ws").join("inner").join("SKILL.md"),
            skill_md("inner", "An in-workspace skill for the scan test."),
        )
        .unwrap();
        let result = dispatch_ok(
            &rt,
            "skills.scan",
            json!({
                "scope": {"kind": "project", "id": "proj-scan"},
                "sourceRoot": authorized_root.join("ws").display().to_string(),
            }),
        )
        .await;
        let candidates = result["candidates"].as_array().unwrap().clone();
        let inner = candidates
            .iter()
            .find(|c| c["relativePath"] == json!("inner"))
            .expect("inner skill discovered in the authorized root");
        assert_eq!(inner["valid"], json!(true));
    }

    #[tokio::test]
    async fn plan_and_apply_import_two_phase_roundtrip() {
        let (_tmp, root) = fresh_home("skills-import");
        let (rt, _iid) = open_runtime(&root);
        // 源：两个技能目录（一个与目标冲突，一个新）。
        let source = root.parent().unwrap().join("import-src");
        std::fs::create_dir_all(source.join("alpha")).unwrap();
        std::fs::write(
            source.join("alpha").join("SKILL.md"),
            skill_md("alpha", "The alpha skill imported from a directory."),
        )
        .unwrap();
        std::fs::write(source.join("alpha").join("extra.md"), "alpha extra").unwrap();
        std::fs::create_dir_all(source.join("beta")).unwrap();
        std::fs::write(
            source.join("beta").join("SKILL.md"),
            skill_md("beta", "The beta skill imported from a directory."),
        )
        .unwrap();
        // 目标已有 alpha。
        put_skill(&rt, "alpha").await;

        // plan：skip 策略（默认）→ alpha 冲突，beta 导入。
        let plan = dispatch_ok(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "directory",
                "sourceRef": source.display().to_string(),
            }),
        )
        .await;
        let changes = plan["changes"].as_array().unwrap().clone();
        assert_eq!(changes.len(), 2);
        let find = |source_name: &str| {
            changes
                .iter()
                .find(|c| c["source"] == json!(source_name))
                .unwrap()
                .clone()
        };
        assert_eq!(find("alpha")["action"], json!("skip"));
        assert_eq!(find("beta")["action"], json!("import"));
        let plan_id = plan["planId"].as_str().unwrap().to_owned();
        let plan_hash = plan["planHash"].as_str().unwrap().to_owned();
        let source_hash = plan["sourceHash"].as_str().unwrap().to_owned();
        let expected_revision = plan["expectedRevision"].as_str().unwrap().to_owned();

        // apply（幂等提交）。
        let (_, receipt) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan_id,
                    "planHash": plan_hash,
                    "expectedRevision": expected_revision,
                }),
                Some("key-apply-1"),
                None,
            )
            .await
            .unwrap();
        let operation =
            wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let result = operation["result"].clone();
        assert_eq!(result["importedSkillIds"], json!(["beta"]));
        assert_eq!(result["skipped"], json!(["alpha"]));
        assert_eq!(result["renamed"], json!([]));
        assert_eq!(result["generation"], Value::Null);

        // 源内容与导入结果规范可比（导出→导入锚）。
        let read = dispatch_ok(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "beta"}),
        )
        .await;
        assert_eq!(
            read["content"].as_str().unwrap(),
            skill_md("beta", "The beta skill imported from a directory.")
        );
        assert_eq!(
            read["contentHash"],
            json!(sha256_hex(
                skill_md("beta", "The beta skill imported from a directory.").as_bytes()
            ))
        );

        // 重放同 key：不重复导入。
        let (_, replay) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan_id,
                    "planHash": plan_hash,
                    "expectedRevision": expected_revision,
                }),
                Some("key-apply-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], json!(true));

        // 计划一次性：另一个 key 再 apply → PLAN_STALE。
        let error = dispatch_err_keyed(
            &rt,
            "skills.apply_import",
            json!({
                "scope": global_scope(),
                "planId": plan_id,
                "planHash": plan_hash,
                "expectedRevision": result["revision"],
            }),
            "key-apply-stale-1",
        )
        .await;
        assert_eq!(error.code, "PLAN_STALE");

        // 源变化 → 新计划的 apply 拒绝（INTEGRITY_ERROR）。
        let plan = dispatch_ok(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "directory",
                "sourceRef": source.display().to_string(),
                "conflictPolicy": "overwrite",
            }),
        )
        .await;
        std::fs::write(
            source.join("beta").join("SKILL.md"),
            skill_md("beta", "Mutated after the plan was created."),
        )
        .unwrap();
        let (_, receipt) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan["planId"],
                    "planHash": plan["planHash"],
                    "expectedRevision": plan["expectedRevision"],
                }),
                Some("key-apply-2"),
                None,
            )
            .await
            .unwrap();
        let operation =
            wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(operation["state"], json!("failed"));
        assert_eq!(operation["result"]["code"], json!("INTEGRITY_ERROR"));
        // 源 hash 恢复（给后续断言一个干净状态无关紧要，这里只确认失败）。
        let _ = source_hash;

        // rename 策略：冲突目标改名。
        let plan = dispatch_ok(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "directory",
                "sourceRef": source.display().to_string(),
                "conflictPolicy": "rename",
            }),
        )
        .await;
        let find = |source_name: &str| {
            plan["changes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["source"] == json!(source_name))
                .unwrap()
                .clone()
        };
        assert_eq!(find("alpha")["action"], json!("rename"));
        assert_eq!(find("alpha")["target"], json!("alpha-imported"));
        let (_, receipt) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan["planId"],
                    "planHash": plan["planHash"],
                    "expectedRevision": plan["expectedRevision"],
                }),
                Some("key-apply-3"),
                None,
            )
            .await
            .unwrap();
        let operation =
            wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let renamed = operation["result"]["renamed"].as_array().unwrap().clone();
        assert!(
            renamed.contains(&json!({"from": "alpha", "to": "alpha-imported"})),
            "alpha is renamed per the rename policy: {renamed:?}"
        );

        // 旧 planHash → PLAN_STALE。
        let error = dispatch_err_keyed(
            &rt,
            "skills.apply_import",
            json!({
                "scope": global_scope(),
                "planId": plan["planId"],
                "planHash": "deadbeef",
                "expectedRevision": plan["expectedRevision"],
            }),
            "key-apply-stale-2",
        )
        .await;
        assert_eq!(error.code, "PLAN_STALE");
    }

    #[tokio::test]
    async fn plan_import_rejects_unsupported_origins_and_legacy_rules() {
        let (_tmp, root) = fresh_home("skills-plan-origins");
        let (rt, _iid) = open_runtime(&root);

        // registry / archive：诚实失败。
        let error = dispatch_err(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "registry",
                "sourceRef": "https://registry.example/skills",
            }),
        )
        .await;
        assert_eq!(error.code, "UPSTREAM_ERROR");
        let error = dispatch_err(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "archive",
                "sourceRef": "/tmp/skills.zip",
            }),
        )
        .await;
        assert_eq!(error.code, "UNAVAILABLE");

        // legacy：必须带 legacyFormat。
        let error = dispatch_err(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "legacy",
                "sourceRef": "/tmp/legacy",
            }),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 源不存在 → NOT_FOUND。
        let error = dispatch_err(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "directory",
                "sourceRef": "/definitely/not/here",
            }),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn scan_and_plan_ignore_directory_symlinks_and_anchor_to_project_workspace() {
        let (_tmp, root) = fresh_home("skills-symlink-anchor");
        let (rt, instance_id) = open_runtime(&root);
        let authorized_root = root.parent().unwrap().join("authorized-anchor");
        let ws = authorized_root.join("ws-a");
        let sibling = authorized_root.join("ws-sibling");
        let outside = root.parent().unwrap().join("outside-secret");
        std::fs::create_dir_all(ws.join("legit")).unwrap();
        std::fs::write(
            ws.join("legit").join("SKILL.md"),
            skill_md("legit", "Legit workspace skill."),
        )
        .unwrap();
        std::fs::create_dir_all(sibling.join("secret")).unwrap();
        std::fs::write(
            sibling.join("secret").join("SKILL.md"),
            skill_md("secret", "Sibling project secret skill."),
        )
        .unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("SKILL.md"),
            skill_md("escaped", "Outside directory reached via symlink."),
        )
        .unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, ws.join("escape-link")).unwrap();

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-anchor', ?1, ?2, ?3)",
                rusqlite::params![
                    instance_id,
                    authorized_root.display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        rt.store()
            .register_project(&json!({
                "projectId": "proj-a",
                "name": "Project A",
                "environmentId": "local",
                "root": {"instanceId": instance_id, "rootId": "root-anchor", "relativePath": "ws-a"},
            }))
            .unwrap();

        // 兄弟项目目录（同属 authorized_root，但在 project.workspace 外）必须被拒绝。
        let error = dispatch_err(
            &rt,
            "skills.scan",
            json!({
                "scope": {"kind": "project", "id": "proj-a"},
                "sourceRoot": sibling.display().to_string(),
            }),
        )
        .await;
        assert_eq!(error.code, "PATH_OUTSIDE_SCOPE");

        let error = dispatch_err(
            &rt,
            "skills.plan_import",
            json!({
                "scope": {"kind": "project", "id": "proj-a"},
                "origin": "directory",
                "sourceRef": sibling.display().to_string(),
            }),
        )
        .await;
        assert_eq!(error.code, "PATH_OUTSIDE_SCOPE");

        // 工作区内扫描与计划导入均不得跟随目录 symlink。
        let scan = dispatch_ok(
            &rt,
            "skills.scan",
            json!({
                "scope": {"kind": "project", "id": "proj-a"},
                "sourceRoot": ws.display().to_string(),
                "recursive": true,
            }),
        )
        .await;
        let paths: Vec<&str> = scan["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["relativePath"].as_str())
            .collect();
        assert!(paths.contains(&"legit"), "legit subdir scanned: {paths:?}");
        assert!(
            !paths.iter().any(|p| p.contains("escape-link")),
            "directory symlink must not be traversed: {paths:?}"
        );

        let plan = dispatch_ok(
            &rt,
            "skills.plan_import",
            json!({
                "scope": {"kind": "project", "id": "proj-a"},
                "origin": "directory",
                "sourceRef": ws.display().to_string(),
            }),
        )
        .await;
        let sources: Vec<&str> = plan["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["source"].as_str())
            .collect();
        assert_eq!(sources, vec!["legit"]);
    }

    #[tokio::test]
    async fn plan_and_apply_import_supports_single_skill_root() {
        let (_tmp, root) = fresh_home("skills-single-root");
        let (rt, _iid) = open_runtime(&root);
        let single = root.parent().unwrap().join("solo-skill");
        std::fs::create_dir_all(single.join("refs")).unwrap();
        std::fs::write(
            single.join("SKILL.md"),
            skill_md("solo-skill", "Single skill directory imported directly."),
        )
        .unwrap();
        std::fs::write(single.join("refs").join("guide.md"), "solo guide").unwrap();

        let plan = dispatch_ok(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "directory",
                "sourceRef": single.display().to_string(),
            }),
        )
        .await;
        let changes = plan["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["source"], json!("solo-skill"));
        assert_eq!(changes[0]["target"], json!("solo-skill"));
        assert_eq!(changes[0]["action"], json!("import"));

        let (_, receipt) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan["planId"],
                    "planHash": plan["planHash"],
                    "expectedRevision": plan["expectedRevision"],
                }),
                Some("key-single-root-1"),
                None,
            )
            .await
            .unwrap();
        let op = wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(op["state"], json!("succeeded"), "single root apply: {op:?}");
        assert_eq!(op["result"]["importedSkillIds"], json!(["solo-skill"]));

        let read = dispatch_ok(
            &rt,
            "skills.read",
            json!({
                "scope": global_scope(),
                "skillId": "solo-skill",
                "relativePath": "refs/guide.md",
            }),
        )
        .await;
        assert_eq!(read["content"], json!("solo guide"));
    }

    #[tokio::test]
    async fn apply_import_rolls_back_dirs_and_db_on_mid_batch_failure() {
        let (_tmp, root) = fresh_home("skills-atomic-rollback");
        let (rt, _iid) = open_runtime(&root);
        // 预装 alpha v1，随后批量导入 overwrite(alpha v2) + beta（注入 DB 失败）。
        put_skill(&rt, "alpha").await;
        let source = root.parent().unwrap().join("batch-src");
        std::fs::create_dir_all(source.join("alpha")).unwrap();
        std::fs::write(
            source.join("alpha").join("SKILL.md"),
            skill_md("alpha", "Updated alpha v2 in batch."),
        )
        .unwrap();
        std::fs::create_dir_all(source.join("beta")).unwrap();
        std::fs::write(
            source.join("beta").join("SKILL.md"),
            skill_md("beta", "Beta triggers mid-batch rollback."),
        )
        .unwrap();

        let plan = dispatch_ok(
            &rt,
            "skills.plan_import",
            json!({
                "scope": global_scope(),
                "origin": "directory",
                "sourceRef": source.display().to_string(),
                "conflictPolicy": "overwrite",
            }),
        )
        .await;

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute_batch(
                "CREATE TRIGGER test_fail_beta
                 BEFORE INSERT ON v1_skills
                 WHEN NEW.skill_id = 'beta'
                 BEGIN
                     SELECT RAISE(ABORT, 'simulated sqlite failure on beta');
                 END;",
            )
            .unwrap();
        }

        let (_, receipt) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan["planId"],
                    "planHash": plan["planHash"],
                    "expectedRevision": plan["expectedRevision"],
                }),
                Some("key-rollback-fail"),
                None,
            )
            .await
            .unwrap();
        let op = wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(op["state"], json!("failed"));

        // 回滚验证：alpha 恢复 v1 内容，beta 目录不存在，plan 仍为 open。
        let read_alpha = dispatch_ok(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "alpha"}),
        )
        .await;
        assert_eq!(
            read_alpha["content"],
            json!(skill_md("alpha", "A skill for the extensions test suite."))
        );
        let beta_err = dispatch_err(
            &rt,
            "skills.read",
            json!({"scope": global_scope(), "skillId": "beta"}),
        )
        .await;
        assert_eq!(beta_err.code, "NOT_FOUND");
        assert!(
            !skill_dir(rt.store(), &SkillScope::global(), "beta").exists(),
            "failed batch must remove partially published beta dir"
        );

        // 移除故障注入后，同计划可重试成功，且相同 idempotencyKey 重放直接 deduplicated。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute_batch("DROP TRIGGER test_fail_beta;").unwrap();
        }
        let (_, retry_receipt) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan["planId"],
                    "planHash": plan["planHash"],
                    "expectedRevision": plan["expectedRevision"],
                }),
                Some("key-rollback-retry"),
                None,
            )
            .await
            .unwrap();
        let retry_op = wait_operation(
            &rt,
            retry_receipt["operationId"].as_str().unwrap_or_default(),
        )
        .await;
        assert_eq!(retry_op["state"], json!("succeeded"));
        assert_eq!(
            retry_op["result"]["importedSkillIds"],
            json!(["alpha", "beta"])
        );

        let (_, replay) = rt
            .dispatch(
                "skills.apply_import",
                json!({
                    "scope": global_scope(),
                    "planId": plan["planId"],
                    "planHash": plan["planHash"],
                    "expectedRevision": plan["expectedRevision"],
                }),
                Some("key-rollback-retry"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], json!(true));
    }
}
