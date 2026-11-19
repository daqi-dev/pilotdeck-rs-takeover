//! `mcp.*` — MCP 配置、连接控制与会话级覆盖（extensions-management，S3；
//! capability a030）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `mcp.list/get/validate/
//! configure/remove/transition/set_session_servers/set_permission_mode`
//! 条目。TS 基线（cc7c4e6）是语义参考：`mcp.json` 双层配置
//! （`loadMcpServerConfig.ts`：global `<home>/mcp.json` + project
//! `<root>/.pilotdeck/mcp.json`，project 覆盖 global）、`mcpServerStatus`
//! 的 `servers[{name, status, error}]`、`setMcpServers` 的整组替换 +
//! `check_config` 校验、`toggleMcpServer`、以及
//! `setMcpPermissionModeOverride`（mode `default|auto|null`，null 清除）。
//!
//! ## v1 薄实现裁决（与旧线 `runtime/mcp/**` 12k 行的关系）
//!
//! 旧线的 registry/runtime/sdk 是会话进程装配（真实 stdio/http MCP 客户端
//! 生命周期）。按任务简报，v1 落**配置面**：配置的存储、schema 校验、
//! 继承解析与会话级覆盖；运行时装配不在本轮（接线不成比例，报告登记）。
//! 由此推论的诚实性约束（验收「不因配置存在而宣称在线」「失败不假报
//! 可用」）：
//!
//! * `runtime.available` 恒为 `false`——本构建装配了 MCP 运行时才会真实
//!   连接；enabled 服务器的 `reasonCode` 为
//!   `MCP_RUNTIME_NOT_ASSEMBLED`（与 tools.rs 对未装配 provider 的
//!   `SEARCH_PROVIDER_NOT_CONFIGURED` 同纪律），disabled 的为 `null`
//!   （主动停用不是故障）。
//! * `transition.connect/reconnect` 无法做真实握手（「重连成功以真实
//!   握手为准」）→ 受理后以 `UNAVAILABLE`（消息注明运行时未装配）失败，
//!   不假成功。
//! * `generation` 是行级状态机令牌（建/改配置、enable/disable 时递增），
//!   兼作 transition 的 `expectedGeneration` CAS 基准；无运行代际。
//! * `applied` 恒为 `false`（「保存与运行生效分离」：配置已提交、未在
//!   任何运行代际生效）；结果 `generation` 恒为 `null`。
//!
//! ## 数据模型（本域 `reg.schema` DDL，shared state DB）
//!
//! * `v1_mcp_servers`：按 `(scope_key, server_id)` 一行一服务器。
//!   `serverId = name`（同 scope 内名称唯一 ⇒ 稳定 id；验收「同名服务器
//!   使用稳定serverId」），配置原文 `config_json`（schema 校验通过的
//!   封闭对象），`enabled`、`generation`、`configuration_revision`（写入
//!   该行时的 scope 配置版本）。
//! * `v1_mcp_scope_revisions`：每 scope 一个配置版本（CAS 基准）。从未
//!   配置过的 scope 当前版本为哨兵 `absent`（创建用它，见合同
//!   expectedRevision 描述）。
//! * `v1_mcp_session_servers`：会话启用的服务器集合
//!   （`set_session_servers` 的整组替换）。
//! * `v1_mcp_session_config`：会话级配置（启用集合版本 + MCP 权限覆盖），
//!   `revision` 是 `set_session_servers/set_permission_mode` 共用的 CAS
//!   令牌（任一会话配置变更都使另一操作的旧 revision 过期——「过期写
//!   冲突」验收锚）。
//!
//! ## schema（「已注册且版本固定」）
//!
//! 服务器配置按 `agentcpm-pd.mcp.server/1` 校验：封闭字段集（无任意
//! method/shell）、transport 三值、stdio⇒command / http|sse⇒url 的条件
//! 必填（TS `GatewayMcpServerConfig` 的三态联合投影到 JSON Schema 的
//! if/then）。`credentialRef` 必须解析到 credentials 域已登记的凭据
//! （「凭据引用」诊断；秘密本身永不入配置入日志）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::configuration::{REDACTED, sensitive_key};
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339, pinned_run_is_live};

/// 合同 error 表里 protocol 常量尚未覆盖的码。
const CODE_SESSION_BUSY: &str = "SESSION_BUSY";
const CODE_GENERATION_CONFLICT: &str = "GENERATION_CONFLICT";
/// enabled 服务器未连接的 `runtime.reasonCode`。
const REASON_MCP_RUNTIME_NOT_ASSEMBLED: &str = "MCP_RUNTIME_NOT_ASSEMBLED";
/// 从未配置过的 scope 的当前版本哨兵（合同 expectedRevision：创建用
/// `absent`）。
const REVISION_ABSENT: &str = "absent";
/// 会话 MCP 权限覆盖的已注册模式（TS `GatewayMcpPermissionModeOverride
/// Input.mode`：`default` / `auto` / null 清除）。
const PERMISSION_MODES: [&str; 2] = ["default", "auto"];

/// 本域注册且版本固定的服务器配置 schema（mcp.validate/get 的 `schemaId`
/// 必须解析到它）。
const MCP_SERVER_SCHEMA_ID: &str = "agentcpm-pd.mcp.server/1";
const MCP_SERVER_SCHEMA: &str = r#"{
    "$id": "agentcpm-pd.mcp.server/1",
    "type": "object",
    "additionalProperties": false,
    "required": ["name", "transport"],
    "properties": {
        "name": {"type": "string", "minLength": 1, "maxLength": 128,
                  "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"},
        "transport": {"enum": ["stdio", "http", "sse"]},
        "command": {"type": "string", "minLength": 1, "maxLength": 2048},
        "args": {"type": "array", "items": {"type": "string"}, "maxItems": 256},
        "env": {"type": "object", "additionalProperties": {"type": "string"}},
        "cwd": {"type": "string", "minLength": 1, "maxLength": 2048},
        "url": {"type": "string", "minLength": 1, "maxLength": 2048},
        "headers": {"type": "object", "additionalProperties": {"type": "string"}},
        "timeoutMs": {"type": "integer", "minimum": 0, "maximum": 3600000},
        "credentialRef": {"type": "string", "minLength": 1, "maxLength": 128},
        "enabled": {"type": "boolean"}
    },
    "allOf": [
        {"if": {"properties": {"transport": {"const": "stdio"}}, "required": ["transport"]},
         "then": {"required": ["command"]}},
        {"if": {"properties": {"transport": {"enum": ["http", "sse"]}}, "required": ["transport"]},
         "then": {"required": ["url"]}}
    ]
}"#;

const DDL_MCP_SERVERS: &str = "CREATE TABLE IF NOT EXISTS v1_mcp_servers (
    server_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    project_id TEXT,
    session_id TEXT,
    name TEXT NOT NULL,
    transport TEXT NOT NULL,
    config_json TEXT NOT NULL,
    credential_ref TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    generation TEXT NOT NULL,
    configuration_revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (scope_key, server_id)
)";
const DDL_MCP_SCOPE_REVISIONS: &str = "CREATE TABLE IF NOT EXISTS v1_mcp_scope_revisions (
    scope_key TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
