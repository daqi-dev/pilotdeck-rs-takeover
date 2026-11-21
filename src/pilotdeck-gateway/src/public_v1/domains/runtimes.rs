//! `runtimes.*` — external execution environments and their bindings
//! (`cli.task.runtime`, S3; capability `cli.capability.runtime-modules`).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `runtimes.list` / `.describe` /
//! `.configure` / `.connect` / `.release` 条目。TS 基线（cc7c4e6）的语义参考是
//! 旧 Rust 线 `host/`（execution/connection/budget/recovery）与 `module_host/`
//! ——**只读参考**：那是 Module 2.0 的完整进程协议栈，v1 按本轮公共合同新面
//! 实现（薄实现路线，任务简报授权，差异登记在任务报告）。
//!
//! 薄实现的核心取舍（全部对着合同验收锚）：
//!
//! * **native 运行时是进程本身**：id 固定为 `native`，状态恒为 ready（实例
//!   代际即其 generation），不落表。默认选择来源 `selectionSource` 无显式
//!   选择时为 `"builtin-native"`，makeDefault 后为 `"configured"`；选择是
//!   scope 本地的（合同未要求继承；models 域的 model_selection 继承链不搬
//!   用——差异登记）。
//! * **stdio/TCP 的「实际装配」= 受管子进程/持有连接 + 一次协议握手**。
//!   v1 运行时协议是单行 NDJSON 握手（`agentcpm-pd.runtime/1`，v1 新面的
//!   线格式由本域定义，报告登记）：对端在连接建立后输出一行
//!   `{"protocolVersion":"agentcpm-pd.runtime/1","capabilityIds":[...],
//!     "operations":[...],"limits":{...}}`。握手成功才 ready（「协议握手成
//!   功才ready」）；版本不匹配 → `PROTOCOL_UNSUPPORTED`（「版本不匹配明确
//!   不可用，不进行旧JS工厂函数加载」——v1 永不加载 JS 工厂函数）。
//! * **资源归属**：stdio 的子进程归 connect 受理的操作所有（进程内 live 注
//!   册表 + `kill_on_drop`），release 只回收自己拥有的子进程；TCP 只断开
//!   本连接（注册表持有拆分的读写半连接），不停止远端共享服务（「资源释放
//!   归属明确」）。native 无法释放（它就是网关进程），release 的
//!   `retainedResources` 明确报告 `["native"]`。
//! * **断线不重放副作用**：connect/release 是 `retry=idempotentSubmit` 的
//!   异步操作（collections 长操作缝：`operations` 表 + `operation.*` 事件，
//!   可经 `operations.cancel` 取消）；断连后同 key 重试重放受理 receipt，
//!   绝不自动重跑连接/杀进程这类副作用动作。connect 驱动在握手等待期间轮
//!   询取消意图：取消的 connect 回到 configured（未装配）、操作终态为
//!   cancelled（「断连/超时真实失败，无副作用任务自动重放」）。
//! * **重复请求不启动多个子进程**：同 key 去重走 `idempotency_records`；
//!   不同 key 撞上 in-flight 连接时挂起等首个连接终态后按其结果收尾（不
//!   spawn 第二个进程）；已 ready 的运行时再次 connect 是零子进程的即时成
//!   功（重放既有 generation）。
//! * **能力声明与可调用一致**：`runtimes.list`/`describe` 的 capabilityIds /
//!   supportedOperations / limits 全部来自握手落库的实际协商结果；未连接
//!   的 stdio/TCP 运行时 capabilityIds/supportedOperations 为空、
//!   maxConcurrentRuns=0（未协商），`available=false` + `reasonCode`（「不
//!   得由安装成功推导可用」「声明与可调用一致」）。
//!
//! 状态机（`connect_state` 列 + `enabled` 列）：configured（已配置未启动）→
//! starting（连接中）→ ready（握手成功）／ failed（握手或连接失败，带
//! `failure_code`）；disabled 由 `enabled=0` 表达（v1 尚无 enable/disable 操作，
//! 列为后续域预留）；release 成功后回到 configured。`restarting` 在 v1 无重
//! 启路径，不可达（登记为差异）。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_BUSY,
    CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpStream;

use crate::process_group::GroupGuard;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::operations_ops::LongOperationOutcome;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, format_cursor, now_rfc3339};

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for yet.
const CODE_SCHEMA_MISMATCH: &str = "SCHEMA_MISMATCH";
const CODE_PROTOCOL_UNSUPPORTED: &str = "PROTOCOL_UNSUPPORTED";
const CODE_GENERATION_CONFLICT: &str = "GENERATION_CONFLICT";
const CODE_UPSTREAM_ERROR: &str = "UPSTREAM_ERROR";
const CODE_DEADLINE_EXCEEDED: &str = "DEADLINE_EXCEEDED";

/// The v1 runtime protocol id and the only accepted handshake version.
const RUNTIME_PROTOCOL: &str = "agentcpm-pd.runtime/1";
/// The fixed runtime id of the in-process native runtime.
const NATIVE_RUNTIME_ID: &str = "native";
/// The native runtime's provider identity (the gateway product itself).
const NATIVE_PROVIDER_ID: &str = "agentcpm-pd";
/// Configuration schema ids per kind (registered fixed-version ids; a
/// `schemaId` that does not match the declared `kind` answers SCHEMA_MISMATCH).
const SCHEMA_ID_NATIVE: &str = "agentcpm-pd.runtime.native/1";
const SCHEMA_ID_STDIO: &str = "agentcpm-pd.runtime.stdio/1";
const SCHEMA_ID_TCP: &str = "agentcpm-pd.runtime.tcp/1";
/// The capability id every runtime carries once its handshake declared it.
const RUNTIME_MODULES_CAPABILITY: &str = "cli.capability.runtime-modules";
/// Handshake wait bound (connect is async; the driver enforces this deadline
/// on the peer's first line).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// TCP connect bound.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded wait for an already in-flight connect attempt to finish.
const IN_FLIGHT_CONNECT_WAIT: Duration = Duration::from_secs(20);
/// Upper bound on the single NDJSON handshake line from stdio/TCP runtimes (64 KiB).
const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;
const ALLOWED_BARE_COMMANDS: [&str; 6] = [
    "python3",
    "python",
    "node",
    "uvx",
    "npx",
    "agentcpm-runtime",
];
const BLOCKED_SHELL_EXECUTABLES: [&str; 18] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "fish",
    "ash",
    "pwsh",
    "powershell",
    "cmd",
    "cmd.exe",
    "env",
    "sudo",
    "su",
    "doas",
    "osascript",
];
const TRUSTED_SYSTEM_BIN_DIRS: [&str; 4] =
    ["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin", "/bin"];
/// Event schema of the domain's `runtime.configurationChanged` events.
const RUNTIME_EVENT_SCHEMA: &str = "agentcpm-pd.runtime.event/1";
/// The completionResult schema ids of the two async operations.
const CONNECT_RESULT_SCHEMA: &str = "agentcpm-pd.runtime.connect/1";
const RELEASE_RESULT_SCHEMA: &str = "agentcpm-pd.runtime.release/1";

