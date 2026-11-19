//! `hooks.*` — the v1 hook binding configuration and the async hook-receipt
//! flow (`cli.task.hooks`, S3).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 4 个 `hooks.*` 条目；TS 基线
//! （cc7c4e6 `InProcessGateway.submitAsyncHookResult` + `AsyncHookRegistry`）
//! 是回执流的语义参考（重复/过期回执、绑定会话与有效期、过期不复活）。
//!
//! 领域模型：
//!
//! * **配置面**（list/configure）：按 scope 分层的绑定表
//!   `(layer, hookId) → {trigger, ownerId, operationId, enabled, deadlineMs}`
//!   + 层 revision（`hrev-…`，未配置时 `absent`）。`hooks.configure` 整体替
//!     换该层绑定集：只接受已声明 owner/操作（owner 必须是 overlay 层里已安
//!     装的插件、operationId 必须是其清单声明的操作——目标结果 Schema 在绑
//!     定时钉版），非法 trigger（旧线 `HOOK_EVENTS` 白名单）或 deadline 拒
//!     绝；写入后**改写后校验**（重读重验，防串改）。
//! * **触发面**：绑定解析按 overlay（global → project → session，内层同
//!   hookId 覆盖外层，与基线 settings 分层合并同构）。触发创建 invocation
//!   记录（绑定 run、deadline、结果 Schema 钉版、完成令牌只存散列）。
//!   本 v1 装配的生产 run 流水线不自动触发 hook——触发桥是 runtime 任务的
//!   缝合点；[`fire_hook_for_run`] 是该缝合点调用的稳定入口，离线驱动的
//!   `[pd-driver:fire-hook=<hookId>]` 指令通过它做确定性 e2e。
//! * **回执面**（get_invocation / submit_result）：invocation 状态机
//!   `pending → delivered | expired | cancelled`。submit_result 绑定会话、
//!   调用者（完成令牌）、有效期：过期/终态回执拒绝且**不复活 run**；同结
//!   果重复回执只一次生效（`disposition: "duplicate"`），冲突结果报
//!   `IDEMPOTENCY_CONFLICT`；幂等键重放同请求返回原回执。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE,
    ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::schema_validator::validate_against_schema;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::hooks::protocol::HOOK_EVENTS;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::plugins;
use crate::public_v1::domains::plugins::session_layer_stack;
use crate::public_v1::domains::tools::{ToolScope, parse_tool_scope, required_str};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::StoredRun;

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for.
const CODE_PERMISSION_DENIED: &str = "PERMISSION_DENIED";
const CODE_SCHEMA_MISMATCH: &str = "SCHEMA_MISMATCH";
const CODE_INVOCATION_EXPIRED: &str = "INVOCATION_EXPIRED";
const CODE_INVOCATION_TERMINAL: &str = "INVOCATION_TERMINAL";

/// The layer-revision sentinel before the first configure (the contract
/// family's `expectedRevision` convention: "创建使用absent").
pub(crate) const REVISION_ABSENT: &str = "absent";

const DDL_HOOKS_BINDINGS: &str = "CREATE TABLE IF NOT EXISTS v1_hooks_bindings (
    layer_id TEXT NOT NULL,
    hook_id TEXT NOT NULL,
    trigger_name TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    deadline_ms INTEGER NOT NULL,
    result_schema_id TEXT NOT NULL,
    result_schema_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (layer_id, hook_id)
);";

const DDL_HOOKS_META: &str = "CREATE TABLE IF NOT EXISTS v1_hooks_meta (
    layer_id TEXT PRIMARY KEY,
    counter INTEGER NOT NULL DEFAULT 0
);";

const DDL_HOOK_INVOCATIONS: &str = "CREATE TABLE IF NOT EXISTS v1_hook_invocations (
    invocation_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    hook_id TEXT NOT NULL,
    trigger_name TEXT NOT NULL,
    state TEXT NOT NULL,
    result_schema_id TEXT NOT NULL,
    result_schema_json TEXT NOT NULL,
    deadline_ms INTEGER NOT NULL,
    expires_at TEXT NOT NULL,
    completion_token_hash TEXT NOT NULL,
    receipt_id TEXT,
    result_digest TEXT,
    result_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

