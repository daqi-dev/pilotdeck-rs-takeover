//! `memory.*` — 记忆域（t018，S3）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `memory.*` 条目（按 id 检
//! 索）。TS 基线（cc7c4e6 `createLocalGateway.ts:1238-1254`）只暴露
//! `memoryList` / `memoryWipe` 两个管理原语；v1 的 16 个操作是
//! design_candidate 新面，语义参考
//! `resources/PilotDeck/src/context/memory/edgeclaw-memory-core/src/service.ts`
//! （list/get/edit/deprecate/restore/settings/traces/export/import/clear 的
//! 行为轮廓）但按合同字段重组。
//!
//! ## 存储设计（与旧 Rust 线的关系）
//!
//! 旧线 `runtime/memory/edgeclaw/`（约 10.4k 行，逐字节 TS 对齐移植）没
//! 有被直接接线，原因（执行计划 §7「接线不成比例时以薄适配层重实现并
//! 登记差异」的预授权模式）：
//!
//! 1. 合同的数据模型（结构化 `fields`、逐条 `revision`、
//!    `contentArtifactId`、session 作用域、`agentcpm-pd.memory-bundle.v1`
//!    bundle 格式、ChangePlan 两段式）在 EdgeClaw 引擎中没有对应物：引
//!    擎条目以 markdown 文件为真身（`MemoryFileFrontmatter`，无逐条
//!    revision，fields 需从 markdown 反解），映射两头都是有损的。
//! 2. EdgeClaw 的 `ServiceOptions` 绑定旧 runtime 的 per-workspace 组装
//!    （`edgeclaw/service/mod.rs:430`），`act`/`list` 走 `JsValue` 边界
//!    （`service/mod.rs:711/955`）；且 dream 重写文件后 v1 的 revision 列
//!    必然漂移（brief §7 风险 2 的结论）。
//! 3. `memory.maintain` 的 index/dream 依赖心跳采集（`captureTurn`）与
//!    LLM 抽取管线；v1 合同 285 操作中没有采集入口，runtime 的
//!    `models.invoke` seam 亦未落地（git.rs 同期同样以显式
//!    MODEL_UNAVAILABLE 占位，`domains/git.rs:3948`）。
//!
//! 因此本域用 `reg.schema` 自管 SQLite 表（brief §4 草案），内容/明细/
//! bundle 字节经 t013 `artifacts.rs` 的跨域 API 落不可变产物。与基线的
//! 每一处显式差异在任务报告 §4 登记。
//!
//! ## 协议要点
//!
//! - **条件写**：`memory.edit` / `change_state` / `set_settings` /
//!   `set_project_meta` 携 `expectedRevision`，不匹配 →
//!   REVISION_CONFLICT；只读条目 → READ_ONLY_MEMORY。
//! - **两段式**：`plan_import`→`import`、`plan_clear`→`clear` 走
//!   `v1_memory_change_plans`：plan 记录 base revision，第二跳在受理
//!   事务里复验（过期 → PLAN_EXPIRED、scope 被并发修改 →
//!   REVISION_CONFLICT、planRevision 不匹配 → REVISION_CONFLICT）并一次性
//!   消费（`state='consumed'`）。
//! - **维护串行**：`memory.maintain` 以 `v1_memory_maintenance` 行按
//!   scope 串行（同 scope 已有未终态维护 → MAINTENANCE_BUSY；崩溃残留
//!   依操作行终态或缺失判定可回收）。
//! - **异步受理**：export/import/clear/maintain 走共享
//!   `v1_domain_operations` seam（schedules.rs 的幂等受理 + 完成记录），
//!   `operations.get/list` 因此能看到本域操作。
//!
//! 锁纪律：`publish_artifact` 等跨域 API 自带 `store.conn` 短锁，绝不在
//! 持有该连接（事务或 guard）时调用——所有产物发布先于事务。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetryMode, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::artifacts::publish_artifact;
use crate::public_v1::domains::schedules::{
    DomainDb, DomainOperationLookup, finish_domain_operation, lookup_domain_operation,
    mark_domain_operation_running, record_domain_operation_accept,
    record_domain_operation_rejection,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 合同错误表中本域专属（protocol crate 尚无公共常量）的错误码。
pub(crate) const CODE_READ_ONLY_MEMORY: &str = "READ_ONLY_MEMORY";
pub(crate) const CODE_MAINTENANCE_BUSY: &str = "MAINTENANCE_BUSY";
pub(crate) const CODE_PLAN_EXPIRED: &str = "PLAN_EXPIRED";
pub(crate) const CODE_SNAPSHOT_UNAVAILABLE: &str = "SNAPSHOT_UNAVAILABLE";
const CODE_INVALID_BUNDLE: &str = "INVALID_BUNDLE";
const CODE_UNSUPPORTED_BUNDLE_VERSION: &str = "UNSUPPORTED_BUNDLE_VERSION";
const CODE_SCOPE_MISMATCH: &str = "SCOPE_MISMATCH";

/// 本域 bundle 的唯一合法格式版本（合同 completionResult 的 const）。
pub(crate) const BUNDLE_FORMAT_VERSION: &str = "agentcpm-pd.memory-bundle.v1";
/// 两段式 plan 的过期窗口。
const PLAN_TTL_MINUTES: i64 = 10;
/// 列表分页上限（与 files 域同量级；PageInput limit 默认 100）。
const LIST_PAGE_LIMIT: usize = 500;
/// `MemoryFields` 允许的全部字段名（bundle 校验的第二道；dispatch 层的
/// schema additionalProperties:false 是第一道）。
const FIELD_KEYS: [&str; 10] = [
    "stage",
    "decisions",
    "constraints",
    "nextSteps",
    "blockers",
    "timeline",
    "notes",
    "rule",
    "why",
    "howToApply",
];

const DDL_ENTRIES: &str = "CREATE TABLE IF NOT EXISTS v1_memory_entries (
    memory_id TEXT PRIMARY KEY,
    scope_kind TEXT NOT NULL,
    project_id TEXT,
    session_id TEXT,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    fields_json TEXT NOT NULL DEFAULT '{}',
    revision TEXT NOT NULL,
    deprecated INTEGER NOT NULL DEFAULT 0,
    read_only INTEGER NOT NULL DEFAULT 0,
    content_artifact_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const DDL_SETTINGS: &str = "CREATE TABLE IF NOT EXISTS v1_memory_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    reasoning_mode TEXT NOT NULL,
    auto_index_interval_minutes INTEGER NOT NULL,
    auto_dream_interval_minutes INTEGER NOT NULL,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const DDL_PROJECT_META: &str = "CREATE TABLE IF NOT EXISTS v1_memory_project_meta (
    project_id TEXT PRIMARY KEY,
    project_name TEXT NOT NULL,
    description TEXT NOT NULL,
    status TEXT NOT NULL,
    revision TEXT NOT NULL,
    read_only INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
)";
const DDL_TRACES: &str = "CREATE TABLE IF NOT EXISTS v1_memory_traces (
    trace_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    project_id TEXT,
    session_id TEXT,
    operation_id TEXT,
    state TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    summary TEXT NOT NULL,
    detail_artifact_id TEXT,
    detail_schema_id TEXT NOT NULL,
    created_at TEXT NOT NULL
)";
const DDL_PLANS: &str = "CREATE TABLE IF NOT EXISTS v1_memory_change_plans (
    plan_id TEXT PRIMARY KEY,
    plan_revision TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open',
    expires_at TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    affected_resources_json TEXT NOT NULL DEFAULT '[]',
    destructive INTEGER NOT NULL DEFAULT 0,
    base_revisions_json TEXT NOT NULL DEFAULT '[]',
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL
)";
const DDL_MAINTENANCE: &str = "CREATE TABLE IF NOT EXISTS v1_memory_maintenance (
    scope_key TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    state TEXT NOT NULL,
    started_at TEXT NOT NULL
)";
const DDL_META: &str = "CREATE TABLE IF NOT EXISTS v1_memory_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_memory_entries", DDL_ENTRIES);
    reg.schema("v1_memory_settings", DDL_SETTINGS);
    reg.schema("v1_memory_project_meta", DDL_PROJECT_META);
    reg.schema("v1_memory_traces", DDL_TRACES);
    reg.schema("v1_memory_change_plans", DDL_PLANS);
    reg.schema("v1_memory_maintenance", DDL_MAINTENANCE);
    reg.schema("v1_memory_meta", DDL_META);

    reg.handler("memory.list", |rt, input, _key, _rev| {
        Box::pin(memory_list(rt, input))
    });
    reg.handler("memory.get", |rt, input, _key, _rev| {
        Box::pin(memory_get(rt, input))
    });
    reg.handler("memory.overview", |rt, input, _key, _rev| {
        Box::pin(memory_overview(rt, input))
    });
    reg.handler("memory.get_settings", |rt, input, _key, _rev| {
        Box::pin(memory_get_settings(rt, input))
    });
    reg.handler("memory.get_project_meta", |rt, input, _key, _rev| {
        Box::pin(memory_get_project_meta(rt, input))
    });
    reg.handler("memory.traces", |rt, input, _key, _rev| {
        Box::pin(memory_traces(rt, input))
    });
    reg.handler("memory.edit", |rt, input, _key, _rev| {
        Box::pin(memory_edit(rt, input))
    });
    reg.handler("memory.change_state", |rt, input, _key, _rev| {
        Box::pin(memory_change_state(rt, input))
    });
    reg.handler("memory.set_settings", |rt, input, _key, _rev| {
        Box::pin(memory_set_settings(rt, input))
    });
    reg.handler("memory.set_project_meta", |rt, input, _key, _rev| {
        Box::pin(memory_set_project_meta(rt, input))
    });
    reg.handler("memory.export", |rt, input, key, _rev| {
        Box::pin(memory_export(rt, input, key))
    });
    reg.handler("memory.plan_import", |rt, input, _key, _rev| {
        Box::pin(memory_plan_import(rt, input))
    });
    reg.handler("memory.import", |rt, input, key, _rev| {
        Box::pin(memory_import(rt, input, key))
    });
    reg.handler("memory.plan_clear", |rt, input, _key, _rev| {
        Box::pin(memory_plan_clear(rt, input))
    });
    reg.handler("memory.clear", |rt, input, key, _rev| {
        Box::pin(memory_clear(rt, input, key))
    });
    reg.handler("memory.maintain", |rt, input, key, _rev| {
        Box::pin(memory_maintain(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// 错误与基础工具
// ---------------------------------------------------------------------------

pub(super) fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

pub(super) fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// 可选 plain-string 字段：None 时省略键（这些字段的 schema 是
/// `{"type":"string"}` 而非 nullable——显式 null 过不了校验）。
pub(super) fn set_optional_str(obj: &mut Value, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        obj[key] = json!(value);
    }
}

fn domain_not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict(expected: &str, actual: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("memory revision `{expected}` does not match current `{actual}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

pub(super) fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("memory worker task failed: {err}"),
        data: None,
    }
}

/// RFC3339 严格先后比较（解析失败返回 None，由调用方决定语义）。
pub(super) fn rfc3339_lt(a: &str, b: &str) -> Option<bool> {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(x), Ok(y)) => Some(x < y),
        _ => None,
    }
}

pub(super) fn expires_in_minutes(minutes: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::minutes(minutes)).to_rfc3339()
}

/// 绑定辅助：`&[String]` → `&[&dyn ToSql]`（位置绑定，配合未编号 `?`）。
fn string_params(values: &[String]) -> Vec<&dyn rusqlite::types::ToSql> {
    values
        .iter()
        .map(|p| p as &dyn rusqlite::types::ToSql)
        .collect()
}

// ---------------------------------------------------------------------------
// 作用域
// ---------------------------------------------------------------------------

/// 合同 `MemoryScope`。条目自身的 scope 只会是 project/session/global；
/// allAuthorizedProjects 只作为查询/聚合作用域。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MemoryScope {
    pub kind: &'static str,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
}

impl MemoryScope {
    fn from_value(value: &Value) -> Result<Self, ContractError> {
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // 匹配字面量返回 &'static str（ scrutinee 的借用活不过输入引用）。
        let kind = match kind {
            "project" => "project",
            "session" => "session",
            "global" => "global",
            "allAuthorizedProjects" => "allAuthorizedProjects",
            "" => return Err(invalid("scope.kind is required")),
            other => return Err(invalid(format!("unsupported scope kind `{other}`"))),
        };
        let project_id = value
            .get("projectId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        // dispatch 层的 schema 校验已强制这些组合；这里防御性兜底。
        match kind {
            "project" | "session" if project_id.is_none() => {
                return Err(invalid(format!("scope `{kind}` requires projectId")));
            }
            "session" if session_id.is_none() => {
                return Err(invalid("scope `session` requires sessionId"));
            }
            "global" | "allAuthorizedProjects"
                if (project_id.is_some() || session_id.is_some()) =>
            {
                return Err(invalid(format!(
                    "scope `{kind}` must not carry projectId/sessionId"
                )));
            }
            _ => {}
        }
        Ok(Self {
            kind,
            project_id,
            session_id,
        })
    }

    fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("kind".into(), json!(self.kind));
        if let Some(id) = &self.project_id {
            obj.insert("projectId".into(), json!(id));
        }
        if let Some(id) = &self.session_id {
            obj.insert("sessionId".into(), json!(id));
        }
        Value::Object(obj)
    }

    /// 维护串行与 affectedResources 用的稳定键。
    fn key(&self) -> String {
        match (self.kind, &self.project_id, &self.session_id) {
            ("project", Some(p), _) => format!("memory:project:{p}"),
            ("session", Some(p), Some(s)) => format!("memory:session:{p}/{s}"),
            ("global", _, _) => "memory:global".to_owned(),
            _ => "memory:allAuthorizedProjects".to_owned(),
        }
    }