const DDL_V1_RUNTIMES: &str = "CREATE TABLE IF NOT EXISTS v1_runtimes (
    scope_key TEXT NOT NULL,
    runtime_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    schema_id TEXT NOT NULL,
    configuration_json TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    revision TEXT NOT NULL,
    generation TEXT NOT NULL,
    protocol_version TEXT,
    capability_ids_json TEXT NOT NULL DEFAULT '[]',
    operations_json TEXT NOT NULL DEFAULT '[]',
    limits_json TEXT,
    connect_state TEXT NOT NULL DEFAULT 'configured',
    failure_code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (scope_key, runtime_id)
)";
const DDL_V1_RUNTIME_SELECTIONS: &str = "CREATE TABLE IF NOT EXISTS v1_runtime_selections (
    scope_key TEXT PRIMARY KEY,
    runtime_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_runtimes", DDL_V1_RUNTIMES);
    reg.schema("v1_runtime_selections", DDL_V1_RUNTIME_SELECTIONS);
    reg.handler("runtimes.list", |rt, input, _key, _rev| {
        Box::pin(runtimes_list(rt, input))
    });
    reg.handler("runtimes.describe", |rt, input, _key, _rev| {
        Box::pin(runtimes_describe(rt, input))
    });
    reg.handler("runtimes.configure", |rt, input, _key, _rev| {
        Box::pin(runtimes_configure(rt, input))
    });
    reg.handler("runtimes.connect", |rt, input, key, _rev| {
        Box::pin(runtimes_connect(rt, input, key))
    });
    reg.handler("runtimes.release", |rt, input, key, _rev| {
        Box::pin(runtimes_release(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn runtime_invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn runtime_not_found(runtime_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("runtime `{runtime_id}` not found in the requested scope"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict(current: Option<&str>, expected: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!(
            "expectedRevision `{expected}` does not match the current revision `{}`; re-read and retry",
            current.unwrap_or("<absent>")
        ),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn generation_conflict(current: &str, expected: &str) -> ContractError {
    ContractError::new(
        CODE_GENERATION_CONFLICT,
        format!("expectedGeneration `{expected}` does not match the live generation `{current}`"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn schema_mismatch(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_SCHEMA_MISMATCH,
        message.into(),
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

// ---------------------------------------------------------------------------
// Scope (the runtimes/host shape: kind global|project|session + id). Local
// copy per the substrate's no-cross-domain-import precedent (models.rs).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RuntimeScope {
    Global,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl RuntimeScope {
    /// Canonical storage key (the contract scope JSON, stable field order).
    pub(super) fn key(&self) -> String {
        match self {
            Self::Global => "{\"kind\":\"global\"}".to_owned(),
            Self::Project { project_id } => {
                format!("{{\"kind\":\"project\",\"id\":\"{project_id}\"}}")
            }
            Self::Session { session_id, .. } => {
                format!("{{\"kind\":\"session\",\"id\":\"{session_id}\"}}")
            }
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project { .. } => "project",
            Self::Session { .. } => "session",
        }
    }

    /// The scope's `id` member (null for global).
    pub(super) fn id(&self) -> Value {
        match self {
            Self::Global => Value::Null,
            Self::Project { project_id } => json!(project_id),
            Self::Session { session_id, .. } => json!(session_id),
        }
    }
}

/// Parses and validates the contract `scope` object. Dispatch already ran the
/// schema validation; the existence checks follow the `tools.rs` precedent (a
/// scope naming a missing project/session answers INVALID_ARGUMENT — the
/// `runtimes.list` error list carries no NOT_FOUND).
pub(super) fn parse_runtime_scope(
    rt: &PublicV1Runtime,
    input: &Value,
) -> Result<RuntimeScope, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(runtime_invalid("input.scope is required"));
    };
    let id = scope.get("id").and_then(Value::as_str);
    match scope.get("kind").and_then(Value::as_str) {
        Some("global") => {
            if id.is_some() {
                return Err(runtime_invalid("scope.id must be null for kind=global"));
            }
            Ok(RuntimeScope::Global)
        }
        Some("project") => {
            let Some(project_id) = id.filter(|value| !value.is_empty()) else {
                return Err(runtime_invalid("scope.id is required for kind=project"));
            };
            if rt.store().get_project(project_id).is_err() {
                return Err(runtime_invalid(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(RuntimeScope::Project {
                project_id: project_id.to_owned(),
            })
        }
        Some("session") => {
            let Some(session_id) = id.filter(|value| !value.is_empty()) else {
                return Err(runtime_invalid("scope.id is required for kind=session"));
            };
            let session = rt.store().get_session(session_id).map_err(|_| {
                runtime_invalid(format!("scope session `{session_id}` does not exist"))
            })?;
            let Some(project_id) = session["session"].get("projectId").and_then(Value::as_str)
            else {
                return Err(runtime_invalid(format!(
                    "scope session `{session_id}` has no project"
                )));
            };
            Ok(RuntimeScope::Session {
                project_id: project_id.to_owned(),
                session_id: session_id.to_owned(),
            })
        }
        other => Err(runtime_invalid(format!(
            "scope.kind must be global|project|session; got `{other:?}`"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

struct StoredRuntime {
    runtime_id: String,
    kind: String,
    provider_id: String,
    schema_id: String,
    configuration: Value,
    enabled: bool,
    revision: String,
    generation: String,
    protocol_version: Option<String>,
    capability_ids: Vec<String>,
    operations: Vec<String>,
    limits: Option<Value>,
    connect_state: String,
    failure_code: Option<String>,
}

impl StoredRuntime {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        // The indexes are positions in `RUNTIME_COLUMNS` (15 columns, 0..=14):
        // capability_ids_json=10, operations_json=11, limits_json=12 (nullable,
        // e.g. a configured-but-never-connected row), connect_state=13,
        // failure_code=14. The previous off-by-one read operations as
        // capability_ids, the NULL limits_json as operations (a storage error
        // on any configure-before-connect row), connect_state as limits, and
        // failure_code as connect_state plus an out-of-range failure_code.
        let capability_raw: String = row.get(10)?;
        let operations_raw: String = row.get(11)?;
        Ok(Self {
            runtime_id: row.get(1)?,
            kind: row.get(2)?,
            provider_id: row.get(3)?,
            schema_id: row.get(4)?,
            configuration: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or(Value::Null),
            enabled: row.get::<_, i64>(6)? != 0,
            revision: row.get(7)?,
            generation: row.get(8)?,
            protocol_version: row.get(9)?,
            capability_ids: serde_json::from_str(&capability_raw).unwrap_or_default(),
            operations: serde_json::from_str(&operations_raw).unwrap_or_default(),
            limits: row
                .get::<_, Option<String>>(12)?
                .and_then(|text| serde_json::from_str(&text).ok()),
            connect_state: row.get(13)?,
            failure_code: row.get(14)?,
        })
    }

    /// The contract `Runtime` object (list/describe share it). The status
    /// projection is the acceptance anchor set: disabled/configured/starting/
    /// ready/failed, each with availability and a reason code — never a
    /// guessed default.
    fn to_contract_json(&self) -> Value {
        let (state, available, reason_code, status_generation) = self.status_fields();
        json!({
            "runtimeId": self.runtime_id,
            "kind": self.kind,
            "providerId": self.provider_id,
            "revision": self.revision,
            "generation": self.generation,
            "status": {
                "state": state,
                "available": available,
                "reasonCode": reason_code,
                "generation": status_generation,
            },
            "capabilityIds": self.capability_ids,
        })
    }

    fn status_fields(&self) -> (&'static str, bool, Value, Value) {
        if !self.enabled {
            return ("disabled", false, json!("RUNTIME_DISABLED"), Value::Null);
        }
        match self.connect_state.as_str() {
            "ready" => ("ready", true, Value::Null, json!(self.generation)),
            "starting" => ("starting", false, json!("RUNTIME_STARTING"), Value::Null),
            "failed" => (
                "failed",
                false,
                json!(
                    self.failure_code
                        .clone()
                        .unwrap_or_else(|| "RUNTIME_FAILED".into())
                ),
                Value::Null,
            ),
            // configured and anything else: configured, not assembled.
            _ => (
                "configured",
                false,
                json!("RUNTIME_NOT_CONNECTED"),
                Value::Null,
            ),
        }
    }
}

const RUNTIME_COLUMNS: &str = "scope_key, runtime_id, kind, provider_id, schema_id,
    configuration_json, enabled, revision, generation, protocol_version,
    capability_ids_json, operations_json, limits_json, connect_state, failure_code";

fn load_runtime(
    conn: &Connection,
    scope_key: &str,
    runtime_id: &str,
) -> Result<Option<StoredRuntime>, ContractError> {
    conn.query_row(
        &format!(
            "SELECT {RUNTIME_COLUMNS} FROM v1_runtimes
             WHERE scope_key = ?1 AND runtime_id = ?2"
        ),
        params![scope_key, runtime_id],
        StoredRuntime::from_row,
    )
    .optional()
    .map_err(sql_err)
}

fn load_scope_runtimes(
    conn: &Connection,
    scope_key: &str,
) -> Result<Vec<StoredRuntime>, ContractError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {RUNTIME_COLUMNS} FROM v1_runtimes
             WHERE scope_key = ?1 ORDER BY runtime_id ASC"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![scope_key], StoredRuntime::from_row)
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

/// The native runtime's contract object: the in-process gateway itself.
fn native_runtime_json(meta: &crate::public_v1::store::InstanceMeta) -> Value {
    json!({
        "runtimeId": NATIVE_RUNTIME_ID,
        "kind": "native",
        "providerId": NATIVE_PROVIDER_ID,
        "revision": meta.server_revision,
        "generation": meta.generation,
        "status": {
            "state": "ready",
            "available": true,
            "reasonCode": Value::Null,
            "generation": meta.generation,
        },
        "capabilityIds": [RUNTIME_MODULES_CAPABILITY],
    })
}

/// The native runtime as a row-shaped value (not table-backed; the identity
/// the connect/release paths compare against).
fn native_runtime_row(meta: &crate::public_v1::store::InstanceMeta) -> StoredRuntime {
    StoredRuntime {
        runtime_id: NATIVE_RUNTIME_ID.to_owned(),
        kind: "native".to_owned(),
        provider_id: NATIVE_PROVIDER_ID.to_owned(),
        schema_id: SCHEMA_ID_NATIVE.to_owned(),
        configuration: json!({}),
        enabled: true,
        revision: meta.server_revision.clone(),
        generation: meta.generation.clone(),
        protocol_version: Some(agentcpm_pd_protocol::contract::OPERATION_VERSION.to_owned()),
        capability_ids: vec![RUNTIME_MODULES_CAPABILITY.to_owned()],
        operations: vec![],
        limits: None,
        connect_state: "ready".to_owned(),
        failure_code: None,
    }
}

/// The next monotonic `rev-<n>` (same counter the store's own revisions use).
fn next_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let next = cur.unwrap_or(0) + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("rev-{next:08}"))
}

/// Appends a `runtime.*` event into the shared `events` table with the S2
/// two-step cursor discipline (operations_ops's append, local copy per the
/// substrate's per-domain pattern).
fn append_runtime_event(
    conn: &Connection,
    resource_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    let placeholder = format!("tmp-rt-{}", now_rfc3339());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id,
            payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', ?3, ?4, ?5, ?5, ?6)",
        params![
            placeholder,
            resource_id,
            event_type,
            RUNTIME_EVENT_SCHEMA,
            serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
            now_rfc3339()
        ],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![format_cursor(seq), seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Idempotency (accepted-receipt replay; provider_test.rs's local-copy pattern)
// ---------------------------------------------------------------------------

pub(super) fn replay_accepted_receipt(
    store: &V1Store,
    method_id: &str,
    input: &Value,
    key: &str,
) -> Result<Option<Value>, ContractError> {
    let digest = compute_input_digest(method_id, input);
    let conn = store.conn.lock().expect("sqlite mutex");
    let existing: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT input_digest, receipt_json, rejection_code
             FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, method_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    match existing {
        None => Ok(None),
        Some((stored_digest, receipt_json, rejection)) => {
            if stored_digest != digest {
                return Err(ContractError::new(
                    CODE_IDEMPOTENCY_CONFLICT,
                    format!(
                        "idempotencyKey `{key}` already used for {method_id} with different input"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if let Some(code) = rejection {
                return Err(ContractError::new(
                    code,
                    format!("{method_id} request previously rejected definitively"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            // A replayed receipt reports itself as deduplicated (the
            // provider_test.rs replay pattern); the stored copy keeps the
            // acceptance-time `deduplicated: false`.
            Ok(receipt_json
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .map(|mut receipt| {
                    if let Some(obj) = receipt.as_object_mut() {
                        obj.insert("deduplicated".to_owned(), json!(true));
                    }
                    receipt
                }))
        }
    }
}

pub(super) fn record_acceptance(
    store: &V1Store,
    key: &str,
    method_id: &str,
    input: &Value,
    receipt: &Value,
) -> Result<(), ContractError> {
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    let retained = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    let conn = store.conn.lock().expect("sqlite mutex");
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest,
            request_state, reason, resource_json, receipt_json, rejection_code, observed_at,
            retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')",
        params![
            key,
            method_id,
            digest,
            serde_json::to_string(&json!({"methodId": method_id})).unwrap_or_default(),
            serde_json::to_string(receipt).unwrap_or_default(),
            now,
            retained,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

pub(super) fn record_rejection(
    store: &V1Store,
    key: &str,
    method_id: &str,
    input: &Value,
    code: &str,
) {
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    let retained = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest,
            request_state, reason, resource_json, receipt_json, rejection_code, observed_at,
            retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6,
            'correctRejectedInputWithNewKey')",
        params![key, method_id, digest, code, now, retained],
    );
}

/// Refreshes the replayed receipt's state after the operation terminalizes
/// (same-key retries observe the terminal state; provider_test.rs's pattern).
pub(super) fn update_receipt_terminal_state(
    store: &V1Store,
    key: Option<&str>,
    method_id: &str,
    state: &str,
) {
    let Some(key) = key else {
        return;
    };
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let Ok(raw) = conn.query_row(
        "SELECT receipt_json FROM idempotency_records
         WHERE idempotency_key = ?1 AND method_id = ?2",
        params![key, method_id],
        |r| r.get::<_, String>(0),
    ) else {
        return;
    };
    if let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
        && let Some(obj) = receipt.as_object_mut()
    {
        obj.insert("state".to_owned(), json!(state));
        let _ = conn.execute(
            "UPDATE idempotency_records SET receipt_json = ?1
             WHERE idempotency_key = ?2 AND method_id = ?3",
            params![
                serde_json::to_string(&receipt).unwrap_or(raw),
                key,
                method_id
            ],
        );
    }
}

fn operation_receipt(operation_id: &str, accepted_revision: &str) -> Value {
    json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": accepted_revision,
        "deduplicated": false,
        "state": "queued",
    })
}

// ---------------------------------------------------------------------------
// runtimes.list
// ---------------------------------------------------------------------------

async fn runtimes_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_runtime_scope(&rt, &input)?;
    let store = rt.store();
    let meta = store.meta();
    let scope_key = scope.key();
    let conn = store.conn.lock().expect("sqlite mutex");

    let mut runtimes = vec![native_runtime_json(meta)];
    for row in load_scope_runtimes(&conn, &scope_key)? {
        runtimes.push(row.to_contract_json());
    }

    // Scope-local default selection: an explicit selection (makeDefault) or
    // the implicit builtin native. A selection naming a runtime that no
    // longer exists in this scope is reported as stale, never silently
    // resolved to a different runtime (「自定义选择来源清晰」).
    let selection: Option<(String, String)> = conn
        .query_row(
            "SELECT runtime_id, revision FROM v1_runtime_selections WHERE scope_key = ?1",
            params![scope_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let (default_runtime_id, selection_source) = match selection {
        None => (json!(NATIVE_RUNTIME_ID), "builtin-native".to_owned()),
        Some((selected, _)) => {
            let resolves = selected == NATIVE_RUNTIME_ID
                || load_runtime(&conn, &scope_key, &selected)?.is_some();
            if resolves {
                (json!(selected), "configured".to_owned())
            } else {
                (Value::Null, format!("stale:{selected}"))
            }
        }
    };
    drop(conn);

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "runtimes": runtimes,
            "defaultRuntimeId": default_runtime_id,
            "selectionSource": selection_source,
        }),
    ))
}

// ---------------------------------------------------------------------------
// runtimes.describe
// ---------------------------------------------------------------------------

async fn runtimes_describe(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_runtime_scope(&rt, &input)?;
    let runtime_id = input["runtimeId"].as_str().unwrap_or_default();
    let store = rt.store();
    let meta = store.meta();
    let conn = store.conn.lock().expect("sqlite mutex");

    let (runtime, configuration_schema_id, protocol_version, supported_operations, limits) =
        if runtime_id == NATIVE_RUNTIME_ID {
            // The native runtime's supported operations are the operations
            // this build actually registered (「支持操作列表来自实际协商」) —
            // the same derivation `capabilities_json` uses.
            let mut ids: Vec<String> = agentcpm_pd_protocol::registry::OperationRegistry::global()
                .operations()
                .iter()
                .filter(|op| DomainRegistry::global().is_registered(&op.id))
                .map(|op| op.id.clone())
                .collect();
            ids.sort();
            (
                native_runtime_json(meta),
                SCHEMA_ID_NATIVE.to_owned(),
                agentcpm_pd_protocol::contract::OPERATION_VERSION.to_owned(),
                ids,
                json!({
                    // The v1 run model executes one run per session worker.
                    "maxConcurrentRuns": 1,
                    "supportsCancel": true,
                    "supportsResume": false,
                }),
            )
        } else {
            let Some(row) = load_runtime(&conn, &scope.key(), runtime_id)? else {
                return Err(runtime_not_found(runtime_id).into());
            };
            let limits = row.limits.clone().unwrap_or_else(|| {
                // Unnegotiated (not connected yet): zero capacity is the
                // honest report, not a guessed default.
                json!({
                    "maxConcurrentRuns": 0,
                    "supportsCancel": false,
                    "supportsResume": false,
                })
            });
            let operations = row.operations.clone();
            let schema_id = row.schema_id.clone();
            let protocol = row
                .protocol_version
                .clone()
                .unwrap_or_else(|| RUNTIME_PROTOCOL.to_owned());
            (
                row.to_contract_json(),
                schema_id,
                protocol,
                operations,
                limits,
            )
        };
    drop(conn);

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "runtime": runtime,
            "configurationSchemaId": configuration_schema_id,
            "protocolVersion": protocol_version,
            "supportedOperations": supported_operations,
            "limits": limits,
        }),
    ))
}

// ---------------------------------------------------------------------------
// runtimes.configure
// ---------------------------------------------------------------------------

fn is_blocked_shell_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BLOCKED_SHELL_EXECUTABLES.contains(&lower.as_str())
}

fn validate_stdio_command(home: &Path, command: &str) -> Result<(), String> {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed != command || command.contains('\0') {
        return Err(
            "stdio `command` must be a clean executable name or path without leading/trailing whitespace"
                .to_owned(),
        );
    }
    if !command.contains('/') && !command.contains('\\') {
        if is_blocked_shell_name(command) || !ALLOWED_BARE_COMMANDS.contains(&command) {
            return Err(format!(
                "stdio `command` `{command}` is not permitted (shells blocked; use a managed runtime executable)"
            ));
        }
        return Ok(());
    }
    if command.contains('\\') {
        return Err(format!(
            "stdio `command` `{command}` must not use backslash path separators"
        ));
    }
    let path = Path::new(command);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "stdio `command` `{command}` must be a clean absolute path without `..` segments"
        ));
    }
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return Err(format!(
            "stdio `command` `{command}` has no executable filename"
        ));
    };
    if is_blocked_shell_name(file_name) {
        return Err(format!(
            "stdio `command` `{command}` points to a blocked shell/wrapper `{file_name}`"
        ));
    }
    let runtimes_root = home.join("runtimes");
    let in_runtimes_root = (|| -> Option<bool> {
        let canon_root = runtimes_root.canonicalize().ok()?;
        let parent = path.parent()?;
        let canon_parent = parent.canonicalize().ok()?;
        if !canon_parent.starts_with(&canon_root) {
            return Some(false);
        }
        if let Ok(meta) = path.symlink_metadata()
            && meta.file_type().is_symlink()
        {
            let canon_target = path.canonicalize().ok()?;
            return Some(canon_target.starts_with(&canon_root));
        }
        Some(true)
    })()
    .unwrap_or(false);
    if in_runtimes_root {
        return Ok(());
    }
    let in_trusted_sys = (|| -> Option<bool> {
        let parent = path.parent()?;
        let canon_parent = parent.canonicalize().ok()?;
        let trusted = TRUSTED_SYSTEM_BIN_DIRS.iter().any(|dir| {
            Path::new(dir)
                .canonicalize()
                .map(|c| canon_parent == c)
                .unwrap_or_else(|_| canon_parent == Path::new(dir))
        });
        if !trusted {
            return Some(false);
        }
        if let Ok(canon_target) = path.canonicalize()
            && let Some(target_name) = canon_target.file_name().and_then(|n| n.to_str())
            && is_blocked_shell_name(target_name)
        {
            return Some(false);
        }
        Some(true)
    })()
    .unwrap_or(false);
    if in_trusted_sys {
        return Ok(());
    }
    Err(format!(
        "stdio `command` `{command}` is outside `<home>/runtimes` and trusted system binary directories"
    ))
}

fn is_dangerous_env_key(key: &str) -> bool {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return true;
    }
    let upper = key.to_ascii_uppercase();
    upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.starts_with("PYTHON")
        || upper.starts_with("PERL5")
        || matches!(
            upper.as_str(),
            "PATH"
                | "IFS"
                | "ENV"
                | "BASH_ENV"
                | "ZDOTDIR"
                | "NODE_OPTIONS"
                | "RUBYOPT"
                | "RUBYLIB"
                | "JAVA_TOOL_OPTIONS"
                | "_JAVA_OPTIONS"
                | "GCONV_PATH"
                | "HOSTALIASES"
                | "LOCALDOMAIN"
                | "RES_OPTIONS"
                | "OPENSSL_CONF"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "HOME"
                | "SHELL"
        )
}

fn is_disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            let cgnat = octets[0] == 100 && (octets[1] & 0b1100_0000 == 64);
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || cgnat
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_disallowed_ip(IpAddr::V4(mapped));
            }
            let seg0 = v6.segments()[0];
            let unique_local = (seg0 & 0xfe00) == 0xfc00;
            let link_local = (seg0 & 0xffc0) == 0xfe80;
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || unique_local
                || link_local
        }
    }
}

fn normalize_host(raw: &str) -> &str {
    raw.trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw.trim())
        .trim_end_matches('.')
}

fn validate_tcp_host(host: &str) -> Result<(), String> {
    let trimmed = normalize_host(host);
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) || trimmed.contains('\0') {
        return Err("tcp `host` must be a non-empty hostname or IP without whitespace".to_owned());
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return Err(format!(
            "tcp `host` `{host}` targets a local/internal domain"
        ));
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>()
        && is_disallowed_ip(ip)
    {
        return Err(format!(
            "tcp `host` `{host}` targets a disallowed loopback, private, or link-local IP"
        ));
    }
    Ok(())
}

async fn connect_safe_tcp(host: &str, port: u16) -> Result<TcpStream, ConnectFailure> {
    validate_tcp_host(host).map_err(|reason| ConnectFailure {
        code: CODE_INVALID_ARGUMENT.to_owned(),
        message: reason,
        cancelled: false,
    })?;
    let lookup_host_str = normalize_host(host);
    let resolved: Vec<SocketAddr> = match tokio::time::timeout(
        TCP_CONNECT_TIMEOUT,
        tokio::net::lookup_host((lookup_host_str, port)),
    )
    .await
    {
        Err(_) => {
            return Err(ConnectFailure {
                code: CODE_DEADLINE_EXCEEDED.to_owned(),
                message: format!(
                    "resolving {host}:{port} did not complete within {} ms",
                    TCP_CONNECT_TIMEOUT.as_millis()
                ),
                cancelled: false,
            });
        }
        Ok(Err(err)) => {
            return Err(ConnectFailure {
                code: CODE_UPSTREAM_ERROR.to_owned(),
                message: format!("resolving {host}:{port} failed: {err}"),
                cancelled: false,
            });
        }
        Ok(Ok(iter)) => iter.collect(),
    };
    if resolved.is_empty() {
        return Err(ConnectFailure {
            code: CODE_UPSTREAM_ERROR.to_owned(),
            message: format!("resolving {host}:{port} returned no addresses"),
            cancelled: false,
        });
    }
    for addr in &resolved {
        if is_disallowed_ip(addr.ip()) {
            return Err(ConnectFailure {
                code: CODE_INVALID_ARGUMENT.to_owned(),
                message: format!(
                    "refusing connection to disallowed resolved IP `{}` for `{host}`",
                    addr.ip()
                ),
                cancelled: false,
            });
        }
    }
    match tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(&resolved[..])).await {
        Err(_) => Err(ConnectFailure {
            code: CODE_DEADLINE_EXCEEDED.to_owned(),
            message: format!(
                "connecting to {host}:{port} did not complete within {} ms",
                TCP_CONNECT_TIMEOUT.as_millis()
            ),
            cancelled: false,
        }),
        Ok(Err(err)) => Err(ConnectFailure {
            code: CODE_UPSTREAM_ERROR.to_owned(),
            message: format!("connecting to {host}:{port} failed: {err}"),
            cancelled: false,
        }),
        Ok(Ok(stream)) => Ok(stream),
    }
}

/// Validates the configuration object per kind. Unknown fields are rejected
/// (SCHEMA_MISMATCH): the schemaId of the request must be the kind's
/// registered fixed-version schema, and the configuration must satisfy it
/// (「native、stdio、TCP各自校验配置」「拒绝JS函数文件直装」——v1 不执行
/// 任意 JS，配置只接受声明的字段；并强制 stdio 命令/环境变量白名单与 TCP SSRF 防护)。
fn validate_configuration(
    home: &Path,
    kind: &str,
    schema_id: &str,
    configuration: &Value,
) -> Result<(), ContractError> {
    let expected_schema = match kind {
        "native" => SCHEMA_ID_NATIVE,
        "stdio" => SCHEMA_ID_STDIO,
        "tcp" => SCHEMA_ID_TCP,
        other => {
            return Err(schema_mismatch(format!(
                "kind `{other}` has no registered protocol implementation in this build"
            )));
        }
    };
    if schema_id != expected_schema {
        return Err(schema_mismatch(format!(
            "schemaId `{schema_id}` does not match the registered schema for kind `{kind}` (`{expected_schema}`)"
        )));
    }
    let Some(obj) = configuration.as_object() else {
        return Err(schema_mismatch("configuration must be an object"));
    };
    match kind {
        "native" => {
            if !obj.is_empty() {
                return Err(schema_mismatch(
                    "the native runtime takes no configuration fields",
                ));
            }
        }
        "stdio" => {
            let Some(command) = obj.get("command").and_then(Value::as_str) else {
                return Err(schema_mismatch(
                    "stdio configuration requires a string `command`",
                ));
            };
            if command.trim().is_empty() {
                return Err(schema_mismatch("stdio `command` must not be empty"));
            }
            for (key, value) in obj {
                match key.as_str() {
                    "command" => {}
                    "args" => {
                        let Some(arr) = value.as_array() else {
                            return Err(schema_mismatch("stdio `args` must be an array"));
                        };
                        if !arr.iter().all(Value::is_string) {
                            return Err(schema_mismatch(
                                "stdio `args` entries must all be strings",
                            ));
                        }
                    }
                    "env" => {
                        let Some(env) = value.as_object() else {
                            return Err(schema_mismatch("stdio `env` must be an object"));
                        };
                        if !env.values().all(Value::is_string) {
                            return Err(schema_mismatch("stdio `env` values must all be strings"));
                        }
                    }
                    "cwd" => {
                        if !value.is_string() {
                            return Err(schema_mismatch("stdio `cwd` must be a string"));
                        }
                    }
                    other => {
                        return Err(schema_mismatch(format!(
                            "unknown stdio configuration field `{other}`"
                        )));
                    }
                }
            }
            validate_stdio_command(home, command).map_err(runtime_invalid)?;
            if let Some(env) = obj.get("env").and_then(Value::as_object) {
                for (k, v) in env {
                    if is_dangerous_env_key(k) {
                        return Err(runtime_invalid(format!(
                            "stdio `env` key `{k}` is not permitted for security reasons"
                        )));
                    }
                    if v.as_str().is_some_and(|s| s.contains('\0')) {
                        return Err(runtime_invalid(format!(
                            "stdio `env` value for `{k}` must not contain NUL bytes"
                        )));
                    }
                }
            }
            if let Some(cwd) = obj.get("cwd").and_then(Value::as_str)
                && (cwd.contains('\0')
                    || Path::new(cwd)
                        .components()
                        .any(|c| matches!(c, Component::ParentDir)))
            {
                return Err(runtime_invalid(
                    "stdio `cwd` must not contain NUL bytes or `..` traversal",
                ));
            }
        }
        "tcp" => {
            let Some(host) = obj
                .get("host")
                .and_then(Value::as_str)
                .filter(|host| !host.trim().is_empty())
            else {
                return Err(schema_mismatch(
                    "tcp configuration requires a non-empty string `host`",
                ));
            };
            let port_ok = obj
                .get("port")
                .and_then(Value::as_u64)
                .is_some_and(|port| (1..=65535).contains(&port));
            if !port_ok {
                return Err(schema_mismatch(
                    "tcp configuration requires integer `port` in 1..=65535",
                ));
            }
            for key in obj.keys() {
                if key != "host" && key != "port" {
                    return Err(schema_mismatch(format!(
                        "unknown tcp configuration field `{key}`"
                    )));
                }
            }
            validate_tcp_host(host).map_err(runtime_invalid)?;
        }
        _ => unreachable!("kind checked above"),
    }
    Ok(())
}

async fn runtimes_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_runtime_scope(&rt, &input)?;
    let runtime_id = input["runtimeId"].as_str().unwrap_or_default().to_owned();
    let kind = input["kind"].as_str().unwrap_or_default().to_owned();
    let provider_id = input["providerId"].as_str().unwrap_or_default().to_owned();
    let schema_id = input["schemaId"].as_str().unwrap_or_default().to_owned();
    let configuration = input.get("configuration").cloned().unwrap_or(Value::Null);
    let make_default = input
        .get("makeDefault")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expected_revision = input["expectedRevision"].as_str().unwrap_or_default();

    if runtime_id.is_empty() || provider_id.is_empty() {
        return Err(runtime_invalid("runtimeId and providerId are required").into());
    }
    if runtime_id == NATIVE_RUNTIME_ID && kind != "native" {
        return Err(runtime_invalid(format!(
            "runtimeId `{NATIVE_RUNTIME_ID}` is reserved for the in-process native runtime"
        ))
        .into());
    }
    validate_configuration(rt.store().home(), &kind, &schema_id, &configuration)?;

    let store = rt.store().clone();
    let scope_key = scope.key();
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;
    let existing = load_runtime(&tx, &scope_key, &runtime_id)?;

    // CAS (「创建使用absent，不能省略为无条件覆盖」).
    let revision = match (&existing, expected_revision) {
        (Some(row), "absent") => {
            return Err(revision_conflict(Some(&row.revision), expected_revision).into());
        }
        (Some(row), expected) if expected != row.revision => {
            return Err(revision_conflict(Some(&row.revision), expected).into());
        }
        (None, expected) if expected != "absent" => {
            return Err(revision_conflict(None, expected).into());
        }
        _ => next_rev(&tx)?,
    };

    let now = now_rfc3339();
    match &existing {
        Some(_) => {
            // Reconfiguration does not steal bindings of live runs (「配置改
            // 变不偷换已有run绑定」): only the configuration columns move; a
            // live assembly keeps its generation until connect/release. A
            // reconfigure also resets the assembly facts (capabilityIds,
            // protocolVersion, limits) — the previous assembly no longer
            // matches the new configuration.
            tx.execute(
                "UPDATE v1_runtimes SET kind = ?1, provider_id = ?2, schema_id = ?3,
                    configuration_json = ?4, revision = ?5, updated_at = ?6,
                    connect_state = 'configured', failure_code = NULL,
                    protocol_version = NULL, capability_ids_json = '[]',
                    operations_json = '[]', limits_json = NULL
                 WHERE scope_key = ?7 AND runtime_id = ?8",
                params![
                    kind,
                    provider_id,
                    schema_id,
                    serde_json::to_string(&configuration).unwrap_or_default(),
                    revision,
                    now,
                    scope_key,
                    runtime_id
                ],
            )
            .map_err(sql_err)?;
        }
        None => {
            let generation = format!("rtgen-{}", store.next_id());
            tx.execute(
                "INSERT INTO v1_runtimes (scope_key, runtime_id, kind, provider_id, schema_id,
                    configuration_json, enabled, revision, generation, connect_state,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, 'configured', ?9, ?9)",
                params![
                    scope_key,
                    runtime_id,
                    kind,
                    provider_id,
                    schema_id,
                    serde_json::to_string(&configuration).unwrap_or_default(),
                    revision,
                    generation,
                    now
                ],
            )
            .map_err(sql_err)?;
        }
    }
    if make_default {
        tx.execute(
            "INSERT INTO v1_runtime_selections (scope_key, runtime_id, revision, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key) DO UPDATE SET
                runtime_id = excluded.runtime_id,
                revision = excluded.revision,
                updated_at = excluded.updated_at",
            params![scope_key, runtime_id, revision, now],
        )
        .map_err(sql_err)?;
    }
    append_runtime_event(
        &tx,
        &runtime_id,
        "runtime.configurationChanged",
        &json!({
            "runtimeId": runtime_id,
            "revision": revision,
            "kind": kind,
            "makeDefault": make_default,
        }),
    )?;
    tx.commit().map_err(sql_err)?;
    drop(conn);
    store.event_notifier().notify_waiters();

    // The effective generation: live only for an assembled runtime. The
    // native runtime is always live (the instance generation); a fresh
    // stdio/tcp configuration has no assembly yet (「实际生效代际独立可查」).
    let generation = if kind == "native" {
        json!(store.meta().generation)
    } else {
        Value::Null
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "revision": revision,
            "applied": true,
            "generation": generation,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Live runtime resources (the connect operation's owned children/streams)
// ---------------------------------------------------------------------------

/// One live assembly resource owned by a connect operation: a managed stdio
/// child (with its process group guard so descendants are killed together) or a
/// held TCP connection (closed on release).
struct OwnedChild {
    group_guard: GroupGuard,
    child: tokio::process::Child,
}

enum LiveRuntimeResource {
    Child(OwnedChild),
    Tcp(
        tokio::net::tcp::OwnedReadHalf,
        tokio::net::tcp::OwnedWriteHalf,
    ),
}

struct LiveRuntimeEntry {
    owner_op_id: String,
    resource: LiveRuntimeResource,
}

impl LiveRuntimeResource {
    /// Releases the resource this handle owns. stdio: kill the whole owned
    /// process group + reap the child. TCP: drop the halves (closing only this
    /// connection — never the remote shared service).
    async fn release(self) {
        match self {
            Self::Child(mut owned) => {
                owned.group_guard.signal(rustix::process::Signal::KILL);
                let _ = owned.child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(5), owned.child.wait()).await;
                owned.group_guard.disarm();
            }
            Self::Tcp(_read, _write) => {}
        }
    }
}

fn live_runtime_registry() -> &'static Mutex<HashMap<String, LiveRuntimeEntry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, LiveRuntimeEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_resource_key(home: &Path, scope_key: &str, runtime_id: &str) -> String {
    format!("{}|{}|{}", home.display(), scope_key, runtime_id)
}

fn register_live_runtime(key: String, owner_op_id: String, resource: LiveRuntimeResource) {
    let previous = live_runtime_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            key,
            LiveRuntimeEntry {
                owner_op_id,
                resource,
            },
        );
    if let Some(previous) = previous {
        tokio::spawn(async move {
            previous.resource.release().await;
        });
    }
}

fn take_live_runtime(key: &str) -> Option<LiveRuntimeResource> {
    live_runtime_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(key)
        .map(|entry| entry.resource)
}

fn take_live_runtime_if_owner(key: &str, owner_op_id: &str) -> Option<LiveRuntimeResource> {
    let mut reg = live_runtime_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if reg.get(key).is_some_and(|e| e.owner_op_id == owner_op_id) {
        reg.remove(key).map(|entry| entry.resource)
    } else {
        None
    }
}

fn live_runtime_exists(key: &str) -> bool {
    live_runtime_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(key)
}

// ---------------------------------------------------------------------------
// runtimes.connect (async, idempotentSubmit)
// ---------------------------------------------------------------------------

async fn runtimes_connect(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let method_id = "runtimes.connect";

    // Idempotent intake: same key + same input replays the accepted receipt
    // (「重复请求不启动多个子进程」的受理面)；different input conflicts.
    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = replay_accepted_receipt(&store, method_id, &input, key)?
    {
        return Ok((ResponseOutcome::Accepted, replay));
    }

    let prechecked = precheck_connect(&rt, &input, &store);
    let (scope, row) = match prechecked {
        Ok(pair) => pair,
        Err(err) => {
            if let Some(key) = idempotency_key.as_deref() {
                record_rejection(&store, key, method_id, &input, &err.code);
            }
            return Err(err.into());
        }
    };

    let (operation_id, accepted_revision) = store.begin_long_operation(
        method_id,
        &json!({"kind": scope.kind(), "id": scope.id()}),
        true,
    )?;
    let receipt = operation_receipt(&operation_id, &accepted_revision);
    if let Some(key) = idempotency_key.as_deref()
        && let Err(err) = record_acceptance(&store, key, method_id, &input, &receipt)
    {
        let _ = store.finish_long_operation(
            &operation_id,
            LongOperationOutcome::Failed {
                code: err.code.clone(),
                message: err.message.clone(),
            },
        );
        return Err(err.into());
    }
    store.event_notifier().notify_waiters();

    if row.kind == "native" {
        // The native runtime is already assembled: complete immediately with
        // the instance generation — zero subprocess, zero side effects.
        let completion = json!({
            "runtimeId": NATIVE_RUNTIME_ID,
            "generation": store.meta().generation,
            "protocolVersion": agentcpm_pd_protocol::contract::OPERATION_VERSION,
            "state": "ready",
            "capabilityIds": [RUNTIME_MODULES_CAPABILITY],
        });
        store.finish_long_operation(
            &operation_id,
            LongOperationOutcome::Succeeded {
                result: completion,
                result_schema_id: Some(CONNECT_RESULT_SCHEMA.to_owned()),
            },
        )?;
        update_receipt_terminal_state(&store, idempotency_key.as_deref(), method_id, "succeeded");
        store.event_notifier().notify_waiters();
        return Ok((ResponseOutcome::Accepted, receipt));
    }

    tokio::spawn(connect_driver(
        store.clone(),
        scope.key(),
        row,
        operation_id,
        idempotency_key,
    ));
    Ok((ResponseOutcome::Accepted, receipt))
}

/// Pre-accept validation (zero side effects): scope, runtime existence,
/// revision CAS, and the disabled gate.
fn precheck_connect(
    rt: &PublicV1Runtime,
    input: &Value,
    store: &Arc<V1Store>,
) -> Result<(RuntimeScope, StoredRuntime), ContractError> {
    let scope = parse_runtime_scope(rt, input)?;
    let runtime_id = input["runtimeId"].as_str().unwrap_or_default();
    let expected_revision = input["expectedRevision"].as_str().unwrap_or_default();
    let row = if runtime_id == NATIVE_RUNTIME_ID {
        native_runtime_row(store.meta())
    } else {
        let conn = store.conn.lock().expect("sqlite mutex");
        let Some(row) = load_runtime(&conn, &scope.key(), runtime_id)? else {
            return Err(runtime_not_found(runtime_id));
        };
        drop(conn);
        row
    };
    // The revision CAS applies to the native runtime too (its revision is
    // the server revision of this instance).
    if row.revision != expected_revision {
        return Err(revision_conflict(Some(&row.revision), expected_revision));
    }
    if !row.enabled {
        return Err(ContractError::new(
            CODE_UNAVAILABLE,
            format!("runtime `{runtime_id}` is disabled; enable it before connecting"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    Ok((scope, row))
}

/// Why a connect attempt ended without an assembly.
struct ConnectFailure {
    code: String,
    message: String,
    /// The connect was cancelled through `operations.cancel` before any
    /// assembly happened: the runtime returns to configured and the
    /// operation terminalizes as cancelled (not failed).
    cancelled: bool,
}

/// The async connect driver: owns the whole assembly window — spawn/connect,
/// the handshake wait, the cancel poll, and the durable terminal state.
enum ConnectClaim {
    Acquired(StoredRuntime),
    AlreadyReady(StoredRuntime),
    InFlight,
    Aborted(String, String),
}

enum ReadyPersistOutcome {
    Persisted,
    Conflict,
    StorageFailed,
}

async fn connect_driver(
    store: Arc<V1Store>,
    scope_key: String,
    row: StoredRuntime,
    operation_id: String,
    idempotency_key: Option<String>,
) {
    let home = store.home().to_path_buf();
    let resource_key = runtime_resource_key(&home, &scope_key, &row.runtime_id);
    let _ = store.mark_operation_running(&operation_id);

    // 原子 CAS 抢占 starting 态：仅单个 connect 可从 configured/failed 迁入
    // starting 并启动子进程/连接；并发到达者见到 starting 进入有界可取消等待。
    let claim = {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        match conn.transaction() {
            Err(err) => ConnectClaim::Aborted(
                CODE_UNAVAILABLE.to_owned(),
                format!("sqlite storage error: {err}"),
            ),
            Ok(tx) => match load_runtime(&tx, &scope_key, &row.runtime_id) {
                Err(err) => ConnectClaim::Aborted(err.code, err.message),
                Ok(None) => ConnectClaim::Aborted(
                    CODE_NOT_FOUND.to_owned(),
                    format!(
                        "runtime `{}` disappeared before connect started",
                        row.runtime_id
                    ),
                ),
                Ok(Some(cur)) if !cur.enabled => ConnectClaim::Aborted(
                    CODE_UNAVAILABLE.to_owned(),
                    format!(
                        "runtime `{}` is disabled; enable it before connecting",
                        cur.runtime_id
                    ),
                ),
                Ok(Some(cur)) if cur.revision != row.revision => ConnectClaim::Aborted(
                    CODE_REVISION_CONFLICT.to_owned(),
                    format!(
                        "runtime `{}` revision changed to `{}` before connect started",
                        cur.runtime_id, cur.revision
                    ),
                ),
                Ok(Some(cur)) if cur.connect_state == "ready" => ConnectClaim::AlreadyReady(cur),
                Ok(Some(cur)) if cur.connect_state == "starting" => ConnectClaim::InFlight,
                Ok(Some(cur)) => {
                    let changed = tx
                        .execute(
                            "UPDATE v1_runtimes
                             SET connect_state = 'starting', failure_code = NULL, updated_at = ?1
                             WHERE scope_key = ?2 AND runtime_id = ?3 AND revision = ?4
                               AND connect_state IN ('configured', 'failed')",
                            params![now_rfc3339(), scope_key, row.runtime_id, row.revision],
                        )
                        .unwrap_or(0);
                    if changed == 1 && tx.commit().is_ok() {
                        ConnectClaim::Acquired(cur)
                    } else {
                        ConnectClaim::InFlight
                    }
                }
            },
        }
    };

    let claimed_row = match claim {
        ConnectClaim::AlreadyReady(ready_row) => {
            finish_connect(
                &store,
                &operation_id,
                idempotency_key.as_deref(),
                LongOperationOutcome::Succeeded {
                    result: connect_completion(&ready_row),
                    result_schema_id: Some(CONNECT_RESULT_SCHEMA.to_owned()),
                },
                "succeeded",
            );
            return;
        }
        ConnectClaim::Aborted(code, message) => {
            finish_connect(
                &store,
                &operation_id,
                idempotency_key.as_deref(),
                LongOperationOutcome::Failed { code, message },
                "failed",
            );
            return;
        }
        ConnectClaim::InFlight => {
            match wait_for_in_flight_connect(&store, &operation_id, &scope_key, &row.runtime_id)
                .await
            {
                WaitConnectOutcome::Cancelled => {
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Cancelled,
                        "cancelled",
                    );
                    return;
                }
                WaitConnectOutcome::TimedOut => {
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Failed {
                            code: CODE_DEADLINE_EXCEEDED.to_owned(),
                            message: format!(
                                "waiting for in-flight connect of runtime `{}` timed out after {} ms",
                                row.runtime_id,
                                IN_FLIGHT_CONNECT_WAIT.as_millis()
                            ),
                        },
                        "failed",
                    );
                    return;
                }
                WaitConnectOutcome::LeftStarting => {}
            }
            let conn = store.conn.lock().expect("sqlite mutex");
            let now_row = load_runtime(&conn, &scope_key, &row.runtime_id)
                .ok()
                .flatten();
            drop(conn);
            match now_row {
                Some(now) if now.connect_state == "ready" && now.revision == row.revision => {
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Succeeded {
                            result: connect_completion(&now),
                            result_schema_id: Some(CONNECT_RESULT_SCHEMA.to_owned()),
                        },
                        "succeeded",
                    );
                    return;
                }
                Some(now) if now.revision != row.revision => {
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Failed {
                            code: CODE_REVISION_CONFLICT.to_owned(),
                            message: format!(
                                "runtime `{}` was reconfigured while waiting for connect",
                                now.runtime_id
                            ),
                        },
                        "failed",
                    );
                    return;
                }
                Some(now) => {
                    let code = now
                        .failure_code
                        .clone()
                        .unwrap_or_else(|| CODE_UPSTREAM_ERROR.to_owned());
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Failed {
                            code,
                            message: format!(
                                "a concurrent connect for runtime `{}` did not assemble",
                                now.runtime_id
                            ),
                        },
                        "failed",
                    );
                    return;
                }
                None => {
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Failed {
                            code: CODE_UPSTREAM_ERROR.to_owned(),
                            message: format!(
                                "runtime `{}` disappeared while waiting for a concurrent connect",
                                row.runtime_id
                            ),
                        },
                        "failed",
                    );
                    return;
                }
            }
        }
        ConnectClaim::Acquired(cur) => cur,
    };

    let result: Result<Handshake, ConnectFailure> = match claimed_row.kind.as_str() {
        "stdio" => stdio_connect(&store, &operation_id, &claimed_row, &resource_key).await,
        "tcp" => tcp_connect(&store, &operation_id, &claimed_row, &resource_key).await,
        other => Err(ConnectFailure {
            code: CODE_PROTOCOL_UNSUPPORTED.to_owned(),
            message: format!("kind `{other}` has no connect path in this build"),
            cancelled: false,
        }),
    };

    match result {
        Ok(handshake) => {
            let generation = format!("rtgen-{}", store.next_id());
            let completion = json!({
                "runtimeId": claimed_row.runtime_id,
                "generation": generation,
                "protocolVersion": handshake.protocol_version,
                "state": "ready",
                "capabilityIds": handshake.capability_ids,
            });
            let persist_outcome = {
                let mut conn = store.conn.lock().expect("sqlite mutex");
                match conn.transaction() {
                    Err(_) => ReadyPersistOutcome::StorageFailed,
                    Ok(tx) => {
                        let res = tx.execute(
                            "UPDATE v1_runtimes SET connect_state = 'ready', generation = ?1,
                                protocol_version = ?2, capability_ids_json = ?3,
                                operations_json = ?4, limits_json = ?5, failure_code = NULL,
                                updated_at = ?6
                             WHERE scope_key = ?7 AND runtime_id = ?8
                               AND revision = ?9 AND connect_state = 'starting'",
                            params![
                                generation,
                                handshake.protocol_version,
                                serde_json::to_string(&handshake.capability_ids)
                                    .unwrap_or_else(|_| "[]".into()),
                                serde_json::to_string(&handshake.operations)
                                    .unwrap_or_else(|_| "[]".into()),
                                serde_json::to_string(&handshake.limits)
                                    .unwrap_or_else(|_| "null".into()),
                                now_rfc3339(),
                                scope_key,
                                claimed_row.runtime_id,
                                claimed_row.revision
                            ],
                        );
                        match res {
                            Ok(1) => {
                                if tx.commit().is_ok() {
                                    ReadyPersistOutcome::Persisted
                                } else {
                                    ReadyPersistOutcome::StorageFailed
                                }
                            }
                            Ok(_) => ReadyPersistOutcome::Conflict,
                            Err(_) => ReadyPersistOutcome::StorageFailed,
                        }
                    }
                }
            };
            match persist_outcome {
                ReadyPersistOutcome::Persisted => {
                    store.event_notifier().notify_waiters();
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Succeeded {
                            result: completion,
                            result_schema_id: Some(CONNECT_RESULT_SCHEMA.to_owned()),
                        },
                        "succeeded",
                    );
                }
                ReadyPersistOutcome::Conflict => {
                    // 握手期间发生 configure/release/delete：释放旧归属资源且不覆盖新配置状态。
                    if let Some(resource) = take_live_runtime_if_owner(&resource_key, &operation_id)
                    {
                        resource.release().await;
                    }
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Failed {
                            code: CODE_REVISION_CONFLICT.to_owned(),
                            message: format!(
                                "runtime `{}` configuration or state changed during handshake",
                                claimed_row.runtime_id
                            ),
                        },
                        "failed",
                    );
                }
                ReadyPersistOutcome::StorageFailed => {
                    if let Some(resource) = take_live_runtime_if_owner(&resource_key, &operation_id)
                    {
                        resource.release().await;
                    }
                    set_connect_state_if_starting(
                        &store,
                        &scope_key,
                        &claimed_row.runtime_id,
                        &claimed_row.revision,
                        "failed",
                        Some("STORAGE_UNAVAILABLE"),
                    );
                    finish_connect(
                        &store,
                        &operation_id,
                        idempotency_key.as_deref(),
                        LongOperationOutcome::Failed {
                            code: CODE_UNAVAILABLE.to_owned(),
                            message: "persisting the assembled runtime state failed".to_owned(),
                        },
                        "failed",
                    );
                }
            }
        }
        Err(failure) => {
            // 仅释放属于本操作自己的资源（避免误杀新配置的资源）。
            if let Some(resource) = take_live_runtime_if_owner(&resource_key, &operation_id) {
                resource.release().await;
            }
            if failure.cancelled {
                set_connect_state_if_starting(
                    &store,
                    &scope_key,
                    &claimed_row.runtime_id,
                    &claimed_row.revision,
                    "configured",
                    None,
                );
                finish_connect(
                    &store,
                    &operation_id,
                    idempotency_key.as_deref(),
                    LongOperationOutcome::Cancelled,
                    "cancelled",
                );
            } else {
                set_connect_state_if_starting(
                    &store,
                    &scope_key,
                    &claimed_row.runtime_id,
                    &claimed_row.revision,
                    "failed",
                    Some(&failure.code),
                );
                finish_connect(
                    &store,
                    &operation_id,
                    idempotency_key.as_deref(),
                    LongOperationOutcome::Failed {
                        code: failure.code,
                        message: failure.message,
                    },
                    "failed",
                );
            }
        }
    }
}