const DDL_MCP_SESSION_SERVERS: &str = "CREATE TABLE IF NOT EXISTS v1_mcp_session_servers (
    session_id TEXT NOT NULL,
    server_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (session_id, server_id)
)";
const DDL_MCP_SESSION_CONFIG: &str = "CREATE TABLE IF NOT EXISTS v1_mcp_session_config (
    session_id TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    permission_mode TEXT,
    permission_mode_source TEXT,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_mcp_servers", DDL_MCP_SERVERS);
    reg.schema("v1_mcp_scope_revisions", DDL_MCP_SCOPE_REVISIONS);
    reg.schema("v1_mcp_session_servers", DDL_MCP_SESSION_SERVERS);
    reg.schema("v1_mcp_session_config", DDL_MCP_SESSION_CONFIG);

    reg.handler("mcp.list", |rt, input, _key, _rev| {
        Box::pin(mcp_list(rt, input))
    });
    reg.handler("mcp.get", |rt, input, _key, _rev| {
        Box::pin(mcp_get(rt, input))
    });
    reg.handler("mcp.validate", |rt, input, _key, _rev| {
        Box::pin(mcp_validate(rt, input))
    });
    reg.handler("mcp.configure", |rt, input, _key, _rev| {
        Box::pin(mcp_configure(rt, input))
    });
    reg.handler("mcp.remove", |rt, input, _key, _rev| {
        Box::pin(mcp_remove(rt, input))
    });
    reg.handler("mcp.transition", |rt, input, key, _rev| {
        Box::pin(mcp_transition(rt, input, key))
    });
    reg.handler("mcp.set_session_servers", |rt, input, _key, _rev| {
        Box::pin(mcp_set_session_servers(rt, input))
    });
    reg.handler("mcp.set_permission_mode", |rt, input, _key, _rev| {
        Box::pin(mcp_set_permission_mode(rt, input))
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
        format!("mcp expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
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
        message: format!("mcp worker task failed: {err}"),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// 作用域
// ---------------------------------------------------------------------------

/// 合同 `scope`（`{kind: global|project|session, id}`）。目标必须存在
/// （`INVALID_ARGUMENT`——list/validate/configure 的错误表没有 NOT_FOUND，
/// 域内统一）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct McpScope {
    kind: &'static str,
    project_id: Option<String>,
    session_id: Option<String>,
}

impl McpScope {
    fn key(&self) -> String {
        match (self.kind, &self.project_id, &self.session_id) {
            ("project", Some(p), _) => format!("project:{p}"),
            ("session", _, Some(s)) => format!("session:{s}"),
            _ => "global".to_owned(),
        }
    }

    fn to_scope_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "id": match self.kind {
                "project" => self.project_id.clone(),
                "session" => self.session_id.clone(),
                _ => None,
            },
        })
    }

    /// 解析后的 `effectiveSource` 标签。
    fn label(&self) -> String {
        match self.kind {
            "project" => format!("project:{}", self.project_id.as_deref().unwrap_or("")),
            "session" => format!("session:{}", self.session_id.as_deref().unwrap_or("")),
            _ => "global".to_owned(),
        }
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

fn parse_scope(rt: &PublicV1Runtime, input: &Value) -> Result<McpScope, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(invalid("input.scope is required"));
    };
    let id = scope.get("id").and_then(Value::as_str);
    match scope.get("kind").and_then(Value::as_str) {
        Some("global") => {
            if id.is_some() {
                return Err(invalid("scope.id must be null for kind=global"));
            }
            Ok(McpScope::global())
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
            Ok(McpScope {
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
            Ok(McpScope {
                kind: "session",
                project_id: Some(project_id.to_owned()),
                session_id: Some(session_id.to_owned()),
            })
        }
        _ => Err(invalid("scope.kind must be global|project|session")),
    }
}

// ---------------------------------------------------------------------------
// 配置校验（schema + 域规则 → 合同 Diagnostics）
// ---------------------------------------------------------------------------

/// 合同 `Diagnostic` 形态（`{code, path, message, severity}`）。
fn diagnostic(code: &str, path: Option<&str>, message: impl Into<String>, severity: &str) -> Value {
    json!({
        "code": code,
        "path": path,
        "message": message.into(),
        "severity": severity,
    })
}

fn schema_value() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| serde_json::from_str(MCP_SERVER_SCHEMA).unwrap_or(Value::Null))
}

/// 校验单个服务器配置对象，返回全部诊断（error 诊断使
/// validate/configure 拒绝）。
fn validate_server_config(conn: &Connection, config: &Value, index: Option<usize>) -> Vec<Value> {
    let mut diagnostics = Vec::new();
    let path_prefix = index.map(|i| format!("servers[{i}]")).unwrap_or_default();
    match agentcpm_pd_protocol::schema_validator::validate_against_schema(config, schema_value()) {
        Ok(()) => {}
        Err(errors) => {
            for error in errors {
                let path = if path_prefix.is_empty() {
                    error
                        .path
                        .trim_start_matches('$')
                        .trim_start_matches('.')
                        .to_owned()
                } else {
                    format!("{path_prefix}{}", error.path.trim_start_matches('$'))
                };
                diagnostics.push(diagnostic(
                    "SCHEMA_VIOLATION",
                    Some(&path),
                    error.message,
                    "error",
                ));
            }
        }
    }
    // 凭据引用必须解析（「凭据引用」验收锚）：credentials 域已登记同库表。
    if let Some(credential_ref) = config.get("credentialRef").and_then(Value::as_str) {
        let known: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM v1_credentials WHERE credential_id = ?1",
                params![credential_ref],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if known.is_none() {
            let path = if path_prefix.is_empty() {
                "credentialRef".to_owned()
            } else {
                format!("{path_prefix}.credentialRef")
            };
            diagnostics.push(diagnostic(
                "CREDENTIAL_NOT_FOUND",
                Some(&path),
                format!(
                    "credentialRef `{credential_ref}` does not resolve to a registered credential"
                ),
                "error",
            ));
        }
    }
    diagnostics
}

/// `mcp.validate` 的 `configuration`（单对象）与 `mcp.configure` 的
/// `servers[]` 共用的校验：schema 通过 + 重复名检测 + 凭据引用。
fn validate_server_set(conn: &Connection, servers: &[Value]) -> Vec<Value> {
    let mut diagnostics = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (index, config) in servers.iter().enumerate() {
        diagnostics.extend(validate_server_config(conn, config, Some(index)));
        if let Some(name) = config.get("name").and_then(Value::as_str)
            && let Some(first) = seen.insert(name, index)
        {
            diagnostics.push(diagnostic(
                "DUPLICATE_NAME",
                Some(&format!("servers[{index}].name")),
                format!("duplicate MCP server name `{name}` (first at servers[{first}])"),
                "error",
            ));
        }
    }
    diagnostics
}

fn has_error(diagnostics: &[Value]) -> bool {
    diagnostics
        .iter()
        .any(|d| d.get("severity").and_then(Value::as_str) == Some("error"))
}