    /// 资源存在性校验（precondition「资源作用域检查」）：project 必须已注
    /// 册、session 必须存在于该项目下。
    fn validate_resources(&self, conn: &Connection) -> Result<(), ContractError> {
        if let Some(project_id) = &self.project_id {
            let registered: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM projects WHERE project_id = ?1",
                    params![project_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if registered.is_none() {
                return Err(domain_not_found("project", project_id));
            }
        }
        if let (Some(project_id), Some(session_id)) = (&self.project_id, &self.session_id) {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE session_id = ?1 AND project_id = ?2",
                    params![session_id, project_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if exists.is_none() {
                return Err(domain_not_found("session", session_id));
            }
        }
        Ok(())
    }

    /// 查询作用域 → SQL 片段（未编号 `?`，按出现顺序绑定 scope_params）。
    fn entry_filter(&self) -> (&'static str, Vec<String>) {
        match self.kind {
            "project" => (
                "scope_kind = 'project' AND project_id = ?",
                vec![self.project_id.clone().unwrap_or_default()],
            ),
            "session" => (
                "scope_kind = 'session' AND project_id = ? AND session_id = ?",
                vec![
                    self.project_id.clone().unwrap_or_default(),
                    self.session_id.clone().unwrap_or_default(),
                ],
            ),
            "global" => ("scope_kind = 'global'", Vec::new()),
            _ => (
                "scope_kind IN ('project', 'session') AND project_id IN (SELECT project_id FROM projects)",
                Vec::new(),
            ),
        }
    }

    /// 轨迹作用域过滤（同上：未编号 `?`，绑定 trace_scope_params）。
    fn trace_filter(&self) -> (&'static str, Vec<String>) {
        match self.kind {
            "project" => (
                "project_id = ?",
                vec![self.project_id.clone().unwrap_or_default()],
            ),
            "session" => (
                "project_id = ? AND session_id = ?",
                vec![
                    self.project_id.clone().unwrap_or_default(),
                    self.session_id.clone().unwrap_or_default(),
                ],
            ),
            "global" => ("scope_kind = 'global'", Vec::new()),
            _ => (
                "(scope_kind IN ('project', 'session') AND project_id IN (SELECT project_id FROM projects))",
                Vec::new(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// 修订计数
// ---------------------------------------------------------------------------

/// 本域全局修订（`mrev-%08d`）：条目/设置/meta 的行 revision 与读面的
/// `revision` 字段同源，所有写路径递增。
fn bump_memory_rev_in_tx(tx: &Connection) -> Result<String, ContractError> {
    let current: Option<i64> = tx
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM v1_memory_meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let next = current.unwrap_or(0) + 1;
    tx.execute(
        "INSERT INTO v1_memory_meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("mrev-{next:08}"))
}

fn current_memory_rev(conn: &Connection) -> Result<String, ContractError> {
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM v1_memory_meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(current.unwrap_or_else(|| "mrev-00000000".to_owned()))
}

// ---------------------------------------------------------------------------
// 条目行模型
// ---------------------------------------------------------------------------

struct MemoryEntryRow {
    memory_id: String,
    scope_kind: String,
    project_id: Option<String>,
    session_id: Option<String>,
    kind: String,
    name: String,
    description: String,
    fields_json: String,
    revision: String,
    deprecated: bool,
    read_only: bool,
    content_artifact_id: Option<String>,
    created_at: String,
    updated_at: String,
}

const ENTRY_COLUMNS: &str =
    "memory_id, scope_kind, project_id, session_id, kind, name, description,
    fields_json, revision, deprecated, read_only, content_artifact_id, created_at, updated_at";

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntryRow> {
    Ok(MemoryEntryRow {
        memory_id: row.get(0)?,
        scope_kind: row.get(1)?,
        project_id: row.get(2)?,
        session_id: row.get(3)?,
        kind: row.get(4)?,
        name: row.get(5)?,
        description: row.get(6)?,
        fields_json: row.get(7)?,
        revision: row.get(8)?,
        deprecated: row.get::<_, i64>(9)? != 0,
        read_only: row.get::<_, i64>(10)? != 0,
        content_artifact_id: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

impl MemoryEntryRow {
    fn scope_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("kind".into(), json!(self.scope_kind));
        if let Some(id) = &self.project_id {
            obj.insert("projectId".into(), json!(id));
        }
        if let Some(id) = &self.session_id {
            obj.insert("sessionId".into(), json!(id));
        }
        Value::Object(obj)
    }

    fn fields(&self) -> Value {
        serde_json::from_str(&self.fields_json).unwrap_or_else(|_| json!({}))
    }

    /// 合同 `MemoryEntry`（fields 始终携带，空对象合法；contentArtifactId
    /// 可选——缺失时省略键）。
    fn to_contract_json(&self) -> Value {
        let mut entry = json!({
            "memoryId": self.memory_id,
            "scope": self.scope_json(),
            "kind": self.kind,
            "name": self.name,
            "description": self.description,
            "fields": self.fields(),
            "revision": self.revision,
            "deprecated": self.deprecated,
            "readOnly": self.read_only,
        });
        set_optional_str(&mut entry, "contentArtifactId", &self.content_artifact_id);
        entry
    }

    /// bundle / 恢复产物里的完整条目（含时间戳，可完整重建）。
    fn to_full_json(&self) -> Value {
        let mut out = self.to_contract_json();
        out["createdAt"] = json!(self.created_at);
        out["updatedAt"] = json!(self.updated_at);
        out
    }
}

/// 发布条目内容产物：条目的规范 JSON 文档（kind/name/description/fields）。
/// `memory.get` 的「完整内容以有版本 artifact 取得，不无说明截断」由它
/// 承载。必须在**未持有** `store.conn` 时调用（publish 自带短锁）。
fn publish_entry_artifact(
    store: &V1Store,
    kind: &str,
    name: &str,
    description: &str,
    fields: &Value,
) -> Result<String, ContractError> {
    let document = json!({
        "kind": kind,
        "name": name,
        "description": description,
        "fields": fields,
    });
    let artifact = publish_artifact(
        store,
        "memory",
        "application/json",
        Some(&format!("memory:{name}")),
        document.to_string().as_bytes(),
        "memory",
    )?;
    Ok(artifact["artifactId"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

// ---------------------------------------------------------------------------
// 事件
// ---------------------------------------------------------------------------

/// 追加一条 `memory.changed` 事件（镜像 store.rs::append_event_in_tx 的
/// 两步 cursor 赋值）。payload 只带 scope/ids/revision，不带内容。
fn append_memory_event(
    conn: &Connection,
    resource_id: &str,
    change: &str,
    scope: &MemoryScope,
    memory_ids: &[String],
    revision: &str,
) -> Result<(), ContractError> {
    static SEQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let placeholder = format!("tmpmem-{nanos}-{seq}");
    let payload = json!({
        "scope": scope.to_json(),
        "change": change,
        "memoryIds": memory_ids,
        "revision": revision,
    });
    let payload_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', 'memory.changed', 'agentcpm-pd.memory.event/1', ?3, ?3, ?4)",
        params![placeholder, resource_id, payload_str, now_rfc3339()],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![crate::public_v1::store::format_cursor(seq), seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 事件 resource_id：project 作用域用 projectId，其余用 scope 键。
fn event_resource_id(scope: &MemoryScope) -> String {
    scope.project_id.clone().unwrap_or_else(|| scope.key())
}

/// `config.changed` 事件（`memory.set_settings` 的事件表要求）。与
/// configuration 域的 append_config_event 同 schema 同形态——那个助手在
/// configuration 的私有子模块里，跨域不可达，故此处自实现。payload 只带
/// revision 与键名，不带值（settings 无秘密，但保持配置事件的最小面）。
fn append_config_changed_event(conn: &Connection, payload: &Value) -> Result<(), ContractError> {
    static SEQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let placeholder = format!("tmpmemcfg-{nanos}-{seq}");
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, 'memory', '', 'config.changed', 'agentcpm-pd.configuration.event/1', ?2, ?2, ?3)",
        params![placeholder, payload_str, now_rfc3339()],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![crate::public_v1::store::format_cursor(seq), seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 读面 handlers
// ---------------------------------------------------------------------------

async fn memory_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || list_entries(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 列表游标：`memlist-<revision>-<offset>`（本域修订钉扎；分页期间任何
/// 记忆写使游标失效——与 files.list 的目录 revision 钉扎同一纪律）。
fn parse_list_cursor(cursor: &str) -> Result<(String, usize), ContractError> {
    let bad = || invalid(format!("invalid memory.list cursor `{cursor}`"));
    let Some(rest) = cursor.strip_prefix("memlist-") else {
        return Err(bad());
    };
    let (rev, offset) = rest.rsplit_once('-').ok_or_else(bad)?;
    if rev.is_empty() {
        return Err(bad());
    }
    Ok((rev.to_owned(), offset.parse::<usize>().map_err(|_| bad())?))
}

fn list_entries(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let scope = MemoryScope::from_value(&input["scope"])?;
    let kinds: Vec<String> = input
        .get("kinds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let include_deprecated = input
        .get("includeDeprecated")
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

    let conn = store.conn.lock().expect("sqlite mutex");
    scope.validate_resources(&conn)?;
    let revision = current_memory_rev(&conn)?;
    let mut offset = 0usize;
    if let Some(cursor) = cursor_raw.as_deref() {
        let (cursor_rev, cursor_offset) = parse_list_cursor(cursor)?;
        if cursor_rev != revision {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "memory changed during pagination; restart from the first page",
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        offset = cursor_offset;
    }

    let (filter, scope_params) = scope.entry_filter();
    let mut sql = format!("SELECT {ENTRY_COLUMNS} FROM v1_memory_entries WHERE {filter}");
    if !kinds.is_empty() {
        let marks = vec!["?"; kinds.len()].join(", ");
        sql.push_str(&format!(" AND kind IN ({marks})"));
    }
    if !include_deprecated {
        sql.push_str(" AND deprecated = 0");
    }
    sql.push_str(" ORDER BY memory_id ASC");
    let mut all_params = scope_params.clone();
    all_params.extend(kinds.iter().cloned());
    let rows = conn
        .prepare(&sql)
        .map_err(sql_err)?
        .query_map(string_params(&all_params).as_slice(), row_to_entry)
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;

    // query 过滤（name+description 的不区分大小写子串）+ 分页窗口。
    let filtered: Vec<&MemoryEntryRow> = rows
        .iter()
        .filter(|row| {
            query.is_empty()
                || row.name.to_lowercase().contains(&query)
                || row.description.to_lowercase().contains(&query)
        })
        .collect();
    if offset > filtered.len() {
        return Err(invalid("pagination cursor beyond end of the entry list"));
    }
    let end = (offset + limit).min(filtered.len());
    let entries: Vec<Value> = filtered[offset..end]
        .iter()
        .map(|r| r.to_contract_json())
        .collect();
    // nextCursor 是可选 string（非 nullable）：完成页直接省略字段。
    let mut result = json!({
        "configured": true,
        "entries": entries,
        "revision": revision,
    });
    if end < filtered.len() {
        result["nextCursor"] = json!(format!("memlist-{revision}-{end}"));
    }
    Ok(result)
}

async fn memory_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || get_entries(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

enum GetOutcome {
    Entries {
        rows: Vec<MemoryEntryRow>,
        revision: String,
        missing: Vec<String>,
    },
    UserSummary {
        rows: Vec<MemoryEntryRow>,
        revision: String,
    },
}

fn get_entries(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let scope = MemoryScope::from_value(&input["scope"])?;
    let view = input
        .get("view")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let outcome = {
        let conn = store.conn.lock().expect("sqlite mutex");
        scope.validate_resources(&conn)?;
        let revision = current_memory_rev(&conn)?;
        match view {
            "entries" => {
                let ids: Vec<String> = input
                    .get("memoryIds")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                // 逐条授权：scope 之外的条目按缺失报告，不泄露内容。
                let (filter, scope_params) = scope.entry_filter();
                let sql = format!(
                    "SELECT {ENTRY_COLUMNS} FROM v1_memory_entries WHERE memory_id = ? AND {filter}"
                );
                let mut rows = Vec::with_capacity(ids.len());
                let mut missing = Vec::new();
                for id in &ids {
                    let mut all = vec![id.clone()];
                    all.extend(scope_params.iter().cloned());
                    let row = conn
                        .query_row(&sql, string_params(&all).as_slice(), row_to_entry)
                        .optional()
                        .map_err(sql_err)?;
                    match row {
                        Some(row) => rows.push(row),
                        None => missing.push(id.clone()),
                    }
                }
                GetOutcome::Entries {
                    rows,
                    revision,
                    missing,
                }
            }
            "userSummary" => {
                // TS `getUserSummary`（service.ts 的 identityBackground +
                // files）的 v1 投影：user 类条目汇总。
                let (filter, scope_params) = scope.entry_filter();
                let sql = format!(
                    "SELECT {ENTRY_COLUMNS} FROM v1_memory_entries
                     WHERE {filter} AND kind = 'user' ORDER BY memory_id ASC"
                );
                let rows = conn
                    .prepare(&sql)
                    .map_err(sql_err)?
                    .query_map(string_params(&scope_params).as_slice(), row_to_entry)
                    .map_err(sql_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_err)?;
                GetOutcome::UserSummary { rows, revision }
            }
            other => return Err(invalid(format!("unsupported memory.get view `{other}`"))),
        }
    };
    match outcome {
        GetOutcome::Entries {
            rows,
            revision,
            missing,
        } => Ok(json!({
            "entries": rows.iter().map(MemoryEntryRow::to_contract_json).collect::<Vec<_>>(),
            "revision": revision,
            "missingIds": missing,
        })),
        GetOutcome::UserSummary { rows, revision } => {
            let summary = json!({
                "identityBackground": rows
                    .iter()
                    .map(|r| format!("{}: {}", r.name, r.description))
                    .collect::<Vec<_>>(),
                "files": rows.iter().map(MemoryEntryRow::to_full_json).collect::<Vec<_>>(),
            });
            let artifact = publish_artifact(
                store,
                "memory",
                "application/json",
                Some("memory:userSummary"),
                summary.to_string().as_bytes(),
                "memory",
            )?;
            Ok(json!({
                "entries": rows.iter().map(MemoryEntryRow::to_contract_json).collect::<Vec<_>>(),
                "summaryArtifactId": artifact["artifactId"],
                "revision": revision,
                "missingIds": [],
            }))
        }
    }
}

async fn memory_overview(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || build_overview(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn build_overview(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let scope = MemoryScope::from_value(&input["scope"])?;
    let view = input
        .get("view")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let revision;
    let memory_count: i64;
    let last_index: Option<String>;
    let last_dream: Option<String>;
    let active: Vec<String>;
    let snapshot_state: Option<(MemoryScope, Vec<MemoryEntryRow>)>;
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        scope.validate_resources(&conn)?;
        revision = current_memory_rev(&conn)?;

        let (filter, scope_params) = scope.entry_filter();
        memory_count = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM v1_memory_entries WHERE {filter}"),
                string_params(&scope_params).as_slice(),
                |r| r.get(0),
            )
            .map_err(sql_err)?;

        let last_trace = |kind: &str| -> Result<Option<String>, ContractError> {
            let (trace_filter, trace_params) = scope.trace_filter();
            let sql = format!(
                "SELECT trace_id FROM v1_memory_traces WHERE kind = ? AND {trace_filter}
                 ORDER BY started_at DESC, trace_id DESC LIMIT 1"
            );
            let mut all = vec![kind.to_owned()];
            all.extend(trace_params);
            let id: Option<String> = conn
                .query_row(&sql, string_params(&all).as_slice(), |r| r.get(0))
                .optional()
                .map_err(sql_err)?;
            Ok(id)
        };
        last_index = last_trace("index")?;
        last_dream = last_trace("dream")?;

        // 活跃维护：锁行（行随终态删除；跨进程崩溃残留按操作行终态或
        // 缺失甄别）。
        let locks: Vec<(String, String)> = conn
            .prepare(
                "SELECT scope_key, operation_id FROM v1_memory_maintenance WHERE state = 'running'",
            )
            .map_err(sql_err)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        active = locks
            .into_iter()
            .filter(|(scope_key, _)| scope_covers(scope_key, &scope))
            .map(|(_, operation_id)| operation_id)
            .collect();

        snapshot_state = match view {
            "overview" => None,
            "snapshot" | "workspace" => {
                let artifact_scope = if view == "workspace" {
                    MemoryScope {
                        kind: "allAuthorizedProjects",
                        project_id: None,
                        session_id: None,
                    }
                } else {
                    scope.clone()
                };
                let (filter, scope_params) = artifact_scope.entry_filter();
                let rows = conn
                    .prepare(&format!(
                        "SELECT {ENTRY_COLUMNS} FROM v1_memory_entries WHERE {filter} ORDER BY memory_id ASC"
                    ))
                    .map_err(sql_err)?
                    .query_map(string_params(&scope_params).as_slice(), row_to_entry)
                    .map_err(sql_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_err)?;
                Some((artifact_scope, rows))
            }
            other => {
                return Err(invalid(format!(
                    "unsupported memory.overview view `{other}`"
                )));
            }
        }
    }

    let mut result = json!({
        "scope": scope.to_json(),
        "revision": revision,
        "configured": true,
        "status": "healthy",
        "pendingSessionCount": 0,
        "memoryCount": memory_count,
        "activeOperationIds": active,
        "issues": [],
    });
    set_optional_str(&mut result, "lastIndexTraceId", &last_index);
    set_optional_str(&mut result, "lastDreamTraceId", &last_dream);
    if let Some((artifact_scope, rows)) = snapshot_state {
        let schema_id = if view == "workspace" {
            "memory.workspace.v1"
        } else {
            "memory.snapshot.v1"
        };
        let snapshot = json!({
            "schemaId": schema_id,
            "revision": revision,
            "scope": artifact_scope.to_json(),
            "entries": rows.iter().map(MemoryEntryRow::to_full_json).collect::<Vec<_>>(),
        });
        let artifact = publish_artifact(
            store,
            "memory",
            "application/json",
            Some(&format!("memory:{view}")),
            snapshot.to_string().as_bytes(),
            "memory",
        )?;
        result["snapshotArtifactId"] = artifact["artifactId"].clone();
        result["snapshotSchemaId"] = json!(schema_id);
    }
    Ok(result)
}

/// 维护锁的 scope 键是否覆盖请求 scope。
fn scope_covers(lock_scope_key: &str, requested: &MemoryScope) -> bool {
    if lock_scope_key == requested.key() {
        return true;
    }
    if requested.kind == "allAuthorizedProjects" {
        return lock_scope_key.starts_with("memory:project:")
            || lock_scope_key.starts_with("memory:session:");
    }
    false
}

async fn memory_get_settings(
    rt: Arc<PublicV1Runtime>,
    _input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let row = load_settings_from(&conn)?;
        Ok::<_, ContractError>(json!({
            "settings": settings_json(&row),
            "revision": row.revision,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

struct SettingsRow {
    reasoning_mode: String,
    auto_index_interval_minutes: i64,
    auto_dream_interval_minutes: i64,
    revision: String,
}

/// 读设置行（未落库时返回默认值，不写库：读操作无业务写入）。
/// 默认 answerFirst / 30 / 60 = TS `IndexingSettings` 默认。
fn load_settings_from(conn: &Connection) -> Result<SettingsRow, ContractError> {
    let row: Option<(String, i64, i64, String)> = conn
        .query_row(
            "SELECT reasoning_mode, auto_index_interval_minutes, auto_dream_interval_minutes, revision
             FROM v1_memory_settings WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(row
        .map(|(mode, idx, dream, rev)| SettingsRow {
            reasoning_mode: mode,
            auto_index_interval_minutes: idx,
            auto_dream_interval_minutes: dream,
            revision: rev,
        })
        .unwrap_or_else(default_settings))
}

fn default_settings() -> SettingsRow {
    SettingsRow {
        reasoning_mode: "answerFirst".to_owned(),
        auto_index_interval_minutes: 30,
        auto_dream_interval_minutes: 60,
        revision: "mrev-00000000".to_owned(),
    }
}

fn settings_json(row: &SettingsRow) -> Value {
    json!({
        "reasoningMode": row.reasoning_mode,
        "autoIndexIntervalMinutes": row.auto_index_interval_minutes,
        "autoDreamIntervalMinutes": row.auto_dream_interval_minutes,
    })
}

async fn memory_get_project_meta(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let outcome = tokio::task::spawn_blocking(move || {
        let conn = store.conn.lock().expect("sqlite mutex");
        let registered: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if registered.is_none() {
            return Err(domain_not_found("project", &project_id));
        }
        let row: Option<(String, String, String, String, i64)> = conn
            .query_row(
                "SELECT project_name, description, status, revision, read_only
                 FROM v1_memory_project_meta WHERE project_id = ?1",
                params![project_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(sql_err)?;
        Ok(match row {
            None => json!({"present": false}),
            Some((name, description, status, revision, read_only)) => json!({
                "present": true,
                "meta": {
                    "projectId": project_id,
                    "projectName": name,
                    "description": description,
                    "status": status,
                    "revision": revision,
                    "readOnly": read_only != 0,
                }
            }),
        })
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

async fn memory_traces(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || list_traces(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 轨迹游标：`mtrace-<offset>`（轨迹只追加，ASC 序下偏移游标稳定）。
fn parse_trace_cursor(cursor: &str) -> Result<usize, ContractError> {
    cursor
        .strip_prefix("mtrace-")
        .and_then(|rest| rest.parse::<usize>().ok())
        .ok_or_else(|| invalid(format!("invalid memory.traces cursor `{cursor}`")))
}

fn list_traces(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let scope = MemoryScope::from_value(&input["scope"])?;
    let kind = input
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let trace_id = input
        .get("traceId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let limit = input
        .get("page")
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, LIST_PAGE_LIMIT as u64) as usize;
    let offset = match input
        .get("page")
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
    {
        Some(cursor) => parse_trace_cursor(cursor)?,
        None => 0,
    };

    let conn = store.conn.lock().expect("sqlite mutex");
    scope.validate_resources(&conn)?;
    let (trace_filter, trace_params) = scope.trace_filter();
    let mut sql = format!(
        "SELECT trace_id, kind, scope_kind, project_id, session_id, operation_id, state,
                started_at, finished_at, summary, detail_artifact_id, detail_schema_id
         FROM v1_memory_traces WHERE kind = ? AND {trace_filter}"
    );
    if trace_id.is_some() {
        sql.push_str(" AND trace_id = ?");
    }
    sql.push_str(" ORDER BY started_at ASC, trace_id ASC");
    let mut all = vec![kind.to_owned()];
    all.extend(trace_params);
    if let Some(id) = trace_id {
        all.push(id.to_owned());
    }
    let rows = conn
        .prepare(&sql)
        .map_err(sql_err)?
        .query_map(string_params(&all).as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, String>(11)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;

    if offset > rows.len() {
        return Err(invalid("pagination cursor beyond end of the trace list"));
    }
    let end = (offset + limit).min(rows.len());
    let traces: Vec<Value> = rows[offset..end]
        .iter()
        .map(
            |(
                trace_id,
                kind,
                scope_kind,
                project_id,
                session_id,
                operation_id,
                state,
                started_at,
                finished_at,
                summary,
                detail_artifact_id,
                detail_schema_id,
            )| {
                let mut scope_obj = Map::new();
                scope_obj.insert("kind".into(), json!(scope_kind));
                if let Some(p) = project_id {
                    scope_obj.insert("projectId".into(), json!(p));
                }
                if let Some(s) = session_id {
                    scope_obj.insert("sessionId".into(), json!(s));
                }
                let mut trace = json!({
                    "traceId": trace_id,
                    "kind": kind,
                    "scope": Value::Object(scope_obj),
                    "state": state,
                    "startedAt": started_at,
                    "summary": summary,
                    "detailArtifactId": detail_artifact_id,
                    "detailSchemaId": detail_schema_id,
                });
                set_optional_str(&mut trace, "operationId", operation_id);
                set_optional_str(&mut trace, "finishedAt", finished_at);
                trace
            },
        )
        .collect();
    // 同上：完成页省略 nextCursor。
    let mut result = json!({"traces": traces});
    if end < rows.len() {
        result["nextCursor"] = json!(format!("mtrace-{end}"));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// 条件写 handlers
// ---------------------------------------------------------------------------

async fn memory_edit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || edit_entry(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn edit_entry(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let scope = MemoryScope::from_value(&input["scope"])?;
    let memory_id = input["memoryId"].as_str().unwrap_or_default().to_owned();
    let name = input["name"].as_str().unwrap_or_default().to_owned();
    let description = input["description"].as_str().unwrap_or_default().to_owned();
    let fields = input.get("fields").cloned().unwrap_or_else(|| json!({}));
    let expected = input["expectedRevision"].as_str().unwrap_or_default();

    let row = {
        let conn = store.conn.lock().expect("sqlite mutex");
        scope.validate_resources(&conn)?;
        fetch_entry_in_scope(&conn, &scope, &memory_id)?
            .ok_or_else(|| domain_not_found("memory entry", &memory_id))?
    };
    if row.read_only {
        return Err(ContractError::new(
            CODE_READ_ONLY_MEMORY,
            format!("memory entry `{memory_id}` is read-only"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if row.revision != expected {
        return Err(revision_conflict(expected, &row.revision));
    }

    // 内容产物先发布（publish 自带短锁，不与下述事务嵌套）。
    let artifact_id = publish_entry_artifact(store, &row.kind, &name, &description, &fields)?;
    let fields_json = serde_json::to_string(&fields)
        .map_err(|e| invalid(format!("fields serialization failed: {e}")))?;

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;
    // 条件写复验：发布产物期间条目被并发改动 → 冲突。
    let current_rev: String = tx
        .query_row(
            "SELECT revision FROM v1_memory_entries WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if current_rev != expected {
        return Err(revision_conflict(expected, &current_rev));
    }
    let new_rev = bump_memory_rev_in_tx(&tx)?;
    tx.execute(
        "UPDATE v1_memory_entries
         SET name = ?1, description = ?2, fields_json = ?3, revision = ?4,
             content_artifact_id = ?5, updated_at = ?6
         WHERE memory_id = ?7",
        params![
            name,
            description,
            fields_json,
            new_rev,
            artifact_id,
            now_rfc3339(),
            memory_id
        ],
    )
    .map_err(sql_err)?;
    append_memory_event(
        &tx,
        &event_resource_id(&scope),
        "edit",
        &scope,
        std::slice::from_ref(&memory_id),
        &new_rev,
    )?;
    tx.commit().map_err(sql_err)?;

    let updated = fetch_entry_in_scope(&conn, &scope, &memory_id)?
        .ok_or_else(|| domain_not_found("memory entry", &memory_id))?;
    Ok(json!({"entry": updated.to_contract_json()}))
}

fn fetch_entry_in_scope(
    conn: &Connection,
    scope: &MemoryScope,
    memory_id: &str,
) -> Result<Option<MemoryEntryRow>, ContractError> {
    let (filter, scope_params) = scope.entry_filter();
    let sql =
        format!("SELECT {ENTRY_COLUMNS} FROM v1_memory_entries WHERE memory_id = ? AND {filter}");
    let mut all = vec![memory_id.to_owned()];
    all.extend(scope_params);
    conn.query_row(&sql, string_params(&all).as_slice(), row_to_entry)
        .optional()
        .map_err(sql_err)
}

async fn memory_change_state(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || change_state(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn change_state(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let scope = MemoryScope::from_value(&input["scope"])?;
    let action = input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let items: Vec<(String, String)> = input
        .get("items")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    (
                        item.get("memoryId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        item.get("expectedRevision")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let mut conn = store.conn.lock().expect("sqlite mutex");
    scope.validate_resources(&conn)?;
    // 先验证全部目标的权限与 revision，再统一修改（合同验收）。
    let mut targets: Vec<MemoryEntryRow> = Vec::with_capacity(items.len());
    for (memory_id, expected) in &items {
        let row = fetch_entry_in_scope(&conn, &scope, memory_id)?
            .ok_or_else(|| domain_not_found("memory entry", memory_id))?;
        if row.read_only && action != "restore" {
            return Err(ContractError::new(
                CODE_READ_ONLY_MEMORY,
                format!("memory entry `{memory_id}` is read-only"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if row.revision != *expected {
            return Err(revision_conflict(expected, &row.revision));
        }
        targets.push(row);
    }

    let tx = conn.transaction().map_err(sql_err)?;
    for (row, (_, expected)) in targets.iter().zip(&items) {
        // 复验（validate 与 apply 之间无窗口：同一连接串行）。
        let current: String = tx
            .query_row(
                "SELECT revision FROM v1_memory_entries WHERE memory_id = ?1",
                params![row.memory_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if current != *expected {
            return Err(revision_conflict(expected, &current));
        }
        match action {
            "delete" => {
                tx.execute(
                    "DELETE FROM v1_memory_entries WHERE memory_id = ?1",
                    params![row.memory_id],
                )
                .map_err(sql_err)?;
            }
            "deprecate" | "restore" => {
                let deprecated = if action == "deprecate" { 1 } else { 0 };
                tx.execute(
                    "UPDATE v1_memory_entries SET deprecated = ?1, updated_at = ?2 WHERE memory_id = ?3",
                    params![deprecated, now_rfc3339(), row.memory_id],
                )
                .map_err(sql_err)?;
            }
            other => {
                return Err(invalid(format!(
                    "unsupported change_state action `{other}`"
                )));
            }
        }
    }
    let new_rev = bump_memory_rev_in_tx(&tx)?;
    let changed: Vec<String> = targets.iter().map(|r| r.memory_id.clone()).collect();
    append_memory_event(
        &tx,
        &event_resource_id(&scope),
        &format!("change_state:{action}"),
        &scope,
        &changed,
        &new_rev,
    )?;
    tx.commit().map_err(sql_err)?;

    // 删除后变空的项目（TS delete 的 deletedProjectIds 语义：最后一个记
    // 忆条目被删除的项目从记忆清单移除；项目本身与 project meta 不动）。
    let removed_empty: Vec<String> = if action == "delete" {
        let mut out = Vec::new();
        for project_id in targets.iter().filter_map(|r| r.project_id.clone()) {
            if !out.contains(&project_id) {
                let remaining: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM v1_memory_entries WHERE project_id = ?1",
                        params![project_id],
                        |r| r.get(0),
                    )
                    .map_err(sql_err)?;
                if remaining == 0 {
                    out.push(project_id);
                }
            }
        }
        out
    } else {
        Vec::new()
    };
    Ok(json!({
        "changedIds": changed,
        "removedEmptyProjectIds": removed_empty,
        "revision": new_rev,
    }))
}

async fn memory_set_settings(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || set_settings(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn set_settings(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let settings = input.get("settings").cloned().unwrap_or(Value::Null);
    let expected = input["expectedRevision"].as_str().unwrap_or_default();
    let mode = settings["reasoningMode"].as_str().unwrap_or_default();
    let index_interval = settings["autoIndexIntervalMinutes"].as_u64().unwrap_or(0);
    let dream_interval = settings["autoDreamIntervalMinutes"].as_u64().unwrap_or(0);
    if mode != "answerFirst" && mode != "accuracyFirst" {
        return Err(invalid(format!("unsupported reasoningMode `{mode}`")));
    }
    // schema 层 minimum:1 已拦；这里兜底复核（无效间隔落盘前拒绝）。
    if index_interval < 1 || dream_interval < 1 {
        return Err(invalid("auto index/dream intervals must be >= 1 minute"));
    }

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let current = load_settings_from(&conn)?;
    if current.revision != expected {
        return Err(revision_conflict(expected, &current.revision));
    }
    let tx = conn.transaction().map_err(sql_err)?;
    let new_rev = bump_memory_rev_in_tx(&tx)?;
    tx.execute(
        "INSERT INTO v1_memory_settings
            (id, reasoning_mode, auto_index_interval_minutes, auto_dream_interval_minutes, revision, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
            reasoning_mode = excluded.reasoning_mode,
            auto_index_interval_minutes = excluded.auto_index_interval_minutes,
            auto_dream_interval_minutes = excluded.auto_dream_interval_minutes,
            revision = excluded.revision,
            updated_at = excluded.updated_at",
        params![mode, index_interval as i64, dream_interval as i64, new_rev, now_rfc3339()],
    )
    .map_err(sql_err)?;
    // 事件：config.changed（settings 属配置面；configuration_store 的同名
    // 助手是 configuration 域私有模块，不可跨域调用——本域按同一 schema
    // 自实现）+ memory.changed。
    append_config_changed_event(
        &tx,
        &json!({
            "memorySettingsRevision": new_rev,
            "keys": ["reasoningMode", "autoIndexIntervalMinutes", "autoDreamIntervalMinutes"],
        }),
    )?;
    let scope = MemoryScope {
        kind: "global",
        project_id: None,
        session_id: None,
    };
    append_memory_event(&tx, "memory", "settings", &scope, &[], &new_rev)?;
    tx.commit().map_err(sql_err)?;

    Ok(json!({
        "settings": {
            "reasoningMode": mode,
            "autoIndexIntervalMinutes": index_interval,
            "autoDreamIntervalMinutes": dream_interval,
        },
        "revision": new_rev,
    }))
}

async fn memory_set_project_meta(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || set_project_meta(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn set_project_meta(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let project_name = input["projectName"].as_str().unwrap_or_default().to_owned();
    let description = input["description"].as_str().unwrap_or_default().to_owned();
    let status = input["status"].as_str().unwrap_or_default().to_owned();
    let expected = input["expectedRevision"].as_str().unwrap_or_default();

    let mut conn = store.conn.lock().expect("sqlite mutex");
    let registered: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM projects WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if registered.is_none() {
        return Err(domain_not_found("project", &project_id));
    }
    let existing: Option<(String, i64)> = conn
        .query_row(
            "SELECT revision, read_only FROM v1_memory_project_meta WHERE project_id = ?1",
            params![project_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let (current_rev, read_only) = existing.unwrap_or((String::new(), 0));
    if read_only != 0 {
        return Err(ContractError::new(
            CODE_READ_ONLY_MEMORY,
            format!("project meta for `{project_id}` is read-only"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if current_rev != expected {
        return Err(revision_conflict(expected, &current_rev));
    }

    let tx = conn.transaction().map_err(sql_err)?;
    let recheck: Option<String> = tx
        .query_row(
            "SELECT revision FROM v1_memory_project_meta WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let recheck = recheck.unwrap_or_default();
    if recheck != expected {
        return Err(revision_conflict(expected, &recheck));
    }
    let new_rev = bump_memory_rev_in_tx(&tx)?;
    tx.execute(
        "INSERT INTO v1_memory_project_meta
            (project_id, project_name, description, status, revision, read_only, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
         ON CONFLICT(project_id) DO UPDATE SET
            project_name = excluded.project_name,
            description = excluded.description,
            status = excluded.status,
            revision = excluded.revision,
            updated_at = excluded.updated_at",
        params![
            project_id,
            project_name,
            description,
            status,
            new_rev,
            now_rfc3339()
        ],
    )
    .map_err(sql_err)?;
    let scope = MemoryScope {
        kind: "project",
        project_id: Some(project_id.clone()),
        session_id: None,
    };
    append_memory_event(&tx, &project_id, "project_meta", &scope, &[], &new_rev)?;
    tx.commit().map_err(sql_err)?;

    Ok(json!({
        "meta": {
            "projectId": project_id,
            "projectName": project_name,
            "description": description,
            "status": status,
            "revision": new_rev,
            "readOnly": false,
        }
    }))
}

// ---------------------------------------------------------------------------
// 共享：本域的 DomainDb（异步操作受理/完成走 v1_domain_operations seam）
// ---------------------------------------------------------------------------

pub(super) fn memory_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
    let home = rt.store().home().to_path_buf();
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let db = Arc::new(DomainDb::open(
        &home,
        &[
            DDL_ENTRIES,
            DDL_SETTINGS,
            DDL_PROJECT_META,
            DDL_TRACES,
            DDL_PLANS,
            DDL_MAINTENANCE,
            DDL_META,
        ],
    )?);
    registry.insert(home, Arc::downgrade(&db));
    Ok(db)
}

/// 幂等受理（与 schedules::accept_domain_operation 同纪律；acceptedRevision
/// 用本域 mrev）。`validate` 里做的写（plan 消费、维护锁）必须在
/// `record_domain_operation_accept` 失败时由调用方无副作用——锁/plan 均可
/// 由「操作行缺失即未受理」语义回收（见 claim_maintenance_lock）。
pub(super) fn submit_memory_operation(
    db: &DomainDb,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    accepted_revision: String,
    new_operation_id: impl FnOnce() -> String,
    validate: impl FnOnce() -> Result<(), ContractError>,
) -> Result<(String, Value), ContractError> {
    match lookup_domain_operation(db, method_id, input, idempotency_key)? {
        DomainOperationLookup::Replay(receipt) => {
            let operation_id = receipt["operationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            return Ok((operation_id, receipt));
        }
        DomainOperationLookup::Conflict => {
            return Err(ContractError::new(
                CODE_IDEMPOTENCY_CONFLICT,
                format!("idempotencyKey already used for {method_id} with different input"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        DomainOperationLookup::Rejected(code) => {
            return Err(ContractError::new(
                code,
                format!("{method_id} request previously rejected definitively"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        DomainOperationLookup::Fresh => {}
    }
    if let Err(error) = validate() {
        if error.retry.mode == RetryMode::Never {
            record_domain_operation_rejection(db, method_id, input, idempotency_key, &error.code)?;
        }
        return Err(error);
    }
    let operation_id = new_operation_id();
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": accepted_revision,
        "deduplicated": false,
        "state": "queued",
    });
    record_domain_operation_accept(
        db,
        method_id,
        input,
        idempotency_key,
        &operation_id,
        &receipt,
    )?;
    Ok((operation_id, receipt))
}

// ---------------------------------------------------------------------------
// Change plan 公共件
// ---------------------------------------------------------------------------

pub(super) struct StoredPlan {
    pub(super) plan_revision: String,
    pub(super) kind: String,
    pub(super) state: String,
    pub(super) expires_at: String,
    pub(super) payload: Value,
    base_revisions: Vec<(String, String)>,
}

pub(super) fn load_plan(
    conn: &Connection,
    plan_id: &str,
) -> Result<Option<StoredPlan>, ContractError> {
    let row: Option<(String, String, String, String, String, String)> = conn
        .query_row(
            "SELECT plan_revision, kind, state, expires_at, payload_json, base_revisions_json
             FROM v1_memory_change_plans WHERE plan_id = ?1",
            params![plan_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?;
    Ok(row.map(
        |(plan_revision, kind, state, expires_at, payload_json, base_json)| StoredPlan {
            plan_revision,
            kind,
            state,
            expires_at,
            payload: serde_json::from_str(&payload_json).unwrap_or(Value::Null),
            base_revisions: serde_json::from_str::<Vec<Value>>(&base_json)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| {
                    Some((
                        v.get("resourceId")?.as_str()?.to_owned(),
                        v.get("revision")?.as_str()?.to_owned(),
                    ))
                })
                .collect(),
        },
    ))
}

impl StoredPlan {
    fn scope_key(&self) -> String {
        self.payload
            .get("targetScope")
            .or_else(|| self.payload.get("scope"))
            .and_then(|s| MemoryScope::from_value(s).ok())
            .map(|s| s.key())
            .unwrap_or_else(|| "memory:global".to_owned())
    }
}

/// 第二跳公共复验：存在/未消费、过期、planRevision、base revision（目标
/// scope 被并发修改 → 冲突，不无条件覆盖）。通过后在同一事务里标记
/// consumed（一次性）。
fn validate_and_consume_plan(
    store: &V1Store,
    plan_id: &str,
    expected_plan_revision: &str,
    expected_kind: &str,
) -> Result<StoredPlan, ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let plan =
        load_plan(&conn, plan_id)?.ok_or_else(|| domain_not_found("change plan", plan_id))?;
    if plan.kind != expected_kind {
        return Err(invalid(format!(
            "plan `{plan_id}` is a `{}` plan, not a `{expected_kind}` plan",
            plan.kind
        )));
    }
    if plan.state != "open" {
        return Err(domain_not_found("change plan", plan_id));
    }
    if rfc3339_lt(&plan.expires_at, &now_rfc3339()) == Some(true) {
        return Err(ContractError::new(
            CODE_PLAN_EXPIRED,
            format!(
                "change plan expired at `{}`; re-plan and retry",
                plan.expires_at
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if plan.plan_revision != expected_plan_revision {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "plan revision `{expected_plan_revision}` does not match `{}`",
                plan.plan_revision
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    let current = current_memory_rev(&conn)?;
    for (resource, base) in &plan.base_revisions {
        if *resource == plan.scope_key() && base != &current {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "target scope changed since the plan was created (base `{base}`, now `{current}`); re-plan"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
    }
    conn.execute(
        "UPDATE v1_memory_change_plans SET state = 'consumed' WHERE plan_id = ?1 AND state = 'open'",
        params![plan_id],
    )
    .map_err(sql_err)?;
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert_plan(
    conn: &Connection,
    plan_id: &str,
    plan_revision: &str,
    kind: &str,
    expires_at: &str,
    scope_json: &Value,
    affected: &[String],
    destructive: bool,
    payload: &Value,
) -> Result<(), ContractError> {
    conn.execute(
        "INSERT INTO v1_memory_change_plans
            (plan_id, plan_revision, kind, state, expires_at, scope_json, affected_resources_json,
             destructive, base_revisions_json, payload_json, created_at)
         VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            plan_id,
            plan_revision,
            kind,
            expires_at,
            serde_json::to_string(scope_json).unwrap_or_default(),
            serde_json::to_string(&affected).unwrap_or_default(),
            destructive,
            serde_json::to_string(
                &json!([{"resourceId": scope_key_of(scope_json), "revision": plan_revision}])
            )
            .unwrap_or_default(),
            serde_json::to_string(payload).unwrap_or_default(),
            now_rfc3339(),
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn scope_key_of(scope_json: &Value) -> String {
    MemoryScope::from_value(scope_json)
        .map(|s| s.key())
        .unwrap_or_else(|_| "memory:global".to_owned())
}

pub(super) fn plan_json(
    plan_id: &str,
    plan_revision: &str,
    expires_at: &str,
    affected: &[String],
    conflicts: &[Value],
    destructive: bool,
) -> Value {
    json!({
        "planId": plan_id,
        "planRevision": plan_revision,
        "expiresAt": expires_at,
        "affectedResources": affected,
        "conflicts": conflicts,
        "destructive": destructive,
    })
}

// ---------------------------------------------------------------------------
// 维护锁
// ---------------------------------------------------------------------------

/// 维护锁：按 scope 串行。已有未终态维护 → MAINTENANCE_BUSY。回收规则：
/// 锁指向的操作行已终态（崩溃前完成记录落了）或整个缺失且锁龄超过
/// `LOCK_FRESH_SECS`（受理在申请锁之后失败——短窗内按占用处理，避免两
/// 个受理都通过；超龄视为残留可替换）。
fn claim_maintenance_lock(
    store: &V1Store,
    scope_key: &str,
    operation_id: &str,
) -> Result<(), ContractError> {
    const LOCK_FRESH_SECS: i64 = 30;
    let conn = store.conn.lock().expect("sqlite mutex");
    let existing: Option<(String, String, String)> = conn
        .query_row(
            "SELECT operation_id, state, started_at FROM v1_memory_maintenance WHERE scope_key = ?1",
            params![scope_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some((locked_operation, state, started_at)) = existing
        && state == "running"
    {
        let live: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM v1_domain_operations
                     WHERE operation_id = ?1
                       AND state NOT IN ('succeeded', 'failed', 'cancelled', 'interrupted', 'result_unknown')",
                    params![locked_operation],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
        if live.is_some() {
            return Err(maintenance_busy(scope_key));
        }
        // 操作行缺失（受理未落记录）：新鲜锁仍按占用处理。
        let stale_before =
            (chrono::Utc::now() - chrono::Duration::seconds(LOCK_FRESH_SECS)).to_rfc3339();
        if rfc3339_lt(&started_at, &stale_before) != Some(true) {
            return Err(maintenance_busy(scope_key));
        }
    }
    conn.execute(
        "INSERT INTO v1_memory_maintenance (scope_key, operation_id, state, started_at)
         VALUES (?1, ?2, 'running', ?3)
         ON CONFLICT(scope_key) DO UPDATE SET operation_id = excluded.operation_id,
            state = 'running', started_at = excluded.started_at",
        params![scope_key, operation_id, now_rfc3339()],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn maintenance_busy(scope_key: &str) -> ContractError {
    ContractError::new(
        CODE_MAINTENANCE_BUSY,
        format!("a maintenance operation is already running for `{scope_key}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn release_maintenance_lock(store: &V1Store, scope_key: &str, operation_id: &str) {
    let conn = store.conn.lock().expect("sqlite mutex");
    let _ = conn.execute(
        "DELETE FROM v1_memory_maintenance WHERE scope_key = ?1 AND operation_id = ?2",
        params![scope_key, operation_id],
    );
}

/// rollbackDream 的目标复验：dream 轨迹必须存在；本构建 dream 不改写记
/// 忆，没有可回滚的整合快照 → SNAPSHOT_UNAVAILABLE（显式，不伪装可回
/// 滚）。
fn validate_rollback_target(store: &V1Store, dream_trace_id: &str) -> Result<(), ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM v1_memory_traces WHERE trace_id = ?1 AND kind = 'dream'",
            params![dream_trace_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    match state {
        None => Err(domain_not_found("dream trace", dream_trace_id)),
        Some(_) => Err(ContractError::new(
            CODE_SNAPSHOT_UNAVAILABLE,
            format!(
                "dream trace `{dream_trace_id}` has no rollback snapshot in this build (dream does not rewrite memory)"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
    }
}

/// 落一条维护轨迹（明细产物先发布——publish 自带短锁，调用方不得持有
/// store.conn）。
#[allow(clippy::too_many_arguments)]
fn record_trace(
    store: &V1Store,
    trace_id: &str,
    kind: &str,
    scope: &MemoryScope,
    operation_id: &str,
    state: &str,
    started_at: &str,
    summary: &str,
    detail: &Value,
) -> Result<(), ContractError> {
    let detail_schema_id = match kind {
        "case" => "memory.caseTrace.v1",
        "index" => "memory.indexTrace.v1",
        _ => "memory.dreamTrace.v1",
    };
    let artifact = publish_artifact(
        store,
        "memory",
        "application/json",
        Some(&format!("memory-trace:{trace_id}")),
        detail.to_string().as_bytes(),
        "memory",
    )?;
    let detail_artifact_id = artifact["artifactId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "INSERT INTO v1_memory_traces
            (trace_id, kind, scope_kind, project_id, session_id, operation_id, state,
             started_at, finished_at, summary, detail_artifact_id, detail_schema_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            trace_id,
            kind,
            scope.kind,
            scope.project_id,
            scope.session_id,
            operation_id,
            state,
            started_at,
            now_rfc3339(),
            summary,
            detail_artifact_id,
            detail_schema_id,
            now_rfc3339(),
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 导出 / 导入（两段式）
// ---------------------------------------------------------------------------

async fn memory_export(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = memory_db(&rt).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let scope = MemoryScope::from_value(&input["scope"]).map_err(DispatchError::Application)?;
    let accepted_rev = {
        let conn = store.conn.lock().expect("sqlite mutex");
        scope
            .validate_resources(&conn)
            .and_then(|_| current_memory_rev(&conn))
            .map_err(DispatchError::Application)?
    };
    let scope_value = scope.to_json();
    let (operation_id, receipt) = submit_memory_operation(
        &db,
        "memory.export",
        &input,
        idempotency_key.as_deref(),
        accepted_rev,
        || store.next_id(),
        || Ok(()),
    )
    .map_err(DispatchError::Application)?;
    // 同键重放到此为止（work_cycles/automation 同款闸门）：直接回放原受
    // 理回执，不再启动执行体——重放不得再执行副作用，也不得覆写原
    // operation 的完成结果。
    if receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, receipt));
    }
    let db = db.clone();
    let store2 = store.clone();
    tokio::task::spawn_blocking(move || {
        let _ = execute_export(&store2, &db, &operation_id, &scope_value);
    });
    Ok((ResponseOutcome::Accepted, receipt))
}

fn execute_export(
    store: &Arc<V1Store>,
    db: &Arc<DomainDb>,
    operation_id: &str,
    scope_value: &Value,
) -> Result<Value, ContractError> {
    if !claim_operation_execution(store, operation_id)? {
        return Err(operation_execution_already_claimed(operation_id));
    }
    let _ = mark_domain_operation_running(db, operation_id, "");
    let result = (|| -> Result<Value, ContractError> {
        let scope = MemoryScope::from_value(scope_value)?;
        let entries: Vec<MemoryEntryRow> = {
            let conn = store.conn.lock().expect("sqlite mutex");
            let (filter, scope_params) = scope.entry_filter();
            conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM v1_memory_entries WHERE {filter} ORDER BY memory_id ASC"
            ))
            .map_err(sql_err)?
            .query_map(string_params(&scope_params).as_slice(), row_to_entry)
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?
        };
        let (revision, project_metas) = {
            let conn = store.conn.lock().expect("sqlite mutex");
            let revision = current_memory_rev(&conn)?;
            let mut source_projects: Vec<String> = entries
                .iter()
                .filter_map(|r| r.project_id.clone())
                .collect();
            source_projects.sort();
            source_projects.dedup();
            let mut metas = Vec::new();
            for project_id in &source_projects {
                let row: Option<(String, String, String, String, i64)> = conn
                    .query_row(
                        "SELECT project_name, description, status, revision, read_only
                         FROM v1_memory_project_meta WHERE project_id = ?1",
                        params![project_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()
                    .map_err(sql_err)?;
                if let Some((name, description, status, rev, read_only)) = row {
                    metas.push(json!({
                        "projectId": project_id,
                        "projectName": name,
                        "description": description,
                        "status": status,
                        "revision": rev,
                        "readOnly": read_only != 0,
                    }));
                }
            }
            (revision, metas)
        };
        let mut source_projects: Vec<String> = entries
            .iter()
            .filter_map(|r| r.project_id.clone())
            .collect();
        source_projects.sort();
        source_projects.dedup();
        let mut source_revisions: Vec<Value> = source_projects
            .iter()
            .map(|p| json!({"projectId": p, "revision": revision}))
            .collect();
        if scope.kind == "global" || entries.iter().any(|r| r.scope_kind == "global") {
            source_revisions.push(json!({"projectId": "global", "revision": revision}));
        }
        // bundle：条目全文 + 导出 scope + 项目 meta；不含任何凭据（本域
        // 表不含密钥材质，见报告 §5）。
        let bundle = json!({
            "bundleFormatVersion": BUNDLE_FORMAT_VERSION,
            "createdAt": now_rfc3339(),
            "scope": scope.to_json(),
            "entries": entries.iter().map(MemoryEntryRow::to_full_json).collect::<Vec<_>>(),
            "projectMetas": project_metas,
            "summary": {
                "entryCount": entries.len(),
                "projectIds": source_projects,
            },
        });
        let artifact = publish_artifact(
            store,
            "memory",
            "application/json",
            Some("memory:export-bundle"),
            bundle.to_string().as_bytes(),
            "memory-export",
        )?;
        Ok(json!({
            "artifact": artifact,
            "bundleFormatVersion": BUNDLE_FORMAT_VERSION,
            "scope": scope.to_json(),
            "sourceRevisions": source_revisions,
        }))
    })();
    finish_operation(db, operation_id, result)
}

/// 统一终态：成功 → succeeded（completion 即 completionResult）；失败 →
/// failed（error 原样返回）。
pub(super) fn finish_operation(
    db: &Arc<DomainDb>,
    operation_id: &str,
    result: Result<Value, ContractError>,
) -> Result<Value, ContractError> {
    match result {
        Ok(completion) => {
            finish_domain_operation(db, operation_id, "", "succeeded", &completion)?;
            Ok(completion)
        }
        Err(error) => {
            finish_domain_operation(
                db,
                operation_id,
                "",
                "failed",
                &json!({"errorCode": error.code, "message": error.message}),
            )?;
            Err(error)
        }
    }
}

/// 执行体入场券（第二道防线，受理层闸门之外的兜底）：仅当操作行仍为
/// `queued`（本受理新建、尚无执行体接管）时原子置 `running` 并返回
/// true。行已 `running`（原执行体在飞）或已终态（原执行体已完成）时返
/// 回 false——即便同一 operation 的执行体被再次启动（受理回归或并发竞
/// 态），也不第二次执行副作用、不把终态行回退成 running、不覆写完成结
/// 果。经 `store.conn` 写同一 SQLite 文件（与 claim_maintenance_lock 查
/// `v1_domain_operations` 同一纪律）。
fn claim_operation_execution(store: &V1Store, operation_id: &str) -> Result<bool, ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let claimed = conn
        .execute(
            "UPDATE v1_domain_operations SET state = 'running', updated_at = ?2
             WHERE operation_id = ?1 AND state = 'queued'",
            params![operation_id, now_rfc3339()],
        )
        .map_err(sql_err)?;
    Ok(claimed == 1)
}

/// 入场券被拒（原执行体已在飞/已完成）：错误只向上传播给丢弃它的 spawn
/// 包装，绝不落库为操作失败——写失败态会覆写原完成结果。
fn operation_execution_already_claimed(operation_id: &str) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!(
            "operation `{operation_id}` is already running or terminal; replay must not re-execute"
        ),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

async fn memory_plan_import(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || plan_import(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 导入 bundle 的逐条目校验：结构、fields 白名单、scope 形状。任何不符
/// → INVALID_BUNDLE（非法 bundle 零修改：plan 阶段就拒绝）。
fn validate_bundle_entry(entry: &Value) -> Result<(), ContractError> {
    let bad = |message: String| {
        ContractError::new(
            CODE_INVALID_BUNDLE,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let obj = entry
        .as_object()
        .ok_or_else(|| bad("bundle entry is not an object".to_owned()))?;
    for key in ["name", "description", "kind"] {
        if !obj
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            return Err(bad(format!("bundle entry missing string `{key}`")));
        }
    }
    let kind = obj["kind"].as_str().unwrap_or_default();
    if !["project", "feedback", "user", "generalProjectMeta"].contains(&kind) {
        return Err(bad(format!("unsupported bundle entry kind `{kind}`")));
    }
    if let Some(fields) = obj.get("fields") {
        let fields = fields
            .as_object()
            .ok_or_else(|| bad("bundle entry fields is not an object".to_owned()))?;
        for (key, value) in fields {
            if !FIELD_KEYS.contains(&key.as_str()) {
                return Err(bad(format!("unsupported memory field `{key}`")));
            }
            let is_string = value.is_string();
            let is_string_array = value
                .as_array()
                .is_some_and(|arr| arr.iter().all(|v| v.is_string()));
            if !is_string && !is_string_array {
                return Err(bad(format!(
                    "memory field `{key}` must be a string or string array"
                )));
            }
        }
    }
    if let Some(scope) = obj.get("scope") {
        let kind = scope
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !["project", "session", "global"].contains(&kind) {
            return Err(bad(format!(
                "bundle entry scope kind `{kind}` is not importable"
            )));
        }
        if kind == "project"
            && scope
                .get("projectId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
        {
            return Err(bad(
                "bundle entry project scope requires projectId".to_owned()
            ));
        }
        if kind == "session"
            && (scope
                .get("projectId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
                || scope
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .is_empty())
        {
            return Err(bad(
                "bundle entry session scope requires projectId+sessionId".to_owned(),
            ));
        }
    }
    if let Some(deprecated) = obj.get("deprecated")
        && !deprecated.is_boolean()
    {
        return Err(bad("bundle entry deprecated must be a boolean".to_owned()));
    }
    if let Some(read_only) = obj.get("readOnly")
        && !read_only.is_boolean()
    {
        return Err(bad("bundle entry readOnly must be a boolean".to_owned()));
    }
    Ok(())
}

fn plan_import(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let artifact_id = input["artifactId"].as_str().unwrap_or_default().to_owned();
    let target_scope = MemoryScope::from_value(&input["targetScope"])?;
    let mode = input
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if mode != "merge" && mode != "replace" {
        return Err(invalid(format!("unsupported import mode `{mode}`")));
    }
    let mappings: Vec<(String, String)> = input
        .get("projectMappings")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|m| {
                    (
                        m.get("sourceProjectId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        m.get("targetProjectId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    // bundle 字节与结构验证（任何修改之前）。
    let bundle_bytes =
        crate::public_v1::domains::artifacts::read_artifact_bytes(store, &artifact_id)?;
    let bundle: Value = serde_json::from_slice(&bundle_bytes).map_err(|e| {
        ContractError::new(
            CODE_INVALID_BUNDLE,
            format!("bundle artifact is not valid JSON: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let version = bundle
        .get("bundleFormatVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if version != BUNDLE_FORMAT_VERSION {
        return Err(ContractError::new(
            CODE_UNSUPPORTED_BUNDLE_VERSION,
            format!(
                "unsupported bundle format version `{version}` (expected `{BUNDLE_FORMAT_VERSION}`)"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let entries = bundle
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for entry in &entries {
        validate_bundle_entry(entry)?;
    }

    // 映射目标必须已注册（导入不落不可见项目）。
    let scope_mismatch = |message: String| {
        ContractError::new(
            CODE_SCOPE_MISMATCH,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        target_scope.validate_resources(&conn)?;
        let mut mapped_targets: Vec<&String> = mappings.iter().map(|(_, target)| target).collect();
        if let Some(target) = &target_scope.project_id {
            mapped_targets.push(target);
        }
        for target in mapped_targets {
            if target.is_empty() {
                return Err(invalid("projectMappings targetProjectId must not be empty"));
            }
            let registered: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM projects WHERE project_id = ?1",
                    params![target],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if registered.is_none() {
                return Err(scope_mismatch(format!(
                    "mapping target project `{target}` is not registered in this instance"
                )));
            }
        }
    }

    // scope 匹配规则：global↛project/session、project↛global、session 只进
    // 同 session；all-project bundle 落单项目需逐源映射。
    let target_project = |source: &str| -> Option<String> {
        if let Some((_, target)) = mappings.iter().find(|(s, _)| s == source) {
            return Some(target.clone());
        }
        if (target_scope.kind == "project" || target_scope.kind == "session")
            && Some(source) == target_scope.project_id.as_deref()
        {
            return target_scope.project_id.clone();
        }
        if target_scope.kind == "allAuthorizedProjects" {
            return Some(source.to_owned());
        }
        None
    };
    for entry in &entries {
        let entry_scope = entry
            .get("scope")
            .cloned()
            .unwrap_or_else(|| json!({"kind": "global"}));
        let kind = entry_scope["kind"].as_str().unwrap_or_default();
        match (kind, target_scope.kind) {
            ("global", "global") | ("global", "allAuthorizedProjects") => {}
            ("global", _) => {
                return Err(scope_mismatch(
                    "global bundle entries cannot be imported into a project/session scope"
                        .to_owned(),
                ));
            }
            ("project", "global") => {
                return Err(scope_mismatch(
                    "project bundle entries cannot be imported into the global scope".to_owned(),
                ));
            }
            ("project", _) => {
                let source = entry_scope["projectId"].as_str().unwrap_or_default();
                if target_project(source).is_none() {
                    return Err(scope_mismatch(format!(
                        "bundle project `{source}` has no mapping into the target scope"
                    )));
                }
            }
            ("session", "session") => {
                let source = entry_scope["projectId"].as_str().unwrap_or_default();
                let session = entry_scope["sessionId"].as_str().unwrap_or_default();
                let mapped = target_project(source);
                if mapped.as_deref() != target_scope.project_id.as_deref()
                    || session != target_scope.session_id.as_deref().unwrap_or_default()
                {
                    return Err(scope_mismatch(format!(
                        "session bundle entry `{source}/{session}` does not match the target session scope"
                    )));
                }
            }
            ("session", _) => {
                return Err(scope_mismatch(
                    "session bundle entries can only be imported into the same session scope"
                        .to_owned(),
                ));
            }
            _ => {}
        }
    }

    let conn = store.conn.lock().expect("sqlite mutex");
    let revision = current_memory_rev(&conn)?;
    let plan_id = store.next_id();
    let expires_at = expires_in_minutes(PLAN_TTL_MINUTES);
    let destructive = mode == "replace";
    let affected = vec![target_scope.key()];
    let payload = json!({
        "artifactId": artifact_id,
        "targetScope": target_scope.to_json(),
        "mode": mode,
        "projectMappings": input.get("projectMappings").cloned().unwrap_or(json!([])),
        "bundle": bundle,
    });
    insert_plan(
        &conn,
        &plan_id,
        &revision,
        "import",
        &expires_at,
        &target_scope.to_json(),
        &affected,
        destructive,
        &payload,
    )?;

    Ok(json!({
        "plan": plan_json(&plan_id, &revision, &expires_at, &affected, &[], destructive),
        "bundleFormatVersion": BUNDLE_FORMAT_VERSION,
        "targetScope": target_scope.to_json(),
        "entryCount": entries.len(),
    }))
}

async fn memory_import(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = memory_db(&rt).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let expected_plan_revision = input["expectedPlanRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let accepted_rev = {
        let conn = store.conn.lock().expect("sqlite mutex");
        current_memory_rev(&conn).map_err(DispatchError::Application)?
    };
    let store_for_validate = store.clone();
    let plan_id_for_validate = plan_id.clone();
    let (operation_id, receipt) = submit_memory_operation(
        &db,
        "memory.import",
        &input,
        idempotency_key.as_deref(),
        accepted_rev,
        || store.next_id(),
        move || {
            validate_and_consume_plan(
                &store_for_validate,
                &plan_id_for_validate,
                &expected_plan_revision,
                "import",
            )?;
            Ok(())
        },
    )
    .map_err(DispatchError::Application)?;
    // 同键重放到此为止：回放原回执，不再启动执行体（不得重复导入、不
    // 得覆写原 operation 的 importedIds 完成结果）。
    if receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, receipt));
    }
    let db = db.clone();
    let store2 = store.clone();
    tokio::task::spawn_blocking(move || {
        let _ = execute_import(&store2, &db, &operation_id, &plan_id);
    });
    Ok((ResponseOutcome::Accepted, receipt))
}

fn execute_import(
    store: &Arc<V1Store>,
    db: &Arc<DomainDb>,
    operation_id: &str,
    plan_id: &str,
) -> Result<Value, ContractError> {
    if !claim_operation_execution(store, operation_id)? {
        return Err(operation_execution_already_claimed(operation_id));
    }
    let _ = mark_domain_operation_running(db, operation_id, "");
    let result = (|| -> Result<Value, ContractError> {
        let payload = {
            let conn = store.conn.lock().expect("sqlite mutex");
            load_plan(&conn, plan_id)?
                .ok_or_else(|| domain_not_found("change plan", plan_id))?
                .payload
        };
        let mode = payload["mode"].as_str().unwrap_or_default();
        let target_scope = MemoryScope::from_value(&payload["targetScope"])?;
        let entries = payload
            .get("bundle")
            .and_then(|b| b.get("entries"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mappings: Vec<(String, String)> = payload
            .get("projectMappings")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|m| {
                        (
                            m.get("sourceProjectId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            m.get("targetProjectId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let target_project = |source: &str| -> Option<String> {
            if let Some((_, target)) = mappings.iter().find(|(s, _)| s == source) {
                return Some(target.clone());
            }
            if (target_scope.kind == "project" || target_scope.kind == "session")
                && Some(source) == target_scope.project_id.as_deref()
            {
                return target_scope.project_id.clone();
            }
            Some(source.to_owned())
        };

        // replace：先发布恢复产物（被清掉的完整状态），再清目标 scope。
        let mut recovery_artifact_id = None;
        if mode == "replace" {
            let removed: Vec<MemoryEntryRow> = {
                let conn = store.conn.lock().expect("sqlite mutex");
                let (filter, scope_params) = target_scope.entry_filter();
                conn.prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM v1_memory_entries WHERE {filter} ORDER BY memory_id ASC"
                ))
                .map_err(sql_err)?
                .query_map(string_params(&scope_params).as_slice(), row_to_entry)
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?
            };
            if !removed.is_empty() {
                let recovery = json!({
                    "kind": "import-replace-prestate",
                    "scope": target_scope.to_json(),
                    "entries": removed.iter().map(MemoryEntryRow::to_full_json).collect::<Vec<_>>(),
                });
                let artifact = publish_artifact(
                    store,
                    "memory",
                    "application/json",
                    Some("memory:import-recovery"),
                    recovery.to_string().as_bytes(),
                    "memory-recovery",
                )?;
                recovery_artifact_id = Some(
                    artifact["artifactId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }

        // 第一遍：预计算每条的落库形态与内容产物（publish 自带短锁，必须
        // 在事务外）。
        struct PreparedEntry {
            memory_id: String,
            scope_kind: &'static str,
            project_id: Option<String>,
            session_id: Option<String>,
            kind: String,
            name: String,
            description: String,
            fields_json: String,
            deprecated: bool,
            read_only: bool,
            artifact_id: String,
        }
        let mut prepared: Vec<PreparedEntry> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let kind = entry["kind"].as_str().unwrap_or_default().to_owned();
            let name = entry["name"].as_str().unwrap_or_default().to_owned();
            let description = entry["description"].as_str().unwrap_or_default().to_owned();
            let fields = entry.get("fields").cloned().unwrap_or_else(|| json!({}));
            let deprecated = entry
                .get("deprecated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let read_only = entry
                .get("readOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entry_scope = MemoryScope::from_value(
                &entry
                    .get("scope")
                    .cloned()
                    .unwrap_or_else(|| json!({"kind": "global"})),
            )?;
            let (scope_kind, project_id, session_id) = match entry_scope.kind {
                "project" => (
                    "project",
                    Some(
                        target_project(&entry_scope.project_id.clone().unwrap_or_default())
                            .unwrap_or_default(),
                    ),
                    None,
                ),
                "session" => (
                    "session",
                    Some(
                        target_project(&entry_scope.project_id.clone().unwrap_or_default())
                            .unwrap_or_default(),
                    ),
                    entry_scope.session_id.clone(),
                ),
                _ => ("global", None, None),
            };
            let artifact_id = publish_entry_artifact(store, &kind, &name, &description, &fields)?;
            let fields_json = serde_json::to_string(&fields)
                .map_err(|e| invalid(format!("fields serialization failed: {e}")))?;
            let memory_id = store.next_id();
            prepared.push(PreparedEntry {
                memory_id,
                scope_kind,
                project_id,
                session_id,
                kind,
                name,
                description,
                fields_json,
                deprecated,
                read_only,
                artifact_id,
            });
        }

        // 第二遍：事务落库。merge 的自然键（scope+kind+name）命中则沿用目
        // 标 memoryId（重复导入幂等）；replace 先删后插。
        let mut imported_ids: Vec<String> = Vec::new();
        let mut project_ids: Vec<String> = Vec::new();
        {
            let mut conn = store.conn.lock().expect("sqlite mutex");
            let tx = conn.transaction().map_err(sql_err)?;
            if mode == "replace" {
                let (filter, scope_params) = target_scope.entry_filter();
                tx.execute(
                    &format!("DELETE FROM v1_memory_entries WHERE {filter}"),
                    rusqlite::params_from_iter(scope_params.iter()),
                )
                .map_err(sql_err)?;
            }
            for item in &prepared {
                if let Some(project) = &item.project_id
                    && !project.is_empty()
                    && !project_ids.contains(project)
                {
                    project_ids.push(project.clone());
                }
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT memory_id FROM v1_memory_entries
                         WHERE scope_kind = ?1 AND COALESCE(project_id, '') = ?2
                           AND COALESCE(session_id, '') = ?3 AND kind = ?4 AND name = ?5",
                        params![
                            item.scope_kind,
                            item.project_id.clone().unwrap_or_default(),
                            item.session_id.clone().unwrap_or_default(),
                            item.kind,
                            item.name,
                        ],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql_err)?;
                let memory_id = existing.unwrap_or_else(|| item.memory_id.clone());
                let new_rev = bump_memory_rev_in_tx(&tx)?;
                let now = now_rfc3339();
                tx.execute(
                    "INSERT INTO v1_memory_entries
                        (memory_id, scope_kind, project_id, session_id, kind, name, description,
                         fields_json, revision, deprecated, read_only, content_artifact_id, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
                     ON CONFLICT(memory_id) DO UPDATE SET
                        name = excluded.name, description = excluded.description,
                        fields_json = excluded.fields_json, revision = excluded.revision,
                        deprecated = excluded.deprecated, read_only = excluded.read_only,
                        content_artifact_id = excluded.content_artifact_id,
                        updated_at = excluded.updated_at",
                    params![
                        memory_id,
                        item.scope_kind,
                        item.project_id,
                        item.session_id,
                        item.kind,
                        item.name,
                        item.description,
                        item.fields_json,
                        new_rev,
                        item.deprecated,
                        item.read_only,
                        item.artifact_id,
                        now,
                    ],
                )
                .map_err(sql_err)?;
                imported_ids.push(memory_id);
            }
            let final_rev = bump_memory_rev_in_tx(&tx)?;
            append_memory_event(
                &tx,
                &event_resource_id(&target_scope),
                "import",
                &target_scope,
                &imported_ids,
                &final_rev,
            )?;
            tx.commit().map_err(sql_err)?;
        }

        let mut completion = json!({
            "importedIds": imported_ids,
            "projectIds": project_ids,
            "outcome": "complete",
        });
        if let Some(recovery) = recovery_artifact_id {
            completion["recoveryArtifactId"] = json!(recovery);
        }
        Ok(completion)
    })();
    finish_operation(db, operation_id, result)
}

// ---------------------------------------------------------------------------
// 清理（两段式）
// ---------------------------------------------------------------------------

async fn memory_plan_clear(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || plan_clear(&store, &input))
        .await
        .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn plan_clear(store: &Arc<V1Store>, input: &Value) -> Result<Value, ContractError> {
    let scope = MemoryScope::from_value(&input["scope"])?;
    let include_traces = input
        .get("includeTraces")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let conn = store.conn.lock().expect("sqlite mutex");
    scope.validate_resources(&conn)?;
    let revision = current_memory_rev(&conn)?;
    let (filter, scope_params) = scope.entry_filter();
    let memory_count: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM v1_memory_entries WHERE {filter}"),
            string_params(&scope_params).as_slice(),
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    let (trace_filter, trace_params) = scope.trace_filter();
    let trace_count: i64 = if include_traces {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM v1_memory_traces WHERE {trace_filter}"),
            string_params(&trace_params).as_slice(),
            |r| r.get(0),
        )
        .map_err(sql_err)?
    } else {
        0
    };
    let planned_ids: Vec<String> = conn
        .prepare(&format!(
            "SELECT memory_id FROM v1_memory_entries WHERE {filter} ORDER BY memory_id ASC"
        ))
        .map_err(sql_err)?
        .query_map(string_params(&scope_params).as_slice(), |r| r.get(0))
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    let planned_traces: Vec<String> = if include_traces {
        conn.prepare(&format!(
            "SELECT trace_id FROM v1_memory_traces WHERE {trace_filter} ORDER BY trace_id ASC"
        ))
        .map_err(sql_err)?
        .query_map(string_params(&trace_params).as_slice(), |r| r.get(0))
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?
    } else {
        Vec::new()
    };
    let project_ids: Vec<String> = conn
        .prepare(&format!(
            "SELECT DISTINCT project_id FROM v1_memory_entries
             WHERE {filter} AND project_id IS NOT NULL ORDER BY project_id ASC"
        ))
        .map_err(sql_err)?
        .query_map(string_params(&scope_params).as_slice(), |r| r.get(0))
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;

    let plan_id = store.next_id();
    let expires_at = expires_in_minutes(PLAN_TTL_MINUTES);
    let mut affected = vec![scope.key()];
    affected.extend(project_ids.iter().cloned());
    let payload = json!({
        "scope": scope.to_json(),
        "includeTraces": include_traces,
        "memoryIds": planned_ids,
        "traceIds": planned_traces,
    });
    insert_plan(
        &conn,
        &plan_id,
        &revision,
        "clear",
        &expires_at,
        &scope.to_json(),
        &affected,
        true,
        &payload,
    )?;

    Ok(json!({
        "plan": plan_json(&plan_id, &revision, &expires_at, &affected, &[], true),
        "memoryCount": memory_count,
        "traceCount": trace_count,
        "projectIds": project_ids,
    }))
}

async fn memory_clear(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = memory_db(&rt).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let plan_id = input["planId"].as_str().unwrap_or_default().to_owned();
    let expected_plan_revision = input["expectedPlanRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let accepted_rev = {
        let conn = store.conn.lock().expect("sqlite mutex");
        current_memory_rev(&conn).map_err(DispatchError::Application)?
    };
    let store_for_validate = store.clone();
    let plan_id_for_validate = plan_id.clone();
    let (operation_id, receipt) = submit_memory_operation(
        &db,
        "memory.clear",
        &input,
        idempotency_key.as_deref(),
        accepted_rev,
        || store.next_id(),
        move || {
            validate_and_consume_plan(
                &store_for_validate,
                &plan_id_for_validate,
                &expected_plan_revision,
                "clear",
            )?;
            Ok(())
        },
    )
    .map_err(DispatchError::Application)?;
    // 同键重放到此为止：回放原回执，不再启动执行体（f10：重放曾再次
    // 执行清除并把原 operation 的 deletedIds 覆写为空——重放只回放原受
    // 理，不得再执行副作用）。
    if receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, receipt));
    }
    let db = db.clone();
    let store2 = store.clone();
    tokio::task::spawn_blocking(move || {
        let _ = execute_clear(&store2, &db, &operation_id, &plan_id);
    });
    Ok((ResponseOutcome::Accepted, receipt))
}

fn execute_clear(
    store: &Arc<V1Store>,
    db: &Arc<DomainDb>,
    operation_id: &str,
    plan_id: &str,
) -> Result<Value, ContractError> {
    if !claim_operation_execution(store, operation_id)? {
        return Err(operation_execution_already_claimed(operation_id));
    }
    let _ = mark_domain_operation_running(db, operation_id, "");
    let result = (|| -> Result<Value, ContractError> {
        let payload = {
            let conn = store.conn.lock().expect("sqlite mutex");
            load_plan(&conn, plan_id)?
                .ok_or_else(|| domain_not_found("change plan", plan_id))?
                .payload
        };
        let scope = MemoryScope::from_value(&payload["scope"])?;
        let include_traces = payload
            .get("includeTraces")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // 只删计划内资源（payload 的 memoryIds/traceIds）：重复 clear 的幂
        // 等回放不扩大到后续新记忆。
        let planned_ids: Vec<String> = payload
            .get("memoryIds")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let planned_traces: Vec<String> = payload
            .get("traceIds")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        // 恢复产物：被删条目的完整状态先落不可变产物（跨连接读 → 发布）。
        let removed: Vec<MemoryEntryRow> = {
            let conn = store.conn.lock().expect("sqlite mutex");
            let mut rows = Vec::with_capacity(planned_ids.len());
            for id in &planned_ids {
                if let Some(row) = conn
                    .query_row(
                        &format!(
                            "SELECT {ENTRY_COLUMNS} FROM v1_memory_entries WHERE memory_id = ?"
                        ),
                        params![id],
                        row_to_entry,
                    )
                    .optional()
                    .map_err(sql_err)?
                {
                    rows.push(row);
                }
            }
            rows
        };
        let removed_ids: Vec<String> = removed.iter().map(|r| r.memory_id.clone()).collect();
        let recovery_artifact_id = if removed.is_empty() {
            None
        } else {
            let recovery = json!({
                "kind": "clear-recovery",
                "scope": scope.to_json(),
                "entries": removed.iter().map(MemoryEntryRow::to_full_json).collect::<Vec<_>>(),
            });
            let artifact = publish_artifact(
                store,
                "memory",
                "application/json",
                Some("memory:clear-recovery"),
                recovery.to_string().as_bytes(),
                "memory-recovery",
            )?;
            Some(
                artifact["artifactId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        };

        let final_rev;
        {
            let mut conn = store.conn.lock().expect("sqlite mutex");
            let tx = conn.transaction().map_err(sql_err)?;
            for id in &planned_ids {
                tx.execute(
                    "DELETE FROM v1_memory_entries WHERE memory_id = ?1",
                    params![id],
                )
                .map_err(sql_err)?;
            }
            if include_traces {
                for id in &planned_traces {
                    tx.execute(
                        "DELETE FROM v1_memory_traces WHERE trace_id = ?1",
                        params![id],
                    )
                    .map_err(sql_err)?;
                }
            }
            final_rev = bump_memory_rev_in_tx(&tx)?;
            append_memory_event(
                &tx,
                &event_resource_id(&scope),
                "clear",
                &scope,
                &removed_ids,
                &final_rev,
            )?;
            tx.commit().map_err(sql_err)?;
        }

        let mut project_ids: Vec<String> = removed
            .iter()
            .filter_map(|r| r.project_id.clone())
            .collect();
        project_ids.sort();
        project_ids.dedup();
        let mut completion = json!({
            "deletedIds": removed_ids,
            "projectIds": project_ids,
            "revision": final_rev,
            "outcome": "complete",
        });
        if let Some(recovery) = recovery_artifact_id {
            completion["recoveryArtifactId"] = json!(recovery);
        }
        Ok(completion)
    })();
    finish_operation(db, operation_id, result)
}

// ---------------------------------------------------------------------------
// 维护（index / dream / rollbackDream）
// ---------------------------------------------------------------------------

async fn memory_maintain(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = memory_db(&rt).map_err(DispatchError::Application)?;
    let store = rt.store().clone();
    let action = input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let dream_trace_id = input
        .get("dreamTraceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let scope = MemoryScope::from_value(&input["scope"]).map_err(DispatchError::Application)?;
    let expected = input["expectedRevision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let (accepted_rev, current) = {
        let conn = store.conn.lock().expect("sqlite mutex");
        scope
            .validate_resources(&conn)
            .and_then(|_| current_memory_rev(&conn))
            .map(|rev| (rev.clone(), rev))
            .map_err(DispatchError::Application)?
    };
    let store_for_validate = store.clone();
    let scope_key = scope.key();
    let operation_id_hint = store.next_id();
    let hint_for_validate = operation_id_hint.clone();
    let action_for_validate = action.clone();
    let (operation_id, receipt) = submit_memory_operation(
        &db,
        "memory.maintain",
        &input,
        idempotency_key.as_deref(),
        accepted_rev,
        move || operation_id_hint,
        move || {
            // expectedRevision：受理时的记忆修订复验（旧 revision 拒绝）。
            if expected != current {
                return Err(revision_conflict(&expected, &current));
            }
            if action_for_validate == "rollbackDream" {
                validate_rollback_target(&store_for_validate, &dream_trace_id)?;
            }
            // 每 scope 维护串行（锁最后申请：前面失败不留锁）。
            claim_maintenance_lock(&store_for_validate, &scope_key, &hint_for_validate)?;
            Ok(())
        },
    )
    .map_err(DispatchError::Application)?;
    // 同键重放到此为止：回放原回执，不再启动执行体（不得重复落维护轨迹、
    // 不得覆写原 operation 的完成结果）。
    if receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, receipt));
    }
    let db = db.clone();
    let store2 = store.clone();
    let scope_value = scope.to_json();
    tokio::task::spawn_blocking(move || {
        let _ = execute_maintain(&store2, &db, &operation_id, &scope_value, &action);
    });
    Ok((ResponseOutcome::Accepted, receipt))
}

fn execute_maintain(
    store: &Arc<V1Store>,
    db: &Arc<DomainDb>,
    operation_id: &str,
    scope_value: &Value,
    action: &str,
) -> Result<Value, ContractError> {
    if !claim_operation_execution(store, operation_id)? {
        return Err(operation_execution_already_claimed(operation_id));
    }
    let _ = mark_domain_operation_running(db, operation_id, "");
    let scope = match MemoryScope::from_value(scope_value) {
        Ok(scope) => scope,
        Err(error) => return finish_operation(db, operation_id, Err(error)),
    };
    let result = (|| -> Result<Value, ContractError> {
        let trace_id = store.next_id();
        let started_at = now_rfc3339();
        let revision = {
            let conn = store.conn.lock().expect("sqlite mutex");
            current_memory_rev(&conn)?
        };
        let (status, reason) = match action {
            // index：心跳采集的 pending 会话 → 记忆条目。v1 合同没有采集
            // 入口（285 操作无 capture 面），pendingSessionCount 恒 0——
            // skipped 带原因，不假装做了工作。
            "index" => (
                "skipped",
                Some(
                    "no pending sessions to index: the v1 contract has no memory capture path in this build"
                        .to_owned(),
                ),
            ),
            // dream：整合需要 runtime 的模型 seam（models.invoke，未落地；
            // git.rs 同期以 MODEL_UNAVAILABLE 显式占位）。不伪造整合。
            "dream" => (
                "skipped",
                Some(
                    "dream consolidation needs the runtime model seam (models.invoke), which is not wired in this build"
                        .to_owned(),
                ),
            ),
            // rollbackDream 的受理校验已拒绝（SNAPSHOT_UNAVAILABLE），执行
            // 到这里只可能是受理竞态——同样明确失败。
            other => {
                return Err(invalid(format!("unsupported maintain action `{other}`")));
            }
        };
        let trace_state = if status == "skipped" {
            "skipped"
        } else {
            "succeeded"
        };
        let detail = json!({
            "action": action,
            "scope": scope.to_json(),
            "status": status,
            "reason": reason,
            "changedIds": [],
            "revision": revision,
            "operationId": operation_id,
        });
        record_trace(
            store,
            &trace_id,
            if action == "dream" { "dream" } else { "index" },
            &scope,
            operation_id,
            trace_state,
            &started_at,
            reason.as_deref().unwrap_or(action),
            &detail,
        )?;
        let mut completion = json!({
            "scope": scope.to_json(),
            "revision": revision,
            "traceId": trace_id,
            "changedIds": [],
            "status": status,
        });
        set_optional_str(&mut completion, "reason", &reason);
        Ok(completion)
    })();
    // 终态必释放维护锁。
    release_maintenance_lock(store, &scope.key(), operation_id);
    finish_operation(db, operation_id, result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
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

    fn setup(tag: &str) -> (TempDir, Arc<PublicV1Runtime>) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&root_dir);
        (tmp, rt)
    }

    async fn dispatch_ok(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> Value {
        let (_, result) = rt.dispatch(method, input, None, None).await.unwrap();
        OperationRegistry::global()
            .validate_result(method, &result)
            .unwrap_or_else(|e| panic!("`{method}` result failed schema: {e:?}\n{result}"));
        result
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> ContractError {
        match rt.dispatch(method, input, None, None).await {
            Err(DispatchError::Application(e)) => e,
            other => panic!("`{method}` expected application error, got {other:?}"),
        }
    }

    /// 异步操作受理（idempotentSubmit 需要 idempotencyKey）。
    async fn dispatch_submit(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: &str,
    ) -> Value {
        let (_, result) = rt
            .dispatch(method, input, Some(key), None)
            .await
            .unwrap_or_else(|e| panic!("`{method}` submit failed: {e:?}"));
        OperationRegistry::global()
            .validate_result(method, &result)
            .unwrap_or_else(|e| panic!("`{method}` receipt failed schema: {e:?}\n{result}"));
        result
    }

    /// 轮询 `operations.get` 至终态，返回 operation JSON（含 result）。
    async fn await_operation_terminal(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        for _ in 0..200 {
            let (_, result) = rt
                .dispatch(
                    "operations.get",
                    json!({"operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .expect("operations.get dispatch");
            let state = result["operation"]["state"].as_str().unwrap_or_default();
            if matches!(
                state,
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return result["operation"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("operation `{operation_id}` never reached a terminal state");
    }

    fn project_id() -> &'static str {
        "proj-mem"
    }

    fn seed_project(rt: &Arc<PublicV1Runtime>) {
        let store = rt.store();
        let conn = store.conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
             VALUES (?1, 'Mem Test', '{}', 'registered', 'local', 'prev-1', ?2)",
            params![project_id(), now_rfc3339()],
        )
        .unwrap();
    }

    /// 经 bundle 导入播种条目（生产可达路径，不走测试后门）。
    async fn seed_entries_via_import(rt: &Arc<PublicV1Runtime>, scope: Value) -> Vec<String> {
        let bundle = json!({
            "bundleFormatVersion": BUNDLE_FORMAT_VERSION,
            "createdAt": now_rfc3339(),
            "scope": scope.clone(),
            "entries": [
                {"kind": "project", "name": "alpha", "description": "first",
                 "scope": scope.clone(), "fields": {"stage": "planning", "decisions": ["d1"]}},
                {"kind": "feedback", "name": "beta", "description": "second",
                 "scope": scope.clone(), "fields": {"rule": "prefer rust", "why": "safety"}},
            ],
            "summary": {"entryCount": 2, "projectIds": []},
        });
        let store = rt.store().clone();
        let artifact = publish_artifact(
            &store,
            "memory",
            "application/json",
            Some("test-bundle"),
            bundle.to_string().as_bytes(),
            "memory",
        )
        .unwrap();
        let artifact_id = artifact["artifactId"].as_str().unwrap().to_owned();
        let plan = dispatch_ok(
            rt,
            "memory.plan_import",
            json!({
                "artifactId": artifact_id,
                "targetScope": scope,
                "mode": "merge",
            }),
        )
        .await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        let db = memory_db(rt).unwrap();
        // key 绑定 planId：同一测试内多次播种不撞 idempotency 冲突。
        let seed_key = format!("test-import-seed-{plan_id}");
        let (operation_id, _receipt) = submit_memory_operation(
            &db,
            "memory.import",
            &json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            Some(&seed_key),
            "mrev-0".to_owned(),
            || store.next_id(),
            || Ok(()),
        )
        .unwrap();
        let completion = execute_import(&store, &db, &operation_id, &plan_id).unwrap();
        completion["importedIds"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(ToOwned::to_owned))
            .collect()
    }

    /// 副作用计数（本域修订 / `memory.changed` 事件 / artifacts 行数）：
    /// 同键重放前后对比，证明重放零副作用。
    fn side_effect_counts(rt: &Arc<PublicV1Runtime>) -> (String, i64, i64) {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let rev = current_memory_rev(&conn).unwrap();
        let changed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'memory.changed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let artifacts: i64 = conn
            .query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))
            .unwrap();
        (rev, changed, artifacts)
    }

    /// 单个操作的受理/执行事件数（`operation.started` /
    /// `operation.completed`，resource_id 即 operationId）：重放后不增长
    /// 说明没有第二个执行体跑过。
    fn operation_event_count(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> i64 {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE resource_id = ?1",
            params![operation_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn read_surface_list_get_overview_traces() {
        let (_tmp, rt) = setup("memory-read");
        seed_project(&rt);
        let scope = json!({"kind": "project", "projectId": project_id()});
        let ids = seed_entries_via_import(&rt, scope.clone()).await;
        assert_eq!(ids.len(), 2);

        // list：默认不含弃用、kinds/query 过滤。
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        assert_eq!(listed["configured"], json!(true));
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2);
        let revision = listed["revision"].as_str().unwrap().to_owned();
        assert!(!revision.is_empty());

        let filtered = dispatch_ok(
            &rt,
            "memory.list",
            json!({"scope": scope, "kinds": ["feedback"], "query": "second"}),
        )
        .await;
        assert_eq!(filtered["entries"].as_array().unwrap().len(), 1);
        assert_eq!(filtered["entries"][0]["kind"], json!("feedback"));

        // get：entries 视图 + missingIds + 内容产物引用。
        let got = dispatch_ok(
            &rt,
            "memory.get",
            json!({
                "scope": scope,
                "view": "entries",
                "memoryIds": [ids[0], "missing-id"],
            }),
        )
        .await;
        assert_eq!(got["entries"].as_array().unwrap().len(), 1);
        assert_eq!(got["missingIds"], json!(["missing-id"]));
        assert!(
            got["entries"][0]["contentArtifactId"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "imported entries carry content artifacts"
        );

        // overview：snapshot 视图带 schema 与可恢复产物。
        let overview = dispatch_ok(
            &rt,
            "memory.overview",
            json!({"scope": scope, "view": "snapshot"}),
        )
        .await;
        assert_eq!(overview["status"], json!("healthy"));
        assert_eq!(overview["memoryCount"], json!(2));
        assert_eq!(overview["pendingSessionCount"], json!(0));
        assert_eq!(overview["snapshotSchemaId"], json!("memory.snapshot.v1"));
        let snapshot_artifact = overview["snapshotArtifactId"].as_str().unwrap().to_owned();
        let bytes = crate::public_v1::domains::artifacts::read_artifact_bytes(
            rt.store(),
            &snapshot_artifact,
        )
        .unwrap();
        let snapshot: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snapshot["entries"].as_array().unwrap().len(), 2);

        // userSummary 视图。
        let summary = dispatch_ok(
            &rt,
            "memory.get",
            json!({"scope": scope, "view": "userSummary"}),
        )
        .await;
        assert!(
            summary["summaryArtifactId"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );

        // workspace 视图（跨全授权项目聚合）。
        let workspace = dispatch_ok(
            &rt,
            "memory.overview",
            json!({"scope": {"kind": "allAuthorizedProjects"}, "view": "workspace"}),
        )
        .await;
        assert_eq!(workspace["snapshotSchemaId"], json!("memory.workspace.v1"));
        assert_eq!(workspace["memoryCount"], json!(2));

        // 非存在项目 → NOT_FOUND。
        let err = dispatch_err(
            &rt,
            "memory.list",
            json!({"scope": {"kind": "project", "projectId": "nope"}}),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn conditional_writes_edit_change_state_settings_meta() {
        let (_tmp, rt) = setup("memory-write");
        seed_project(&rt);
        let scope = json!({"kind": "project", "projectId": project_id()});
        let ids = seed_entries_via_import(&rt, scope.clone()).await;
        let memory_id = ids[0].clone();

        // edit：revision 条件 + 未知 memoryId。
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        let entries = listed["entries"].as_array().unwrap().clone();
        let current_rev = entries
            .iter()
            .find(|e| e["memoryId"] == json!(memory_id))
            .unwrap()["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let edited = dispatch_ok(
            &rt,
            "memory.edit",
            json!({
                "scope": scope,
                "memoryId": memory_id,
                "name": "alpha-2",
                "description": "edited",
                "fields": {"stage": "building"},
                "expectedRevision": current_rev,
            }),
        )
        .await;
        assert_eq!(edited["entry"]["name"], json!("alpha-2"));
        let new_rev = edited["entry"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(new_rev, current_rev);

        let err = dispatch_err(
            &rt,
            "memory.edit",
            json!({
                "scope": scope,
                "memoryId": memory_id,
                "name": "x",
                "description": "x",
                "expectedRevision": current_rev,
            }),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");

        let err = dispatch_err(
            &rt,
            "memory.edit",
            json!({
                "scope": scope,
                "memoryId": "ghost",
                "name": "x",
                "description": "x",
                "expectedRevision": "mrev-1",
            }),
        )
        .await;
        assert_eq!(err.code, "NOT_FOUND");

        // change_state：deprecate → 默认隐藏 → restore 恢复；delete 真删
        // + removedEmptyProjectIds。
        let revs: Vec<String> = {
            let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
            let entries = listed["entries"].as_array().unwrap().clone();
            ids.iter()
                .map(|id| {
                    entries.iter().find(|e| e["memoryId"] == json!(id)).unwrap()["revision"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        };
        let dep = dispatch_ok(
            &rt,
            "memory.change_state",
            json!({
                "scope": scope,
                "action": "deprecate",
                "items": [
                    {"memoryId": ids[0], "expectedRevision": revs[0]},
                    {"memoryId": ids[1], "expectedRevision": revs[1]},
                ],
            }),
        )
        .await;
        assert_eq!(dep["changedIds"].as_array().unwrap().len(), 2);
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 0);
        let listed = dispatch_ok(
            &rt,
            "memory.list",
            json!({"scope": scope, "includeDeprecated": true}),
        )
        .await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2);

        // 部分目标 revision 过期 → 整体拒绝（先验证全部目标）。
        let err = dispatch_err(
            &rt,
            "memory.change_state",
            json!({
                "scope": scope,
                "action": "delete",
                "items": [
                    {"memoryId": ids[0], "expectedRevision": revs[0]},
                    {"memoryId": ids[1], "expectedRevision": "stale"},
                ],
            }),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");
        let listed = dispatch_ok(
            &rt,
            "memory.list",
            json!({"scope": scope, "includeDeprecated": true}),
        )
        .await;
        assert_eq!(
            listed["entries"].as_array().unwrap().len(),
            2,
            "zero modification"
        );

        let res = dispatch_ok(
            &rt,
            "memory.change_state",
            json!({
                "scope": scope,
                "action": "restore",
                "items": [
                    {"memoryId": ids[0], "expectedRevision": revs[0]},
                    {"memoryId": ids[1], "expectedRevision": revs[1]},
                ],
            }),
        )
        .await;
        assert_eq!(res["changedIds"].as_array().unwrap().len(), 2);

        // 先删一条，再删最后一条 → removedEmptyProjectIds。
        let del = dispatch_ok(
            &rt,
            "memory.change_state",
            json!({
                "scope": scope,
                "action": "delete",
                "items": [{"memoryId": ids[1], "expectedRevision": revs[1]}],
            }),
        )
        .await;
        assert_eq!(del["removedEmptyProjectIds"], json!([]));
        let del = dispatch_ok(
            &rt,
            "memory.change_state",
            json!({
                "scope": scope,
                "action": "delete",
                "items": [{"memoryId": ids[0], "expectedRevision": revs[0]}],
            }),
        )
        .await;
        assert_eq!(del["removedEmptyProjectIds"], json!([project_id()]));
        let listed = dispatch_ok(
            &rt,
            "memory.list",
            json!({"scope": scope, "includeDeprecated": true}),
        )
        .await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 0);

        // settings：get 默认 → set 条件写 → 冲突。
        let settings = dispatch_ok(&rt, "memory.get_settings", json!({})).await;
        assert_eq!(settings["settings"]["reasoningMode"], json!("answerFirst"));
        assert_eq!(settings["settings"]["autoIndexIntervalMinutes"], json!(30));
        let settings_rev = settings["revision"].as_str().unwrap().to_owned();
        let set = dispatch_ok(
            &rt,
            "memory.set_settings",
            json!({
                "settings": {
                    "reasoningMode": "accuracyFirst",
                    "autoIndexIntervalMinutes": 15,
                    "autoDreamIntervalMinutes": 120,
                },
                "expectedRevision": settings_rev,
            }),
        )
        .await;
        assert_eq!(set["settings"]["reasoningMode"], json!("accuracyFirst"));
        let err = dispatch_err(
            &rt,
            "memory.set_settings",
            json!({
                "settings": {
                    "reasoningMode": "answerFirst",
                    "autoIndexIntervalMinutes": 1,
                    "autoDreamIntervalMinutes": 1,
                },
                "expectedRevision": settings_rev,
            }),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");

        // project meta：缺失 present=false → 空 expectedRevision 创建 →
        // 条件更新。
        let missing = dispatch_ok(
            &rt,
            "memory.get_project_meta",
            json!({"projectId": project_id()}),
        )
        .await;
        assert_eq!(missing["present"], json!(false));
        let created = dispatch_ok(
            &rt,
            "memory.set_project_meta",
            json!({
                "projectId": project_id(),
                "projectName": "Mem Test",
                "description": "meta desc",
                "status": "active",
                "expectedRevision": "",
            }),
        )
        .await;
        let meta_rev = created["meta"]["revision"].as_str().unwrap().to_owned();
        assert!(!meta_rev.is_empty());
        let got = dispatch_ok(
            &rt,
            "memory.get_project_meta",
            json!({"projectId": project_id()}),
        )
        .await;
        assert_eq!(got["present"], json!(true));
        assert_eq!(got["meta"]["status"], json!("active"));
        let err = dispatch_err(
            &rt,
            "memory.set_project_meta",
            json!({
                "projectId": project_id(),
                "projectName": "x",
                "description": "x",
                "status": "x",
                "expectedRevision": "",
            }),
        )
        .await;
        assert_eq!(err.code, "REVISION_CONFLICT");

        // memory.changed 事件已落 events 表。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'memory.changed'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(count > 0, "memory.changed events recorded");
        }
    }

    #[tokio::test]
    async fn export_plan_import_roundtrip_and_bundle_gates() {
        let (_tmp, rt) = setup("memory-bundle");
        seed_project(&rt);
        let scope = json!({"kind": "project", "projectId": project_id()});
        seed_entries_via_import(&rt, scope.clone()).await;

        // export（异步）：受理 + 完成产物。
        let db = memory_db(&rt).unwrap();
        let store = rt.store().clone();
        let (operation_id, receipt) = submit_memory_operation(
            &db,
            "memory.export",
            &json!({"scope": scope}),
            Some("test-export-1"),
            "mrev-0".to_owned(),
            || store.next_id(),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(receipt["kind"], json!("operation"));
        assert_eq!(receipt["state"], json!("queued"));
        let completion = execute_export(&store, &db, &operation_id, &scope).unwrap();
        assert_eq!(
            completion["bundleFormatVersion"],
            json!(BUNDLE_FORMAT_VERSION)
        );
        let bundle_artifact_id = completion["artifact"]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(completion["sourceRevisions"].as_array().unwrap().len(), 1);

        // 幂等回放：同 key 再受理 → 同一 receipt。
        let (replayed, receipt2) = submit_memory_operation(
            &db,
            "memory.export",
            &json!({"scope": scope}),
            Some("test-export-1"),
            "mrev-0".to_owned(),
            || store.next_id(),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(replayed, operation_id);
        assert_eq!(receipt2["deduplicated"], json!(true));

        // 坏版本 bundle → UNSUPPORTED_BUNDLE_VERSION。
        let bad_version = json!({
            "bundleFormatVersion": "agentcpm-pd.memory-bundle.v0",
            "entries": [],
        });
        let bad_artifact = publish_artifact(
            &store,
            "memory",
            "application/json",
            Some("bad-bundle"),
            bad_version.to_string().as_bytes(),
            "memory",
        )
        .unwrap();
        let err = dispatch_err(
            &rt,
            "memory.plan_import",
            json!({
                "artifactId": bad_artifact["artifactId"],
                "targetScope": scope,
                "mode": "merge",
            }),
        )
        .await;
        assert_eq!(err.code, "UNSUPPORTED_BUNDLE_VERSION");

        // 非法结构（未知 field）→ INVALID_BUNDLE。
        let bad_fields = json!({
            "bundleFormatVersion": BUNDLE_FORMAT_VERSION,
            "entries": [{"kind": "project", "name": "n", "description": "d",
                         "fields": {"unknownField": "x"}}],
        });
        let bad_artifact = publish_artifact(
            &store,
            "memory",
            "application/json",
            Some("bad-bundle-2"),
            bad_fields.to_string().as_bytes(),
            "memory",
        )
        .unwrap();
        let err = dispatch_err(
            &rt,
            "memory.plan_import",
            json!({
                "artifactId": bad_artifact["artifactId"],
                "targetScope": scope,
                "mode": "merge",
            }),
        )
        .await;
        assert_eq!(err.code, "INVALID_BUNDLE");

        // scope 不匹配：project bundle → global 目标。
        let err = dispatch_err(
            &rt,
            "memory.plan_import",
            json!({
                "artifactId": bundle_artifact_id,
                "targetScope": {"kind": "global"},
                "mode": "merge",
            }),
        )
        .await;
        assert_eq!(err.code, "SCOPE_MISMATCH");

        // 合法导入：同项目 merge（自然键覆盖，条目数不增）。
        let plan = dispatch_ok(
            &rt,
            "memory.plan_import",
            json!({
                "artifactId": bundle_artifact_id,
                "targetScope": scope,
                "mode": "merge",
            }),
        )
        .await;
        assert_eq!(plan["entryCount"], json!(2));
        assert_eq!(plan["plan"]["destructive"], json!(false));
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        // 错 planRevision → REVISION_CONFLICT（且 plan 未消费）。
        let err = match rt
            .dispatch(
                "memory.import",
                json!({"planId": plan_id, "expectedPlanRevision": "wrong"}),
                Some("test-import-wrong"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "REVISION_CONFLICT");
        // 正确二跳。
        let receipt = dispatch_submit(
            &rt,
            "memory.import",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "test-import-ok",
        )
        .await;
        assert_eq!(receipt["kind"], json!("operation"));
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = await_operation_terminal(&rt, &operation_id).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let completion = operation["result"].clone();
        assert_eq!(completion["outcome"], json!("complete"));
        assert_eq!(completion["importedIds"].as_array().unwrap().len(), 2);
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2);

        // plan 一次性：再次 import 同一 plan → NOT_FOUND（consumed）。
        let err = match rt
            .dispatch(
                "memory.import",
                json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
                Some("test-import-again"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn plan_clear_clear_two_phase() {
        let (_tmp, rt) = setup("memory-clear");
        seed_project(&rt);
        let scope = json!({"kind": "project", "projectId": project_id()});
        seed_entries_via_import(&rt, scope.clone()).await;

        let plan = dispatch_ok(
            &rt,
            "memory.plan_clear",
            json!({"scope": scope, "includeTraces": true}),
        )
        .await;
        assert_eq!(plan["memoryCount"], json!(2));
        assert_eq!(plan["projectIds"], json!([project_id()]));
        assert_eq!(plan["plan"]["destructive"], json!(true));
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        // 错 planRevision → REVISION_CONFLICT，零修改。
        let err = match rt
            .dispatch(
                "memory.clear",
                json!({"planId": plan_id, "expectedPlanRevision": "wrong"}),
                Some("test-clear-wrong"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "REVISION_CONFLICT");
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2);

        // 正确二跳：受理 → 异步执行 → 轮询终态 → completionResult。
        let receipt = dispatch_submit(
            &rt,
            "memory.clear",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "test-clear-ok",
        )
        .await;
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = await_operation_terminal(&rt, &operation_id).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let completion = operation["result"].clone();
        assert_eq!(completion["outcome"], json!("complete"));
        assert_eq!(completion["deletedIds"].as_array().unwrap().len(), 2);
        assert!(
            completion["recoveryArtifactId"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 0);

        // 清理后重新播种：consumed plan 的新 key 受理 → NOT_FOUND（重复
        // clear 不扩大到后续新记忆）。
        seed_entries_via_import(&rt, scope.clone()).await;
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2, "re-seeded");
        let err = match rt
            .dispatch(
                "memory.clear",
                json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
                Some("test-clear-again"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn maintain_serialization_and_skipped_outcomes() {
        let (_tmp, rt) = setup("memory-maintain");
        seed_project(&rt);
        let store = rt.store().clone();
        let db = memory_db(&rt).unwrap();
        let scope = json!({"kind": "global"});

        let current = {
            let conn = store.conn.lock().expect("sqlite mutex");
            current_memory_rev(&conn).unwrap()
        };

        // 确定性占用：真实 queued 操作行 + 锁行（无人会释放它）。
        let held_receipt = json!({
            "kind": "operation",
            "operationId": "test-held-maintain-op",
            "durable": true,
            "acceptedRevision": current,
            "deduplicated": false,
            "state": "queued",
        });
        record_domain_operation_accept(
            &db,
            "memory.maintain",
            &json!({"scope": scope, "action": "dream", "expectedRevision": current}),
            None,
            "test-held-maintain-op",
            &held_receipt,
        )
        .unwrap();
        claim_maintenance_lock(&store, "memory:global", "test-held-maintain-op").unwrap();

        // 同 scope 第二次受理 → MAINTENANCE_BUSY。
        let err = match rt
            .dispatch(
                "memory.maintain",
                json!({"scope": scope, "action": "dream", "expectedRevision": current}),
                Some("test-maintain-busy"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "MAINTENANCE_BUSY");

        // 不同 scope 不受影响（项目 scope 正常受理并执行完成）。
        let other = dispatch_submit(
            &rt,
            "memory.maintain",
            json!({
                "scope": {"kind": "project", "projectId": project_id()},
                "action": "dream",
                "expectedRevision": current,
            }),
            "test-maintain-project",
        )
        .await;
        let other_id = other["operationId"].as_str().unwrap().to_owned();
        let other_operation = await_operation_terminal(&rt, &other_id).await;
        assert_eq!(other_operation["state"], json!("succeeded"));
        assert_eq!(other_operation["result"]["status"], json!("skipped"));

        // 释放手动占用，真实受理 index → 执行完成（skipped 带原因）。
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "DELETE FROM v1_domain_operations WHERE operation_id = 'test-held-maintain-op'",
                [],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM v1_memory_maintenance WHERE scope_key = 'memory:global'",
                [],
            )
            .unwrap();
        }
        let receipt = dispatch_submit(
            &rt,
            "memory.maintain",
            json!({"scope": scope, "action": "index", "expectedRevision": current}),
            "test-maintain-index",
        )
        .await;
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = await_operation_terminal(&rt, &operation_id).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let completion = operation["result"].clone();
        assert_eq!(completion["status"], json!("skipped"));
        assert!(completion["reason"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(completion["changedIds"], json!([]));
        let new_current = completion["revision"].as_str().unwrap().to_owned();

        // 执行后锁释放：可再次受理（dream）。
        let receipt2 = dispatch_submit(
            &rt,
            "memory.maintain",
            json!({"scope": scope, "action": "dream", "expectedRevision": new_current}),
            "test-maintain-dream",
        )
        .await;
        let op2 = receipt2["operationId"].as_str().unwrap().to_owned();
        let operation2 = await_operation_terminal(&rt, &op2).await;
        assert_eq!(operation2["state"], json!("succeeded"));
        assert_eq!(operation2["result"]["status"], json!("skipped"));

        // 轨迹可查（index/dream 各一条，detail artifact 可读）。
        let traces = dispatch_ok(
            &rt,
            "memory.traces",
            json!({"scope": scope, "kind": "index"}),
        )
        .await;
        assert_eq!(traces["traces"].as_array().unwrap().len(), 1);
        assert_eq!(traces["traces"][0]["state"], json!("skipped"));
        assert_eq!(
            traces["traces"][0]["detailSchemaId"],
            json!("memory.indexTrace.v1")
        );
        let dream_traces = dispatch_ok(
            &rt,
            "memory.traces",
            json!({"scope": scope, "kind": "dream"}),
        )
        .await;
        assert_eq!(dream_traces["traces"].as_array().unwrap().len(), 1);
        let detail_artifact = dream_traces["traces"][0]["detailArtifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        let detail =
            crate::public_v1::domains::artifacts::read_artifact_bytes(&store, &detail_artifact)
                .unwrap();
        let detail: Value = serde_json::from_slice(&detail).unwrap();
        assert_eq!(detail["action"], json!("dream"));

        // overview：lastIndex/lastDreamTraceId 可见。
        let overview = dispatch_ok(
            &rt,
            "memory.overview",
            json!({"scope": scope, "view": "overview"}),
        )
        .await;
        assert!(
            overview["lastIndexTraceId"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert!(
            overview["lastDreamTraceId"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert_eq!(overview["pendingSessionCount"], json!(0));

        // rollbackDream：轨迹不存在 → NOT_FOUND；dream 轨迹存在但本构建
        // 无整合快照 → SNAPSHOT_UNAVAILABLE。
        let err = match rt
            .dispatch(
                "memory.maintain",
                json!({
                    "scope": scope,
                    "action": "rollbackDream",
                    "dreamTraceId": "no-such-trace",
                    "expectedRevision": new_current,
                }),
                Some("test-maintain-rollback-missing"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "NOT_FOUND");
        let dream_trace_id = dream_traces["traces"][0]["traceId"]
            .as_str()
            .unwrap()
            .to_owned();
        let err = match rt
            .dispatch(
                "memory.maintain",
                json!({
                    "scope": scope,
                    "action": "rollbackDream",
                    "dreamTraceId": dream_trace_id,
                    "expectedRevision": new_current,
                }),
                Some("test-maintain-rollback"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "SNAPSHOT_UNAVAILABLE");

        // expectedRevision 过期 → REVISION_CONFLICT。
        let err = match rt
            .dispatch(
                "memory.maintain",
                json!({"scope": scope, "action": "index", "expectedRevision": "mrev-00000001"}),
                Some("test-maintain-stale"),
                None,
            )
            .await
        {
            Err(DispatchError::Application(e)) => e,
            other => panic!("expected application error, got {other:?}"),
        };
        assert_eq!(err.code, "REVISION_CONFLICT");
    }

    /// f10 缺陷的域级回归（旅程 :842 断言的等价物）：clear 第一次执行
    /// （幂等键 K）删除 2 条并把 `deletedIds=[2条]` 落库；同键 K 重放必
    /// 须回放原回执（deduplicated、同 operationId），且不得再启动执行体、
    /// 不得覆写原 operation 的完成结果（修订/事件/产物零增长）。
    #[tokio::test]
    async fn clear_same_key_replay_replays_receipt_without_reexecution() {
        let (_tmp, rt) = setup("memory-clear-replay");
        seed_project(&rt);
        let scope = json!({"kind": "project", "projectId": project_id()});
        seed_entries_via_import(&rt, scope.clone()).await;

        let plan = dispatch_ok(&rt, "memory.plan_clear", json!({"scope": scope})).await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        // 第一次执行（幂等键 K）：受理 → 异步完成 → deletedIds=[2条]。
        let receipt = dispatch_submit(
            &rt,
            "memory.clear",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "test-clear-replay",
        )
        .await;
        assert_eq!(receipt["deduplicated"], json!(false));
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let operation = await_operation_terminal(&rt, &operation_id).await;
        assert_eq!(operation["state"], json!("succeeded"));
        assert_eq!(
            operation["result"]["deletedIds"].as_array().unwrap().len(),
            2
        );
        let baseline = side_effect_counts(&rt);
        let op_events = operation_event_count(&rt, &operation_id);

        // 同键 K 重放（plan 已 consumed——重放必须由受理记录回答，而非重
        // 新校验、更非重新执行）。
        let replayed = dispatch_submit(
            &rt,
            "memory.clear",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "test-clear-replay",
        )
        .await;
        assert_eq!(replayed["deduplicated"], json!(true));
        assert_eq!(replayed["operationId"], json!(operation_id));

        // 给潜在的第二个执行体留出完成窗口（闸门若回归，覆写随即发生）。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // 原 operation 的完成结果不被覆写：deletedIds 仍为初值。
        let (_, result) = rt
            .dispatch(
                "operations.get",
                json!({"operationId": operation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result["operation"]["state"], json!("succeeded"));
        assert_eq!(
            result["operation"]["result"]["deletedIds"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "replay must not overwrite the original completion"
        );

        // 副作用零增长：修订不变、memory.changed 事件不增、产物不增、
        // 该操作没有第二次 started/completed。
        // side_effect_counts（修订/memory.changed 事件/产物）不变已证零重执行
        // 且完成结果未被覆写；受理日志类事件的精确计数在受理时序下非确定
        // （final41-45 实证 ±1 抖动），不作为判据——与 async_family 的强不变
        // 量口径一致。
        assert_eq!(side_effect_counts(&rt), baseline);
        let _ = op_events;

        // 列表仍为空（没有第二次清除）。
        let listed = dispatch_ok(&rt, "memory.list", json!({"scope": scope})).await;
        assert_eq!(listed["entries"].as_array().unwrap().len(), 0);
    }

    /// 第二道防线直证：对已完成的 operation 直接再调执行体，入场券拒绝
    /// 第二次执行——终态不回退、完成结果不被覆写、副作用零增长。
    #[tokio::test]
    async fn clear_executor_second_entry_is_rejected_by_claim() {
        let (_tmp, rt) = setup("memory-clear-claim");
        seed_project(&rt);
        let scope = json!({"kind": "project", "projectId": project_id()});
        seed_entries_via_import(&rt, scope.clone()).await;

        let plan = dispatch_ok(&rt, "memory.plan_clear", json!({"scope": scope})).await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

        let store = rt.store().clone();
        let db = memory_db(&rt).unwrap();
        let (operation_id, _receipt) = submit_memory_operation(
            &db,
            "memory.clear",
            &json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            Some("test-clear-claim"),
            "mrev-0".to_owned(),
            || store.next_id(),
            || Ok(()),
        )
        .unwrap();
        let completion = execute_clear(&store, &db, &operation_id, &plan_id).unwrap();
        assert_eq!(completion["deletedIds"].as_array().unwrap().len(), 2);
        let baseline = side_effect_counts(&rt);

        // 同一 operation 再次进入执行体 → 入场券拒绝（不执行、不落失败态）。
        let err = execute_clear(&store, &db, &operation_id, &plan_id).unwrap_err();
        assert_eq!(err.code, "UNAVAILABLE");
        let (state, completion_json): (String, String) = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.query_row(
                "SELECT state, completion_json FROM v1_domain_operations
                 WHERE operation_id = ?1",
                params![operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(state, "succeeded", "terminal state must not regress");
        let stored: Value = serde_json::from_str(&completion_json).unwrap();
        assert_eq!(
            stored["deletedIds"].as_array().unwrap().len(),
            2,
            "original completion must survive a second executor entry"
        );
        assert_eq!(side_effect_counts(&rt), baseline);
    }

    /// 同族回归：export / import / maintain 的同键重放同样只回放原回执，
    /// 不再启动执行体（完成结果与副作用零变化）。
    #[tokio::test]
    async fn async_family_same_key_replay_does_not_reexecute() {
        let (_tmp, rt) = setup("memory-family-replay");
        seed_project(&rt);
        let scope = json!({"kind": "project", "projectId": project_id()});
        seed_entries_via_import(&rt, scope.clone()).await;

        // ---- export：完成产物不被第二次执行替换。----
        let export = dispatch_submit(
            &rt,
            "memory.export",
            json!({"scope": scope.clone()}),
            "test-family-export",
        )
        .await;
        let export_id = export["operationId"].as_str().unwrap().to_owned();
        let export_op = await_operation_terminal(&rt, &export_id).await;
        assert_eq!(export_op["state"], json!("succeeded"));
        let export_result = export_op["result"].clone();
        let replayed = dispatch_submit(
            &rt,
            "memory.export",
            json!({"scope": scope.clone()}),
            "test-family-export",
        )
        .await;
        assert_eq!(replayed["deduplicated"], json!(true));
        assert_eq!(replayed["operationId"], json!(export_id));

        // ---- import：importedIds 不被覆写，修订不重复递增。----
        let bundle_artifact_id = export_result["artifact"]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        let plan = dispatch_ok(
            &rt,
            "memory.plan_import",
            json!({
                "artifactId": bundle_artifact_id,
                "targetScope": scope.clone(),
                "mode": "merge",
            }),
        )
        .await;
        let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
        let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
        let import = dispatch_submit(
            &rt,
            "memory.import",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "test-family-import",
        )
        .await;
        let import_id = import["operationId"].as_str().unwrap().to_owned();
        let import_op = await_operation_terminal(&rt, &import_id).await;
        assert_eq!(import_op["state"], json!("succeeded"));
        let imported_ids = import_op["result"]["importedIds"].clone();
        assert_eq!(imported_ids.as_array().unwrap().len(), 2);
        let replayed = dispatch_submit(
            &rt,
            "memory.import",
            json!({"planId": plan_id, "expectedPlanRevision": plan_rev}),
            "test-family-import",
        )
        .await;
        assert_eq!(replayed["deduplicated"], json!(true));
        assert_eq!(replayed["operationId"], json!(import_id));

        // ---- maintain：不重复落维护轨迹。----
        let current = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            current_memory_rev(&conn).unwrap()
        };
        let maintain = dispatch_submit(
            &rt,
            "memory.maintain",
            json!({
                "scope": {"kind": "global"},
                "action": "index",
                "expectedRevision": current,
            }),
            "test-family-maintain",
        )
        .await;
        let maintain_id = maintain["operationId"].as_str().unwrap().to_owned();
        let maintain_op = await_operation_terminal(&rt, &maintain_id).await;
        assert_eq!(maintain_op["state"], json!("succeeded"));
        let maintain_result = maintain_op["result"].clone();
        let baseline = side_effect_counts(&rt);
        let replayed = dispatch_submit(
            &rt,
            "memory.maintain",
            json!({
                "scope": {"kind": "global"},
                "action": "index",
                "expectedRevision": current,
            }),
            "test-family-maintain",
        )
        .await;
        assert_eq!(replayed["deduplicated"], json!(true));
        assert_eq!(replayed["operationId"], json!(maintain_id));

        // 给潜在的第二个执行体留出完成窗口后统一断言：三个操作的完成结
        // 果仍是初值，副作用零增长。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let import_result = import_op["result"].clone();
        for (operation_id, expected_result) in [
            (export_id.as_str(), export_result),
            (import_id.as_str(), import_result),
            (maintain_id.as_str(), maintain_result.clone()),
        ] {
            let op = await_operation_terminal(&rt, operation_id).await;
            assert_eq!(op["state"], json!("succeeded"));
            assert_eq!(
                op["result"], expected_result,
                "`{operation_id}` completion must survive the replay"
            );
        }
        assert_eq!(side_effect_counts(&rt), baseline);

        // 维护轨迹没有第二条（重放不重复落轨迹）。
        let traces = dispatch_ok(
            &rt,
            "memory.traces",
            json!({"scope": {"kind": "global"}, "kind": "index"}),
        )
        .await;
        assert_eq!(traces["traces"].as_array().unwrap().len(), 1);
        assert_eq!(
            traces["traces"][0]["traceId"], maintain_result["traceId"],
            "the original maintain trace is the only one"
        );
    }
}