fn connect_completion(row: &StoredRuntime) -> Value {
    json!({
        "runtimeId": row.runtime_id,
        "generation": row.generation,
        "protocolVersion": row
            .protocol_version
            .clone()
            .unwrap_or_else(|| RUNTIME_PROTOCOL.to_owned()),
        "state": "ready",
        "capabilityIds": row.capability_ids,
    })
}

fn finish_connect(
    store: &Arc<V1Store>,
    operation_id: &str,
    idempotency_key: Option<&str>,
    outcome: LongOperationOutcome,
    receipt_state: &str,
) {
    let _ = store.finish_long_operation(operation_id, outcome);
    update_receipt_terminal_state(store, idempotency_key, "runtimes.connect", receipt_state);
    store.event_notifier().notify_waiters();
}

fn set_connect_state(
    store: &Arc<V1Store>,
    scope_key: &str,
    runtime_id: &str,
    state: &str,
    failure_code: Option<&str>,
) {
    let conn = store.conn.lock().expect("sqlite mutex");
    let _ = conn.execute(
        "UPDATE v1_runtimes SET connect_state = ?1, failure_code = ?2, updated_at = ?3
         WHERE scope_key = ?4 AND runtime_id = ?5",
        params![state, failure_code, now_rfc3339(), scope_key, runtime_id],
    );
}