/// 读回的脱敏配置：敏感键（configuration 域的 `sensitive_key`，即基线
/// `redactConfig` 的键集合）命中的 env/headers 值替换为 `<redacted>`。
fn sanitized_config(config: &Value) -> Value {
    let mut out = config.clone();
    for section in ["env", "headers"] {
        if let Some(map) = out.get_mut(section).and_then(Value::as_object_mut) {
            for (key, value) in map.iter_mut() {
                if sensitive_key(key) && value.is_string() {
                    *value = json!(REDACTED);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 存储行
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StoredMcpServer {
    server_id: String,
    scope: McpScope,
    transport: String,
    credential_ref: Option<String>,
    enabled: bool,
    generation: String,
    configuration_revision: String,
    config: Value,
}

fn row_to_server(row: &rusqlite::Row<'_>, scope: &McpScope) -> rusqlite::Result<StoredMcpServer> {
    let config_text: String = row.get(7)?;
    Ok(StoredMcpServer {
        server_id: row.get(0)?,
        scope: scope.clone(),
        transport: row.get(6)?,
        credential_ref: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        generation: row.get(10)?,
        configuration_revision: row.get(11)?,
        config: serde_json::from_str(&config_text).unwrap_or(Value::Null),
    })
}

const MCP_SERVER_COLUMNS: &str = "server_id, scope_key, scope_kind, project_id, session_id, name, transport, config_json, \
     credential_ref, enabled, generation, configuration_revision";

fn servers_in_scope(
    conn: &Connection,
    scope: &McpScope,
) -> Result<Vec<StoredMcpServer>, ContractError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {MCP_SERVER_COLUMNS} FROM v1_mcp_servers WHERE scope_key = ?1 ORDER BY server_id"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![scope.key()], |row| row_to_server(row, scope))
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    Ok(rows)
}

/// 有效视图：继承链自底向上覆盖（同名后写胜），返回按 serverId 排序的
/// 合并结果与各条目的来源 scope。
fn effective_servers(
    conn: &Connection,
    scope: &McpScope,
    include_inherited: bool,
) -> Result<Vec<StoredMcpServer>, ContractError> {
    let chain: Vec<McpScope> = if include_inherited {
        scope.chain()
    } else {
        vec![scope.clone()]
    };
    let mut merged: HashMap<String, StoredMcpServer> = HashMap::new();
    for layer in chain {
        for server in servers_in_scope(conn, &layer)? {
            // 后写（更内层 scope）覆盖同名。
            merged.insert(server.server_id.clone(), server);
        }
    }
    let mut list: Vec<StoredMcpServer> = merged.into_values().collect();
    list.sort_by(|a, b| a.server_id.cmp(&b.server_id));
    Ok(list)
}

/// scope 当前配置版本（无行 = `absent`）。
fn scope_revision(conn: &Connection, scope_key: &str) -> Result<String, ContractError> {
    let revision: Option<String> = conn
        .query_row(
            "SELECT revision FROM v1_mcp_scope_revisions WHERE scope_key = ?1",
            params![scope_key],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(revision.unwrap_or_else(|| REVISION_ABSENT.to_owned()))
}

/// 快照版本：按解析顺序拼接各贡献层的版本（稳定且能对出哪层动过）。
fn snapshot_revision(
    conn: &Connection,
    scope: &McpScope,
    inherited: bool,
) -> Result<String, ContractError> {
    let chain: Vec<McpScope> = if inherited {
        scope.chain()
    } else {
        vec![scope.clone()]
    };
    let mut parts = Vec::with_capacity(chain.len());
    for layer in chain {
        parts.push(format!(
            "{}:{}",
            layer.kind_short(),
            scope_revision(conn, &layer.key())?
        ));
    }
    Ok(parts.join("+"))
}

impl McpScope {
    fn kind_short(&self) -> &'static str {
        self.kind
    }
}

/// 共享 `meta.rev_counter` 的下一版本（policy.rs 同款：各域持有同一 SQL 的
/// 自有副本，避免跨域共享私有辅助）。
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

/// 事件表游标的唯一占位（store.rs 的 placeholder→`cur-<seq>` 模式）。
fn append_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let placeholder = format!("tmpmcp-{nanos}-{seq}");
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.event/1', ?5, ?5, ?6)",
        params![
            placeholder,
            resource_id,
            session_id,
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

/// 会话忙判定（S2 admission-fence 同款查询：活动 run 或排队 run）。
fn session_busy(conn: &Connection, session_id: &str) -> Result<bool, ContractError> {
    let active: Option<Option<String>> = conn
        .query_row(
            "SELECT active_run_id FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    // rb7 P1（t010 后续）：pin 指向终态（或缺失）run 时不算忙——与
    // submit_run / run.continue / resume_queue 的判定同源。
    let active_run_live = match active.flatten() {
        Some(run_id) => pinned_run_is_live(conn, run_id.as_str()).map_err(sql_err)?,
        None => false,
    };
    if active_run_live {
        return Ok(true);
    }
    let queued: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
            params![session_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    Ok(queued != 0)
}

/// 会话级配置当前版本（无行 = `absent`）。
fn session_config_revision(conn: &Connection, session_id: &str) -> Result<String, ContractError> {
    let revision: Option<String> = conn
        .query_row(
            "SELECT revision FROM v1_mcp_session_config WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(revision.unwrap_or_else(|| REVISION_ABSENT.to_owned()))
}

/// 会话配置 CAS 校验（`set_session_servers` / `set_permission_mode` 共用）。
fn check_session_revision(expected: &str, current: &str) -> Result<(), ContractError> {
    if expected != current {
        return Err(revision_conflict(expected, current));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 投影（合同 item / runtime 形态）
// ---------------------------------------------------------------------------

/// v1 运行时投影（见模块 doc 的诚实性裁决）。
fn runtime_json(server: &StoredMcpServer) -> Value {
    if server.enabled {
        json!({
            "state": "idle",
            "available": false,
            "reasonCode": REASON_MCP_RUNTIME_NOT_ASSEMBLED,
            "generation": server.generation,
        })
    } else {
        json!({
            "state": "disabled",
            "available": false,
            "reasonCode": Value::Null,
            "generation": server.generation,
        })
    }
}

fn server_item_json(server: &StoredMcpServer) -> Value {
    json!({
        "serverId": server.server_id,
        "name": server.server_id,
        "scope": server.scope.to_scope_json(),
        "configurationRevision": server.configuration_revision,
        "transport": server.transport,
        "credentialRef": server.credential_ref,
        "enabled": server.enabled,
        "runtime": runtime_json(server),
    })
}

/// Keyset 分页游标（tools.rs 同款 `after:<serverId>`）。
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
                    "malformed cursor `{cursor}` (expected after:<serverId>)"
                ))
            }),
    }
}

// ---------------------------------------------------------------------------
// mcp.list / mcp.get / mcp.validate（sync 只读）
// ---------------------------------------------------------------------------

async fn mcp_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<Value, ContractError> {
        let scope = parse_scope(&rt, &input)?;
        let include_inherited = input
            .get("includeInherited")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let after = parse_after_cursor(&input)?;

        let conn = store.conn.lock().expect("sqlite mutex");
        let servers = effective_servers(&conn, &scope, include_inherited)?;
        let mut items: Vec<Value> = Vec::new();
        for server in &servers {
            if let Some(after) = &after
                && &server.server_id <= after
            {
                continue;
            }
            items.push(server_item_json(server));
            if items.len() > limit {
                break;
            }
        }
        let next_cursor = if items.len() > limit {
            items.pop();
            items
                .last()
                .and_then(|item| item["serverId"].as_str())
                .map(|last| format!("after:{last}"))
        } else {
            None
        };
        let snapshot = snapshot_revision(&conn, &scope, include_inherited)?;
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

async fn mcp_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<Value, ContractError> {
        let scope = parse_scope(&rt, &input)?;
        let server_id = input
            .get("serverId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`serverId` is required"))?
            .to_owned();
        let conn = store.conn.lock().expect("sqlite mutex");
        let servers = effective_servers(&conn, &scope, true)?;
        let server = servers
            .iter()
            .find(|server| server.server_id == server_id)
            .ok_or_else(|| not_found("mcp server", &server_id))?;
        Ok(json!({
            "server": server_item_json(server),
            "schemaId": MCP_SERVER_SCHEMA_ID,
            "configuration": sanitized_config(&server.config),
            "effectiveSource": server.scope.label(),
            // 本构建未装配 MCP 运行时：从未握手 ⇒ 无已发现工具。
            "toolIds": [],
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

async fn mcp_validate(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        // 前置：目标范围存在（precondition「目标范围存在且主体已授权」）。
        parse_scope(&rt, &input)?;
        let schema_id = input
            .get("schemaId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if schema_id != MCP_SERVER_SCHEMA_ID {
            return Err(invalid(format!(
                "schemaId `{schema_id}` does not resolve to a registered schema \
                 (this runtime registers `{MCP_SERVER_SCHEMA_ID}`)"
            )));
        }
        let configuration = input
            .get("configuration")
            .cloned()
            .ok_or_else(|| invalid("`configuration` is required"))?;
        let conn = store.conn.lock().expect("sqlite mutex");
        let diagnostics = validate_server_config(&conn, &configuration, None);
        Ok(json!({
            "valid": !has_error(&diagnostics),
            "diagnostics": diagnostics,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

// ---------------------------------------------------------------------------
// mcp.configure / mcp.remove（sync 条件写）
// ---------------------------------------------------------------------------

/// 解析 configure 输入（scope 存在性 + schemaId 解析 + servers 数组）。
fn parse_configure(
    rt: &PublicV1Runtime,
    input: &Value,
) -> Result<(McpScope, Vec<Value>), ContractError> {
    let scope = parse_scope(rt, input)?;
    let schema_id = input
        .get("schemaId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema_id != MCP_SERVER_SCHEMA_ID {
        return Err(invalid(format!(
            "schemaId `{schema_id}` does not resolve to a registered schema \
             (this runtime registers `{MCP_SERVER_SCHEMA_ID}`)"
        )));
    }
    let servers = input
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid("`servers` is required"))?;
    Ok((scope, servers))
}

async fn mcp_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let (scope, servers) = parse_configure(&rt, &input)?;
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let mut conn = store.conn.lock().expect("sqlite mutex");
        let diagnostics = validate_server_set(&conn, &servers);
        if has_error(&diagnostics) {
            let mut error = invalid("mcp server set failed validation");
            let mut details = Map::new();
            details.insert("diagnostics".to_owned(), json!(diagnostics));
            error.details = Some(details);
            return Err(error);
        }
        let current = scope_revision(&conn, &scope.key())?;
        if expected != current {
            return Err(revision_conflict(&expected, &current));
        }

        let tx = conn.transaction().map_err(sql_err)?;
        // CAS 复验（校验期间可能被并发改动）。
        let current = scope_revision(&tx, &scope.key())?;
        if expected != current {
            return Err(revision_conflict(&expected, &current));
        }
        let new_revision = next_rev_in_tx(&tx)?;
        let existing: HashMap<String, (bool, String, String)> = servers_in_scope(&tx, &scope)?
            .into_iter()
            .map(|server| {
                (
                    server.server_id.clone(),
                    (server.enabled, server.generation, server.config.to_string()),
                )
            })
            .collect();
        for config in &servers {
            let server_id = config
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let transport = config
                .get("transport")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let credential_ref = config
                .get("credentialRef")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let config_text = serde_json::to_string(config)
                .map_err(|e| invalid(format!("server config serialization failed: {e}")))?;
            let now = now_rfc3339();
            // enabled：新 config 显式给出则用之；省略则沿用现状（TS
            // `setMcpServers` 的 enabled 保留语义）；新服务器默认 true。
            let enabled = config.get("enabled").and_then(Value::as_bool).unwrap_or_else(|| {
                existing.get(&server_id).map(|(enabled, _, _)| *enabled).unwrap_or(true)
            });
            let changed = existing
                .get(&server_id)
                .is_none_or(|(_, _, previous)| *previous != config_text);
            let generation = if changed {
                next_gen_counter(&tx)?
            } else {
                existing
                    .get(&server_id)
                    .map(|(_, generation, _)| generation.clone())
                    .unwrap_or_default()
            };
            tx.execute(
                "INSERT INTO v1_mcp_servers
                    (server_id, scope_key, scope_kind, project_id, session_id, name, transport,
                     config_json, credential_ref, enabled, generation, configuration_revision,
                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
                 ON CONFLICT(scope_key, server_id) DO UPDATE SET
                    transport = excluded.transport,
                    config_json = excluded.config_json,
                    credential_ref = excluded.credential_ref,
                    enabled = excluded.enabled,
                    generation = excluded.generation,
                    configuration_revision = excluded.configuration_revision,
                    updated_at = excluded.updated_at",
                params![
                    server_id,
                    scope.key(),
                    scope.kind,
                    scope.project_id,
                    scope.session_id,
                    server_id,
                    transport,
                    config_text,
                    credential_ref,
                    enabled,
                    generation,
                    new_revision,
                    now,
                ],
            )
            .map_err(sql_err)?;
        }
        // 整组替换：不在新集合里的旧服务器移除（会话绑定保留，同 remove）。
        let keep: Vec<&str> = servers
            .iter()
            .filter_map(|c| c.get("name").and_then(Value::as_str))
            .collect();
        let dropped = servers_in_scope(&tx, &scope)?
            .into_iter()
            .filter(|server| !keep.contains(&server.server_id.as_str()))
            .map(|server| server.server_id)
            .collect::<Vec<_>>();
        for server_id in &dropped {
            tx.execute(
                "DELETE FROM v1_mcp_servers WHERE scope_key = ?1 AND server_id = ?2",
                params![scope.key(), server_id],
            )
            .map_err(sql_err)?;
        }
        tx.execute(
            "INSERT INTO v1_mcp_scope_revisions (scope_key, revision, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_key) DO UPDATE SET revision = excluded.revision, updated_at = excluded.updated_at",
            params![scope.key(), new_revision, now_rfc3339()],
        )
        .map_err(sql_err)?;
        let session_column = scope.session_id.clone().unwrap_or_default();
        append_event_in_tx(
            &tx,
            &format!("mcp-config:{}", scope.key()),
            &session_column,
            "mcp.configurationChanged",
            &json!({
                "scope": scope.to_scope_json(),
                "revision": new_revision,
                "serverIds": keep.iter().map(|s| s.to_owned()).collect::<Vec<_>>(),
                "removedServerIds": dropped,
            }),
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(json!({
            "revision": new_revision,
            // 保存与运行生效分离：v1 无运行代际，永不宣称已生效。
            "applied": false,
            "generation": Value::Null,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

/// 代际计数（v1_mcp_scope_revisions 之外的全局小计数器，借用
/// meta 的 `mcp_generation_counter` 键，与版本计数分开）。
fn next_gen_counter(conn: &Connection) -> Result<String, ContractError> {
    let current: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'mcp_generation_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .unwrap_or(0);
    let next = current + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('mcp_generation_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("mcp-gen-{next:08}"))
}

async fn mcp_remove(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let scope = parse_scope(&rt, &input)?;
        let server_id = input
            .get("serverId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`serverId` is required"))?
            .to_owned();
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let mut conn = store.conn.lock().expect("sqlite mutex");
        let current = scope_revision(&conn, &scope.key())?;
        if expected != current {
            return Err(revision_conflict(&expected, &current));
        }
        // 只看本 scope 自有配置（「不删除同名继承配置」）。
        if servers_in_scope(&conn, &scope)?
            .iter()
            .all(|server| server.server_id != server_id)
        {
            return Err(not_found("mcp server", &server_id));
        }

        let tx = conn.transaction().map_err(sql_err)?;
        let current = scope_revision(&tx, &scope.key())?;
        if expected != current {
            return Err(revision_conflict(&expected, &current));
        }
        let new_revision = next_rev_in_tx(&tx)?;
        tx.execute(
            "DELETE FROM v1_mcp_servers WHERE scope_key = ?1 AND server_id = ?2",
            params![scope.key(), server_id],
        )
        .map_err(sql_err)?;
        // 保留绑定：仍引用该 server 的会话启用集合（显式报告，不静默级联删）。
        let mut stmt = tx
            .prepare("SELECT session_id FROM v1_mcp_session_servers WHERE server_id = ?1 ORDER BY session_id")
            .map_err(sql_err)?;
        let retained: Vec<String> = stmt
            .query_map(params![server_id], |r| r.get(0))
            .map_err(sql_err)?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        tx.execute(
            "INSERT INTO v1_mcp_scope_revisions (scope_key, revision, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_key) DO UPDATE SET revision = excluded.revision, updated_at = excluded.updated_at",
            params![scope.key(), new_revision, now_rfc3339()],
        )
        .map_err(sql_err)?;
        let session_column = scope.session_id.clone().unwrap_or_default();
        append_event_in_tx(
            &tx,
            &server_id,
            &session_column,
            "mcp.configurationChanged",
            &json!({
                "scope": scope.to_scope_json(),
                "revision": new_revision,
                "removedServerIds": [server_id],
                "retainedBindings": retained,
            }),
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(json!({
            "revision": new_revision,
            "applied": false,
            "generation": Value::Null,
            "retainedBindings": retained,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

// ---------------------------------------------------------------------------
// mcp.transition（async, retry=idempotentSubmit）
// ---------------------------------------------------------------------------

/// 本域的 DomainDb（受理记录与 schedules/background_tasks 共表
/// `v1_domain_operations`）。
fn mcp_domain_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
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

async fn mcp_transition(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let db = mcp_domain_db(&rt)?;
    let (scope, server_id, action, expected_generation) = {
        let scope = parse_scope(&rt, &input)?;
        let server_id = input
            .get("serverId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`serverId` is required"))?
            .to_owned();
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let expected_generation = input
            .get("expectedGeneration")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        (scope, server_id, action, expected_generation)
    };
    let store = rt.store().clone();
    let session_id_for_events = scope.session_id.clone().unwrap_or_default();

    let operation = accept_domain_operation(
        &db,
        "mcp.transition",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", store.next_id()),
        || {
            let conn = store.conn.lock().expect("sqlite mutex");
            // 只看本 scope 自有配置（transition 操作的是该 scope 声明的服务器）。
            let server = servers_in_scope(&conn, &scope)?
                .into_iter()
                .find(|server| server.server_id == server_id)
                .ok_or_else(|| not_found("mcp server", &server_id))?;
            if server.generation != expected_generation {
                return Err(ContractError::new(
                    CODE_GENERATION_CONFLICT,
                    format!(
                        "expectedGeneration `{expected_generation}` does not match `{}`",
                        server.generation
                    ),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                ));
            }
            if scope.kind == "session"
                && session_busy(&conn, scope.session_id.as_deref().unwrap_or_default())?
            {
                return Err(ContractError::new(
                    CODE_SESSION_BUSY,
                    format!(
                        "session `{}` has an active or queued run; mcp transitions are refused \
                         while the session is busy",
                        scope.session_id.as_deref().unwrap_or_default()
                    ),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                ));
            }
            Ok(())
        },
    )?;
    let operation_id = operation.operation_id.clone();

    // 执行体分离：受理记录先落，掉线不取消也不重复执行。
    let db_for_task = db.clone();
    let store_for_task = store.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&db_for_task, &operation_id, &session_id_for_events);
        let completion = mcp_transition_execute(
            &store_for_task,
            &scope,
            &server_id,
            &action,
            &expected_generation,
        );
        match completion {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &db_for_task,
                    &operation_id,
                    &session_id_for_events,
                    "succeeded",
                    &completion,
                );
            }
            Err((code, message)) => {
                let _ = finish_domain_operation(
                    &db_for_task,
                    &operation_id,
                    &session_id_for_events,
                    "failed",
                    &json!({"code": code, "message": message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// transition 执行：enable/disable 是配置面状态机（诚实成功）；
/// connect/reconnect 需要真实握手，本构建无 MCP 运行时 → 诚实失败。
fn mcp_transition_execute(
    store: &Arc<V1Store>,
    scope: &McpScope,
    server_id: &str,
    action: &str,
    expected_generation: &str,
) -> Result<Value, (String, String)> {
    let mut conn = store
        .conn
        .lock()
        .map_err(|_| ("INTERNAL".to_owned(), "sqlite mutex poisoned".to_owned()))?;
    let result = (|| -> Result<Value, ContractError> {
        let tx = conn.transaction().map_err(sql_err)?;
        let server = servers_in_scope(&tx, scope)?
            .into_iter()
            .find(|server| server.server_id == server_id)
            .ok_or_else(|| not_found("mcp server", server_id))?;
        if server.generation != expected_generation {
            return Err(ContractError::new(
                CODE_GENERATION_CONFLICT,
                format!(
                    "expectedGeneration `{expected_generation}` does not match `{}`",
                    server.generation
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        match action {
            "enable" | "disable" => {
                let enabled = action == "enable";
                let generation = next_gen_counter(&tx)?;
                tx.execute(
                    "UPDATE v1_mcp_servers SET enabled = ?3, generation = ?4, updated_at = ?5
                     WHERE scope_key = ?1 AND server_id = ?2",
                    params![scope.key(), server_id, enabled, generation, now_rfc3339()],
                )
                .map_err(sql_err)?;
                append_event_in_tx(
                    &tx,
                    server_id,
                    &scope.session_id.clone().unwrap_or_default(),
                    "mcp.configurationChanged",
                    &json!({
                        "scope": scope.to_scope_json(),
                        "serverId": server_id,
                        "action": action,
                        "generation": generation,
                    }),
                )?;
                tx.commit().map_err(sql_err)?;
                let state = if enabled { "idle" } else { "disabled" };
                Ok(json!({
                    "serverId": server_id,
                    "state": state,
                    "generation": generation,
                    "toolIds": [],
                }))
            }
            "connect" | "reconnect" => Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!(
                    "mcp runtime is not assembled in this build; `{action}` cannot perform a real \
                     handshake for server `{server_id}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )),
            other => Err(invalid(format!("unsupported action `{other}`"))),
        }
    })();
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err((error.code, error.message)),
    }
}

// ---------------------------------------------------------------------------
// mcp.set_session_servers / mcp.set_permission_mode（sync 条件写）
// ---------------------------------------------------------------------------

async fn mcp_set_session_servers(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let session_id = input
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`sessionId` is required"))?
            .to_owned();
        let server_ids: Vec<String> = input
            .get("serverIds")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .ok_or_else(|| invalid("`serverIds` is required"))?;
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        // 会话必须存在（合同错误表有 NOT_FOUND）。取回后再拿连接锁——
        // `get_session` 内部也要同一把锁。
        let session = store
            .get_session(&session_id)
            .map_err(|_| not_found("session", &session_id))?;
        let project_id = session["session"]["projectId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let scope = McpScope {
            kind: "session",
            project_id: Some(project_id),
            session_id: Some(session_id.clone()),
        };
        let mut conn = store.conn.lock().expect("sqlite mutex");
        // 未知或未授权 server 拒绝：必须在会话的有效视图内。
        let effective = effective_servers(&conn, &scope, true)?;
        let known: Vec<&str> = effective.iter().map(|s| s.server_id.as_str()).collect();
        for server_id in &server_ids {
            if !known.contains(&server_id.as_str()) {
                return Err(not_found("mcp server", server_id));
            }
        }
        // 会话运行中变更 → SESSION_BUSY（不静默半生效）。
        if session_busy(&conn, &session_id)? {
            return Err(ContractError::new(
                CODE_SESSION_BUSY,
                format!(
                    "session `{session_id}` has an active or queued run; the mcp server set \
                     cannot change while the session is busy"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let current = session_config_revision(&conn, &session_id)?;
        check_session_revision(&expected, &current)?;

        let tx = conn.transaction().map_err(sql_err)?;
        let current = session_config_revision(&tx, &session_id)?;
        check_session_revision(&expected, &current)?;
        let new_revision = next_rev_in_tx(&tx)?;
        tx.execute(
            "INSERT INTO v1_mcp_session_config (session_id, revision, permission_mode, permission_mode_source, updated_at)
             VALUES (?1, ?2, NULL, NULL, ?3)
             ON CONFLICT(session_id) DO UPDATE SET revision = excluded.revision, updated_at = excluded.updated_at",
            params![session_id, new_revision, now_rfc3339()],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM v1_mcp_session_servers WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(sql_err)?;
        for (position, server_id) in server_ids.iter().enumerate() {
            tx.execute(
                "INSERT INTO v1_mcp_session_servers (session_id, server_id, position)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id, server_id) DO UPDATE SET position = excluded.position",
                params![session_id, server_id, position as i64],
            )
            .map_err(sql_err)?;
        }
        append_event_in_tx(
            &tx,
            &session_id,
            &session_id,
            "session.configurationChanged",
            &json!({
                "sessionId": session_id,
                "change": "mcpServers",
                "serverIds": server_ids,
                "revision": new_revision,
            }),
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(json!({
            "revision": new_revision,
            "applied": false,
            "generation": Value::Null,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

async fn mcp_set_permission_mode(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let session_id = input
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("`sessionId` is required"))?
            .to_owned();
        let mode = input.get("mode").cloned().unwrap_or(Value::Null);
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        // 会话存在性先于连接锁（`get_session` 内部取同一把锁）。
        store
            .get_session(&session_id)
            .map_err(|_| not_found("session", &session_id))?;
        let mut conn = store.conn.lock().expect("sqlite mutex");
        // 已注册权限模式；null 清除覆盖（恢复继承）。
        let mode = match &mode {
            Value::Null => None,
            Value::String(value) if PERMISSION_MODES.contains(&value.as_str()) => {
                Some(value.clone())
            }
            other => {
                return Err(invalid(format!(
                    "mode must be one of [{}] or null; got `{}`",
                    PERMISSION_MODES.join(", "),
                    other
                )))
            }
        };
        if session_busy(&conn, &session_id)? {
            return Err(ContractError::new(
                CODE_SESSION_BUSY,
                format!(
                    "session `{session_id}` has an active or queued run; the mcp permission \
                     override cannot change while the session is busy"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let current = session_config_revision(&conn, &session_id)?;
        check_session_revision(&expected, &current)?;

        let tx = conn.transaction().map_err(sql_err)?;
        let current = session_config_revision(&tx, &session_id)?;
        check_session_revision(&expected, &current)?;
        let new_revision = next_rev_in_tx(&tx)?;
        // source 标记覆盖的来源：设置过 override 的行携带 `override`，清除后
        // 回到继承（行保留历史，`permission_mode` 为 NULL）。
        let source = mode.as_ref().map(|_| "override".to_owned());
        tx.execute(
            "INSERT INTO v1_mcp_session_config (session_id, revision, permission_mode, permission_mode_source, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
                revision = excluded.revision,
                permission_mode = excluded.permission_mode,
                permission_mode_source = excluded.permission_mode_source,
                updated_at = excluded.updated_at",
            params![
                session_id,
                new_revision,
                mode,
                source,
                now_rfc3339(),
            ],
        )
        .map_err(sql_err)?;
        append_event_in_tx(
            &tx,
            &session_id,
            &session_id,
            "session.configurationChanged",
            &json!({
                "sessionId": session_id,
                "change": "mcpPermissionMode",
                "mode": mode,
                "revision": new_revision,
            }),
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(json!({
            "revision": new_revision,
            "applied": false,
            "generation": Value::Null,
        }))
    })
    .await
    .map_err(join_err)??;
    Ok((ResponseOutcome::Succeeded, outcome))
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

    fn global_scope() -> Value {
        json!({"kind": "global"})
    }

    fn stdio_server(name: &str) -> Value {
        json!({
            "name": name,
            "transport": "stdio",
            "command": "/usr/bin/env",
            "args": ["true"],
        })
    }

    /// 配置一个服务器并返回新 revision。
    async fn configure_one(rt: &Arc<PublicV1Runtime>, scope: Value, server: Value) -> Value {
        rt.dispatch(
            "mcp.configure",
            json!({
                "scope": scope,
                "schemaId": MCP_SERVER_SCHEMA_ID,
                "servers": [server],
                "expectedRevision": REVISION_ABSENT,
            }),
            None,
            None,
        )
        .await
        .unwrap()
        .1
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
    async fn configure_validates_schema_and_reports_diagnostics() {
        let (_tmp, root) = fresh_home("mcp-validate");
        let (rt, _iid) = open_runtime(&root);

        // schemaId 必须解析到注册 schema。
        let error = dispatch_err(
            &rt,
            "mcp.validate",
            json!({
                "scope": global_scope(),
                "schemaId": "agentcpm-pd.mcp.server/2",
                "configuration": stdio_server("x"),
            }),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 未知字段（任意 method/shell）→ error 诊断。
        let result = dispatch_ok(
            &rt,
            "mcp.validate",
            json!({
                "scope": global_scope(),
                "schemaId": MCP_SERVER_SCHEMA_ID,
                "configuration": {
                    "name": "evil",
                    "transport": "stdio",
                    "command": "sh",
                    "shell": "/bin/bash -c",
                },
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(false));
        let codes: Vec<&str> = result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["code"].as_str().unwrap())
            .collect();
        assert!(codes.contains(&"SCHEMA_VIOLATION"));

        // stdio 缺 command（transport 条件必填）。
        let result = dispatch_ok(
            &rt,
            "mcp.validate",
            json!({
                "scope": global_scope(),
                "schemaId": MCP_SERVER_SCHEMA_ID,
                "configuration": {"name": "no-cmd", "transport": "stdio"},
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(false));

        // 合法配置 → valid。
        let result = dispatch_ok(
            &rt,
            "mcp.validate",
            json!({
                "scope": global_scope(),
                "schemaId": MCP_SERVER_SCHEMA_ID,
                "configuration": stdio_server("ok"),
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(true));
        assert_eq!(result["diagnostics"], json!([]));

        // 凭据引用不解析 → CREDENTIAL_NOT_FOUND。
        let result = dispatch_ok(
            &rt,
            "mcp.validate",
            json!({
                "scope": global_scope(),
                "schemaId": MCP_SERVER_SCHEMA_ID,
                "configuration": {
                    "name": "with-cred",
                    "transport": "http",
                    "url": "https://mcp.example.test/rpc",
                    "credentialRef": "cred-missing",
                },
            }),
        )
        .await;
        assert_eq!(result["valid"], json!(false));
        assert!(
            result["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == json!("CREDENTIAL_NOT_FOUND"))
        );
    }

    #[tokio::test]
    async fn configure_cas_and_atomic_replacement() {
        let (_tmp, root) = fresh_home("mcp-configure");
        let (rt, _iid) = open_runtime(&root);
        let scope = global_scope();

        // 创建：absent。
        let result = configure_one(&rt, scope.clone(), stdio_server("alpha")).await;
        assert_eq!(result["applied"], json!(false));
        assert_eq!(result["generation"], Value::Null);
        let rev1 = result["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev1, REVISION_ABSENT);

        // 重复名 → 拒绝（不落半份配置）。
        let error = dispatch_err(
            &rt,
            "mcp.configure",
            json!({
                "scope": scope,
                "schemaId": MCP_SERVER_SCHEMA_ID,
                "servers": [stdio_server("beta"), stdio_server("beta")],
                "expectedRevision": &rev1,
            }),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        // 上面失败后配置未变：list 仍只有 alpha。
        let listed = dispatch_ok(&rt, "mcp.list", json!({"scope": global_scope()})).await;
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);

        // 过期 revision → REVISION_CONFLICT（验收「过期写冲突」）。
        let error = dispatch_err(
            &rt,
            "mcp.configure",
            json!({
                "scope": scope,
                "schemaId": MCP_SERVER_SCHEMA_ID,
                "servers": [stdio_server("beta")],
                "expectedRevision": REVISION_ABSENT,
            }),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        // 整组替换：alpha 保留，beta 新增。
        let result = rt
            .dispatch(
                "mcp.configure",
                json!({
                    "scope": scope,
                    "schemaId": MCP_SERVER_SCHEMA_ID,
                    "servers": [stdio_server("alpha"), stdio_server("beta")],
                    "expectedRevision": &rev1,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        let rev2 = result["revision"].as_str().unwrap().to_owned();
        let listed = dispatch_ok(&rt, "mcp.list", json!({"scope": global_scope()})).await;
        let items = listed["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 2);
        let find = |name: &str| {
            items
                .iter()
                .find(|i| i["serverId"] == json!(name))
                .unwrap()
                .clone()
        };
        let alpha = find("alpha");
        assert_eq!(alpha["enabled"], json!(true));
        assert_eq!(alpha["runtime"]["available"], json!(false));
        assert_eq!(
            alpha["runtime"]["reasonCode"],
            json!(REASON_MCP_RUNTIME_NOT_ASSEMBLED)
        );
        assert!(alpha["runtime"]["generation"].as_str().is_some());
        // 沿用 TS 语义：重设时未给 enabled → 保留原值（true）。
        assert_eq!(find("beta")["configurationRevision"], json!(rev2));
    }

    #[tokio::test]
    async fn list_get_and_inheritance() {
        let (_tmp, root) = fresh_home("mcp-list");
        let (rt, _iid) = open_runtime(&root);
        let instance_id = rt.store().meta().instance_id.clone();
        rt.store()
            .register_project(&json!({
                "projectId": "proj-mcp",
                "name": "MCP Proj",
                "environmentId": "local",
                "root": {"instanceId": instance_id, "rootId": "root-x", "relativePath": "ws"},
            }))
            .unwrap();
        rt.store()
            .create_session(&json!({"sessionId": "sess-mcp", "projectId": "proj-mcp"}))
            .unwrap();

        configure_one(&rt, global_scope(), stdio_server("shared")).await;
        let project_scope = json!({"kind": "project", "id": "proj-mcp"});
        let result = rt
            .dispatch(
                "mcp.configure",
                json!({
                    "scope": project_scope,
                    "schemaId": MCP_SERVER_SCHEMA_ID,
                    "servers": [stdio_server("shared"), {
                        "name": "proj-only",
                        "transport": "http",
                        "url": "https://mcp.example.test/rpc",
                    }],
                    "expectedRevision": REVISION_ABSENT,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;

        // 继承 + 覆盖：项目视图看到覆盖后的 shared + proj-only。
        let listed = dispatch_ok(&rt, "mcp.list", json!({"scope": project_scope})).await;
        let items = listed["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 2);
        let shared = items
            .iter()
            .find(|i| i["serverId"] == json!("shared"))
            .unwrap();
        assert_eq!(
            shared["scope"],
            json!({"kind": "project", "id": "proj-mcp"})
        );
        // includeInherited=false 只看本层。
        let listed = dispatch_ok(
            &rt,
            "mcp.list",
            json!({"scope": project_scope, "includeInherited": false}),
        )
        .await;
        assert_eq!(listed["items"].as_array().unwrap().len(), 2);

        // 全局视图不含项目专属服务器。
        let listed = dispatch_ok(&rt, "mcp.list", json!({"scope": global_scope()})).await;
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);

        // get：脱敏 + effectiveSource（project 层覆盖了同名的 global 配置）。
        let got = dispatch_ok(
            &rt,
            "mcp.get",
            json!({"scope": project_scope, "serverId": "proj-only"}),
        )
        .await;
        assert_eq!(got["schemaId"], json!(MCP_SERVER_SCHEMA_ID));
        assert_eq!(got["effectiveSource"], json!("project:proj-mcp"));
        assert_eq!(
            got["configuration"]["url"],
            json!("https://mcp.example.test/rpc")
        );
        assert_eq!(got["toolIds"], json!([]));
        let got = dispatch_ok(
            &rt,
            "mcp.get",
            json!({"scope": project_scope, "serverId": "shared"}),
        )
        .await;
        assert_eq!(got["effectiveSource"], json!("project:proj-mcp"));

        // 未知 server → NOT_FOUND。
        let error = dispatch_err(
            &rt,
            "mcp.get",
            json!({"scope": project_scope, "serverId": "nope"}),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // 脱敏：敏感 header 值不回显。
        let result = rt
            .dispatch(
                "mcp.configure",
                json!({
                    "scope": project_scope,
                    "schemaId": MCP_SERVER_SCHEMA_ID,
                    "servers": [{
                        "name": "with-headers",
                        "transport": "http",
                        "url": "https://mcp.example.test/rpc",
                        "headers": {"Authorization": "Bearer should-not-leak"},
                    }],
                    "expectedRevision": result["revision"],
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        let _ = result;
        let got = dispatch_ok(
            &rt,
            "mcp.get",
            json!({"scope": project_scope, "serverId": "with-headers"}),
        )
        .await;
        assert_eq!(
            got["configuration"]["headers"]["Authorization"],
            json!(REDACTED)
        );

        // 分页：after 游标无损（此时有效视图 = global shared + project
        // with-headers，共 2 台）。
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut input = json!({"scope": project_scope, "limit": 1});
            if let Some(cursor) = &cursor {
                input["cursor"] = json!(cursor);
            }
            let page = dispatch_ok(&rt, "mcp.list", input).await;
            seen.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["serverId"].as_str().unwrap().to_owned()),
            );
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        assert_eq!(seen.len(), 2, "paginated walk covers every server once");
    }

    #[tokio::test]
    async fn remove_scoped_and_retained_bindings() {
        let (_tmp, root) = fresh_home("mcp-remove");
        let (rt, _iid) = open_runtime(&root);
        let instance_id = rt.store().meta().instance_id.clone();
        rt.store()
            .register_project(&json!({
                "projectId": "proj-rm",
                "name": "RM",
                "environmentId": "local",
                "root": {"instanceId": instance_id, "rootId": "root-x", "relativePath": "ws"},
            }))
            .unwrap();
        rt.store()
            .create_session(&json!({"sessionId": "sess-rm", "projectId": "proj-rm"}))
            .unwrap();

        let rev = configure_one(&rt, global_scope(), stdio_server("gone")).await;
        let rev = rev["revision"].as_str().unwrap().to_owned();
        // 会话绑定该服务器。
        dispatch_ok(
            &rt,
            "mcp.set_session_servers",
            json!({
                "sessionId": "sess-rm",
                "serverIds": ["gone"],
                "expectedRevision": REVISION_ABSENT,
            }),
        )
        .await;

        let result = rt
            .dispatch(
                "mcp.remove",
                json!({
                    "scope": global_scope(),
                    "serverId": "gone",
                    "expectedRevision": rev,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(result["retainedBindings"], json!(["sess-rm"]));
        // 移除后不存在。
        let error = dispatch_err(
            &rt,
            "mcp.remove",
            json!({
                "scope": global_scope(),
                "serverId": "gone",
                "expectedRevision": result["revision"],
            }),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn transition_state_machine_and_honest_connect_failure() {
        let (_tmp, root) = fresh_home("mcp-transition");
        let (rt, _iid) = open_runtime(&root);
        configure_one(&rt, global_scope(), stdio_server("t-server")).await;

        let listed = dispatch_ok(&rt, "mcp.list", json!({"scope": global_scope()})).await;
        let generation = listed["items"][0]["runtime"]["generation"]
            .as_str()
            .unwrap()
            .to_owned();

        // disable：配置面状态机成功。
        let (_, receipt) = rt
            .dispatch(
                "mcp.transition",
                json!({
                    "scope": global_scope(),
                    "serverId": "t-server",
                    "action": "disable",
                    "expectedGeneration": generation,
                }),
                Some("key-disable"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], json!("operation"));
        let op = wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(op["state"], json!("succeeded"));
        assert_eq!(op["result"]["state"], json!("disabled"));
        let generation2 = op["result"]["generation"].as_str().unwrap().to_owned();
        assert_ne!(generation2, generation);

        // 旧 generation 再 transition → GENERATION_CONFLICT。
        let error = dispatch_err(
            &rt,
            "mcp.transition",
            json!({
                "scope": global_scope(),
                "serverId": "t-server",
                "action": "enable",
                "expectedGeneration": generation,
            }),
        )
        .await;
        assert_eq!(error.code, "GENERATION_CONFLICT");

        // enable 恢复。
        let (_, receipt) = rt
            .dispatch(
                "mcp.transition",
                json!({
                    "scope": global_scope(),
                    "serverId": "t-server",
                    "action": "enable",
                    "expectedGeneration": generation2,
                }),
                Some("key-enable"),
                None,
            )
            .await
            .unwrap();
        let op = wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(op["state"], json!("succeeded"));
        assert_eq!(op["result"]["state"], json!("idle"));
        assert_eq!(op["result"]["toolIds"], json!([]));

        // connect：无运行时 → 诚实失败（不假报可用）。
        let generation3 = op["result"]["generation"].as_str().unwrap().to_owned();
        let (_, receipt) = rt
            .dispatch(
                "mcp.transition",
                json!({
                    "scope": global_scope(),
                    "serverId": "t-server",
                    "action": "connect",
                    "expectedGeneration": generation3,
                }),
                Some("key-connect"),
                None,
            )
            .await
            .unwrap();
        let op = wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(op["state"], json!("failed"));
        assert_eq!(op["result"]["code"], json!("UNAVAILABLE"));

        // 未知服务器 / 幂等重放。
        let error = dispatch_err(
            &rt,
            "mcp.transition",
            json!({
                "scope": global_scope(),
                "serverId": "missing",
                "action": "enable",
                "expectedGeneration": "g",
            }),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // 同 key + 同 input 重放（不重复执行）。
        let (_, replay) = rt
            .dispatch(
                "mcp.transition",
                json!({
                    "scope": global_scope(),
                    "serverId": "t-server",
                    "action": "enable",
                    "expectedGeneration": generation2,
                }),
                Some("key-enable"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], json!(true));
    }

    #[tokio::test]
    async fn session_servers_and_permission_mode_cas() {
        let (_tmp, root) = fresh_home("mcp-session");
        let (rt, _iid) = open_runtime(&root);
        let instance_id = rt.store().meta().instance_id.clone();
        rt.store()
            .register_project(&json!({
                "projectId": "proj-sess",
                "name": "S",
                "environmentId": "local",
                "root": {"instanceId": instance_id, "rootId": "root-x", "relativePath": "ws"},
            }))
            .unwrap();
        rt.store()
            .create_session(&json!({"sessionId": "sess-1", "projectId": "proj-sess"}))
            .unwrap();
        configure_one(&rt, global_scope(), stdio_server("srv-a")).await;

        // 未知 server → NOT_FOUND。
        let error = dispatch_err(
            &rt,
            "mcp.set_session_servers",
            json!({
                "sessionId": "sess-1",
                "serverIds": ["nope"],
                "expectedRevision": REVISION_ABSENT,
            }),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // 设置成功，返回新 revision。
        let result = dispatch_ok(
            &rt,
            "mcp.set_session_servers",
            json!({
                "sessionId": "sess-1",
                "serverIds": ["srv-a"],
                "expectedRevision": REVISION_ABSENT,
            }),
        )
        .await;
        let rev = result["revision"].as_str().unwrap().to_owned();

        // 会话配置 revision 被 server 设置占用：permission mode 用旧值 → 冲突。
        let error = dispatch_err(
            &rt,
            "mcp.set_permission_mode",
            json!({
                "sessionId": "sess-1",
                "mode": "auto",
                "expectedRevision": REVISION_ABSENT,
            }),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        // 用当前 revision 设置 mode；非法 mode 拒绝。
        let error = dispatch_err(
            &rt,
            "mcp.set_permission_mode",
            json!({
                "sessionId": "sess-1",
                "mode": "yolo",
                "expectedRevision": &rev,
            }),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        let result = dispatch_ok(
            &rt,
            "mcp.set_permission_mode",
            json!({
                "sessionId": "sess-1",
                "mode": "default",
                "expectedRevision": &rev,
            }),
        )
        .await;
        let rev2 = result["revision"].as_str().unwrap().to_owned();

        // null 清除覆盖（恢复继承）。
        let result = dispatch_ok(
            &rt,
            "mcp.set_permission_mode",
            json!({
                "sessionId": "sess-1",
                "mode": Value::Null,
                "expectedRevision": &rev2,
            }),
        )
        .await;
        assert_ne!(result["revision"], json!(rev2));

        // 未知会话 → NOT_FOUND。
        let error = dispatch_err(
            &rt,
            "mcp.set_permission_mode",
            json!({
                "sessionId": "sess-missing",
                "mode": Value::Null,
                "expectedRevision": REVISION_ABSENT,
            }),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn session_scope_transition_refused_while_busy() {
        let (_tmp, root) = fresh_home("mcp-busy");
        let (rt, _iid) = open_runtime(&root);
        let instance_id = rt.store().meta().instance_id.clone();
        rt.store()
            .register_project(&json!({
                "projectId": "proj-busy",
                "name": "B",
                "environmentId": "local",
                "root": {"instanceId": instance_id, "rootId": "root-x", "relativePath": "ws"},
            }))
            .unwrap();
        rt.store()
            .create_session(&json!({"sessionId": "sess-busy", "projectId": "proj-busy"}))
            .unwrap();
        let session_scope = json!({"kind": "session", "id": "sess-busy"});
        configure_one(&rt, session_scope.clone(), stdio_server("busy-srv")).await;
        let view = dispatch_ok(&rt, "mcp.list", json!({"scope": session_scope})).await;
        let generation = view["items"][0]["runtime"]["generation"]
            .as_str()
            .unwrap()
            .to_owned();

        // 制造忙会话：活动 run。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE sessions SET active_run_id = 'run-busy' WHERE session_id = 'sess-busy'",
                [],
            )
            .unwrap();
        }
        let error = dispatch_err(
            &rt,
            "mcp.transition",
            json!({
                "scope": session_scope,
                "serverId": "busy-srv",
                "action": "disable",
                "expectedGeneration": generation,
            }),
        )
        .await;
        assert_eq!(error.code, "SESSION_BUSY");
    }
}