const DDL_HOOK_RECEIPTS: &str = "CREATE TABLE IF NOT EXISTS v1_hook_receipts (
    idempotency_key TEXT NOT NULL,
    method_id TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (idempotency_key, method_id)
);";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_hooks_bindings", DDL_HOOKS_BINDINGS);
    reg.schema("v1_hooks_meta", DDL_HOOKS_META);
    reg.schema("v1_hook_invocations", DDL_HOOK_INVOCATIONS);
    reg.schema("v1_hook_receipts", DDL_HOOK_RECEIPTS);
    reg.handler("hooks.list", |rt, input, _key, _rev| {
        Box::pin(hooks_list(rt, input))
    });
    reg.handler("hooks.configure", |rt, input, _key, rev| {
        Box::pin(hooks_configure(rt, input, rev))
    });
    reg.handler("hooks.get_invocation", |rt, input, _key, _rev| {
        Box::pin(hooks_get_invocation(rt, input))
    });
    reg.handler("hooks.submit_result", |rt, input, key, _rev| {
        Box::pin(hooks_submit_result(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn invalid_arg(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
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

fn schema_mismatch(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_SCHEMA_MISMATCH,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn idempotency_conflict(method: &str, key: &str) -> ContractError {
    ContractError::new(
        CODE_IDEMPOTENCY_CONFLICT,
        format!("idempotencyKey `{key}` already used for {method} with different input"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn sql_err(context: &str, error: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("{context}: {error}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

pub(crate) struct BindingRow {
    pub(crate) layer_id: String,
    pub(crate) hook_id: String,
    pub(crate) trigger_name: String,
    pub(crate) owner_id: String,
    pub(crate) operation_id: String,
    pub(crate) enabled: bool,
    pub(crate) deadline_ms: i64,
    pub(crate) result_schema_id: String,
    pub(crate) result_schema_json: String,
}

pub(crate) struct InvocationRow {
    pub(crate) invocation_id: String,
    pub(crate) session_id: String,
    pub(crate) project_id: String,
    pub(crate) run_id: String,
    pub(crate) hook_id: String,
    pub(crate) trigger_name: String,
    pub(crate) state: String,
    pub(crate) result_schema_id: String,
    pub(crate) result_schema_json: String,
    pub(crate) deadline_ms: i64,
    pub(crate) expires_at: String,
    pub(crate) completion_token_hash: String,
    pub(crate) receipt_id: Option<String>,
    pub(crate) result_digest: Option<String>,
}

pub(crate) struct HooksDb {
    conn: Mutex<Connection>,
}

impl HooksDb {
    pub(crate) fn open(home: &Path) -> Result<Self, ContractError> {
        let dir = home.join("v1");
        std::fs::create_dir_all(&dir).map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("mkdir {}: {e}", dir.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        let conn = Connection::open(dir.join("state.sqlite"))
            .map_err(|e| sql_err("open v1 state db", e))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| sql_err("pragma v1 state db", e))?;
        for ddl in [
            DDL_HOOKS_BINDINGS,
            DDL_HOOKS_META,
            DDL_HOOK_INVOCATIONS,
            DDL_HOOK_RECEIPTS,
        ] {
            conn.execute_batch(ddl)
                .map_err(|e| sql_err("hooks ddl", e))?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with_conn<T>(
        &self,
        context: &str,
        body: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, ContractError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        body(&conn).map_err(|e| sql_err(context, e))
    }

    fn with_tx<T>(
        &self,
        context: &str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, rusqlite::Error>,
    ) -> Result<T, ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(|e| sql_err(context, e))?;
        let out = body(&tx).map_err(|e| sql_err(context, e))?;
        tx.commit().map_err(|e| sql_err(context, e))?;
        Ok(out)
    }

    // ---- bindings ---------------------------------------------------------

    fn bindings_for_layer(&self, layer_id: &str) -> Result<Vec<BindingRow>, ContractError> {
        self.with_conn("list hook bindings", |conn| {
            let mut stmt = conn.prepare(
                "SELECT layer_id, hook_id, trigger_name, owner_id, operation_id, enabled,
                        deadline_ms, result_schema_id, result_schema_json
                 FROM v1_hooks_bindings WHERE layer_id = ?1 ORDER BY hook_id ASC",
            )?;
            let rows = stmt
                .query_map(params![layer_id], row_to_binding)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    fn layer_revision(&self, layer_id: &str) -> Result<String, ContractError> {
        self.with_conn("read hooks revision", |conn| {
            let counter: Option<i64> = conn
                .query_row(
                    "SELECT counter FROM v1_hooks_meta WHERE layer_id = ?1",
                    params![layer_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(counter
                .map(|value| format!("hrev-{value:08}"))
                .unwrap_or_else(|| REVISION_ABSENT.to_owned()))
        })
    }

    /// 整体替换一层绑定集并推进层 revision（`hrev-…`）。
    fn replace_bindings(
        &self,
        layer_id: &str,
        bindings: &[BindingRow],
    ) -> Result<String, ContractError> {
        self.with_tx("write hook bindings", |tx| {
            tx.execute(
                "DELETE FROM v1_hooks_bindings WHERE layer_id = ?1",
                params![layer_id],
            )?;
            for row in bindings {
                let now = now_rfc3339();
                tx.execute(
                    "INSERT INTO v1_hooks_bindings
                        (layer_id, hook_id, trigger_name, owner_id, operation_id, enabled,
                         deadline_ms, result_schema_id, result_schema_json, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                    params![
                        row.layer_id,
                        row.hook_id,
                        row.trigger_name,
                        row.owner_id,
                        row.operation_id,
                        row.enabled as i64,
                        row.deadline_ms,
                        row.result_schema_id,
                        row.result_schema_json,
                        now,
                    ],
                )?;
            }
            let counter: i64 = tx
                .query_row(
                    "SELECT counter FROM v1_hooks_meta WHERE layer_id = ?1",
                    params![layer_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let next = counter + 1;
            tx.execute(
                "INSERT INTO v1_hooks_meta (layer_id, counter) VALUES (?1, ?2)
                 ON CONFLICT(layer_id) DO UPDATE SET counter = excluded.counter",
                params![layer_id, next],
            )?;
            Ok(format!("hrev-{next:08}"))
        })
    }

    // ---- invocations ------------------------------------------------------

    fn insert_invocation(&self, row: &InvocationRow) -> Result<(), ContractError> {
        self.with_conn("insert hook invocation", |conn| {
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO v1_hook_invocations
                    (invocation_id, session_id, project_id, run_id, hook_id, trigger_name,
                     state, result_schema_id, result_schema_json, deadline_ms, expires_at,
                     completion_token_hash, receipt_id, result_digest, result_json,
                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10, ?11, NULL,
                         NULL, NULL, ?12, ?12)",
                params![
                    row.invocation_id,
                    row.session_id,
                    row.project_id,
                    row.run_id,
                    row.hook_id,
                    row.trigger_name,
                    row.result_schema_id,
                    row.result_schema_json,
                    row.deadline_ms,
                    row.expires_at,
                    row.completion_token_hash,
                    now,
                ],
            )?;
            Ok(())
        })
    }

    fn invocation(&self, invocation_id: &str) -> Result<Option<InvocationRow>, ContractError> {
        self.with_conn("read hook invocation", |conn| {
            conn.query_row(
                "SELECT invocation_id, session_id, project_id, run_id, hook_id, trigger_name,
                        state, result_schema_id, result_schema_json, deadline_ms, expires_at,
                        completion_token_hash, receipt_id, result_digest
                 FROM v1_hook_invocations WHERE invocation_id = ?1",
                params![invocation_id],
                row_to_invocation,
            )
            .optional()
        })
    }

    /// 原子终态迁移：只从 `from` 态迁移（提交与过期/取消的竞争安全网）。
    fn transition_invocation(
        &self,
        invocation_id: &str,
        from: &str,
        to: &str,
    ) -> Result<bool, ContractError> {
        self.with_conn("transition hook invocation", |conn| {
            let changed = conn.execute(
                "UPDATE v1_hook_invocations
                 SET state = ?3, updated_at = ?4
                 WHERE invocation_id = ?1 AND state = ?2",
                params![invocation_id, from, to, now_rfc3339()],
            )?;
            Ok(changed == 1)
        })
    }

    /// The delivery write: pending → delivered with the receipt and result,
    /// in one atomic row update.
    fn deliver_invocation(
        &self,
        invocation_id: &str,
        receipt_id: &str,
        result_digest: &str,
        result_json: &str,
    ) -> Result<bool, ContractError> {
        self.with_conn("deliver hook invocation", |conn| {
            let changed = conn.execute(
                "UPDATE v1_hook_invocations
                 SET state = 'delivered', receipt_id = ?2, result_digest = ?3, result_json = ?4,
                     updated_at = ?5
                 WHERE invocation_id = ?1 AND state = 'pending'",
                params![
                    invocation_id,
                    receipt_id,
                    result_digest,
                    result_json,
                    now_rfc3339()
                ],
            )?;
            Ok(changed == 1)
        })
    }

    fn next_invocation_id(&self) -> Result<String, ContractError> {
        self.with_tx("allocate invocation id", |tx| {
            let counter: i64 = tx
                .query_row(
                    "SELECT counter FROM v1_hooks_meta WHERE layer_id = '__invocations__'",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let next = counter + 1;
            tx.execute(
                "INSERT INTO v1_hooks_meta (layer_id, counter) VALUES ('__invocations__', ?1)
                 ON CONFLICT(layer_id) DO UPDATE SET counter = excluded.counter",
                params![next],
            )?;
            Ok(format!("hinv-{next:08}"))
        })
    }

    fn next_receipt_id(&self) -> Result<String, ContractError> {
        self.with_tx("allocate receipt id", |tx| {
            let counter: i64 = tx
                .query_row(
                    "SELECT counter FROM v1_hooks_meta WHERE layer_id = '__receipts__'",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let next = counter + 1;
            tx.execute(
                "INSERT INTO v1_hooks_meta (layer_id, counter) VALUES ('__receipts__', ?1)
                 ON CONFLICT(layer_id) DO UPDATE SET counter = excluded.counter",
                params![next],
            )?;
            Ok(format!("hkr-{next:08}"))
        })
    }

    // ---- submit_result 幂等记录 ---------------------------------------------

    fn lookup_receipt(
        &self,
        method_id: &str,
        key: &str,
        digest: &str,
    ) -> Result<Option<Result<Value, ()>>, ContractError> {
        self.with_conn("lookup hook receipt", |conn| {
            let stored: Option<(String, String)> = conn
                .query_row(
                    "SELECT input_digest, receipt_json FROM v1_hook_receipts
                     WHERE idempotency_key = ?1 AND method_id = ?2",
                    params![key, method_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            Ok(stored.map(|(stored_digest, receipt_json)| {
                if stored_digest != digest {
                    Err(())
                } else {
                    Ok(serde_json::from_str(&receipt_json).unwrap_or(Value::Null))
                }
            }))
        })
    }

    fn record_receipt(
        &self,
        method_id: &str,
        key: &str,
        digest: &str,
        receipt: &Value,
    ) -> Result<(), ContractError> {
        self.with_conn("record hook receipt", |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO v1_hook_receipts
                    (idempotency_key, method_id, input_digest, receipt_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    key,
                    method_id,
                    digest,
                    serde_json::to_string(receipt).unwrap_or_else(|_| "{}".into()),
                    now_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    /// The domain event append (the store's placeholder→`cur-<seq>` pattern).
    fn append_event(
        &self,
        resource_id: &str,
        session_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), ContractError> {
        self.with_conn("append hooks event", |conn| {
            let placeholder =
                format!("tmp-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
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
            )?;
            let seq = conn.last_insert_rowid();
            conn.execute(
                "UPDATE events SET cursor = ?1 WHERE seq = ?2",
                params![format!("cur-{seq:012}"), seq],
            )?;
            Ok(())
        })
    }
}

fn row_to_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<BindingRow> {
    Ok(BindingRow {
        layer_id: row.get(0)?,
        hook_id: row.get(1)?,
        trigger_name: row.get(2)?,
        owner_id: row.get(3)?,
        operation_id: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        deadline_ms: row.get(6)?,
        result_schema_id: row.get(7)?,
        result_schema_json: row.get(8)?,
    })
}

fn row_to_invocation(row: &rusqlite::Row<'_>) -> rusqlite::Result<InvocationRow> {
    Ok(InvocationRow {
        invocation_id: row.get(0)?,
        session_id: row.get(1)?,
        project_id: row.get(2)?,
        run_id: row.get(3)?,
        hook_id: row.get(4)?,
        trigger_name: row.get(5)?,
        state: row.get(6)?,
        result_schema_id: row.get(7)?,
        result_schema_json: row.get(8)?,
        deadline_ms: row.get(9)?,
        expires_at: row.get(10)?,
        completion_token_hash: row.get(11)?,
        receipt_id: row.get(12)?,
        result_digest: row.get(13)?,
    })
}

/// The per-dataRoot singleton (the tools.rs seam pattern).
fn hooks_db(rt: &PublicV1Runtime) -> Result<Arc<HooksDb>, ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut reg = HOOKS_DB_REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(db) = reg.get(&home).and_then(Weak::upgrade) {
        return Ok(db);
    }
    let db = Arc::new(HooksDb::open(&home)?);
    reg.insert(home, Arc::downgrade(&db));
    Ok(db)
}

static HOOKS_DB_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<HooksDb>>>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Effective state (lazy expiry: an overdue pending invocation reads expired)
// ---------------------------------------------------------------------------

fn parse_rfc3339(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
}

fn is_expired(row: &InvocationRow) -> bool {
    parse_rfc3339(&row.expires_at)
        .map(|expires_at| chrono::Utc::now() >= expires_at)
        .unwrap_or(false)
}

/// Applies lazy expiry to a pending row and returns the effective state
/// (`pending` / `delivered` / `expired` / `cancelled`).
fn effective_state(db: &HooksDb, row: &InvocationRow) -> Result<String, ContractError> {
    if row.state == "pending" && is_expired(row) {
        // 过期即终态：后续读取与提交都按过期处理（过期回执不能复活 run）。
        let _ = db.transition_invocation(&row.invocation_id, "pending", "expired")?;
        return Ok("expired".to_owned());
    }
    Ok(row.state.clone())
}

fn result_digest(result: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_string(result).unwrap_or_default().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Handlers — configuration surface
// ---------------------------------------------------------------------------

async fn hooks_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let layer_id = plugins::scope_layer_id(&scope);
    let db = hooks_db(&rt)?;
    let revision = db.layer_revision(&layer_id)?;
    let mut hooks = Vec::new();
    for row in db.bindings_for_layer(&layer_id)? {
        // 读取面重验目标：绑定后插件被移除时如实报告不可用。
        let target_resolvable = plugins::resolve_plugin_operation(
            &rt,
            &stack_for_scope(&scope),
            &row.owner_id,
            &row.operation_id,
        )?
        .is_some_and(|target| {
            target.result_schema_id == row.result_schema_id
                && serde_json::to_string(&target.result_schema).ok().as_deref()
                    == Some(row.result_schema_json.as_str())
        });
        let (state, reason_code) = if !target_resolvable {
            ("registered", json!("TARGET_OPERATION_UNRESOLVED"))
        } else if row.enabled {
            // 生产 run 流水线未接线触发（触发桥属 runtime 任务的缝合点）；
            // 离线驱动的 fire-hook 指令是当前唯一触发路径。
            ("registered", json!("HOOK_TRIGGER_NOT_ASSEMBLED"))
        } else {
            ("disabled", Value::Null)
        };
        hooks.push(json!({
            "hookId": row.hook_id,
            "trigger": row.trigger_name,
            "enabled": row.enabled,
            "runtime": {
                "state": state,
                "available": false,
                "reasonCode": reason_code,
                "generation": Value::Null,
            },
            "deadlineMs": row.deadline_ms,
            "resultSchemaId": row.result_schema_id,
        }));
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({"revision": revision, "hooks": hooks}),
    ))
}

/// The overlay stack a scope's owner resolution runs against.
fn stack_for_scope(scope: &ToolScope) -> Vec<String> {
    match scope {
        ToolScope::Global => session_layer_stack("", ""),
        ToolScope::Project { project_id } => session_layer_stack(project_id, ""),
        ToolScope::Session {
            project_id,
            session_id,
        } => session_layer_stack(project_id, session_id),
    }
}

async fn hooks_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let layer_id = plugins::scope_layer_id(&scope);
    let stack = stack_for_scope(&scope);
    let expected = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let bindings = input
        .get("bindings")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid_arg("`bindings` is required"))?;
    let db = hooks_db(&rt)?;
    let session_column = match &scope {
        ToolScope::Session { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };

    // CAS against the layer revision.
    let current = db.layer_revision(&layer_id)?;
    if expected != current {
        return Err(ContractError::new(
            "REVISION_CONFLICT",
            format!("hooks.configure expectedRevision `{expected}` does not match `{current}`"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    // 逐绑定校验：只允许已声明授权 owner/操作；非法 deadline 或 trigger 拒绝。
    let mut seen_hook_ids: Vec<String> = Vec::new();
    let mut rows = Vec::with_capacity(bindings.len());
    for binding in &bindings {
        let hook_id = binding
            .get("hookId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_arg("binding.hookId must be a non-empty string"))?
            .to_owned();
        if seen_hook_ids.contains(&hook_id) {
            return Err(invalid_arg(format!("duplicate hookId `{hook_id}` in bindings")).into());
        }
        seen_hook_ids.push(hook_id.clone());
        let trigger = binding
            .get("trigger")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_arg(format!("binding `{hook_id}` requires a trigger")))?;
        if !HOOK_EVENTS.contains(&trigger) {
            return Err(invalid_arg(format!(
                "binding `{hook_id}` has an unsupported trigger `{trigger}`"
            ))
            .into());
        }
        let owner_id = binding
            .get("ownerId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                invalid_arg(format!("binding `{hook_id}` requires a non-empty ownerId"))
            })?
            .to_owned();
        let operation_id = binding
            .get("operationId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                invalid_arg(format!(
                    "binding `{hook_id}` requires a non-empty operationId"
                ))
            })?
            .to_owned();
        let Some(target) =
            plugins::resolve_plugin_operation(&rt, &stack, &owner_id, &operation_id)?
        else {
            return Err(invalid_arg(format!(
                "binding `{hook_id}` targets `{owner_id}.{operation_id}`, which is not a \
                 declared operation of an installed plugin in this scope"
            ))
            .into());
        };
        let deadline_ms = binding
            .get("deadlineMs")
            .and_then(Value::as_i64)
            .filter(|value| *value >= 1)
            .ok_or_else(|| invalid_arg(format!("binding `{hook_id}` requires deadlineMs >= 1")))?;
        let enabled = binding
            .get("enabled")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid_arg(format!("binding `{hook_id}` requires enabled")))?;
        rows.push(BindingRow {
            layer_id: layer_id.clone(),
            hook_id,
            trigger_name: trigger.to_owned(),
            owner_id,
            operation_id,
            enabled,
            deadline_ms,
            result_schema_id: target.result_schema_id,
            result_schema_json: serde_json::to_string(&target.result_schema)
                .unwrap_or_else(|_| "{}".into()),
        });
    }

    // 写入（整体替换该层绑定集并推进层 revision）。
    let revision = db.replace_bindings(&layer_id, &rows)?;
    // 改写后校验：重读并核对钉版 Schema。
    for expected_row in &rows {
        let Some(stored) = db
            .bindings_for_layer(&layer_id)?
            .into_iter()
            .find(|row| row.hook_id == expected_row.hook_id)
        else {
            return Err(sql_err(
                "post-write verification",
                rusqlite::Error::QueryReturnedNoRows,
            )
            .into());
        };
        if stored.result_schema_id != expected_row.result_schema_id
            || stored.result_schema_json != expected_row.result_schema_json
        {
            // 合同错误表里 hooks.configure 没有 SCHEMA_MISMATCH：改写后校验
            // 发现钉版不一致属于内部一致性故障 → UNAVAILABLE。
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!(
                    "post-write verification failed for binding `{}` (target operation schema \
                     changed between validation and write)",
                    expected_row.hook_id
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
    }
    db.append_event(
        &format!("hooks:{layer_id}"),
        &session_column,
        "hooks.configurationChanged",
        &json!({
            "scope": {
                "kind": match &scope {
                    ToolScope::Global => "global",
                    ToolScope::Project { .. } => "project",
                    ToolScope::Session { .. } => "session",
                },
                "id": match &scope {
                    ToolScope::Global => Value::Null,
                    ToolScope::Project { project_id } => json!(project_id),
                    ToolScope::Session { session_id, .. } => json!(session_id),
                },
            },
            "revision": revision,
            "bindingCount": rows.len(),
        }),
    )?;
    rt.store().event_notifier().notify_waiters();
    // 本构建没有挂钩运行时代际：applied=false / generation=null 如实报告。
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "revision": revision,
            "applied": false,
            "generation": Value::Null,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Handlers — receipt surface
// ---------------------------------------------------------------------------

async fn hooks_get_invocation(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = required_str(&input, "sessionId")?;
    let invocation_id = required_str(&input, "invocationId")?;
    let db = hooks_db(&rt)?;
    // 不存在 ID 与无权限均不泄露其他会话内容：同码 NOT_FOUND。
    let Some(row) = db
        .invocation(&invocation_id)?
        .filter(|row| row.session_id == session_id)
    else {
        return Err(not_found("hook invocation", &invocation_id).into());
    };
    let state = effective_state(&db, &row)?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "invocationId": row.invocation_id,
            "hookId": row.hook_id,
            "runId": row.run_id,
            "state": state,
            "expiresAt": row.expires_at,
            "resultSchemaId": row.result_schema_id,
            "receiptId": row.receipt_id.clone().map(Value::String).unwrap_or(Value::Null),
        }),
    ))
}

async fn hooks_submit_result(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = required_str(&input, "sessionId")?;
    let invocation_id = required_str(&input, "invocationId")?;
    let completion_token_ref = required_str(&input, "completionTokenRef")?;
    let schema_id = required_str(&input, "schemaId")?;
    let result = input
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_arg("`result` is required"))?;
    let db = hooks_db(&rt)?;

    // 幂等先行：同 key 同请求重放原回执；同 key 不同请求是内容冲突。
    let digest = compute_input_digest("hooks.submit_result", &input);
    if let Some(key) = idempotency_key.as_deref() {
        match db.lookup_receipt("hooks.submit_result", key, &digest)? {
            Some(Ok(receipt)) => return Ok((ResponseOutcome::Succeeded, receipt)),
            Some(Err(())) => return Err(idempotency_conflict("hooks.submit_result", key).into()),
            None => {}
        }
    }

    let Some(row) = db
        .invocation(&invocation_id)?
        .filter(|row| row.session_id == session_id)
    else {
        return Err(not_found("hook invocation", &invocation_id).into());
    };

    // 绑定调用者：完成令牌（散列比对）。
    if token_hash(&completion_token_ref) != row.completion_token_hash {
        return Err(ContractError::new(
            CODE_PERMISSION_DENIED,
            "completionTokenRef does not match the invocation's completion token",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    // 状态机：过期/终态回执拒绝且不复活 run（拒绝无副作用）。
    let state = effective_state(&db, &row)?;
    match state.as_str() {
        "delivered" => {
            // 同结果重复只一次生效；冲突结果报 IDEMPOTENCY_CONFLICT。
            let stored_receipt = row.receipt_id.clone().unwrap_or_default();
            if row.result_digest.as_deref() == Some(result_digest(&result).as_str()) {
                return Ok((
                    ResponseOutcome::Succeeded,
                    json!({
                        "invocationId": row.invocation_id,
                        "receiptId": stored_receipt,
                        "disposition": "duplicate",
                        "runId": row.run_id,
                    }),
                ));
            }
            return Err(ContractError::new(
                CODE_IDEMPOTENCY_CONFLICT,
                format!(
                    "hook invocation `{invocation_id}` already has a different delivered result; \
                     conflicting results are rejected"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        "expired" => {
            return Err(ContractError::new(
                CODE_INVOCATION_EXPIRED,
                format!("hook invocation `{invocation_id}` has expired; its run cannot be revived"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        "cancelled" | "failed" => {
            return Err(ContractError::new(
                CODE_INVOCATION_TERMINAL,
                format!(
                    "hook invocation `{invocation_id}` is terminal ({state}); its run cannot be \
                     revived"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        _ => {}
    };

    // 结果按钉版 Schema 校验（未知字段拒绝由 Schema 自身声明约束）。
    if schema_id != row.result_schema_id {
        return Err(schema_mismatch(format!(
            "schemaId `{schema_id}` does not match the invocation's pinned result schema \
             `{}` of hook `{}`",
            row.result_schema_id, row.hook_id
        ))
        .into());
    }
    let result_schema: Value =
        serde_json::from_str(&row.result_schema_json).unwrap_or_else(|_| json!({"type": "object"}));
    if !result.is_object() {
        return Err(schema_mismatch("`result` must be an object").into());
    }
    if let Err(errors) = validate_against_schema(&result, &result_schema) {
        let details: Vec<Value> = errors
            .iter()
            .map(|error| json!({"path": error.path, "message": error.message}))
            .collect();
        let mut error = schema_mismatch(format!(
            "hook result does not satisfy the pinned schema of `{}`",
            row.hook_id
        ));
        let mut map = serde_json::Map::new();
        map.insert("validationErrors".to_owned(), Value::Array(details));
        error.details = Some(map);
        return Err(error.into());
    }

    // 交付：原子 pending → delivered。
    let receipt_id = db.next_receipt_id()?;
    if !db.deliver_invocation(
        &invocation_id,
        &receipt_id,
        &result_digest(&result),
        &serde_json::to_string(&result).unwrap_or_else(|_| "{}".into()),
    )? {
        // 竞争输给了过期/取消：按对方的终态重答（不能复活 run）。
        let row = db
            .invocation(&invocation_id)?
            .expect("invocation existed a moment ago");
        let state = effective_state(&db, &row)?;
        return Err(ContractError::new(
            if state == "expired" {
                CODE_INVOCATION_EXPIRED
            } else {
                CODE_INVOCATION_TERMINAL
            },
            format!(
                "hook invocation `{invocation_id}` became `{state}` while the result was being \
                 submitted; its run cannot be revived"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    // 事件：合同事件（invocation 资源）+ run 事件流里的结果投影（run.events
    // 可观察，与 project_turn_event 的 run.progress 投影同构）。
    db.append_event(
        &invocation_id,
        &session_id,
        "hook.resultAccepted",
        &json!({
            "invocationId": invocation_id,
            "hookId": row.hook_id,
            "runId": row.run_id,
            "receiptId": receipt_id,
        }),
    )?;
    let _ = rt.store().append_run_progress_event(
        &row.run_id,
        &session_id,
        "run.progress",
        &json!({
            "runId": row.run_id,
            "hookResult": {
                "invocationId": invocation_id,
                "hookId": row.hook_id,
                "receiptId": receipt_id,
                "result": result,
            },
        }),
    );
    rt.store().event_notifier().notify_waiters();
    rt.store().state_notifier().notify_waiters();

    let receipt = json!({
        "invocationId": invocation_id,
        "receiptId": receipt_id,
        "disposition": "accepted",
        "runId": row.run_id,
    });
    if let Some(key) = idempotency_key.as_deref() {
        db.record_receipt("hooks.submit_result", key, &digest, &receipt)?;
    }
    Ok((ResponseOutcome::Succeeded, receipt))
}

// ---------------------------------------------------------------------------
// The fire seam (the run pipeline's trigger bridge calls this; the offline
// driver's [pd-driver:fire-hook=<hookId>] directive is its deterministic user)
// ---------------------------------------------------------------------------

pub(crate) struct FiredHook {
    pub(crate) invocation_id: String,
    pub(crate) hook_id: String,
    /// The binding's deadline in milliseconds (the driver bounds its wait
    /// with it; the authoritative expiry is the row's `expires_at`, applied
    /// lazily on every read and submit).
    pub(crate) deadline_ms: i64,
}

/// Resolves the binding overlay for `run`'s session and fires `hook_id`:
/// creates the invocation record (bound to the run, with the binding's
/// deadline and pinned result schema) and emits the `hook.invoked` run event
/// the executor reads the completion-token reference from. `Ok(None)` means
/// no enabled binding with that id exists for the session. The overlay walk
/// is outside-in with the innermost definition of a hookId winning (a
/// disabled session-layer binding shadows an enabled project one).
pub(crate) fn fire_hook_for_run(
    rt: &Arc<PublicV1Runtime>,
    run: &StoredRun,
    hook_id: &str,
) -> Result<Option<FiredHook>, ContractError> {
    let db = hooks_db(rt)?;
    let layers = session_layer_stack(&run.project_id, &run.session_id);
    let mut binding: Option<BindingRow> = None;
    for layer_id in &layers {
        if let Some(row) = db
            .bindings_for_layer(layer_id)?
            .into_iter()
            .find(|row| row.hook_id == hook_id)
        {
            binding = Some(row);
        }
    }
    let Some(binding) = binding.filter(|row| row.enabled) else {
        return Ok(None);
    };

    let invocation_id = db.next_invocation_id()?;
    let completion_token_ref = format!("hkct-{}", uuid::Uuid::new_v4().simple());
    let deadline = chrono::Duration::milliseconds(binding.deadline_ms);
    let expires_at = (chrono::Utc::now() + deadline).to_rfc3339();
    db.insert_invocation(&InvocationRow {
        invocation_id: invocation_id.clone(),
        session_id: run.session_id.clone(),
        project_id: run.project_id.clone(),
        run_id: run.run_id.clone(),
        hook_id: binding.hook_id.clone(),
        trigger_name: binding.trigger_name.clone(),
        state: "pending".to_owned(),
        result_schema_id: binding.result_schema_id.clone(),
        result_schema_json: binding.result_schema_json.clone(),
        deadline_ms: binding.deadline_ms,
        expires_at: expires_at.clone(),
        completion_token_hash: token_hash(&completion_token_ref),
        receipt_id: None,
        result_digest: None,
    })?;
    let fired = FiredHook {
        invocation_id,
        hook_id: binding.hook_id.clone(),
        deadline_ms: binding.deadline_ms,
    };
    let _ = rt.store().append_run_progress_event(
        &run.run_id,
        &run.session_id,
        "hook.invoked",
        &json!({
            "runId": run.run_id,
            "invocationId": fired.invocation_id,
            "hookId": binding.hook_id,
            "trigger": binding.trigger_name,
            "ownerId": binding.owner_id,
            "operationId": binding.operation_id,
            "resultSchemaId": binding.result_schema_id,
            "deadlineMs": binding.deadline_ms,
            "expiresAt": expires_at,
            "completionTokenRef": completion_token_ref,
        }),
    );
    rt.store().event_notifier().notify_waiters();
    rt.store().state_notifier().notify_waiters();
    Ok(Some(fired))
}

/// The invocation's effective state for the driver's wait loop.
pub(crate) struct InvocationState {
    pub(crate) state: String,
}

pub(crate) fn invocation_state(
    rt: &PublicV1Runtime,
    invocation_id: &str,
) -> Result<Option<InvocationState>, ContractError> {
    let db = hooks_db(rt)?;
    let Some(row) = db.invocation(invocation_id)? else {
        return Ok(None);
    };
    let state = effective_state(&db, &row)?;
    Ok(Some(InvocationState { state }))
}

/// Marks a pending invocation expired (idempotent; whoever gets there first
/// wins — the driver's deadline or a racing read).
pub(crate) fn expire_invocation(
    rt: &PublicV1Runtime,
    invocation_id: &str,
) -> Result<bool, ContractError> {
    let db = hooks_db(rt)?;
    db.transition_invocation(invocation_id, "pending", "expired")
}

/// Marks a pending invocation cancelled (the run it was bound to was
/// cancelled; a late result answers `INVOCATION_TERMINAL`).
pub(crate) fn cancel_invocation(
    rt: &PublicV1Runtime,
    invocation_id: &str,
) -> Result<bool, ContractError> {
    let db = hooks_db(rt)?;
    db.transition_invocation(invocation_id, "pending", "cancelled")
}

// ---------------------------------------------------------------------------
// Tests (contract positive/negative cases against the real dispatch table;
// the fire flow runs the real offline driver through start_session_worker)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::{InstanceMeta, V1Store, now_rfc3339 as store_now};
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

    fn seed_project(rt: &Arc<PublicV1Runtime>, root_dir: &Path, project_id: &str) -> PathBuf {
        let store = rt.store();
        let declared_root = root_dir.parent().unwrap().join("authorized-hooks");
        std::fs::create_dir_all(&declared_root).unwrap();
        let workspace_path = declared_root.join("ws");
        std::fs::create_dir_all(&workspace_path).unwrap();
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-hooks', ?1, ?2, ?3)",
                params![
                    store.meta().instance_id,
                    declared_root.display().to_string(),
                    store_now(),
                ],
            )
            .unwrap();
        }
        rt.store()
            .register_project(&json!({
                "projectId": project_id,
                "name": "Hooks Tests",
                "environmentId": "local",
                "root": {"instanceId": store.meta().instance_id, "rootId": "root-hooks", "relativePath": "ws"},
            }))
            .unwrap();
        workspace_path
    }

    /// 播种一个带清单（声明 analyze 操作 + 结果 Schema）的插件并安装。
    async fn seed_installed_plugin(
        rt: &Arc<PublicV1Runtime>,
        workspace: &Path,
        scope: &Value,
        plugin_id: &str,
    ) {
        let dir = workspace.join("sources").join(plugin_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&json!({
                "name": plugin_id,
                "displayName": "Hook owner",
                "entry": "index.js",
                "version": "1.0.0",
                "operations": [{
                    "operationId": "analyze",
                    "resultSchema": {
                        "type": "object",
                        "properties": {"verdict": {"type": "string"}},
                        "required": ["verdict"],
                        "additionalProperties": false
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("index.js"), "// entry\n").unwrap();
        let (_, receipt) = rt
            .dispatch(
                "plugins.install",
                json!({
                    "scope": scope,
                    "pluginId": plugin_id,
                    "sourceRef": dir.display().to_string(),
                    "version": "1.0.0",
                    "expectedInstalledVersion": Value::Null,
                }),
                Some(&format!("key-{plugin_id}-install")),
                None,
            )
            .await
            .unwrap();
        wait_operation(rt, receipt["operationId"].as_str().unwrap()).await;
    }

    async fn wait_operation(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
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
            let state = got["operation"]["state"].as_str().unwrap_or_default();
            if matches!(
                state,
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return got["operation"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "operation `{operation_id}` never became terminal"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn dispatch_err(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: Option<&str>,
    ) -> ContractError {
        match rt.dispatch(method, input, key, None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    fn assert_result_valid(method: &str, result: &Value) {
        OperationRegistry::global()
            .validate_result(method, result)
            .unwrap_or_else(|e| panic!("`{method}` result failed schema validation: {e:?}"));
    }

    async fn configure_binding(
        rt: &Arc<PublicV1Runtime>,
        scope: &Value,
        hook_id: &str,
        trigger: &str,
        deadline_ms: i64,
        expected_revision: &str,
    ) -> Result<Value, ContractError> {
        let (_, result) = rt
            .dispatch(
                "hooks.configure",
                json!({
                    "scope": scope,
                    "bindings": [{
                        "hookId": hook_id,
                        "trigger": trigger,
                        "ownerId": "owner-plugin",
                        "operationId": "analyze",
                        "enabled": true,
                        "deadlineMs": deadline_ms,
                    }],
                    "expectedRevision": expected_revision,
                }),
                None,
                None,
            )
            .await
            .map_err(|error| match error {
                DispatchError::Application(error) => error,
                other => panic!("unexpected dispatch failure: {other:?}"),
            })?;
        Ok(result)
    }

    #[tokio::test]
    async fn configure_validates_and_lists_bindings() {
        let (tmp, root_dir) = fresh_home("hook-configure");
        let (rt, _iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-hooks");
        let scope = json!({"kind": "project", "id": "proj-hooks"});
        seed_installed_plugin(&rt, &workspace, &scope, "owner-plugin").await;

        // 未配置层：revision=absent。
        let (_, listed) = rt
            .dispatch("hooks.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        assert_result_valid("hooks.list", &listed);
        assert_eq!(listed["revision"], "absent");
        assert_eq!(listed["hooks"].as_array().map(Vec::len), Some(0));

        // 未知 owner → INVALID_ARGUMENT。
        let error = dispatch_err(
            &rt,
            "hooks.configure",
            json!({
                "scope": scope,
                "bindings": [{
                    "hookId": "h1", "trigger": "PostToolUse", "ownerId": "ghost-plugin",
                    "operationId": "analyze", "enabled": true, "deadlineMs": 5000,
                }],
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 未声明操作 → INVALID_ARGUMENT。
        let error = dispatch_err(
            &rt,
            "hooks.configure",
            json!({
                "scope": scope,
                "bindings": [{
                    "hookId": "h1", "trigger": "PostToolUse", "ownerId": "owner-plugin",
                    "operationId": "not-declared", "enabled": true, "deadlineMs": 5000,
                }],
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 非法 trigger → INVALID_ARGUMENT。
        let error = dispatch_err(
            &rt,
            "hooks.configure",
            json!({
                "scope": scope,
                "bindings": [{
                    "hookId": "h1", "trigger": "NotARealEvent", "ownerId": "owner-plugin",
                    "operationId": "analyze", "enabled": true, "deadlineMs": 5000,
                }],
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 非法 deadline → INVALID_ARGUMENT。
        let error = dispatch_err(
            &rt,
            "hooks.configure",
            json!({
                "scope": scope,
                "bindings": [{
                    "hookId": "h1", "trigger": "PostToolUse", "ownerId": "owner-plugin",
                    "operationId": "analyze", "enabled": true, "deadlineMs": 0,
                }],
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 合法配置：absent → hrev。
        let configured = configure_binding(&rt, &scope, "h1", "PostToolUse", 5000, "absent")
            .await
            .unwrap();
        assert_result_valid("hooks.configure", &configured);
        let revision = configured["revision"].as_str().unwrap().to_owned();
        assert!(revision.starts_with("hrev-"));
        assert_eq!(configured["applied"], false);
        assert_eq!(configured["generation"], Value::Null);

        let (_, listed) = rt
            .dispatch("hooks.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["revision"], json!(revision));
        let hooks = listed["hooks"].as_array().cloned().unwrap_or_default();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["hookId"], "h1");
        assert_eq!(hooks[0]["trigger"], "PostToolUse");
        assert_eq!(hooks[0]["enabled"], true);
        assert_eq!(hooks[0]["deadlineMs"], 5000);
        assert_eq!(
            hooks[0]["resultSchemaId"],
            "agentcpm-pd.plugin.owner-plugin.op.analyze.result/1"
        );
        // 未加载 Hook 不得标可用。
        assert_eq!(hooks[0]["runtime"]["available"], false);
        assert_eq!(hooks[0]["runtime"]["state"], "registered");
        assert_eq!(
            hooks[0]["runtime"]["reasonCode"],
            "HOOK_TRIGGER_NOT_ASSEMBLED"
        );

        // 旧 revision → REVISION_CONFLICT。
        let error = dispatch_err(
            &rt,
            "hooks.configure",
            json!({
                "scope": scope,
                "bindings": [],
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        // 目标插件被移除后：绑定读取面如实报告目标不可解析。
        let (_, receipt) = rt
            .dispatch(
                "plugins.remove",
                json!({
                    "scope": scope,
                    "pluginId": "owner-plugin",
                    "expectedVersion": "1.0.0",
                }),
                Some("key-remove-owner"),
                None,
            )
            .await
            .unwrap();
        wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        let (_, listed) = rt
            .dispatch("hooks.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        assert_eq!(
            listed["hooks"][0]["runtime"]["reasonCode"],
            "TARGET_OPERATION_UNRESOLVED"
        );
        drop(tmp);
    }

    /// 提交一次 run 并等待终态，返回 (run_id, 最终 run JSON)。
    async fn drive_run(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        session_id: &str,
        text: &str,
        key: &str,
    ) -> (String, Value) {
        let (_, submitted) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": text}],
                }),
                Some(key),
                None,
            )
            .await
            .expect("run.submit");
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        rt.start_session_worker(run_id.clone());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, got) = rt
                .dispatch(
                    "run.get",
                    json!({"instanceId": iid, "runId": run_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["run"]["terminal"] == true {
                return (run_id, got["run"].clone());
            }
            assert!(Instant::now() < deadline, "run never became terminal");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// 等待某个 hook invocation 出现并返回其 get_invocation 结果。
    async fn wait_invocation(rt: &Arc<PublicV1Runtime>, session_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let db = hooks_db(rt).unwrap();
            let mut stmt = db
                .with_conn("scan invocations", |conn| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT invocation_id FROM v1_hook_invocations
                             WHERE session_id = ?1 ORDER BY created_at DESC LIMIT 1",
                        )
                        .unwrap();
                    let rows = stmt
                        .query_map(params![session_id], |r| r.get::<_, String>(0))
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap();
                    Ok(rows)
                })
                .unwrap();
            if let Some(id) = stmt.pop() {
                let (_, got) = rt
                    .dispatch(
                        "hooks.get_invocation",
                        json!({"sessionId": session_id, "invocationId": id}),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                return got;
            }
            assert!(
                Instant::now() < deadline,
                "no hook invocation ever appeared for `{session_id}`"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn fire_and_submit_result_delivers_and_run_resumes() {
        let (tmp, root_dir) = fresh_home("hook-flow");
        let (rt, iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-flow");
        let project_scope = json!({"kind": "project", "id": "proj-flow"});
        seed_installed_plugin(&rt, &workspace, &project_scope, "owner-plugin").await;
        configure_binding(&rt, &project_scope, "on-stop", "Stop", 30000, "absent")
            .await
            .unwrap();

        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "sess-flow", "projectId": "proj-flow"}),
            None,
            None,
        )
        .await
        .unwrap();

        // 提交一个带 fire-hook 指令的 run：run 等待 hook 结果。
        let handle = tokio::spawn({
            let rt = rt.clone();
            let iid = iid.clone();
            async move {
                drive_run(
                    &rt,
                    &iid,
                    "sess-flow",
                    "please run [pd-driver:fire-hook=on-stop]",
                    "key-flow-run",
                )
                .await
            }
        });

        // invocation 出现：pending + 绑定 run + 结果 Schema 钉版。
        let invocation = wait_invocation(&rt, "sess-flow").await;
        assert_result_valid("hooks.get_invocation", &invocation);
        let invocation_id = invocation["invocationId"].as_str().unwrap().to_owned();
        assert_eq!(invocation["state"], "pending");
        assert_eq!(invocation["hookId"], "on-stop");
        assert_eq!(
            invocation["resultSchemaId"],
            "agentcpm-pd.plugin.owner-plugin.op.analyze.result/1"
        );
        assert!(invocation["receiptId"].is_null());
        let run_id = invocation["runId"].as_str().unwrap().to_owned();
        assert!(parse_rfc3339(invocation["expiresAt"].as_str().unwrap()).is_some());

        // 从 run 事件流读出完成令牌引用（执行器的发现路径）。
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let token = events["frame"]["events"]
            .as_array()
            .and_then(|events| {
                events
                    .iter()
                    .find(|event| event["type"] == "hook.invoked")
                    .and_then(|event| {
                        event["payload"]["completionTokenRef"]
                            .as_str()
                            .map(str::to_owned)
                    })
            })
            .expect("hook.invoked event carries the completion token ref");
        assert!(token.starts_with("hkct-"));

        // 错误令牌 → PERMISSION_DENIED。
        let error = dispatch_err(
            &rt,
            "hooks.submit_result",
            json!({
                "sessionId": "sess-flow",
                "invocationId": invocation_id,
                "completionTokenRef": "hkct-forged",
                "schemaId": invocation["resultSchemaId"],
                "result": {"verdict": "ok"},
            }),
            Some("key-submit-forged"),
        )
        .await;
        assert_eq!(error.code, "PERMISSION_DENIED");

        // Schema 不匹配（未知字段）→ SCHEMA_MISMATCH。
        let error = dispatch_err(
            &rt,
            "hooks.submit_result",
            json!({
                "sessionId": "sess-flow",
                "invocationId": invocation_id,
                "completionTokenRef": token,
                "schemaId": invocation["resultSchemaId"],
                "result": {"verdict": "ok", "shell": "bash"},
            }),
            Some("key-submit-schema"),
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // 错误 schemaId → SCHEMA_MISMATCH。
        let error = dispatch_err(
            &rt,
            "hooks.submit_result",
            json!({
                "sessionId": "sess-flow",
                "invocationId": invocation_id,
                "completionTokenRef": token,
                "schemaId": "agentcpm-pd.other/1",
                "result": {"verdict": "ok"},
            }),
            Some("key-submit-schema-id"),
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // 合法提交：accepted，run 复活并成功携带 hook 结果。
        let (_, submitted) = rt
            .dispatch(
                "hooks.submit_result",
                json!({
                    "sessionId": "sess-flow",
                    "invocationId": invocation_id,
                    "completionTokenRef": token,
                    "schemaId": invocation["resultSchemaId"],
                    "result": {"verdict": "allow"},
                }),
                Some("key-submit-1"),
                None,
            )
            .await
            .unwrap();
        assert_result_valid("hooks.submit_result", &submitted);
        assert_eq!(submitted["disposition"], "accepted");
        assert_eq!(submitted["runId"], json!(run_id));
        let receipt_id = submitted["receiptId"].as_str().unwrap().to_owned();

        let (finished_run_id, run) = handle.await.unwrap();
        assert_eq!(finished_run_id, run_id);
        assert_eq!(run["state"], "succeeded");
        // 合同面 run.result 只能是 Artifact|null：复活不带结果值；hook 结果
        // 的可观察面是 submit_result 落在 run 事件流里的 run.progress 投影。
        assert_eq!(run["result"], Value::Null);
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let hook_progress = events["frame"]["events"]
            .as_array()
            .expect("run.events frame")
            .iter()
            .find(|event| {
                event["type"] == "run.progress" && event["payload"]["hookResult"].is_object()
            })
            .expect("run.progress projection of the hook result");
        assert_eq!(
            hook_progress["payload"]["hookResult"]["result"]["verdict"],
            "allow"
        );

        // 幂等重放：同 key 同请求返回原回执。
        let (_, replay) = rt
            .dispatch(
                "hooks.submit_result",
                json!({
                    "sessionId": "sess-flow",
                    "invocationId": invocation_id,
                    "completionTokenRef": token,
                    "schemaId": invocation["resultSchemaId"],
                    "result": {"verdict": "allow"},
                }),
                Some("key-submit-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["disposition"], "accepted");
        assert_eq!(replay["receiptId"], json!(receipt_id));

        // 同结果、新 key → duplicate（只一次生效）。
        let (_, duplicate) = rt
            .dispatch(
                "hooks.submit_result",
                json!({
                    "sessionId": "sess-flow",
                    "invocationId": invocation_id,
                    "completionTokenRef": token,
                    "schemaId": invocation["resultSchemaId"],
                    "result": {"verdict": "allow"},
                }),
                Some("key-submit-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(duplicate["disposition"], "duplicate");
        assert_eq!(duplicate["receiptId"], json!(receipt_id));

        // 冲突结果 → IDEMPOTENCY_CONFLICT。
        let error = dispatch_err(
            &rt,
            "hooks.submit_result",
            json!({
                "sessionId": "sess-flow",
                "invocationId": invocation_id,
                "completionTokenRef": token,
                "schemaId": invocation["resultSchemaId"],
                "result": {"verdict": "deny"},
            }),
            Some("key-submit-3"),
        )
        .await;
        assert_eq!(error.code, "IDEMPOTENCY_CONFLICT");

        // 终态回执：已交付 → 再提交不同结果仍拒绝；get_invocation 反映 delivered。
        let (_, invocation) = rt
            .dispatch(
                "hooks.get_invocation",
                json!({"sessionId": "sess-flow", "invocationId": invocation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(invocation["state"], "delivered");
        assert_eq!(invocation["receiptId"], json!(receipt_id));

        // 跨会话读取 → NOT_FOUND（不泄露）。
        let error = dispatch_err(
            &rt,
            "hooks.get_invocation",
            json!({"sessionId": "sess-other", "invocationId": invocation_id}),
            None,
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
        drop(tmp);
    }

    #[tokio::test]
    async fn expired_receipt_cannot_revive_the_run() {
        let (tmp, root_dir) = fresh_home("hook-expiry");
        let (rt, iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-exp");
        let project_scope = json!({"kind": "project", "id": "proj-exp"});
        seed_installed_plugin(&rt, &workspace, &project_scope, "owner-plugin").await;
        // 短 deadline：过期可确定性观察。
        configure_binding(&rt, &project_scope, "quick-hook", "Stop", 800, "absent")
            .await
            .unwrap();
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "sess-exp", "projectId": "proj-exp"}),
            None,
            None,
        )
        .await
        .unwrap();

        let handle = tokio::spawn({
            let rt = rt.clone();
            let iid = iid.clone();
            async move {
                drive_run(
                    &rt,
                    &iid,
                    "sess-exp",
                    "[pd-driver:fire-hook=quick-hook]",
                    "key-exp-run",
                )
                .await
            }
        });

        let invocation = wait_invocation(&rt, "sess-exp").await;
        let invocation_id = invocation["invocationId"].as_str().unwrap().to_owned();
        let run_id = invocation["runId"].as_str().unwrap().to_owned();

        // 等 run 终态：过期 hook → run 失败。
        let (finished_run_id, run) = handle.await.unwrap();
        assert_eq!(finished_run_id, run_id);
        assert_eq!(run["state"], "failed");
        assert_eq!(run["outcomeCode"], "HOOK_INVOCATION_EXPIRED");

        // invocation 已过期；迟到回执拒绝且不复活 run。
        let (_, invocation) = rt
            .dispatch(
                "hooks.get_invocation",
                json!({"sessionId": "sess-exp", "invocationId": invocation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(invocation["state"], "expired");

        // run 事件流里有完成令牌引用（迟到提交者拿得到令牌，仍被拒）。
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let token = events["frame"]["events"]
            .as_array()
            .and_then(|events| {
                events
                    .iter()
                    .find(|event| event["type"] == "hook.invoked")
                    .and_then(|event| {
                        event["payload"]["completionTokenRef"]
                            .as_str()
                            .map(str::to_owned)
                    })
            })
            .expect("hook.invoked event carries the token");
        let error = dispatch_err(
            &rt,
            "hooks.submit_result",
            json!({
                "sessionId": "sess-exp",
                "invocationId": invocation_id,
                "completionTokenRef": token,
                "schemaId": invocation["resultSchemaId"],
                "result": {"verdict": "late"},
            }),
            Some("key-late-submit"),
        )
        .await;
        assert_eq!(error.code, "INVOCATION_EXPIRED");
        // 拒绝无副作用：run 仍是失败态，invocation 仍过期。
        let (_, run_after) = rt
            .dispatch(
                "run.get",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(run_after["run"]["state"], "failed");
        drop(tmp);
    }

    #[tokio::test]
    async fn cancelled_run_makes_the_invocation_terminal() {
        let (tmp, root_dir) = fresh_home("hook-cancel");
        let (rt, iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-cancel");
        let project_scope = json!({"kind": "project", "id": "proj-cancel"});
        seed_installed_plugin(&rt, &workspace, &project_scope, "owner-plugin").await;
        configure_binding(&rt, &project_scope, "long-hook", "Stop", 60000, "absent")
            .await
            .unwrap();
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "sess-cancel", "projectId": "proj-cancel"}),
            None,
            None,
        )
        .await
        .unwrap();

        let handle = tokio::spawn({
            let rt = rt.clone();
            let iid = iid.clone();
            async move {
                drive_run(
                    &rt,
                    &iid,
                    "sess-cancel",
                    "[pd-driver:fire-hook=long-hook]",
                    "key-cancel-run",
                )
                .await
            }
        });

        let invocation = wait_invocation(&rt, "sess-cancel").await;
        let invocation_id = invocation["invocationId"].as_str().unwrap().to_owned();
        let run_id = invocation["runId"].as_str().unwrap().to_owned();

        // 取消 run：invocation 进入 cancelled 终态。
        // run.cancel 是 idempotentSubmit（合同 retry）：必须携带 idempotencyKey，
        // 否则 -32086（dispatch_err 同族）。
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": run_id}),
            Some("key-cancel-run-cancel"),
            None,
        )
        .await
        .unwrap();
        let (_finished_id, run) = handle.await.unwrap();
        assert_eq!(run["state"], "cancelled");

        let (_, invocation) = rt
            .dispatch(
                "hooks.get_invocation",
                json!({"sessionId": "sess-cancel", "invocationId": invocation_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(invocation["state"], "cancelled");

        // 终态回执 → INVOCATION_TERMINAL。
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let token = events["frame"]["events"]
            .as_array()
            .and_then(|events| {
                events
                    .iter()
                    .find(|event| event["type"] == "hook.invoked")
                    .and_then(|event| {
                        event["payload"]["completionTokenRef"]
                            .as_str()
                            .map(str::to_owned)
                    })
            })
            .expect("hook.invoked event carries the token");
        let error = dispatch_err(
            &rt,
            "hooks.submit_result",
            json!({
                "sessionId": "sess-cancel",
                "invocationId": invocation_id,
                "completionTokenRef": token,
                "schemaId": invocation["resultSchemaId"],
                "result": {"verdict": "late"},
            }),
            Some("key-cancel-submit"),
        )
        .await;
        assert_eq!(error.code, "INVOCATION_TERMINAL");
        drop(tmp);
    }

    #[tokio::test]
    async fn fire_without_binding_fails_the_run_and_overlays_shadow() {
        let (tmp, root_dir) = fresh_home("hook-overlay");
        let (rt, iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-ovl");
        let project_scope = json!({"kind": "project", "id": "proj-ovl"});
        seed_installed_plugin(&rt, &workspace, &project_scope, "owner-plugin").await;
        // 项目层绑定 shadow-hook（deadlne 短）。
        configure_binding(&rt, &project_scope, "shadow-hook", "Stop", 60000, "absent")
            .await
            .unwrap();
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "sess-ovl", "projectId": "proj-ovl"}),
            None,
            None,
        )
        .await
        .unwrap();

        // 无绑定 → run 失败 HOOK_BINDING_NOT_FOUND。
        let (run_id, run) = drive_run(
            &rt,
            &iid,
            "sess-ovl",
            "[pd-driver:fire-hook=missing-hook]",
            "key-ovl-missing",
        )
        .await;
        assert_eq!(run["state"], "failed");
        assert_eq!(run["outcomeCode"], "HOOK_BINDING_NOT_FOUND");

        // run 失败后队列按 run_failed 自动暂停（OfflineRunDriver 契约行为）：
        // 后续 run.submit 会被 QUEUE_PAUSED 拒绝，先确认暂停再恢复队列。
        let (_, qget) = rt
            .dispatch(
                "queue.get",
                json!({"instanceId": iid, "sessionId": "sess-ovl"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(qget["queue"]["paused"], true);
        assert_eq!(qget["queue"]["pauseReason"], "run_failed");
        let (_, resumed) = rt
            .dispatch(
                "queue.resume",
                json!({
                    "instanceId": iid,
                    "sessionId": "sess-ovl",
                    "expectedRevision": qget["queue"]["revision"],
                    "acknowledgedPauseReason": qget["queue"]["pauseReason"],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(resumed["queue"]["paused"], false);

        // 会话层覆盖项目层同 hookId：deadline 用内层的。
        let session_scope = json!({"kind": "session", "id": "sess-ovl"});
        let configured =
            configure_binding(&rt, &session_scope, "shadow-hook", "Stop", 60000, "absent")
                .await
                .unwrap();
        assert!(
            configured["revision"]
                .as_str()
                .unwrap()
                .starts_with("hrev-")
        );

        // 内层禁用后：外层同 id 绑定不生效（内层覆盖）。
        let (_, _) = rt
            .dispatch(
                "hooks.configure",
                json!({
                    "scope": session_scope,
                    "bindings": [{
                        "hookId": "shadow-hook", "trigger": "Stop", "ownerId": "owner-plugin",
                        "operationId": "analyze", "enabled": false, "deadlineMs": 60000,
                    }],
                    "expectedRevision": configured["revision"],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let (run_id2, run2) = drive_run(
            &rt,
            &iid,
            "sess-ovl",
            "[pd-driver:fire-hook=shadow-hook]",
            "key-ovl-shadow",
        )
        .await;
        assert_eq!(run2["state"], "failed");
        assert_eq!(run2["outcomeCode"], "HOOK_BINDING_NOT_FOUND");
        assert_ne!(run_id, run_id2);
        drop(tmp);
    }
}