fn set_connect_state_if_starting(
    store: &Arc<V1Store>,
    scope_key: &str,
    runtime_id: &str,
    revision: &str,
    state: &str,
    failure_code: Option<&str>,
) {
    let conn = store.conn.lock().expect("sqlite mutex");
    let _ = conn.execute(
        "UPDATE v1_runtimes SET connect_state = ?1, failure_code = ?2, updated_at = ?3
         WHERE scope_key = ?4 AND runtime_id = ?5 AND revision = ?6 AND connect_state = 'starting'",
        params![
            state,
            failure_code,
            now_rfc3339(),
            scope_key,
            runtime_id,
            revision
        ],
    );
}

/// The parsed runtime handshake.
struct Handshake {
    protocol_version: String,
    capability_ids: Vec<String>,
    operations: Vec<String>,
    limits: Value,
}

fn parse_handshake(line: &str) -> Result<Handshake, ConnectFailure> {
    let failure = |code: &str, message: String| ConnectFailure {
        code: code.to_owned(),
        message,
        cancelled: false,
    };
    let value: Value = serde_json::from_str(line).map_err(|err| {
        failure(
            CODE_PROTOCOL_UNSUPPORTED,
            format!("runtime handshake is not valid JSON: {err}"),
        )
    })?;
    let version = value
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if version != RUNTIME_PROTOCOL {
        return Err(failure(
            CODE_PROTOCOL_UNSUPPORTED,
            format!(
                "runtime protocol version `{version}` is not supported (this build speaks `{RUNTIME_PROTOCOL}`)"
            ),
        ));
    }
    let strings = |field: &str| -> Vec<String> {
        value
            .get(field)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let limits = value.get("limits").cloned().unwrap_or_else(|| {
        json!({
            "maxConcurrentRuns": 0,
            "supportsCancel": false,
            "supportsResume": false,
        })
    });
    Ok(Handshake {
        protocol_version: version.to_owned(),
        capability_ids: strings("capabilityIds"),
        operations: strings("operations"),
        limits,
    })
}

/// Polls the cancel intent while waiting on `fut`; returns the inner outcome
/// or `None` when the operation was cancelled (the caller then cleans up).
async fn with_cancel_poll<T>(
    store: &Arc<V1Store>,
    operation_id: &str,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    let mut fut = Box::pin(fut);
    tokio::select! {
        outcome = &mut fut => Some(outcome),
        _ = poll_cancel(store, operation_id) => None,
    }
}

/// Resolves once `operations.cancel` recorded its intent for the operation.
async fn poll_cancel(store: &Arc<V1Store>, operation_id: &str) {
    loop {
        if store
            .operation_cancel_requested(operation_id)
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

enum WaitConnectOutcome {
    LeftStarting,
    Cancelled,
    TimedOut,
}

/// Waits until the runtime's connect state leaves `starting` (or row disappears),
/// bounded by `IN_FLIGHT_CONNECT_WAIT` and responsive to `operations.cancel`.
async fn wait_for_in_flight_connect(
    store: &Arc<V1Store>,
    operation_id: &str,
    scope_key: &str,
    runtime_id: &str,
) -> WaitConnectOutcome {
    let wait_loop = async {
        loop {
            {
                let conn = store.conn.lock().expect("sqlite mutex");
                let state: Option<String> = conn
                    .query_row(
                        "SELECT connect_state FROM v1_runtimes
                         WHERE scope_key = ?1 AND runtime_id = ?2",
                        params![scope_key, runtime_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                match state.as_deref() {
                    Some("starting") => {}
                    Some(_) | None => return,
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let timed = tokio::time::timeout(IN_FLIGHT_CONNECT_WAIT, wait_loop);
    match with_cancel_poll(store, operation_id, timed).await {
        None => WaitConnectOutcome::Cancelled,
        Some(Err(_)) => WaitConnectOutcome::TimedOut,
        Some(Ok(())) => WaitConnectOutcome::LeftStarting,
    }
}

/// Spawns the managed stdio child and reads its handshake. The child env is
/// PATH plus the configured env only — the gateway's own environment (and
/// any secrets in it) is never inherited by the runtime process.
async fn stdio_connect(
    store: &Arc<V1Store>,
    operation_id: &str,
    row: &StoredRuntime,
    resource_key: &str,
) -> Result<Handshake, ConnectFailure> {
    let command = row
        .configuration
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args: Vec<String> = row
        .configuration
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let extra_env: Option<Map<String, Value>> = row
        .configuration
        .get("env")
        .and_then(Value::as_object)
        .cloned();
    let cwd: Option<String> = row
        .configuration
        .get("cwd")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let home = store.home();
    if let Err(reason) = validate_stdio_command(home, &command) {
        return Err(ConnectFailure {
            code: CODE_INVALID_ARGUMENT.to_owned(),
            message: reason,
            cancelled: false,
        });
    }
    if let Some(extra_env) = &extra_env {
        for (key, value) in extra_env {
            if is_dangerous_env_key(key) {
                return Err(ConnectFailure {
                    code: CODE_INVALID_ARGUMENT.to_owned(),
                    message: format!("stdio `env` key `{key}` is not permitted"),
                    cancelled: false,
                });
            }
            if value.as_str().is_some_and(|s| s.contains('\0')) {
                return Err(ConnectFailure {
                    code: CODE_INVALID_ARGUMENT.to_owned(),
                    message: format!("stdio `env` value for `{key}` contains NUL byte"),
                    cancelled: false,
                });
            }
        }
    }

    let mut cmd = tokio::process::Command::new(&command);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env_clear();
    #[cfg(unix)]
    cmd.process_group(0);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    if let Some(extra_env) = &extra_env {
        for (key, value) in extra_env {
            if let Some(text) = value.as_str() {
                cmd.env(key, text);
            }
        }
    }
    if let Some(cwd) = &cwd {
        cmd.current_dir(cwd);
    }

    let mut child = cmd.spawn().map_err(|err| ConnectFailure {
        code: CODE_UPSTREAM_ERROR.to_owned(),
        message: format!("spawning the managed stdio runtime failed: {err}"),
        cancelled: false,
    })?;
    let group_guard = GroupGuard(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw),
    );
    let stdout = child.stdout.take().expect("piped stdout");
    register_live_runtime(
        resource_key.to_owned(),
        operation_id.to_owned(),
        LiveRuntimeResource::Child(OwnedChild { group_guard, child }),
    );

    let read = async {
        let mut reader = BufReader::new(stdout.take((MAX_HANDSHAKE_BYTES + 1) as u64));
        let mut buf = Vec::new();
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.read_until(b'\n', &mut buf)).await {
            Err(_) => Err(ConnectFailure {
                code: CODE_DEADLINE_EXCEEDED.to_owned(),
                message: format!(
                    "runtime did not complete the `{RUNTIME_PROTOCOL}` handshake within {} ms",
                    HANDSHAKE_TIMEOUT.as_millis()
                ),
                cancelled: false,
            }),
            Ok(Err(err)) => Err(ConnectFailure {
                code: CODE_UPSTREAM_ERROR.to_owned(),
                message: format!("reading the runtime handshake failed: {err}"),
                cancelled: false,
            }),
            Ok(Ok(0)) => Err(ConnectFailure {
                code: CODE_UPSTREAM_ERROR.to_owned(),
                message: "the runtime process closed its stdout before the handshake".to_owned(),
                cancelled: false,
            }),
            Ok(Ok(_)) => {
                if buf.len() > MAX_HANDSHAKE_BYTES {
                    return Err(ConnectFailure {
                        code: CODE_PROTOCOL_UNSUPPORTED.to_owned(),
                        message: format!(
                            "runtime handshake line exceeded {MAX_HANDSHAKE_BYTES} bytes"
                        ),
                        cancelled: false,
                    });
                }
                let line = String::from_utf8(buf).map_err(|err| ConnectFailure {
                    code: CODE_PROTOCOL_UNSUPPORTED.to_owned(),
                    message: format!("runtime handshake is not valid UTF-8: {err}"),
                    cancelled: false,
                })?;
                parse_handshake(line.trim_end_matches(['\r', '\n']))
            }
        }
    };
    match with_cancel_poll(store, operation_id, read).await {
        Some(outcome) => outcome,
        None => Err(ConnectFailure {
            code: "CANCELLED".to_owned(),
            message: "the connect operation was cancelled before the handshake completed"
                .to_owned(),
            cancelled: true,
        }),
    }
}

/// Opens the configured TCP connection (this connection only), reads the
/// handshake from it, and keeps the halves registered as the live assembly.
async fn tcp_connect(
    store: &Arc<V1Store>,
    operation_id: &str,
    row: &StoredRuntime,
    resource_key: &str,
) -> Result<Handshake, ConnectFailure> {
    let host = row
        .configuration
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let port = row
        .configuration
        .get("port")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let port_u16 = u16::try_from(port).map_err(|_| ConnectFailure {
        code: CODE_INVALID_ARGUMENT.to_owned(),
        message: "tcp `port` is out of range".to_owned(),
        cancelled: false,
    })?;
    let stream = connect_safe_tcp(&host, port_u16).await?;
    let _ = stream.set_nodelay(true);
    let (read_half, write_half) = stream.into_split();
    register_live_runtime(
        resource_key.to_owned(),
        operation_id.to_owned(),
        LiveRuntimeResource::Tcp(read_half, write_half),
    );
    // Take the halves back for the handshake read; re-register on success.
    let (read_half, write_half) = match take_live_runtime_if_owner(resource_key, operation_id) {
        Some(LiveRuntimeResource::Tcp(read_half, write_half)) => (read_half, write_half),
        Some(other) => {
            register_live_runtime(resource_key.to_owned(), operation_id.to_owned(), other);
            return Err(ConnectFailure {
                code: CODE_UPSTREAM_ERROR.to_owned(),
                message: "the tcp connection handle was replaced before the handshake".to_owned(),
                cancelled: false,
            });
        }
        None => {
            return Err(ConnectFailure {
                code: CODE_UPSTREAM_ERROR.to_owned(),
                message: "the tcp connection handle vanished before the handshake".to_owned(),
                cancelled: false,
            });
        }
    };

    let read = async {
        let mut reader = BufReader::new(read_half.take((MAX_HANDSHAKE_BYTES + 1) as u64));
        let mut buf = Vec::new();
        let read_outcome =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.read_until(b'\n', &mut buf)).await;
        let read_half = reader.into_inner().into_inner();
        let parsed = match read_outcome {
            Err(_) => Err(ConnectFailure {
                code: CODE_DEADLINE_EXCEEDED.to_owned(),
                message: format!(
                    "the tcp runtime did not complete the `{RUNTIME_PROTOCOL}` handshake within {} ms",
                    HANDSHAKE_TIMEOUT.as_millis()
                ),
                cancelled: false,
            }),
            Ok(Err(err)) => Err(ConnectFailure {
                code: CODE_UPSTREAM_ERROR.to_owned(),
                message: format!("reading the tcp runtime handshake failed: {err}"),
                cancelled: false,
            }),
            Ok(Ok(0)) => Err(ConnectFailure {
                code: CODE_UPSTREAM_ERROR.to_owned(),
                message: "the tcp runtime closed the connection before the handshake".to_owned(),
                cancelled: false,
            }),
            Ok(Ok(_)) => {
                if buf.len() > MAX_HANDSHAKE_BYTES {
                    Err(ConnectFailure {
                        code: CODE_PROTOCOL_UNSUPPORTED.to_owned(),
                        message: format!(
                            "the tcp runtime handshake line exceeded {MAX_HANDSHAKE_BYTES} bytes"
                        ),
                        cancelled: false,
                    })
                } else {
                    match String::from_utf8(buf) {
                        Ok(line) => parse_handshake(line.trim_end_matches(['\r', '\n'])),
                        Err(err) => Err(ConnectFailure {
                            code: CODE_PROTOCOL_UNSUPPORTED.to_owned(),
                            message: format!("the tcp runtime handshake is not valid UTF-8: {err}"),
                            cancelled: false,
                        }),
                    }
                }
            }
        };
        (parsed, read_half)
    };
    match with_cancel_poll(store, operation_id, read).await {
        Some((Ok(handshake), read_half)) => {
            register_live_runtime(
                resource_key.to_owned(),
                operation_id.to_owned(),
                LiveRuntimeResource::Tcp(read_half, write_half),
            );
            Ok(handshake)
        }
        Some((Err(failure), read_half)) => {
            // Dropping both halves closes exactly this connection.
            drop(read_half);
            drop(write_half);
            Err(failure)
        }
        None => {
            drop(write_half);
            Err(ConnectFailure {
                code: "CANCELLED".to_owned(),
                message: "the connect operation was cancelled before the handshake completed"
                    .to_owned(),
                cancelled: true,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// runtimes.release (async, idempotentSubmit)
// ---------------------------------------------------------------------------

async fn runtimes_release(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let method_id = "runtimes.release";

    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = replay_accepted_receipt(&store, method_id, &input, key)?
    {
        return Ok((ResponseOutcome::Accepted, replay));
    }

    let prechecked = precheck_release(&rt, &input, &store);
    let (scope, row, resource_key, active_runs) = match prechecked {
        Ok(quadruple) => quadruple,
        Err(err) => {
            if let Some(key) = idempotency_key.as_deref() {
                record_rejection(&store, key, method_id, &input, &err.code);
            }
            return Err(err.into());
        }
    };

    let (operation_id, accepted_revision) = store.begin_long_operation(
        method_id,
        &json!({"kind": scope.kind(), "id": scope.id()}),
        true,
    )?;
    let receipt = operation_receipt(&operation_id, &accepted_revision);
    if let Some(key) = idempotency_key.as_deref()
        && let Err(err) = record_acceptance(&store, key, method_id, &input, &receipt)
    {
        let _ = store.finish_long_operation(
            &operation_id,
            LongOperationOutcome::Failed {
                code: err.code.clone(),
                message: err.message.clone(),
            },
        );
        return Err(err.into());
    }
    store.event_notifier().notify_waiters();

    tokio::spawn(release_driver(
        rt.clone(),
        store.clone(),
        scope.key(),
        row,
        resource_key,
        active_runs,
        operation_id,
        idempotency_key,
    ));
    Ok((ResponseOutcome::Accepted, receipt))
}

/// Pre-accept validation: runtime existence, generation CAS, and the busy
/// gate (active runs without `cancelActive` answer RESOURCE_BUSY).
fn precheck_release(
    rt: &PublicV1Runtime,
    input: &Value,
    store: &Arc<V1Store>,
) -> Result<(RuntimeScope, StoredRuntime, String, Vec<String>), ContractError> {
    let scope = parse_runtime_scope(rt, input)?;
    let runtime_id = input["runtimeId"].as_str().unwrap_or_default();
    let expected_generation = input["expectedGeneration"].as_str().unwrap_or_default();
    let cancel_active = input
        .get("cancelActive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let home = store.home().to_path_buf();

    let row = if runtime_id == NATIVE_RUNTIME_ID {
        native_runtime_row(store.meta())
    } else {
        let conn = store.conn.lock().expect("sqlite mutex");
        let Some(row) = load_runtime(&conn, &scope.key(), runtime_id)? else {
            return Err(runtime_not_found(runtime_id));
        };
        drop(conn);
        row
    };
    // The generation CAS applies to the native runtime too (its generation is
    // the instance generation).
    if row.generation != expected_generation {
        return Err(generation_conflict(&row.generation, expected_generation));
    }

    // Active runs: only the native runtime executes v1 runs today; a custom
    // stdio/tcp runtime has no run bindings (thin line, documented).
    let active_runs: Vec<String> = if row.kind == "native" {
        let conn = store.conn.lock().expect("sqlite mutex");
        let mut stmt = conn
            .prepare(
                "SELECT run_id FROM runs WHERE terminal = 0 ORDER BY created_at ASC, run_id ASC",
            )
            .map_err(sql_err)?;
        stmt.query_map([], |r| r.get::<_, String>(0))
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?
    } else {
        Vec::new()
    };
    if !active_runs.is_empty() && !cancel_active {
        return Err(ContractError::new(
            CODE_RESOURCE_BUSY,
            format!(
                "runtime `{runtime_id}` has {} active run(s); pass cancelActive=true to cancel them",
                active_runs.len()
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }

    let resource_key = runtime_resource_key(&home, &scope.key(), runtime_id);
    Ok((scope, row, resource_key, active_runs))
}

/// The release driver: cancels the requested runs, drains the owned
/// resources, and reports exactly what was retained.
#[allow(clippy::too_many_arguments)]
async fn release_driver(
    rt: Arc<PublicV1Runtime>,
    store: Arc<V1Store>,
    scope_key: String,
    row: StoredRuntime,
    resource_key: String,
    active_runs: Vec<String>,
    operation_id: String,
    idempotency_key: Option<String>,
) {
    let _ = store.mark_operation_running(&operation_id);
    let _ = store.record_operation_progress(&operation_id, "phase", &json!("draining"));

    // cancelActive: each cancelled run stays individually queryable to its
    // own terminal state (「显式cancelActive逐个可查终态」).
    let mut cancelled_run_ids = Vec::new();
    for run_id in &active_runs {
        if let Ok(outcome) = store.cancel_run(run_id, None) {
            if outcome.should_notify_worker {
                rt.signal_run_cancel(run_id);
            }
            cancelled_run_ids.push(run_id.clone());
        }
    }

    // Resource ownership (「stdio仅回收拥有的子进程，TCP仅断开本连接，不
    // 停止远端共享服务」).
    let mut retained: Vec<String> = Vec::new();
    let released: bool;
    if row.kind == "native" {
        // The native runtime is the gateway process itself; it cannot be
        // released and is reported as retained explicitly.
        retained.push(NATIVE_RUNTIME_ID.to_owned());
        released = true;
    } else {
        match take_live_runtime(&resource_key) {
            Some(resource) => {
                resource.release().await;
                released = !live_runtime_exists(&resource_key);
                if !released {
                    retained.push(row.runtime_id.clone());
                }
            }
            None => {
                // No live assembly (never connected, already released, or the
                // process died on its own): nothing owned, nothing retained.
                released = true;
            }
        }
        set_connect_state(&store, &scope_key, &row.runtime_id, "configured", None);
    }

    let completion = json!({
        "runtimeId": row.runtime_id,
        "generation": row.generation,
        "released": released,
        "cancelledRunIds": cancelled_run_ids,
        "retainedResources": retained,
    });
    let _ = store.finish_long_operation(
        &operation_id,
        LongOperationOutcome::Succeeded {
            result: completion,
            result_schema_id: Some(RELEASE_RESULT_SCHEMA.to_owned()),
        },
    );
    update_receipt_terminal_state(
        &store,
        idempotency_key.as_deref(),
        "runtimes.release",
        "succeeded",
    );
    store.event_notifier().notify_waiters();
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;

    /// A runtime over a fresh home (the git.rs test pattern). The TempDir is
    /// returned so the home outlives the runtime handle.
    fn test_runtime() -> (Arc<PublicV1Runtime>, String, TempDir) {
        let tmp = TempDir::new("runtimes");
        let root = tmp.0.join("home");
        initialize_instance(&root, None, Some("local"), Some(true)).unwrap();
        let file = crate::public_v1::bootstrap::load_local_instance_and_token(&root, None)
            .unwrap()
            .0;
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id,
            product: file.product,
            protocol_version: file.protocol_version,
            node_id: file.node_id,
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision,
        };
        let store = V1Store::open_and_recover(&root, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        let iid = rt.store().meta().instance_id.clone();
        (rt, iid, tmp)
    }

    fn global_scope() -> Value {
        json!({"kind": "global"})
    }

    fn handshake_line() -> String {
        json!({
            "protocolVersion": RUNTIME_PROTOCOL,
            "capabilityIds": ["demo.cap"],
            "operations": ["demo.op"],
            "limits": {
                "maxConcurrentRuns": 2,
                "supportsCancel": true,
                "supportsResume": false
            },
        })
        .to_string()
    }

    /// A stdio runtime configuration whose peer handshakes and stays alive
    /// long enough for the test to observe the ready state.
    fn stdio_config() -> Value {
        let script = format!(
            "import sys, time; sys.stdout.write({:?} + '\\n'); sys.stdout.flush(); time.sleep(2)",
            handshake_line()
        );
        json!({
            "command": "python3",
            "args": ["-c", script],
        })
    }

    async fn configure_stdio(
        rt: &Arc<PublicV1Runtime>,
        runtime_id: &str,
        make_default: bool,
    ) -> Value {
        let (_, result) = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": runtime_id,
                    "kind": "stdio",
                    "providerId": "demo-provider",
                    "schemaId": SCHEMA_ID_STDIO,
                    "configuration": stdio_config(),
                    "makeDefault": make_default,
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        result
    }

    async fn wait_operation_state(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        operation_id: &str,
        want: &str,
    ) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let (_, res) = rt
                .dispatch(
                    "operations.get",
                    json!({"instanceId": iid, "operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if res["operation"]["state"] == json!(want) {
                return res;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "operation {operation_id} did not reach `{want}`; last: {res}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn err_code(err: &DispatchError) -> String {
        match err {
            DispatchError::Application(contract) => contract.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    fn current_revision(rt: &Arc<PublicV1Runtime>, runtime_id: &str) -> String {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        load_runtime(&conn, "{\"kind\":\"global\"}", runtime_id)
            .unwrap()
            .expect("runtime row")
            .revision
    }

    #[tokio::test]
    async fn configure_list_describe_native_default_roundtrip() {
        let (rt, _iid, _home) = test_runtime();

        // Native only: builtin default, ready, no secrets in output.
        let (_, listed) = rt
            .dispatch(
                "runtimes.list",
                json!({"scope": global_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();
        reg.validate_result("runtimes.list", &listed).unwrap();
        assert_eq!(listed["runtimes"].as_array().unwrap().len(), 1);
        assert_eq!(listed["defaultRuntimeId"], "native");
        assert_eq!(listed["selectionSource"], "builtin-native");
        assert_eq!(listed["runtimes"][0]["status"]["state"], "ready");
        assert_eq!(listed["runtimes"][0]["status"]["available"], true);

        let (_, described) = rt
            .dispatch(
                "runtimes.describe",
                json!({"scope": global_scope(), "runtimeId": "native"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("runtimes.describe", &described)
            .unwrap();
        assert_eq!(described["configurationSchemaId"], SCHEMA_ID_NATIVE);
        assert!(
            described["supportedOperations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|op| op == "run.submit")
        );
        assert_eq!(described["limits"]["maxConcurrentRuns"], 1);

        // Configure a custom runtime + makeDefault: selection source flips.
        let configured = configure_stdio(&rt, "rt-a", true).await;
        reg.validate_result("runtimes.configure", &configured)
            .unwrap();
        assert_eq!(configured["applied"], true);
        assert_eq!(configured["generation"], Value::Null);

        let (_, listed) = rt
            .dispatch(
                "runtimes.list",
                json!({"scope": global_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(listed["defaultRuntimeId"], "rt-a");
        assert_eq!(listed["selectionSource"], "configured");
        let custom = listed["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["runtimeId"] == json!("rt-a"))
            .unwrap();
        assert_eq!(custom["status"]["state"], "configured");
        assert_eq!(custom["status"]["available"], false);
        assert_eq!(custom["status"]["reasonCode"], "RUNTIME_NOT_CONNECTED");

        let (_, described) = rt
            .dispatch(
                "runtimes.describe",
                json!({"scope": global_scope(), "runtimeId": "rt-a"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("runtimes.describe", &described)
            .unwrap();
        assert_eq!(
            described["supportedOperations"].as_array().unwrap().len(),
            0
        );
        assert_eq!(described["limits"]["maxConcurrentRuns"], 0);

        // Unknown runtime describes as NOT_FOUND.
        let err = rt
            .dispatch(
                "runtimes.describe",
                json!({"scope": global_scope(), "runtimeId": "rt-none"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "NOT_FOUND");
    }

    #[tokio::test]
    async fn configure_cas_and_schema_validation() {
        let (rt, _iid, _home) = test_runtime();

        // Unknown schema id for the kind.
        let err = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-x",
                    "kind": "stdio",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_TCP,
                    "configuration": stdio_config(),
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "SCHEMA_MISMATCH");

        // stdio without command.
        let err = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-x",
                    "kind": "stdio",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_STDIO,
                    "configuration": {"args": ["a"]},
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "SCHEMA_MISMATCH");

        // tcp with a bad port.
        let err = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-x",
                    "kind": "tcp",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_TCP,
                    "configuration": {"host": "127.0.0.1", "port": 70000},
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "SCHEMA_MISMATCH");

        // native with configuration fields.
        let err = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "native",
                    "kind": "native",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_NATIVE,
                    "configuration": {"anything": true},
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "SCHEMA_MISMATCH");

        // Create, then CAS conflicts.
        configure_stdio(&rt, "rt-cas", false).await;
        let err = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-cas",
                    "kind": "stdio",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_STDIO,
                    "configuration": stdio_config(),
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "REVISION_CONFLICT");
        let err = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-cas",
                    "kind": "stdio",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_STDIO,
                    "configuration": stdio_config(),
                    "expectedRevision": "rev-00000099",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "REVISION_CONFLICT");

        // A configure on an unknown runtime with a non-absent revision is a
        // conflict, not a create.
        let err = rt
            .dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-ghost",
                    "kind": "stdio",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_STDIO,
                    "configuration": stdio_config(),
                    "expectedRevision": "rev-00000001",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "REVISION_CONFLICT");
    }

    #[tokio::test]
    async fn connect_stdio_handshake_then_release() {
        let (rt, iid, _home) = test_runtime();
        let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();
        configure_stdio(&rt, "rt-stdio-1", false).await;

        let (_, receipt) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-stdio-1",
                    "expectedRevision": current_revision(&rt, "rt-stdio-1"),
                }),
                Some("key-connect-1"),
                None,
            )
            .await
            .unwrap();
        reg.validate_result("runtimes.connect", &receipt).unwrap();
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        let done = wait_operation_state(&rt, &iid, &operation_id, "succeeded").await;
        let result = done["operation"]["result"].clone();
        assert_eq!(result["state"], "ready");
        assert_eq!(result["protocolVersion"], RUNTIME_PROTOCOL);
        assert_eq!(result["capabilityIds"], json!(["demo.cap"]));

        // The listing now reports the negotiated facts.
        let (_, listed) = rt
            .dispatch(
                "runtimes.list",
                json!({"scope": global_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        let custom = listed["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["runtimeId"] == json!("rt-stdio-1"))
            .unwrap();
        assert_eq!(custom["status"]["state"], "ready");
        assert_eq!(custom["status"]["available"], true);
        assert_eq!(custom["capabilityIds"], json!(["demo.cap"]));
        let generation = custom["generation"].as_str().unwrap().to_owned();

        let (_, described) = rt
            .dispatch(
                "runtimes.describe",
                json!({"scope": global_scope(), "runtimeId": "rt-stdio-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(described["supportedOperations"], json!(["demo.op"]));
        assert_eq!(described["limits"]["maxConcurrentRuns"], 2);

        // Same key replays the receipt; no second child.
        let (_, replay) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-stdio-1",
                    "expectedRevision": current_revision(&rt, "rt-stdio-1"),
                }),
                Some("key-connect-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);

        // A second connect under a different key is a zero-spawn success
        // with the same live generation.
        let (_, second) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-stdio-1",
                    "expectedRevision": current_revision(&rt, "rt-stdio-1"),
                }),
                Some("key-connect-2"),
                None,
            )
            .await
            .unwrap();
        let second_op = second["operationId"].as_str().unwrap().to_owned();
        let second_done = wait_operation_state(&rt, &iid, &second_op, "succeeded").await;
        assert_eq!(
            second_done["operation"]["result"]["generation"],
            json!(generation)
        );

        // Release with the live generation.
        let (_, release_receipt) = rt
            .dispatch(
                "runtimes.release",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-stdio-1",
                    "expectedGeneration": generation,
                }),
                Some("key-release-1"),
                None,
            )
            .await
            .unwrap();
        reg.validate_result("runtimes.release", &release_receipt)
            .unwrap();
        let release_op = release_receipt["operationId"].as_str().unwrap().to_owned();
        let released = wait_operation_state(&rt, &iid, &release_op, "succeeded").await;
        let result = released["operation"]["result"].clone();
        assert_eq!(result["released"], true);
        assert_eq!(result["cancelledRunIds"], json!([]));
        assert_eq!(result["retainedResources"], json!([]));

        let (_, listed) = rt
            .dispatch(
                "runtimes.list",
                json!({"scope": global_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        let custom = listed["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["runtimeId"] == json!("rt-stdio-1"))
            .unwrap();
        assert_eq!(custom["status"]["state"], "configured");
    }

    #[tokio::test]
    async fn connect_protocol_mismatch_fails_honestly() {
        let (rt, iid, _home) = test_runtime();
        let bad_handshake = r#"{"protocolVersion":"vendor/9","capabilityIds":[]}"#;
        let bad_script = format!(
            "import sys, time; sys.stdout.write({:?} + '\\n'); sys.stdout.flush(); time.sleep(1)",
            bad_handshake
        );
        rt.dispatch(
            "runtimes.configure",
            json!({
                "scope": global_scope(),
                "runtimeId": "rt-bad",
                "kind": "stdio",
                "providerId": "p",
                "schemaId": SCHEMA_ID_STDIO,
                "configuration": {
                    "command": "python3",
                    "args": ["-c", bad_script],
                },
                "expectedRevision": "absent",
            }),
            None,
            None,
        )
        .await
        .unwrap();

        let (_, receipt) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-bad",
                    "expectedRevision": current_revision(&rt, "rt-bad"),
                }),
                Some("key-bad-1"),
                None,
            )
            .await
            .unwrap();
        let op_id = receipt["operationId"].as_str().unwrap().to_owned();
        let done = wait_operation_state(&rt, &iid, &op_id, "failed").await;
        assert_eq!(done["operation"]["error"]["code"], "PROTOCOL_UNSUPPORTED");

        let (_, listed) = rt
            .dispatch(
                "runtimes.list",
                json!({"scope": global_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        let custom = listed["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["runtimeId"] == json!("rt-bad"))
            .unwrap();
        assert_eq!(custom["status"]["state"], "failed");
        assert_eq!(custom["status"]["reasonCode"], "PROTOCOL_UNSUPPORTED");
    }

    #[tokio::test]
    async fn release_busy_and_cancel_active_on_native() {
        let (rt, iid, _home) = test_runtime();
        // A project + session with one active run (sleep driver).
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": "proj-rt",
                "name": "Rt",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "r1", "relativePath": "w"}
            }),
            None,
            None,
        )
        .await
        .unwrap();
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "sess-rt", "projectId": "proj-rt"}),
            None,
            None,
        )
        .await
        .unwrap();
        let (_, sub) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": "sess-rt",
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=8000]"}]
                }),
                Some("key-rt-run-1"),
                None,
            )
            .await
            .unwrap();
        let run_id = sub["runId"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Default release: busy.
        let err = rt
            .dispatch(
                "runtimes.release",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "native",
                    "expectedGeneration": rt.store().meta().generation,
                }),
                Some("key-rel-busy"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "RESOURCE_BUSY");

        // cancelActive: the run terminalizes as cancelled, each queryable.
        let (_, receipt) = rt
            .dispatch(
                "runtimes.release",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "native",
                    "expectedGeneration": rt.store().meta().generation,
                    "cancelActive": true,
                }),
                Some("key-rel-cancel"),
                None,
            )
            .await
            .unwrap();
        let op_id = receipt["operationId"].as_str().unwrap().to_owned();
        let done = wait_operation_state(&rt, &iid, &op_id, "succeeded").await;
        let result = done["operation"]["result"].clone();
        assert_eq!(result["cancelledRunIds"], json!([run_id]));
        assert_eq!(result["retainedResources"], json!(["native"]));

        let (_, run) = rt
            .dispatch(
                "run.get",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(run["run"]["state"], "cancelled");
    }

    #[tokio::test]
    async fn connect_revision_and_not_found_gates() {
        let (rt, _iid, _home) = test_runtime();
        let err = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-none",
                    "expectedRevision": "rev-00000001",
                }),
                Some("key-ghost"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "NOT_FOUND");

        configure_stdio(&rt, "rt-rev", false).await;
        let err = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-rev",
                    "expectedRevision": "rev-00000042",
                }),
                Some("key-rev"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "REVISION_CONFLICT");
    }

    #[tokio::test]
    async fn release_generation_conflict_gate() {
        let (rt, _iid, _home) = test_runtime();
        configure_stdio(&rt, "rt-gen", false).await;
        let err = rt
            .dispatch(
                "runtimes.release",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-gen",
                    "expectedGeneration": "rtgen-does-not-exist",
                }),
                Some("key-gen"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "GENERATION_CONFLICT");
    }

    #[tokio::test]
    async fn connect_concurrent_claims_once_and_does_not_overwrite_reconfigure() {
        let (rt, iid, tmp) = test_runtime();
        let spawn_log = tmp.0.join("connect-spawns.log");
        let slow_script = format!(
            "import pathlib, sys, time; \
             p = pathlib.Path({:?}); \
             p.write_text((p.read_text() if p.exists() else '') + 'spawn\\n'); \
             time.sleep(0.35); \
             sys.stdout.write({:?} + '\\n'); sys.stdout.flush(); time.sleep(3)",
            spawn_log.display().to_string(),
            handshake_line()
        );
        rt.dispatch(
            "runtimes.configure",
            json!({
                "scope": global_scope(),
                "runtimeId": "rt-race",
                "kind": "stdio",
                "providerId": "p",
                "schemaId": SCHEMA_ID_STDIO,
                "configuration": {"command": "python3", "args": ["-c", slow_script]},
                "expectedRevision": "absent",
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let rev1 = current_revision(&rt, "rt-race");

        // 两个并发 connect（不同 idempotencyKey）：仅 1 个实际 spawn 子进程，另一个等待合并成功。
        let req_a = json!({
            "scope": global_scope(),
            "runtimeId": "rt-race",
            "expectedRevision": rev1,
        });
        let (res_a, res_b) = tokio::join!(
            rt.dispatch("runtimes.connect", req_a.clone(), Some("k-race-a"), None),
            rt.dispatch("runtimes.connect", req_a, Some("k-race-b"), None),
        );
        let (_, rec_a) = res_a.unwrap();
        let (_, rec_b) = res_b.unwrap();
        let done_a = wait_operation_state(
            &rt,
            &iid,
            rec_a["operationId"].as_str().unwrap(),
            "succeeded",
        )
        .await;
        let done_b = wait_operation_state(
            &rt,
            &iid,
            rec_b["operationId"].as_str().unwrap(),
            "succeeded",
        )
        .await;
        assert_eq!(
            done_a["operation"]["result"]["generation"],
            done_b["operation"]["result"]["generation"]
        );
        let spawns = std::fs::read_to_string(&spawn_log).unwrap_or_default();
        assert_eq!(
            spawns.lines().count(),
            1,
            "concurrent connect must spawn only once: {spawns:?}"
        );

        // 握手期间并发 reconfigure：旧 connect 不得用 stale ready 覆盖新 configured 状态。
        let slow2 = format!(
            "import sys, time; time.sleep(0.5); sys.stdout.write({:?} + '\\n'); sys.stdout.flush(); time.sleep(2)",
            handshake_line()
        );
        rt.dispatch(
            "runtimes.configure",
            json!({
                "scope": global_scope(),
                "runtimeId": "rt-reconf",
                "kind": "stdio",
                "providerId": "p",
                "schemaId": SCHEMA_ID_STDIO,
                "configuration": {"command": "python3", "args": ["-c", slow2]},
                "expectedRevision": "absent",
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let rev_old = current_revision(&rt, "rt-reconf");
        let (_, conn_rec) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-reconf",
                    "expectedRevision": rev_old,
                }),
                Some("k-reconf-conn"),
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        rt.dispatch(
            "runtimes.configure",
            json!({
                "scope": global_scope(),
                "runtimeId": "rt-reconf",
                "kind": "stdio",
                "providerId": "p",
                "schemaId": SCHEMA_ID_STDIO,
                "configuration": stdio_config(),
                "expectedRevision": rev_old,
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let conn_done = wait_operation_state(
            &rt,
            &iid,
            conn_rec["operationId"].as_str().unwrap(),
            "failed",
        )
        .await;
        assert_eq!(conn_done["operation"]["error"]["code"], "REVISION_CONFLICT");
        let row = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            load_runtime(&conn, "{\"kind\":\"global\"}", "rt-reconf")
                .unwrap()
                .unwrap()
        };
        assert_eq!(row.connect_state, "configured");
    }

    #[tokio::test]
    async fn wait_for_in_flight_connect_exits_on_deleted_row_and_cancels_cleanly() {
        let (rt, iid, _tmp) = test_runtime();
        configure_stdio(&rt, "rt-wait-del", false).await;
        let rev_del = current_revision(&rt, "rt-wait-del");
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE v1_runtimes SET connect_state = 'starting' WHERE runtime_id = 'rt-wait-del'",
                [],
            )
            .unwrap();
        }
        let (_, rec_del) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-wait-del",
                    "expectedRevision": rev_del,
                }),
                Some("k-wait-del"),
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "DELETE FROM v1_runtimes WHERE runtime_id = 'rt-wait-del'",
                [],
            )
            .unwrap();
        }
        wait_operation_state(
            &rt,
            &iid,
            rec_del["operationId"].as_str().unwrap(),
            "failed",
        )
        .await;

        // 遗留 starting 时 operations.cancel 必须能唤醒并终结等待者。
        configure_stdio(&rt, "rt-wait-cancel", false).await;
        let rev_c = current_revision(&rt, "rt-wait-cancel");
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE v1_runtimes SET connect_state = 'starting' WHERE runtime_id = 'rt-wait-cancel'",
                [],
            )
            .unwrap();
        }
        let (_, rec_c) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-wait-cancel",
                    "expectedRevision": rev_c,
                }),
                Some("k-wait-cancel"),
                None,
            )
            .await
            .unwrap();
        let op_c = rec_c["operationId"].as_str().unwrap().to_owned();
        rt.dispatch(
            "operations.cancel",
            json!({"instanceId": iid, "operationId": op_c}),
            Some("k-wait-cancel-op"),
            None,
        )
        .await
        .unwrap();
        wait_operation_state(&rt, &iid, &op_c, "cancelled").await;
    }

    #[tokio::test]
    async fn configure_rejects_shell_dangerous_env_and_ssrf_hosts() {
        let (rt, _iid, _tmp) = test_runtime();
        for bad_cmd in ["sh", "/bin/bash", "/usr/bin/env"] {
            let err = rt
                .dispatch(
                    "runtimes.configure",
                    json!({
                        "scope": global_scope(),
                        "runtimeId": "rt-bad-cmd",
                        "kind": "stdio",
                        "providerId": "p",
                        "schemaId": SCHEMA_ID_STDIO,
                        "configuration": {"command": bad_cmd, "args": ["-c", "echo hi"]},
                        "expectedRevision": "absent",
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(err_code(&err), "INVALID_ARGUMENT", "bad_cmd={bad_cmd}");
        }
        for (env_k, env_v) in [
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib"),
            ("NODE_OPTIONS", "--require /tmp/evil.js"),
        ] {
            let err = rt
                .dispatch(
                    "runtimes.configure",
                    json!({
                        "scope": global_scope(),
                        "runtimeId": "rt-bad-env",
                        "kind": "stdio",
                        "providerId": "p",
                        "schemaId": SCHEMA_ID_STDIO,
                        "configuration": {
                            "command": "python3",
                            "args": [],
                            "env": {env_k: env_v},
                        },
                        "expectedRevision": "absent",
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(err_code(&err), "INVALID_ARGUMENT", "env_k={env_k}");
        }
        for bad_host in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "::1",
            "localhost",
        ] {
            let err = rt
                .dispatch(
                    "runtimes.configure",
                    json!({
                        "scope": global_scope(),
                        "runtimeId": "rt-bad-tcp",
                        "kind": "tcp",
                        "providerId": "p",
                        "schemaId": SCHEMA_ID_TCP,
                        "configuration": {"host": bad_host, "port": 9000},
                        "expectedRevision": "absent",
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(err_code(&err), "INVALID_ARGUMENT", "bad_host={bad_host}");
        }
    }

    #[tokio::test]
    async fn stdio_connect_handles_stderr_flood_overlong_handshake_and_kills_process_group() {
        let (rt, iid, tmp) = test_runtime();
        // 1) stderr 暴写不阻塞握手。
        let noisy_script = format!(
            "import sys, time; sys.stderr.write('E' * 262144); sys.stderr.flush(); \
             sys.stdout.write({:?} + '\\n'); sys.stdout.flush(); time.sleep(2)",
            handshake_line()
        );
        rt.dispatch(
            "runtimes.configure",
            json!({
                "scope": global_scope(),
                "runtimeId": "rt-stderr",
                "kind": "stdio",
                "providerId": "p",
                "schemaId": SCHEMA_ID_STDIO,
                "configuration": {"command": "python3", "args": ["-c", noisy_script]},
                "expectedRevision": "absent",
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let (_, rec_noisy) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-stderr",
                    "expectedRevision": current_revision(&rt, "rt-stderr"),
                }),
                Some("k-rt-stderr"),
                None,
            )
            .await
            .unwrap();
        wait_operation_state(
            &rt,
            &iid,
            rec_noisy["operationId"].as_str().unwrap(),
            "succeeded",
        )
        .await;

        // 2) 超长握手行拒绝（不 OOM）。
        let flood_script = "import sys; sys.stdout.write('A' * 70000 + '\\n'); sys.stdout.flush()";
        rt.dispatch(
            "runtimes.configure",
            json!({
                "scope": global_scope(),
                "runtimeId": "rt-flood",
                "kind": "stdio",
                "providerId": "p",
                "schemaId": SCHEMA_ID_STDIO,
                "configuration": {"command": "python3", "args": ["-c", flood_script]},
                "expectedRevision": "absent",
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let (_, rec_flood) = rt
            .dispatch(
                "runtimes.connect",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-flood",
                    "expectedRevision": current_revision(&rt, "rt-flood"),
                }),
                Some("k-rt-flood"),
                None,
            )
            .await
            .unwrap();
        let flood_done = wait_operation_state(
            &rt,
            &iid,
            rec_flood["operationId"].as_str().unwrap(),
            "failed",
        )
        .await;
        assert_eq!(
            flood_done["operation"]["error"]["code"],
            "PROTOCOL_UNSUPPORTED"
        );

        // 3) release 必须杀死整个进程组（含派生孙进程）。
        #[cfg(unix)]
        {
            let marker = tmp.0.join("rt-grandchild-alive");
            let pgroup_script = format!(
                "import subprocess, sys, time; \
                 subprocess.Popen([sys.executable, '-c', 'import pathlib, time; time.sleep(0.9); pathlib.Path({:?}).write_text(\"alive\")']); \
                 sys.stdout.write({:?} + '\\n'); sys.stdout.flush(); time.sleep(10)",
                marker.display().to_string(),
                handshake_line()
            );
            rt.dispatch(
                "runtimes.configure",
                json!({
                    "scope": global_scope(),
                    "runtimeId": "rt-pgroup",
                    "kind": "stdio",
                    "providerId": "p",
                    "schemaId": SCHEMA_ID_STDIO,
                    "configuration": {"command": "python3", "args": ["-c", pgroup_script]},
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
            let (_, rec_pg) = rt
                .dispatch(
                    "runtimes.connect",
                    json!({
                        "scope": global_scope(),
                        "runtimeId": "rt-pgroup",
                        "expectedRevision": current_revision(&rt, "rt-pgroup"),
                    }),
                    Some("k-rt-pg-conn"),
                    None,
                )
                .await
                .unwrap();
            let done_pg = wait_operation_state(
                &rt,
                &iid,
                rec_pg["operationId"].as_str().unwrap(),
                "succeeded",
            )
            .await;
            let generation = done_pg["operation"]["result"]["generation"]
                .as_str()
                .unwrap()
                .to_owned();
            let (_, rec_rel) = rt
                .dispatch(
                    "runtimes.release",
                    json!({
                        "scope": global_scope(),
                        "runtimeId": "rt-pgroup",
                        "expectedGeneration": generation,
                    }),
                    Some("k-rt-pg-rel"),
                    None,
                )
                .await
                .unwrap();
            wait_operation_state(
                &rt,
                &iid,
                rec_rel["operationId"].as_str().unwrap(),
                "succeeded",
            )
            .await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(
                !marker.exists(),
                "runtimes.release must kill grandchild via process group"
            );
        }
    }
}
