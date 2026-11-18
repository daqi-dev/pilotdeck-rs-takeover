//! `plugins.*` — the v1 plugin lifecycle domain (`cli.task.hooks`, S3).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 7 个 `plugins.*` 条目；TS
//! 基线（cc7c4e6 `ui/server/routes/plugins.js` + `utils/plugin-loader.js`）
//! 是语义参考（清单校验规则、名称/来源校验、临时目录安装的「半安装不可
//! 见」机制都来自它）。
//!
//! 领域模型（合同重设计后的 v1 形态，旧线 `runtime/plugins/*` 装配假定不
//! 适用，见任务报告的复用登记）：
//!
//! * 安装面按 scope（global/project/session）分层：一行 `(layer, pluginId)`
//!   是一个已安装插件；安装资源（清单树的拷贝）落在领域自己的
//!   `<home>/v1/plugins/<layer-slug>/<pluginId>` 下，remove 只回收该目录。
//! * `install` 是 `retry=idempotentSubmit` 异步操作（受理即回执，执行体抄
//!   基线 `.tmp-` 目录模式：拷进临时目录、全部成功才原子换名+落行——失败
//!   无半安装公告，list/get 永远看不到安装中的插件）。
//! * `sourceRef` 形态（合同只给了 string；选择登记在报告）：本地目录（含
//!   合法 `manifest.json`，必须解析进 scope 锚点内——project/session 用项目
//!   工作区、global 用实例 home，越界报 `PATH_OUTSIDE_SCOPE`）或 git URL
//!   （`https://`/`git@` 前缀，基线同款校验；本构建无网络抓取桥，受理后
//!   以 `UPSTREAM_ERROR` 诚实失败）。
//! * `expectedInstalledVersion` 是 CAS：`null` 表示「当前必须未安装」，
//!   字符串表示「当前必须是这个版本」（更新路径）。
//! * `transition`（enable/disable/reload）按 `expectedGeneration` CAS；
//!   generation 是绑定代际的稳定 id（`pgen-…`），enable/reload 分配新代
//!   际，disable 停新调用并保留旧代际（在途绑定清单 `retainedBindings`）。
//! * **诚实不可用**：本 v1 网关装配没有插件运行时（没有进程/代码加载桥），
//!   所以 `runtime.available` 恒为 false——已安装、已加载状态分别可见
//!   （`installed` 布尔 + `runtime.state`），但任何插件都不公告可用能力；
//!   `plugins.invoke` 对已声明操作受理后以 `PLUGIN_EXECUTION_UNAVAILABLE`
//!   诚实失败（效果执行桥属 runtime/extensions 任务，tools.test 同款先例）。
//! * `remove` 默认保留配置（`removeConfiguration=false`）：配置暂存进
//!   `v1_plugins_retired`，重装时恢复；`removeConfiguration=true` 连暂存
//!   一起清掉。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::schema_validator::validate_against_schema;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map as JsonMap, Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::{ProjectResolveError, resolve_project};
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::domains::tools::{ToolScope, parse_tool_scope, required_str};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for.
const CODE_PERMISSION_DENIED: &str = "PERMISSION_DENIED";
const CODE_VERSION_CONFLICT: &str = "VERSION_CONFLICT";
const CODE_GENERATION_CONFLICT: &str = "GENERATION_CONFLICT";
const CODE_RESOURCE_BUSY: &str = "RESOURCE_BUSY";
const CODE_PATH_OUTSIDE_SCOPE: &str = "PATH_OUTSIDE_SCOPE";
const CODE_UPSTREAM_ERROR: &str = "UPSTREAM_ERROR";
const CODE_SCHEMA_MISMATCH: &str = "SCHEMA_MISMATCH";
const CODE_REVISION_CONFLICT: &str = "REVISION_CONFLICT";
const CODE_PLUGIN_EXECUTION_UNAVAILABLE: &str = "PLUGIN_EXECUTION_UNAVAILABLE";

/// The `expectedGeneration` sentinel for "the plugin has no runtime
/// generation yet" — the same convention the contract family spells out for
/// `expectedRevision` ("创建使用absent").
pub(crate) const GENERATION_ABSENT: &str = "absent";

/// 基线 `validateManifest` 的名称规则（`ui/server/utils/plugin-loader.js`）。
fn plugin_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The uniform schema body a plugin without a declared result schema gets
/// (hook/invocation results are data, never executed; no method/shell field is
/// ever interpreted).
const DEFAULT_OPERATION_RESULT_SCHEMA: &str = r#"{"type":"object","additionalProperties":true}"#;
/// A plugin without a declared configuration schema admits no configuration
/// keys ("未知字段拒绝" — an empty object is the only valid value).
const DEFAULT_CONFIGURATION_SCHEMA: &str = r#"{"type":"object","additionalProperties":false}"#;

const DDL_PLUGINS: &str = "CREATE TABLE IF NOT EXISTS v1_plugins (
    layer_id TEXT NOT NULL,
    plugin_id TEXT NOT NULL,
    version TEXT NOT NULL,
    manifest_hash TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    generation TEXT,
    manifest_json TEXT NOT NULL,
    operations_json TEXT NOT NULL DEFAULT '[]',
    config_schema_json TEXT NOT NULL,
    configuration_json TEXT NOT NULL DEFAULT '{}',
    config_revision TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (layer_id, plugin_id)
);";

const DDL_PLUGINS_META: &str = "CREATE TABLE IF NOT EXISTS v1_plugins_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

const DDL_PLUGINS_RETIRED: &str = "CREATE TABLE IF NOT EXISTS v1_plugins_retired (
    layer_id TEXT NOT NULL,
    plugin_id TEXT NOT NULL,
    configuration_json TEXT NOT NULL,
    config_revision TEXT NOT NULL,
    retired_at TEXT NOT NULL,
    PRIMARY KEY (layer_id, plugin_id)
);";

/// plugins.invoke 的受理行（remove 的 RESOURCE_BUSY 检查面：活动调用未排空
/// 则拒绝移除）。本构建 invoke 全部受理后立刻终态化，所以恒为空——但检查
/// 是真检查，不是占位。
const DDL_PLUGIN_INVOCATIONS: &str = "CREATE TABLE IF NOT EXISTS v1_plugin_invocations (
    invocation_id TEXT PRIMARY KEY,
    layer_id TEXT NOT NULL,
    plugin_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_plugins", DDL_PLUGINS);
    reg.schema("v1_plugins_meta", DDL_PLUGINS_META);
    reg.schema("v1_plugins_retired", DDL_PLUGINS_RETIRED);
    reg.schema("v1_plugin_invocations", DDL_PLUGIN_INVOCATIONS);
    reg.handler("plugins.list", |rt, input, _key, _rev| {
        Box::pin(plugins_list(rt, input))
    });
    reg.handler("plugins.get", |rt, input, _key, _rev| {
        Box::pin(plugins_get(rt, input))
    });
    reg.handler("plugins.install", |rt, input, key, _rev| {
        Box::pin(plugins_install(rt, input, key))
    });
    reg.handler("plugins.configure", |rt, input, _key, rev| {
        Box::pin(plugins_configure(rt, input, rev))
    });
    reg.handler("plugins.transition", |rt, input, key, _rev| {
        Box::pin(plugins_transition(rt, input, key))
    });
    reg.handler("plugins.remove", |rt, input, key, _rev| {
        Box::pin(plugins_remove(rt, input, key))
    });
    reg.handler("plugins.invoke", |rt, input, key, _rev| {
        Box::pin(plugins_invoke(rt, input, key))
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

fn version_conflict(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_VERSION_CONFLICT,
        message.into(),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn generation_conflict(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_GENERATION_CONFLICT,
        message.into(),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn path_outside_scope(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_PATH_OUTSIDE_SCOPE,
        message.into(),
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
// Layer model (scope → durable layer id + filesystem slug)
// ---------------------------------------------------------------------------

fn layer_key(kind: &str, id: &str) -> String {
    serde_json::to_string(&json!({"kind": kind, "id": id})).unwrap_or_else(|_| kind.to_owned())
}

pub(crate) fn scope_layer_id(scope: &ToolScope) -> String {
    match scope {
        ToolScope::Global => "global".to_owned(),
        ToolScope::Project { project_id } => layer_key("project", project_id),
        ToolScope::Session { session_id, .. } => layer_key("session", session_id),
    }
}

/// The overlay stack a run in `session` resolves plugin owners against
/// (bottom-up: global, project, session; inner layers shadow outer ones).
pub(crate) fn session_layer_stack(project_id: &str, session_id: &str) -> Vec<String> {
    let mut layers = vec!["global".to_owned()];
    if !project_id.is_empty() {
        layers.push(layer_key("project", project_id));
    }
    if !session_id.is_empty() {
        layers.push(layer_key("session", session_id));
    }
    layers
}

/// Filesystem-safe slug for a layer id (arbitrary caller ids never reach a
/// path component).
fn layer_slug(layer_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(layer_id.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    hex[..16].to_owned()
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

pub(crate) struct PluginRow {
    pub(crate) layer_id: String,
    pub(crate) plugin_id: String,
    pub(crate) version: String,
    pub(crate) manifest_hash: String,
    pub(crate) source_ref: String,
    pub(crate) enabled: bool,
    pub(crate) generation: Option<String>,
    pub(crate) manifest_json: String,
    pub(crate) operations_json: String,
    pub(crate) config_schema_json: String,
    pub(crate) configuration_json: String,
    pub(crate) config_revision: String,
}

/// One manifest-declared plugin operation.
#[derive(Clone, Debug)]
pub(crate) struct PluginOperationDecl {
    pub(crate) operation_id: String,
    pub(crate) input_schema_id: String,
    pub(crate) input_schema: Value,
    pub(crate) result_schema_id: String,
    pub(crate) result_schema: Value,
    pub(crate) permission: String,
}

/// The domain's SQLite handle (a second connection to the shared
/// `<home>/v1/state.sqlite`, the schedules/configuration seam).
pub(crate) struct PluginsDb {
    conn: Mutex<Connection>,
}

impl PluginsDb {
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
            DDL_PLUGINS,
            DDL_PLUGINS_META,
            DDL_PLUGINS_RETIRED,
            DDL_PLUGIN_INVOCATIONS,
        ] {
            conn.execute_batch(ddl)
                .map_err(|e| sql_err("plugins ddl", e))?;
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

    fn next_meta_counter(&self, key: &str) -> Result<i64, ContractError> {
        self.with_tx("bump plugins counter", |tx| {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM v1_plugins_meta WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;
            let next = current.unwrap_or(0) + 1;
            tx.execute(
                "INSERT INTO v1_plugins_meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, next.to_string()],
            )?;
            Ok(next)
        })
    }

    pub(crate) fn row(
        &self,
        layer_id: &str,
        plugin_id: &str,
    ) -> Result<Option<PluginRow>, ContractError> {
        self.with_conn("read plugin", |conn| {
            conn.query_row(
                "SELECT layer_id, plugin_id, version, manifest_hash, source_ref, enabled,
                        generation, manifest_json, operations_json, config_schema_json,
                        configuration_json, config_revision, updated_at
                 FROM v1_plugins WHERE layer_id = ?1 AND plugin_id = ?2",
                params![layer_id, plugin_id],
                row_to_plugin,
            )
            .optional()
        })
    }

    pub(crate) fn rows_for_layer(&self, layer_id: &str) -> Result<Vec<PluginRow>, ContractError> {
        self.with_conn("list plugins", |conn| {
            let mut stmt = conn.prepare(
                "SELECT layer_id, plugin_id, version, manifest_hash, source_ref, enabled,
                        generation, manifest_json, operations_json, config_schema_json,
                        configuration_json, config_revision, updated_at
                 FROM v1_plugins WHERE layer_id = ?1 ORDER BY plugin_id ASC",
            )?;
            let rows = stmt
                .query_map(params![layer_id], row_to_plugin)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// The first row for `plugin_id` across `layers` (inner layers shadow
    /// outer ones, so the stack is walked outside-in and the first hit wins).
    pub(crate) fn row_in_layers(
        &self,
        layers: &[String],
        plugin_id: &str,
    ) -> Result<Option<PluginRow>, ContractError> {
        for layer_id in layers {
            if let Some(row) = self.row(layer_id, plugin_id)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn upsert_row(&self, row: &PluginRow) -> Result<(), ContractError> {
        self.with_conn("write plugin", |conn| {
            conn.execute(
                "INSERT INTO v1_plugins
                    (layer_id, plugin_id, version, manifest_hash, source_ref, enabled, generation,
                     manifest_json, operations_json, config_schema_json, configuration_json,
                     config_revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
                 ON CONFLICT(layer_id, plugin_id) DO UPDATE SET
                    version = excluded.version,
                    manifest_hash = excluded.manifest_hash,
                    source_ref = excluded.source_ref,
                    generation = excluded.generation,
                    manifest_json = excluded.manifest_json,
                    operations_json = excluded.operations_json,
                    config_schema_json = excluded.config_schema_json,
                    configuration_json = excluded.configuration_json,
                    config_revision = excluded.config_revision,
                    updated_at = excluded.updated_at",
                params![
                    row.layer_id,
                    row.plugin_id,
                    row.version,
                    row.manifest_hash,
                    row.source_ref,
                    row.enabled as i64,
                    row.generation,
                    row.manifest_json,
                    row.operations_json,
                    row.config_schema_json,
                    row.configuration_json,
                    row.config_revision,
                    now_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    fn delete_row(&self, layer_id: &str, plugin_id: &str) -> Result<(), ContractError> {
        self.with_conn("delete plugin", |conn| {
            conn.execute(
                "DELETE FROM v1_plugins WHERE layer_id = ?1 AND plugin_id = ?2",
                params![layer_id, plugin_id],
            )?;
            Ok(())
        })
    }

    /// The retired-configuration stash: remove keeps the configuration when
    /// `removeConfiguration=false` so a re-install restores it.
    fn stash_retired(
        &self,
        layer_id: &str,
        plugin_id: &str,
        configuration_json: &str,
        config_revision: &str,
    ) -> Result<(), ContractError> {
        self.with_conn("stash retired config", |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO v1_plugins_retired
                    (layer_id, plugin_id, configuration_json, config_revision, retired_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    layer_id,
                    plugin_id,
                    configuration_json,
                    config_revision,
                    now_rfc3339()
                ],
            )?;
            Ok(())
        })
    }

    fn take_retired(
        &self,
        layer_id: &str,
        plugin_id: &str,
    ) -> Result<Option<(String, String)>, ContractError> {
        self.with_conn("take retired config", |conn| {
            conn.query_row(
                "SELECT configuration_json, config_revision FROM v1_plugins_retired
                 WHERE layer_id = ?1 AND plugin_id = ?2",
                params![layer_id, plugin_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
    }

    fn drop_retired(&self, layer_id: &str, plugin_id: &str) -> Result<(), ContractError> {
        self.with_conn("drop retired config", |conn| {
            conn.execute(
                "DELETE FROM v1_plugins_retired WHERE layer_id = ?1 AND plugin_id = ?2",
                params![layer_id, plugin_id],
            )?;
            Ok(())
        })
    }

    // ---- plugins.invoke 的活动调用登记（remove 的 RESOURCE_BUSY 面）--------

    fn insert_invocation(
        &self,
        invocation_id: &str,
        layer_id: &str,
        plugin_id: &str,
        operation_id: &str,
    ) -> Result<(), ContractError> {
        let now = now_rfc3339();
        self.with_conn("insert plugin invocation", |conn| {
            conn.execute(
                "INSERT INTO v1_plugin_invocations
                    (invocation_id, layer_id, plugin_id, operation_id, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?5)",
                params![invocation_id, layer_id, plugin_id, operation_id, now],
            )?;
            Ok(())
        })
    }

    fn finish_invocation(&self, invocation_id: &str) -> Result<(), ContractError> {
        self.with_conn("finish plugin invocation", |conn| {
            conn.execute(
                "UPDATE v1_plugin_invocations
                 SET state = 'finished', updated_at = ?2 WHERE invocation_id = ?1",
                params![invocation_id, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    fn active_invocations(&self, layer_id: &str, plugin_id: &str) -> Result<usize, ContractError> {
        self.with_conn("count active invocations", |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM v1_plugin_invocations
                 WHERE layer_id = ?1 AND plugin_id = ?2 AND state = 'running'",
                params![layer_id, plugin_id],
                |r| r.get(0),
            )?;
            Ok(count as usize)
        })
    }

    /// The domain event append (the store's placeholder→`cur-<seq>` pattern,
    /// through this domain's connection).
    fn append_event(
        &self,
        resource_id: &str,
        session_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), ContractError> {
        self.with_conn("append plugins event", |conn| {
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

fn row_to_plugin(row: &rusqlite::Row<'_>) -> rusqlite::Result<PluginRow> {
    Ok(PluginRow {
        layer_id: row.get(0)?,
        plugin_id: row.get(1)?,
        version: row.get(2)?,
        manifest_hash: row.get(3)?,
        source_ref: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        generation: row.get(6)?,
        manifest_json: row.get(7)?,
        operations_json: row.get(8)?,
        config_schema_json: row.get(9)?,
        configuration_json: row.get(10)?,
        config_revision: row.get(11)?,
    })
}

/// The per-dataRoot singletons (the tools.rs seam pattern): one plugins DB and
/// one operations DB shared with the schedules/background_tasks records.
fn plugins_db(rt: &PublicV1Runtime) -> Result<Arc<PluginsDb>, ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut reg = PLUGINS_DB_REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(db) = reg.get(&home).and_then(Weak::upgrade) {
        return Ok(db);
    }
    let db = Arc::new(PluginsDb::open(&home)?);
    reg.insert(home, Arc::downgrade(&db));
    Ok(db)
}

static PLUGINS_DB_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<PluginsDb>>>> = OnceLock::new();

fn operations_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
    fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<DomainDb>>> {
        static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }
    let home = rt.store().home().to_path_buf();
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(db) = reg.get(&home).and_then(Weak::upgrade) {
        return Ok(db);
    }
    let db = Arc::new(DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?);
    reg.insert(home, Arc::downgrade(&db));
    Ok(db)
}

// ---------------------------------------------------------------------------
// Manifest (baseline `validateManifest` rules, v1 projection)
// ---------------------------------------------------------------------------

/// The processed manifest of an install candidate.
pub(crate) struct PreparedManifest {
    pub(crate) manifest_json: String,
    pub(crate) manifest_hash: String,
    pub(crate) operations: Vec<PluginOperationDecl>,
    pub(crate) config_schema: String,
}

fn manifest_str_field<'a>(manifest: &'a JsonMap<String, Value>, key: &str) -> Option<&'a str> {
    manifest.get(key).and_then(Value::as_str)
}

/// 顶层 `additionalProperties` 未声明时硬编码为 false：合同要求「未知字段
/// 拒绝」，清单没写就按拒绝处理（报告登记）。
fn harden_unknown_fields(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if schema.get("type").and_then(Value::as_str) == Some("object")
        && schema.get("additionalProperties").is_none()
        && let Some(object) = schema.as_object_mut()
    {
        object.insert("additionalProperties".to_owned(), json!(false));
    }
    schema
}

fn parse_operations(
    plugin_id: &str,
    manifest: &JsonMap<String, Value>,
) -> Result<Vec<PluginOperationDecl>, ContractError> {
    let Some(raw) = manifest.get("operations") else {
        return Ok(Vec::new());
    };
    let Some(items) = raw.as_array() else {
        return Err(invalid_arg("manifest `operations` must be an array"));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(object) = item.as_object() else {
            return Err(invalid_arg("manifest `operations` entries must be objects"));
        };
        let operation_id = manifest_str_field(object, "operationId")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                invalid_arg("manifest operation entries require a non-empty `operationId`")
            })?
            .to_owned();
        let input_schema = object
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "additionalProperties": false}));
        if !input_schema.is_object() {
            return Err(invalid_arg(format!(
                "manifest operation `{operation_id}` inputSchema must be an object"
            )));
        }
        let result_schema = object
            .get("resultSchema")
            .filter(|&value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| {
                serde_json::from_str(DEFAULT_OPERATION_RESULT_SCHEMA)
                    .expect("default result schema parses")
            });
        if !result_schema.is_object() {
            return Err(invalid_arg(format!(
                "manifest operation `{operation_id}` resultSchema must be an object"
            )));
        }
        out.push(PluginOperationDecl {
            permission: manifest_str_field(object, "permission")
                .unwrap_or_default()
                .to_owned(),
            input_schema_id: format!("agentcpm-pd.plugin.{plugin_id}.op.{operation_id}.input/1"),
            input_schema,
            result_schema_id: format!("agentcpm-pd.plugin.{plugin_id}.op.{operation_id}.result/1"),
            result_schema,
            operation_id,
        });
    }
    Ok(out)
}

/// Reads and validates `manifest.json` from a local source directory. The
/// rules are the baseline `validateManifest` set (`pluginId` must equal
/// `manifest.name`; `version` consistency with the request when the manifest
/// declares one).
fn prepare_manifest_from_dir(
    dir: &Path,
    plugin_id: &str,
    version: &str,
) -> Result<PreparedManifest, ContractError> {
    let manifest_path = dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
        invalid_arg(format!(
            "plugin source has no readable manifest.json ({}): {e}",
            manifest_path.display()
        ))
    })?;
    let manifest: Value = serde_json::from_str(&raw)
        .map_err(|e| invalid_arg(format!("manifest.json is not valid JSON: {e}")))?;
    let Some(object) = manifest.as_object() else {
        return Err(invalid_arg("manifest.json must contain a JSON object"));
    };
    // 基线 REQUIRED_MANIFEST_FIELDS：name/displayName/entry。
    for field in ["name", "displayName", "entry"] {
        if !manifest_str_field(object, field).is_some_and(|value| !value.is_empty()) {
            return Err(invalid_arg(format!(
                "manifest missing or invalid required field: {field}"
            )));
        }
    }
    let name = manifest_str_field(object, "name").unwrap_or_default();
    if !plugin_id_valid(name) {
        return Err(invalid_arg(
            "manifest name must only contain letters, numbers, hyphens, and underscores",
        ));
    }
    if name != plugin_id {
        return Err(invalid_arg(format!(
            "manifest name `{name}` does not match the requested pluginId `{plugin_id}`"
        )));
    }
    if let Some(manifest_version) = manifest_str_field(object, "version").filter(|v| !v.is_empty())
        && manifest_version != version
    {
        return Err(invalid_arg(format!(
            "manifest declares version `{manifest_version}` but the request targets `{version}`"
        )));
    }
    if let Some(kind) = object.get("type").and_then(Value::as_str)
        && !matches!(kind, "react" | "module")
    {
        return Err(invalid_arg(format!(
            "invalid plugin type: {kind} (must be react or module)"
        )));
    }
    if let Some(slot) = object.get("slot").and_then(Value::as_str)
        && slot != "tab"
    {
        return Err(invalid_arg(format!(
            "invalid plugin slot: {slot} (must be tab)"
        )));
    }
    let entry = manifest_str_field(object, "entry").unwrap_or_default();
    if entry.contains("..") || Path::new(entry).is_absolute() {
        return Err(invalid_arg("entry must be a relative path without \"..\""));
    }
    if let Some(server) = object.get("server") {
        let invalid = !server.is_string()
            || server.as_str().is_some_and(|value| value.contains(".."))
            || server
                .as_str()
                .is_some_and(|value| Path::new(value).is_absolute());
        if !server.is_null() && invalid {
            return Err(invalid_arg(
                "server entry must be a relative path string without \"..\"",
            ));
        }
    }
    if let Some(permissions) = object.get("permissions")
        && !permissions
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item.is_string()))
    {
        return Err(invalid_arg("permissions must be an array of strings"));
    }
    let config_schema = match object.get("configurationSchema") {
        None => DEFAULT_CONFIGURATION_SCHEMA.to_owned(),
        Some(schema) if schema.is_object() => serde_json::to_string(&harden_unknown_fields(schema))
            .unwrap_or_else(|_| DEFAULT_CONFIGURATION_SCHEMA.to_owned()),
        Some(_) => {
            return Err(invalid_arg(
                "manifest configurationSchema must be a JSON Schema object",
            ));
        }
    };
    let operations = parse_operations(plugin_id, object)?;
    // Canonical digest: serde_json objects are key-sorted maps, so the
    // serialized form is stable for the same manifest content.
    let manifest_json = serde_json::to_string(&manifest)
        .map_err(|e| invalid_arg(format!("manifest re-serialization failed: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(manifest_json.as_bytes());
    Ok(PreparedManifest {
        manifest_hash: format!("{:x}", hasher.finalize()),
        operations,
        config_schema,
        manifest_json,
    })
}

// ---------------------------------------------------------------------------
// Source resolution
// ---------------------------------------------------------------------------

enum SourcePlan {
    /// A git URL (`https://` / `git@` — the baseline's accepted forms). No
    /// fetch bridge is assembled in this build, so installs from it fail
    /// with `UPSTREAM_ERROR` after acceptance.
    GitUrl,
    /// A local directory inside the scope's anchor, manifest already
    /// validated.
    LocalDir {
        source_dir: PathBuf,
        manifest: PreparedManifest,
    },
}

/// The path anchor a local `sourceRef` must resolve inside: the project
/// workspace for project/session scopes, the instance home for global.
fn scope_anchor(rt: &PublicV1Runtime, scope: &ToolScope) -> Result<PathBuf, ContractError> {
    match scope {
        ToolScope::Global => Ok(rt.store().home().to_path_buf()),
        ToolScope::Project { project_id } | ToolScope::Session { project_id, .. } => {
            match resolve_project(rt.store(), project_id) {
                Ok(workspace) => Ok(workspace.workspace),
                Err(ProjectResolveError::NotFound(message)) => Err(invalid_arg(message)),
                Err(ProjectResolveError::Virtual(message)) => Err(invalid_arg(message)),
                Err(ProjectResolveError::Contract(error)) => Err(error),
            }
        }
    }
}

fn resolve_source(
    rt: &PublicV1Runtime,
    scope: &ToolScope,
    plugin_id: &str,
    version: &str,
    source_ref: &str,
) -> Result<SourcePlan, ContractError> {
    if source_ref.starts_with("https://") || source_ref.starts_with("git@") {
        if source_ref.trim() != source_ref || source_ref.len() < 8 {
            return Err(invalid_arg("sourceRef is not a usable git URL"));
        }
        return Ok(SourcePlan::GitUrl);
    }
    if source_ref.is_empty() {
        return Err(invalid_arg("sourceRef is required"));
    }
    let anchor = scope_anchor(rt, scope)?;
    let anchor = std::fs::canonicalize(&anchor).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!(
                "scope anchor {} cannot be canonicalized: {e}",
                anchor.display()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let candidate = Path::new(source_ref);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        anchor.join(candidate)
    };
    let canonical = std::fs::canonicalize(&joined).map_err(|_| {
        path_outside_scope(format!(
            "plugin source `{source_ref}` does not resolve inside the scope anchor {}",
            anchor.display()
        ))
    })?;
    if !canonical.starts_with(&anchor) {
        return Err(path_outside_scope(format!(
            "plugin source `{source_ref}` resolves outside the scope anchor {}",
            anchor.display()
        )));
    }
    if !canonical.is_dir() {
        return Err(invalid_arg(format!(
            "plugin source `{source_ref}` is not a directory"
        )));
    }
    let manifest = prepare_manifest_from_dir(&canonical, plugin_id, version)?;
    Ok(SourcePlan::LocalDir {
        source_dir: canonical,
        manifest,
    })
}

// ---------------------------------------------------------------------------
// Installation resources (the domain-owned tree under <home>/v1/plugins)
// ---------------------------------------------------------------------------

fn install_dir_for(home: &Path, layer_id: &str, plugin_id: &str) -> PathBuf {
    home.join("v1")
        .join("plugins")
        .join(layer_slug(layer_id))
        .join(plugin_id)
}

/// Copies a source tree (directories and plain files; symlinks and special
/// entries are skipped — canonicalization already rejected symlink escapes,
/// and the installed copy must not smuggle one back in).
fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Projection (contract item shapes)
// ---------------------------------------------------------------------------

/// The runtime view every list/get item carries. Honest unavailability: this
/// gateway build assembles no plugin runtime, so `available` is false for
/// every plugin (the reason is explicit while enabled; a disabled plugin is
/// deliberate, not a failure).
fn runtime_json(row: &PluginRow) -> Value {
    let (state, reason_code) = if row.enabled {
        ("not-loaded", json!("PLUGIN_RUNTIME_NOT_ASSEMBLED"))
    } else {
        ("disabled", Value::Null)
    };
    json!({
        "state": state,
        "available": false,
        "reasonCode": reason_code,
        "generation": row.generation.clone().map(Value::String).unwrap_or(Value::Null),
    })
}

fn plugin_item_json(row: &PluginRow) -> Value {
    json!({
        "pluginId": row.plugin_id,
        "version": row.version,
        "manifestHash": row.manifest_hash,
        "configurationRevision": row.config_revision,
        "installed": true,
        "enabled": row.enabled,
        "runtime": runtime_json(row),
    })
}

fn configuration_schema_id(plugin_id: &str) -> String {
    format!("agentcpm-pd.plugin.{plugin_id}.configuration/1")
}

fn parse_operations_json(row: &PluginRow) -> Vec<PluginOperationDecl> {
    serde_json::from_str::<Value>(&row.operations_json)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .map(|items| {
            items
                .into_iter()
                .filter_map(|item| {
                    Some(PluginOperationDecl {
                        operation_id: item.get("operationId")?.as_str()?.to_owned(),
                        input_schema_id: item.get("inputSchemaId")?.as_str()?.to_owned(),
                        input_schema: item.get("inputSchema")?.clone(),
                        result_schema_id: item.get("resultSchemaId")?.as_str()?.to_owned(),
                        result_schema: item.get("resultSchema")?.clone(),
                        permission: item
                            .get("permission")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn operations_json(operations: &[PluginOperationDecl]) -> String {
    let items: Vec<Value> = operations
        .iter()
        .map(|decl| {
            json!({
                "operationId": decl.operation_id,
                "inputSchemaId": decl.input_schema_id,
                "inputSchema": decl.input_schema,
                "resultSchemaId": decl.result_schema_id,
                "resultSchema": decl.result_schema,
                "permission": decl.permission,
            })
        })
        .collect();
    serde_json::to_string(&Value::Array(items)).unwrap_or_else(|_| "[]".into())
}

/// The masked configuration projection (the configuration domain's
/// sensitive-key rule and mask value).
fn masked_configuration(configuration: &Value) -> Value {
    use crate::public_v1::domains::configuration::{REDACTED, sensitive_key};
    let Some(object) = configuration.as_object() else {
        return json!({});
    };
    let mut out = JsonMap::new();
    for (key, value) in object {
        if sensitive_key(key) {
            out.insert(key.clone(), json!(REDACTED));
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Value::Object(out)
}

/// The policy gate for plugin-scoped selectors (the t005 surface, read the
/// same way `tools.list` reads it): a `deny` rule matching the plugin id (or
/// `*`) blocks the invocation for this scope.
fn policy_denies(rt: &PublicV1Runtime, scope: &ToolScope, plugin_id: &str) -> bool {
    let policy_scope = match scope {
        ToolScope::Global => crate::public_v1::domains::policy::PolicyScope::Instance,
        ToolScope::Project { project_id } => {
            crate::public_v1::domains::policy::PolicyScope::Project {
                project_id: project_id.clone(),
            }
        }
        ToolScope::Session {
            project_id,
            session_id,
        } => crate::public_v1::domains::policy::PolicyScope::Session {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        },
    };
    let Ok(view) = rt.store().get_policy(&policy_scope) else {
        return false;
    };
    view.policy
        .get("rules")
        .and_then(Value::as_array)
        .is_some_and(|rules| {
            rules.iter().any(|rule| {
                rule.get("effect").and_then(Value::as_str) == Some("deny")
                    && rule
                        .get("tool")
                        .and_then(Value::as_str)
                        .is_some_and(|selector| selector == "*" || selector == plugin_id)
            })
        })
}

// ---------------------------------------------------------------------------
// Cross-domain read API (hooks bindings resolve their target operations here)
// ---------------------------------------------------------------------------

pub(crate) struct PluginTargetOperation {
    pub(crate) result_schema_id: String,
    pub(crate) result_schema: Value,
}

/// Resolves `ownerId.operationId` against the plugins installed across the
/// overlay `layers` (inner layers shadow outer ones). `None` means the
/// owner/operation pair is not declared.
pub(crate) fn resolve_plugin_operation(
    rt: &PublicV1Runtime,
    layers: &[String],
    owner_id: &str,
    operation_id: &str,
) -> Result<Option<PluginTargetOperation>, ContractError> {
    let db = plugins_db(rt)?;
    let Some(row) = db.row_in_layers(layers, owner_id)? else {
        return Ok(None);
    };
    let Some(decl) = parse_operations_json(&row)
        .into_iter()
        .find(|decl| decl.operation_id == operation_id)
    else {
        return Ok(None);
    };
    Ok(Some(PluginTargetOperation {
        result_schema_id: decl.result_schema_id,
        result_schema: decl.result_schema,
    }))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn plugins_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let layer_id = scope_layer_id(&scope);
    let db = plugins_db(&rt)?;
    let rows = db.rows_for_layer(&layer_id)?;

    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let after = match input.get("cursor").and_then(Value::as_str) {
        None => None,
        Some(cursor) => Some(
            cursor
                .strip_prefix("after:")
                .filter(|rest| !rest.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    invalid_arg(format!("malformed cursor `{cursor}` (expected after:<id>)"))
                })?,
        ),
    };

    let mut items = Vec::new();
    let mut next_cursor = None;
    for row in &rows {
        if let Some(after) = &after
            && &row.plugin_id <= after
        {
            continue;
        }
        if items.len() >= limit {
            next_cursor = items
                .last()
                .and_then(|item: &Value| item["pluginId"].as_str())
                .map(|last| format!("after:{last}"));
            break;
        }
        items.push(plugin_item_json(row));
    }
    // A deterministic snapshot token: any state change moves it.
    let mut hasher = Sha256::new();
    for row in &rows {
        hasher.update(row.plugin_id.as_bytes());
        hasher.update(row.version.as_bytes());
        hasher.update(row.config_revision.as_bytes());
        hasher.update([row.enabled as u8]);
        hasher.update(row.generation.as_deref().unwrap_or("").as_bytes());
    }
    let hex = format!("{:x}", hasher.finalize());
    let snapshot = format!("psnap-{}", &hex[..16]);
    Ok((
        ResponseOutcome::Succeeded,
        json!({"items": items, "nextCursor": next_cursor, "snapshotRevision": snapshot}),
    ))
}

async fn plugins_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let plugin_id = required_str(&input, "pluginId")?;
    let layer_id = scope_layer_id(&scope);
    let db = plugins_db(&rt)?;
    let Some(row) = db.row(&layer_id, &plugin_id)? else {
        return Err(not_found("plugin", &plugin_id).into());
    };
    let configuration: Value =
        serde_json::from_str(&row.configuration_json).unwrap_or_else(|_| json!({}));
    let operations: Vec<Value> = parse_operations_json(&row)
        .iter()
        .map(|decl| {
            json!({
                "operationId": decl.operation_id,
                "inputSchemaId": decl.input_schema_id,
                "resultSchemaId": decl.result_schema_id,
                "permission": decl.permission,
                "available": false,
            })
        })
        .collect();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "plugin": plugin_item_json(&row),
            "configurationSchemaId": configuration_schema_id(&plugin_id),
            "configuration": masked_configuration(&configuration),
            "operations": operations,
        }),
    ))
}

/// `plugins.install` — async, `retry=idempotentSubmit`.
async fn plugins_install(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let plugin_id = required_str(&input, "pluginId")?;
    let source_ref = required_str(&input, "sourceRef")?;
    let version = required_str(&input, "version")?;
    let expected_installed = input
        .get("expectedInstalledVersion")
        .and_then(|value| value.as_str().map(str::to_owned));
    let layer_id = scope_layer_id(&scope);
    let db = plugins_db(&rt)?;
    let ops = operations_db(&rt)?;

    if !plugin_id_valid(&plugin_id) {
        return Err(invalid_arg(
            "pluginId must only contain letters, numbers, hyphens, and underscores",
        )
        .into());
    }
    // 来源/清单解析在受理前完成（校验名称、来源、路径、版本与清单）。
    let plan = resolve_source(&rt, &scope, &plugin_id, &version, &source_ref)?;

    let validation = || -> Result<(), ContractError> {
        let existing = db.row(&layer_id, &plugin_id)?;
        match (&expected_installed, existing) {
            (None, Some(row)) => {
                return Err(version_conflict(format!(
                    "plugin `{plugin_id}` is already installed at version `{}`; pass \
                     expectedInstalledVersion to update it",
                    row.version
                )));
            }
            (Some(expected), None) => {
                return Err(version_conflict(format!(
                    "plugin `{plugin_id}` is not installed (expected `{expected}`)"
                )));
            }
            (Some(expected), Some(row)) => {
                if row.version != *expected {
                    return Err(version_conflict(format!(
                        "plugin `{plugin_id}` is installed at version `{}`, expected `{expected}`",
                        row.version
                    )));
                }
            }
            (None, None) => {}
        }
        Ok(())
    };

    let operation = accept_domain_operation(
        &ops,
        "plugins.install",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        validation,
    )?;
    let operation_id = operation.operation_id.clone();
    let session_column = match &scope {
        ToolScope::Session { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };

    let runner_db = db.clone();
    let runner_rt = rt.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&ops, &operation_id, &session_column);
        let completion = tokio::task::spawn_blocking(move || {
            execute_install(
                &runner_db,
                &runner_rt,
                &layer_id,
                &plugin_id,
                &version,
                &source_ref,
                expected_installed.as_deref(),
                plan,
            )
        })
        .await
        .unwrap_or_else(|join| {
            Err((
                CODE_UPSTREAM_ERROR.to_owned(),
                format!("install executor panicked: {join}"),
            ))
        });
        match completion {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &ops,
                    &operation_id,
                    &session_column,
                    "succeeded",
                    &completion,
                );
            }
            Err((code, message)) => {
                let _ = finish_domain_operation(
                    &ops,
                    &operation_id,
                    &session_column,
                    "failed",
                    &json!({"code": code, "message": message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// The install executor: `.tmp-` staged copy, atomic rename, then the row
/// write (the baseline `installPluginFromGit` sequence). Any failure leaves
/// no row — a half install is never announced.
#[allow(clippy::too_many_arguments)]
fn execute_install(
    db: &Arc<PluginsDb>,
    rt: &Arc<PublicV1Runtime>,
    layer_id: &str,
    plugin_id: &str,
    version: &str,
    source_ref: &str,
    expected_installed: Option<&str>,
    plan: SourcePlan,
) -> Result<Value, (String, String)> {
    let installed_root = rt
        .store()
        .home()
        .join("v1")
        .join("plugins")
        .join(layer_slug(layer_id));
    let target = installed_root.join(plugin_id);
    let (source_dir, manifest) = match plan {
        SourcePlan::GitUrl => {
            // 本构建无插件源抓取桥（网络面属 runtime/extensions 装配）：
            // 受理后诚实失败，无半安装残留。
            return Err((
                CODE_UPSTREAM_ERROR.to_owned(),
                "plugin source fetch is not assembled in this gateway build \
                 (no network bridge for git sources); use a local directory sourceRef"
                    .to_owned(),
            ));
        }
        SourcePlan::LocalDir {
            source_dir,
            manifest,
        } => (source_dir, manifest),
    };

    // Executor-side CAS re-check: a racing install may have moved the row
    // between validation and here.
    let existing = db
        .row(layer_id, plugin_id)
        .map_err(|e| (e.code, e.message))?;
    match (expected_installed, existing) {
        (None, None) => {}
        (None, Some(row)) => {
            return Err((
                CODE_VERSION_CONFLICT.to_owned(),
                format!(
                    "plugin `{plugin_id}` was installed concurrently at version `{}`",
                    row.version
                ),
            ));
        }
        (Some(expected), row) => {
            let current = row.as_ref().map(|row| row.version.as_str());
            if current != Some(expected) {
                return Err((
                    CODE_VERSION_CONFLICT.to_owned(),
                    format!(
                        "plugin `{plugin_id}` is installed at version `{}` (concurrently \
                         changed), expected `{expected}`",
                        current.unwrap_or("<none>")
                    ),
                ));
            }
        }
    }

    let fs_result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&installed_root)?;
        let tmp = installed_root.join(format!(
            ".tmp-{plugin_id}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        if let Err(error) = (|| -> std::io::Result<()> {
            copy_tree(&source_dir, &tmp)?;
            if target.exists() {
                std::fs::remove_dir_all(&target)?;
            }
            std::fs::rename(&tmp, &target)
        })() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(error);
        }
        Ok(())
    })();
    fs_result.map_err(|error| {
        (
            CODE_UPSTREAM_ERROR.to_owned(),
            format!("installing plugin `{plugin_id}` resources failed: {error}"),
        )
    })?;

    // 恢复 remove 保留的配置（默认保留配置的重装语义）。
    let (configuration_json, config_revision, enabled) = match db
        .row(layer_id, plugin_id)
        .map_err(|e| (e.code, e.message))?
    {
        Some(existing) => (
            existing.configuration_json,
            existing.config_revision,
            existing.enabled,
        ),
        None => {
            let (configuration, revision) = db
                .take_retired(layer_id, plugin_id)
                .map_err(|e| (e.code, e.message))?
                .unwrap_or_else(|| ("{}".to_owned(), String::new()));
            (configuration, revision, false)
        }
    };

    let row = PluginRow {
        layer_id: layer_id.to_owned(),
        plugin_id: plugin_id.to_owned(),
        version: version.to_owned(),
        manifest_hash: manifest.manifest_hash.clone(),
        source_ref: source_ref.to_owned(),
        enabled,
        // A new version invalidates the previous runtime binding generation.
        generation: None,
        manifest_json: manifest.manifest_json.clone(),
        operations_json: operations_json(&manifest.operations),
        config_schema_json: manifest.config_schema.clone(),
        configuration_json,
        config_revision,
    };
    db.upsert_row(&row).map_err(|e| (e.code, e.message))?;
    Ok(json!({
        "pluginId": plugin_id,
        "version": version,
        "manifestHash": manifest.manifest_hash,
        "installed": true,
        "enabled": enabled,
        "available": false,
    }))
}

/// `plugins.configure` — sync, `retry=conditionalWrite`.
async fn plugins_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let plugin_id = required_str(&input, "pluginId")?;
    let schema_id = required_str(&input, "schemaId")?;
    let configuration = input
        .get("configuration")
        .cloned()
        .ok_or_else(|| invalid_arg("`configuration` is required"))?;
    let expected = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let layer_id = scope_layer_id(&scope);
    let db = plugins_db(&rt)?;
    let session_column = match &scope {
        ToolScope::Session { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };

    let Some(row) = db.row(&layer_id, &plugin_id)? else {
        return Err(not_found("plugin", &plugin_id).into());
    };
    let current_revision = row.config_revision.clone();
    let expected_current = if current_revision.is_empty() {
        GENERATION_ABSENT.to_owned()
    } else {
        current_revision.clone()
    };
    if expected != expected_current {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "plugins.configure expectedRevision `{expected}` does not match `{expected_current}`"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    let expected_schema_id = configuration_schema_id(&plugin_id);
    if schema_id != expected_schema_id {
        return Err(schema_mismatch(format!(
            "schemaId `{schema_id}` does not match the registered configuration schema \
             `{expected_schema_id}` of plugin `{plugin_id}`"
        ))
        .into());
    }
    let config_schema: Value = serde_json::from_str(&row.config_schema_json)
        .unwrap_or_else(|_| serde_json::from_str(DEFAULT_CONFIGURATION_SCHEMA).expect("default"));
    let stored_configuration: Value =
        serde_json::from_str(&row.configuration_json).unwrap_or_else(|_| json!({}));

    // 校验新配置（未知字段拒绝；掩码秘密回传保真）。
    let merged = merge_masked_secrets(&configuration, &stored_configuration)?;
    if !merged.is_object() {
        return Err(schema_mismatch("`configuration` must be an object").into());
    }
    if let Err(errors) = validate_against_schema(&merged, &config_schema) {
        let details: Vec<Value> = errors
            .iter()
            .map(|error| json!({"path": error.path, "message": error.message}))
            .collect();
        let mut error = schema_mismatch(format!(
            "configuration does not satisfy the schema of `{plugin_id}`"
        ));
        let mut map = JsonMap::new();
        map.insert("validationErrors".to_owned(), Value::Array(details));
        error.details = Some(map);
        return Err(error.into());
    }

    let new_revision = format!(
        "pcfg-{:08}",
        db.next_meta_counter("pcfg_counter")
            .map_err(DispatchError::from)?
    );
    db.with_conn("write plugin configuration", |conn| {
        conn.execute(
            "UPDATE v1_plugins
             SET configuration_json = ?3, config_revision = ?4, updated_at = ?5
             WHERE layer_id = ?1 AND plugin_id = ?2",
            params![
                layer_id,
                plugin_id,
                serde_json::to_string(&merged).unwrap_or_else(|_| "{}".into()),
                new_revision,
                now_rfc3339(),
            ],
        )?;
        Ok(())
    })
    .map_err(DispatchError::from)?;
    db.append_event(
        &format!("plugin:{layer_id}:{plugin_id}"),
        &session_column,
        "plugin.configurationChanged",
        &json!({
            "scope": {"kind": scope_kind(&scope), "id": scope_id(&scope)},
            "pluginId": plugin_id,
            "configurationRevision": new_revision,
        }),
    )
    .map_err(DispatchError::from)?;
    rt.store().event_notifier().notify_waiters();
    // 「错误配置不替换当前可用代际」的另一半：本构建没有可应用配置的运行
    // 时代际，所以 applied=false / generation=null 是如实报告。
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "revision": new_revision,
            "applied": false,
            "generation": Value::Null,
        }),
    ))
}

fn scope_kind(scope: &ToolScope) -> &str {
    match scope {
        ToolScope::Global => "global",
        ToolScope::Project { .. } => "project",
        ToolScope::Session { .. } => "session",
    }
}

fn scope_id(scope: &ToolScope) -> Value {
    match scope {
        ToolScope::Global => Value::Null,
        ToolScope::Project { project_id } => json!(project_id),
        ToolScope::Session { session_id, .. } => json!(session_id),
    }
}

/// The masked-secret round trip: a sensitive key whose submitted value is
/// the read-path mask keeps the stored secret (the configuration domain's
/// `preserveMaskedSecrets` rule).
fn merge_masked_secrets(submitted: &Value, stored: &Value) -> Result<Value, ContractError> {
    use crate::public_v1::domains::configuration::{REDACTED, sensitive_key};
    let Some(submitted) = submitted.as_object() else {
        return Ok(submitted.clone());
    };
    let stored_object = stored.as_object().cloned().unwrap_or_default();
    let mut out = JsonMap::new();
    for (key, value) in submitted {
        if sensitive_key(key)
            && value.as_str() == Some(REDACTED)
            && let Some(current) = stored_object.get(key)
        {
            out.insert(key.clone(), current.clone());
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Ok(Value::Object(out))
}

/// `plugins.transition` — async, `retry=idempotentSubmit`.
async fn plugins_transition(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let plugin_id = required_str(&input, "pluginId")?;
    let action = required_str(&input, "action")?;
    let expected_generation = required_str(&input, "expectedGeneration")?;
    let layer_id = scope_layer_id(&scope);
    let db = plugins_db(&rt)?;
    let ops = operations_db(&rt)?;

    if !matches!(action.as_str(), "enable" | "disable" | "reload") {
        return Err(invalid_arg(format!(
            "action must be enable, disable or reload; got `{action}`"
        ))
        .into());
    }

    let validation = || -> Result<(), ContractError> {
        let Some(row) = db.row(&layer_id, &plugin_id)? else {
            return Err(not_found("plugin", &plugin_id));
        };
        let current_generation = row
            .generation
            .clone()
            .unwrap_or_else(|| GENERATION_ABSENT.to_owned());
        if expected_generation != current_generation {
            return Err(generation_conflict(format!(
                "plugin `{plugin_id}` runtime generation is `{current_generation}`, \
                 expected `{expected_generation}`"
            )));
        }
        Ok(())
    };

    let operation = accept_domain_operation(
        &ops,
        "plugins.transition",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        validation,
    )?;
    let operation_id = operation.operation_id.clone();
    let session_column = match &scope {
        ToolScope::Session { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };

    let runner_db = db.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&ops, &operation_id, &session_column);
        let outcome = runner_db
            .with_tx("transition plugin generation", |tx| {
                // CAS again inside the write window.
                let (enabled, generation): (i64, Option<String>) = tx.query_row(
                    "SELECT enabled, generation FROM v1_plugins
                     WHERE layer_id = ?1 AND plugin_id = ?2",
                    params![layer_id, plugin_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let current = generation
                    .clone()
                    .unwrap_or_else(|| GENERATION_ABSENT.to_owned());
                if current != expected_generation {
                    return Ok(Err(current));
                }
                let new_generation = if action == "disable" {
                    // 禁用保留旧代际（在途绑定清单挂旧代际），只停新调用。
                    generation.clone()
                } else {
                    let seq: i64 = tx.query_row(
                        "SELECT CAST(value AS INTEGER) FROM v1_plugins_meta
                         WHERE key = 'pgen_counter'",
                        [],
                        |r| r.get(0),
                    )?;
                    let seq = seq + 1;
                    tx.execute(
                        "INSERT INTO v1_plugins_meta (key, value) VALUES ('pgen_counter', ?1)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        params![seq.to_string()],
                    )?;
                    Some(format!("pgen-{seq:08}"))
                };
                let new_enabled = match action.as_str() {
                    "enable" => 1,
                    "disable" => 0,
                    _ => enabled,
                };
                if action == "enable" || action == "disable" {
                    tx.execute(
                        "UPDATE v1_plugins SET enabled = ?3, generation = ?4, updated_at = ?5
                         WHERE layer_id = ?1 AND plugin_id = ?2",
                        params![
                            layer_id,
                            plugin_id,
                            new_enabled,
                            new_generation,
                            now_rfc3339()
                        ],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE v1_plugins SET generation = ?3, updated_at = ?4
                         WHERE layer_id = ?1 AND plugin_id = ?2",
                        params![layer_id, plugin_id, new_generation, now_rfc3339()],
                    )?;
                }
                Ok(Ok((new_enabled, new_generation)))
            })
            .and_then(|inner| {
                inner.map_err(|current| {
                    generation_conflict(format!(
                        "plugin `{plugin_id}` runtime generation changed concurrently to \
                         `{current}`"
                    ))
                })
            });
        match outcome {
            Ok((enabled, generation)) => {
                let state = if enabled != 0 {
                    "not-loaded"
                } else {
                    "disabled"
                };
                let _ = finish_domain_operation(
                    &ops,
                    &operation_id,
                    &session_column,
                    "succeeded",
                    &json!({
                        "pluginId": plugin_id,
                        "enabled": enabled != 0,
                        "generation": generation.map(Value::String).unwrap_or(Value::Null),
                        "state": state,
                        "retainedBindings": [],
                    }),
                );
            }
            Err(error) => {
                let _ = finish_domain_operation(
                    &ops,
                    &operation_id,
                    &session_column,
                    "failed",
                    &json!({"code": error.code, "message": error.message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// `plugins.remove` — async, `retry=idempotentSubmit`.
async fn plugins_remove(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let plugin_id = required_str(&input, "pluginId")?;
    let expected_version = required_str(&input, "expectedVersion")?;
    let remove_configuration = input
        .get("removeConfiguration")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let layer_id = scope_layer_id(&scope);
    let db = plugins_db(&rt)?;
    let ops = operations_db(&rt)?;

    let validation = || -> Result<(), ContractError> {
        let Some(row) = db.row(&layer_id, &plugin_id)? else {
            return Err(not_found("plugin", &plugin_id));
        };
        if row.version != expected_version {
            return Err(version_conflict(format!(
                "plugin `{plugin_id}` is installed at version `{}`, expected `{expected_version}`",
                row.version
            )));
        }
        // 活动调用未排空则明确 RESOURCE_BUSY。
        if db.active_invocations(&layer_id, &plugin_id)? > 0 {
            return Err(ContractError::new(
                CODE_RESOURCE_BUSY,
                format!("plugin `{plugin_id}` still has invocations in flight"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        Ok(())
    };

    let operation = accept_domain_operation(
        &ops,
        "plugins.remove",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        validation,
    )?;
    let operation_id = operation.operation_id.clone();
    let session_column = match &scope {
        ToolScope::Session { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };

    let runner_db = db.clone();
    let runner_rt = rt.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&ops, &operation_id, &session_column);
        let outcome = tokio::task::spawn_blocking(move || {
            execute_remove(
                &runner_db,
                &runner_rt,
                &layer_id,
                &plugin_id,
                &expected_version,
                remove_configuration,
            )
        })
        .await
        .unwrap_or_else(|join| {
            Err((
                CODE_UPSTREAM_ERROR.to_owned(),
                format!("remove executor panicked: {join}"),
            ))
        });
        match outcome {
            Ok(completion) => {
                let _ = finish_domain_operation(
                    &ops,
                    &operation_id,
                    &session_column,
                    "succeeded",
                    &completion,
                );
            }
            Err((code, message)) => {
                let _ = finish_domain_operation(
                    &ops,
                    &operation_id,
                    &session_column,
                    "failed",
                    &json!({"code": code, "message": message}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

fn execute_remove(
    db: &Arc<PluginsDb>,
    rt: &Arc<PublicV1Runtime>,
    layer_id: &str,
    plugin_id: &str,
    expected_version: &str,
    remove_configuration: bool,
) -> Result<Value, (String, String)> {
    // Executor-side re-check (racing installs/transitions).
    let Some(row) = db
        .row(layer_id, plugin_id)
        .map_err(|e| (e.code, e.message))?
    else {
        return Err((
            "NOT_FOUND".to_owned(),
            format!("plugin `{plugin_id}` not found"),
        ));
    };
    if row.version != expected_version {
        return Err((
            CODE_VERSION_CONFLICT.to_owned(),
            format!(
                "plugin `{plugin_id}` is installed at version `{}`, expected `{expected_version}`",
                row.version
            ),
        ));
    }
    if db
        .active_invocations(layer_id, plugin_id)
        .map_err(|e| (e.code, e.message))?
        > 0
    {
        return Err((
            CODE_RESOURCE_BUSY.to_owned(),
            format!("plugin `{plugin_id}` still has invocations in flight"),
        ));
    }

    // 只回收领域自己的安装目录（不删除插件外项目文件：本地来源目录属于
    // 调用方，永不触碰）。
    let install_dir = install_dir_for(rt.store().home(), layer_id, plugin_id);
    if install_dir.exists()
        && let Err(error) = std::fs::remove_dir_all(&install_dir)
    {
        return Err((
            CODE_UPSTREAM_ERROR.to_owned(),
            format!(
                "removing plugin resources under {} failed: {error}",
                install_dir.display()
            ),
        ));
    }
    let configuration_retained = if remove_configuration {
        db.drop_retired(layer_id, plugin_id)
            .map_err(|e| (e.code, e.message))?;
        false
    } else {
        db.stash_retired(
            layer_id,
            plugin_id,
            &row.configuration_json,
            &row.config_revision,
        )
        .map_err(|e| (e.code, e.message))?;
        true
    };
    db.delete_row(layer_id, plugin_id)
        .map_err(|e| (e.code, e.message))?;
    Ok(json!({
        "pluginId": plugin_id,
        "removed": true,
        "configurationRetained": configuration_retained,
        "retainedResources": if configuration_retained {
            json!(["configuration"])
        } else {
            json!([])
        },
    }))
}

/// `plugins.invoke` — async, `retry=idempotentSubmit`.
async fn plugins_invoke(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let plugin_id = required_str(&input, "pluginId")?;
    let operation_id = required_str(&input, "operationId")?;
    let schema_id = required_str(&input, "schemaId")?;
    let arguments = input
        .get("arguments")
        .cloned()
        .ok_or_else(|| invalid_arg("`arguments` is required"))?;
    let layer_id = scope_layer_id(&scope);
    let db = plugins_db(&rt)?;
    let ops = operations_db(&rt)?;

    // Pre-accept rejections (unknown plugin/operation, schema mismatch,
    // unauthorized scope, disabled plugin) happen before any plugin
    // execution — no URL/path forwarding exists to misuse.
    let Some(row) = db.row(&layer_id, &plugin_id)? else {
        return Err(not_found("plugin", &plugin_id).into());
    };
    let Some(decl) = parse_operations_json(&row)
        .into_iter()
        .find(|decl| decl.operation_id == operation_id)
    else {
        return Err(not_found("plugin operation", &operation_id).into());
    };
    if schema_id != decl.input_schema_id {
        return Err(schema_mismatch(format!(
            "schemaId `{schema_id}` does not match the declared input schema \
             `{}` of `{plugin_id}.{operation_id}`",
            decl.input_schema_id
        ))
        .into());
    }
    if !arguments.is_object() {
        return Err(schema_mismatch("`arguments` must be an object").into());
    }
    if let Err(errors) = validate_against_schema(&arguments, &decl.input_schema) {
        let details: Vec<Value> = errors
            .iter()
            .map(|error| json!({"path": error.path, "message": error.message}))
            .collect();
        let mut error = schema_mismatch(format!(
            "arguments do not satisfy the input schema of `{plugin_id}.{operation_id}`"
        ));
        let mut map = JsonMap::new();
        map.insert("validationErrors".to_owned(), Value::Array(details));
        error.details = Some(map);
        return Err(error.into());
    }
    if policy_denies(&rt, &scope, &plugin_id) {
        return Err(ContractError::new(
            CODE_PERMISSION_DENIED,
            format!("plugin `{plugin_id}` is denied by policy for this scope"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    if !row.enabled {
        return Err(ContractError::new(
            CODE_UNAVAILABLE,
            format!("plugin `{plugin_id}` is disabled"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    let operation = accept_domain_operation(
        &ops,
        "plugins.invoke",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || Ok(()),
    )?;
    let domain_operation_id = operation.operation_id.clone();
    let session_column = match &scope {
        ToolScope::Session { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };

    // 登记活动调用（remove 的 RESOURCE_BUSY 面）。
    db.insert_invocation(&domain_operation_id, &layer_id, &plugin_id, &operation_id)
        .map_err(DispatchError::from)?;
    let runner_db = db.clone();
    let plugin_operation_id = operation_id.clone();
    let plugin_id_for_message = plugin_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&ops, &domain_operation_id, &session_column);
        // 本构建没有插件执行桥：受理后如实失败（tools.test 的
        // TOOL_EXECUTION_UNAVAILABLE 同款先例；执行桥属 runtime 任务）。
        let _ = finish_domain_operation(
            &ops,
            &domain_operation_id,
            &session_column,
            "failed",
            &json!({
                "code": CODE_PLUGIN_EXECUTION_UNAVAILABLE,
                "message": format!(
                    "plugin `{plugin_id_for_message}` operation `{plugin_operation_id}` is not \
                     wired into an execution bridge in this runtime assembly"
                ),
            }),
        );
        let _ = runner_db.finish_invocation(&domain_operation_id);
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// Tests (contract positive/negative cases against the real dispatch table)
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

    /// 播种一个带真实授权根的项目（tools.rs 测试同款）。
    fn seed_project(rt: &Arc<PublicV1Runtime>, root_dir: &Path, project_id: &str) -> PathBuf {
        let store = rt.store();
        let declared_root = root_dir.parent().unwrap().join("authorized-plugins");
        std::fs::create_dir_all(&declared_root).unwrap();
        let workspace_path = declared_root.join("ws");
        std::fs::create_dir_all(&workspace_path).unwrap();
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-plugins', ?1, ?2, ?3)",
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
                "name": "Plugins Tests",
                "environmentId": "local",
                "root": {"instanceId": store.meta().instance_id, "rootId": "root-plugins", "relativePath": "ws"},
            }))
            .unwrap();
        workspace_path
    }

    /// 在工作区内播种一个合法插件源目录。
    fn seed_plugin_source(
        workspace: &Path,
        name: &str,
        version: &str,
        operations: Value,
    ) -> PathBuf {
        let dir = workspace.join("sources").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "name": name,
            "displayName": format!("Test {name}"),
            "entry": "index.js",
            "version": version,
            "operations": operations,
        });
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("index.js"), "// plugin entry\n").unwrap();
        dir
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

    async fn install(
        rt: &Arc<PublicV1Runtime>,
        scope: &Value,
        plugin_id: &str,
        source_ref: &str,
        version: &str,
        expected_installed: Value,
        key: &str,
    ) -> Result<(ResponseOutcome, Value), DispatchError> {
        rt.dispatch(
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": plugin_id,
                "sourceRef": source_ref,
                "version": version,
                "expectedInstalledVersion": expected_installed,
            }),
            Some(key),
            None,
        )
        .await
    }

    /// Polls `operations.get` until the operation is terminal.
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

    fn assert_result_valid(method: &str, result: &Value) {
        OperationRegistry::global()
            .validate_result(method, result)
            .unwrap_or_else(|e| panic!("`{method}` result failed schema validation: {e:?}"));
    }

    fn sample_operations() -> Value {
        json!([
            {
                "operationId": "analyze",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                },
                "resultSchema": {
                    "type": "object",
                    "properties": {"score": {"type": "number"}},
                    "required": ["score"],
                    "additionalProperties": false
                },
                "permission": "plugins.invoke:analyze"
            }
        ])
    }

    #[tokio::test]
    async fn install_list_get_lifecycle_and_half_install_invisibility() {
        let (tmp, root_dir) = fresh_home("plug-lifecycle");
        let (rt, _iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-plug");
        let scope = json!({"kind": "project", "id": "proj-plug"});
        let source = seed_plugin_source(&workspace, "demo-plugin", "1.2.0", sample_operations());
        let source_ref = source.display().to_string();

        // 空目录：list 通过 schema 校验。
        let (_, listed) = rt
            .dispatch("plugins.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        assert_result_valid("plugins.list", &listed);
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));

        // 安装：受理即回执，完成后 installed=true / enabled=false（安装成功
        // 不自动宣称已启用）。
        let (outcome, receipt) = install(
            &rt,
            &scope,
            "demo-plugin",
            &source_ref,
            "1.2.0",
            Value::Null,
            "key-install-1",
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        assert_eq!(receipt["kind"], "operation");
        assert_eq!(receipt["durable"], true);
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        assert_eq!(operation["result"]["pluginId"], "demo-plugin");
        assert_eq!(operation["result"]["installed"], true);
        assert_eq!(operation["result"]["enabled"], false);
        assert_eq!(operation["result"]["available"], false);
        assert!(
            operation["result"]["manifestHash"]
                .as_str()
                .is_some_and(|h| h.len() == 64),
            "manifest hash is a sha256 hex digest"
        );

        let (_, listed) = rt
            .dispatch("plugins.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        let items = listed["items"].as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["pluginId"], "demo-plugin");
        assert_eq!(items[0]["version"], "1.2.0");
        assert_eq!(items[0]["installed"], true);
        assert_eq!(items[0]["enabled"], false);
        assert_eq!(items[0]["runtime"]["state"], "disabled");
        assert_eq!(items[0]["runtime"]["available"], false);
        assert_eq!(items[0]["runtime"]["reasonCode"], Value::Null);

        // get：清单投影 + 声明操作。
        let (_, got) = rt
            .dispatch(
                "plugins.get",
                json!({"scope": scope, "pluginId": "demo-plugin"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_result_valid("plugins.get", &got);
        assert_eq!(
            got["configurationSchemaId"],
            "agentcpm-pd.plugin.demo-plugin.configuration/1"
        );
        assert_eq!(got["configuration"], json!({}));
        let operations = got["operations"].as_array().cloned().unwrap_or_default();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0]["operationId"], "analyze");
        assert_eq!(operations[0]["available"], false);
        assert_eq!(
            operations[0]["inputSchemaId"],
            "agentcpm-pd.plugin.demo-plugin.op.analyze.input/1"
        );

        // 幂等重放：同 key 同输入 → 同回执。
        let (_, replay) = install(
            &rt,
            &scope,
            "demo-plugin",
            &source_ref,
            "1.2.0",
            json!("1.2.0"),
            "key-install-2",
        )
        .await
        .unwrap();
        let replay_op = wait_operation(&rt, replay["operationId"].as_str().unwrap()).await;
        assert_eq!(replay_op["state"], "succeeded");

        // 预期外版本（CAS）→ VERSION_CONFLICT，旧插件仍可用。
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "demo-plugin",
                "sourceRef": source_ref,
                "version": "1.3.0",
                "expectedInstalledVersion": "9.9.9",
            }),
            Some("key-install-stale"),
        )
        .await;
        assert_eq!(error.code, "VERSION_CONFLICT");

        // 未安装却声明期望版本 → VERSION_CONFLICT。
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "other-plugin",
                "sourceRef": source_ref,
                "version": "1.0.0",
                "expectedInstalledVersion": "1.0.0",
            }),
            Some("key-install-missing"),
        )
        .await;
        assert_eq!(error.code, "VERSION_CONFLICT");

        // 安装中的插件不可见（半安装不公告）：失败的 URL 安装不留行。
        let (_, receipt) = install(
            &rt,
            &scope,
            "ghost-plugin",
            "https://example.com/ghost.git",
            "0.1.0",
            Value::Null,
            "key-install-url",
        )
        .await
        .unwrap();
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "failed");
        assert_eq!(operation["result"]["code"], "UPSTREAM_ERROR");
        let (_, listed) = rt
            .dispatch(
                "plugins.list",
                json!({"scope": scope, "limit": 200}),
                None,
                None,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["pluginId"].as_str())
            .collect();
        assert_eq!(ids, vec!["demo-plugin"], "the failed install left no row");

        // 安装资源落盘在领域目录内。
        let install_dir = root_dir
            .join("v1")
            .join("plugins")
            .join(layer_slug(&layer_key("project", "proj-plug")))
            .join("demo-plugin");
        assert!(install_dir.join("manifest.json").is_file());
        drop(tmp);
    }

    #[tokio::test]
    async fn install_rejects_bad_names_sources_and_manifests() {
        let (tmp, root_dir) = fresh_home("plug-reject");
        let (rt, _iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-reject");
        let scope = json!({"kind": "project", "id": "proj-reject"});
        let source = seed_plugin_source(&workspace, "good-plugin", "1.0.0", json!([]));

        // 非法名称。
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "../escape",
                "sourceRef": source.display().to_string(),
                "version": "1.0.0",
                "expectedInstalledVersion": Value::Null,
            }),
            Some("key-reject-1"),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 越界路径（工作区之外）→ PATH_OUTSIDE_SCOPE。
        let outside = root_dir.parent().unwrap().join("outside-src");
        std::fs::create_dir_all(&outside).unwrap();
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "outside-plugin",
                "sourceRef": outside.display().to_string(),
                "version": "1.0.0",
                "expectedInstalledVersion": Value::Null,
            }),
            Some("key-reject-2"),
        )
        .await;
        assert_eq!(error.code, "PATH_OUTSIDE_SCOPE");

        // 缺 manifest 的目录。
        let empty = workspace.join("empty-src");
        std::fs::create_dir_all(&empty).unwrap();
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "empty-plugin",
                "sourceRef": empty.display().to_string(),
                "version": "1.0.0",
                "expectedInstalledVersion": Value::Null,
            }),
            Some("key-reject-3"),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // manifest.name 与 pluginId 不一致。
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "other-name",
                "sourceRef": source.display().to_string(),
                "version": "1.0.0",
                "expectedInstalledVersion": Value::Null,
            }),
            Some("key-reject-4"),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 版本与 manifest 声明不一致。
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "good-plugin",
                "sourceRef": source.display().to_string(),
                "version": "2.0.0",
                "expectedInstalledVersion": Value::Null,
            }),
            Some("key-reject-5"),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // 非法 URL 前缀（既不是 https/git 也不是可解析路径）。
        let error = dispatch_err(
            &rt,
            "plugins.install",
            json!({
                "scope": scope,
                "pluginId": "bad-url-plugin",
                "sourceRef": "http://insecure.example.com/x.git",
                "version": "1.0.0",
                "expectedInstalledVersion": Value::Null,
            }),
            Some("key-reject-6"),
        )
        .await;
        assert_eq!(
            error.code, "PATH_OUTSIDE_SCOPE",
            "a non-https URL is treated as a (missing) local path"
        );

        let (_, listed) = rt
            .dispatch("plugins.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        assert_eq!(
            listed["items"].as_array().map(Vec::len),
            Some(0),
            "no rejected install left a row"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn configure_validates_schemas_and_masks_secrets() {
        let (tmp, root_dir) = fresh_home("plug-configure");
        let (rt, _iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-cfg");
        let scope = json!({"kind": "project", "id": "proj-cfg"});
        let source = seed_plugin_source(
            &workspace,
            "cfg-plugin",
            "1.0.0",
            json!([{
                "operationId": "analyze",
                "resultSchema": {"type": "object", "properties": {"score": {"type": "number"}}}
            }]),
        );
        let manifest = json!({
            "name": "cfg-plugin",
            "displayName": "Cfg",
            "entry": "index.js",
            "version": "1.0.0",
            "configurationSchema": {
                "type": "object",
                "properties": {"endpoint": {"type": "string"}, "apiKey": {"type": "string"}},
                "required": ["endpoint"],
                "additionalProperties": false
            }
        });
        std::fs::write(
            source.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        install(
            &rt,
            &scope,
            "cfg-plugin",
            &source.display().to_string(),
            "1.0.0",
            Value::Null,
            "key-cfg-install",
        )
        .await
        .unwrap();

        // 缺 schemaId / 错误 schemaId → SCHEMA_MISMATCH。
        let error = dispatch_err(
            &rt,
            "plugins.configure",
            json!({
                "scope": scope,
                "pluginId": "cfg-plugin",
                "schemaId": "agentcpm-pd.plugin.other.configuration/1",
                "configuration": {"endpoint": "https://api.example.test"},
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // 未知字段拒绝。
        let error = dispatch_err(
            &rt,
            "plugins.configure",
            json!({
                "scope": scope,
                "pluginId": "cfg-plugin",
                "schemaId": "agentcpm-pd.plugin.cfg-plugin.configuration/1",
                "configuration": {"endpoint": "https://api.example.test", "shell": "bash"},
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");
        assert!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("validationErrors"))
                .is_some(),
            "schema mismatch carries validation details"
        );

        // 首次配置：absent → 新 revision；applied=false（无可应用代际）。
        let (_, configured) = rt
            .dispatch(
                "plugins.configure",
                json!({
                    "scope": scope,
                    "pluginId": "cfg-plugin",
                    "schemaId": "agentcpm-pd.plugin.cfg-plugin.configuration/1",
                    "configuration": {"endpoint": "https://api.example.test", "apiKey": "sk-live-123"},
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_result_valid("plugins.configure", &configured);
        let revision = configured["revision"].as_str().unwrap().to_owned();
        assert!(revision.starts_with("pcfg-"));
        assert_eq!(configured["applied"], false);
        assert_eq!(configured["generation"], Value::Null);

        // 读取面脱敏；掩码回传保真。
        let (_, got) = rt
            .dispatch(
                "plugins.get",
                json!({"scope": scope, "pluginId": "cfg-plugin"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["configuration"]["endpoint"], "https://api.example.test");
        assert_eq!(got["configuration"]["apiKey"], "<redacted>");

        let (_, roundtrip) = rt
            .dispatch(
                "plugins.configure",
                json!({
                    "scope": scope,
                    "pluginId": "cfg-plugin",
                    "schemaId": "agentcpm-pd.plugin.cfg-plugin.configuration/1",
                    "configuration": {"endpoint": "https://v2.example.test", "apiKey": "<redacted>"},
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let revision2 = roundtrip["revision"].as_str().unwrap().to_owned();
        assert_ne!(revision, revision2);
        let (_, got) = rt
            .dispatch(
                "plugins.get",
                json!({"scope": scope, "pluginId": "cfg-plugin"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["configuration"]["apiKey"], "<redacted>");
        // 掩码回传保真：存储值未被字面量覆盖（提交同值仍校验通过）。
        let error = dispatch_err(
            &rt,
            "plugins.configure",
            json!({
                "scope": scope,
                "pluginId": "cfg-plugin",
                "schemaId": "agentcpm-pd.plugin.cfg-plugin.configuration/1",
                "configuration": {"endpoint": "https://v2.example.test", "apiKey": "<redacted>"},
                "expectedRevision": revision.clone(),
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT", "stale revision rejects");

        // 未知插件 → NOT_FOUND。
        let error = dispatch_err(
            &rt,
            "plugins.configure",
            json!({
                "scope": scope,
                "pluginId": "ghost",
                "schemaId": "agentcpm-pd.plugin.ghost.configuration/1",
                "configuration": {},
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
        drop(tmp);
    }

    #[tokio::test]
    async fn transition_lifecycle_and_generation_cas() {
        let (tmp, root_dir) = fresh_home("plug-transition");
        let (rt, _iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-trans");
        let scope = json!({"kind": "project", "id": "proj-trans"});
        let source = seed_plugin_source(&workspace, "trans-plugin", "1.0.0", json!([]));
        install(
            &rt,
            &scope,
            "trans-plugin",
            &source.display().to_string(),
            "1.0.0",
            Value::Null,
            "key-trans-install",
        )
        .await
        .unwrap();

        // 未知插件 → NOT_FOUND。
        let error = dispatch_err(
            &rt,
            "plugins.transition",
            json!({
                "scope": scope,
                "pluginId": "ghost",
                "action": "enable",
                "expectedGeneration": "absent",
            }),
            Some("key-trans-ghost"),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // enable：absent CAS 通过，分配新代际。
        let (_, receipt) = rt
            .dispatch(
                "plugins.transition",
                json!({
                    "scope": scope,
                    "pluginId": "trans-plugin",
                    "action": "enable",
                    "expectedGeneration": "absent",
                }),
                Some("key-trans-enable"),
                None,
            )
            .await
            .unwrap();
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        assert_eq!(operation["result"]["enabled"], true);
        let generation = operation["result"]["generation"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(generation.starts_with("pgen-"));
        assert_eq!(operation["result"]["state"], "not-loaded");
        assert_eq!(operation["result"]["retainedBindings"], json!([]));

        // 旧 expectedGeneration → GENERATION_CONFLICT。
        let error = dispatch_err(
            &rt,
            "plugins.transition",
            json!({
                "scope": scope,
                "pluginId": "trans-plugin",
                "action": "reload",
                "expectedGeneration": "absent",
            }),
            Some("key-trans-stale"),
        )
        .await;
        assert_eq!(error.code, "GENERATION_CONFLICT");

        // 已启用插件的 runtime 视图：state=not-loaded + 真实原因。
        let (_, listed) = rt
            .dispatch("plugins.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        let item = &listed["items"][0];
        assert_eq!(item["enabled"], true);
        assert_eq!(item["runtime"]["state"], "not-loaded");
        assert_eq!(item["runtime"]["available"], false);
        assert_eq!(
            item["runtime"]["reasonCode"],
            "PLUGIN_RUNTIME_NOT_ASSEMBLED"
        );
        assert_eq!(item["runtime"]["generation"], json!(generation));

        // disable：保留旧代际，停新调用。
        let (_, receipt) = rt
            .dispatch(
                "plugins.transition",
                json!({
                    "scope": scope,
                    "pluginId": "trans-plugin",
                    "action": "disable",
                    "expectedGeneration": generation,
                }),
                Some("key-trans-disable"),
                None,
            )
            .await
            .unwrap();
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        assert_eq!(operation["result"]["enabled"], false);
        assert_eq!(operation["result"]["generation"], json!(generation));

        let (_, listed) = rt
            .dispatch("plugins.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["items"][0]["runtime"]["state"], "disabled");
        drop(tmp);
    }

    #[tokio::test]
    async fn remove_retains_configuration_by_default_and_reinstall_restores() {
        let (tmp, root_dir) = fresh_home("plug-remove");
        let (rt, _iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-rm");
        let scope = json!({"kind": "project", "id": "proj-rm"});
        let source = seed_plugin_source(
            &workspace,
            "rm-plugin",
            "1.0.0",
            json!([{
                "operationId": "analyze",
                "resultSchema": {"type": "object"}
            }]),
        );
        let manifest = json!({
            "name": "rm-plugin",
            "displayName": "Rm",
            "entry": "index.js",
            "version": "1.0.0",
            "configurationSchema": {
                "type": "object",
                "properties": {"token": {"type": "string"}},
                "additionalProperties": false
            }
        });
        std::fs::write(
            source.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        install(
            &rt,
            &scope,
            "rm-plugin",
            &source.display().to_string(),
            "1.0.0",
            Value::Null,
            "key-rm-install",
        )
        .await
        .unwrap();
        let (_, configured) = rt
            .dispatch(
                "plugins.configure",
                json!({
                    "scope": scope,
                    "pluginId": "rm-plugin",
                    "schemaId": "agentcpm-pd.plugin.rm-plugin.configuration/1",
                    "configuration": {"token": "keep-me"},
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let config_revision = configured["revision"].as_str().unwrap().to_owned();

        // 版本不匹配 → VERSION_CONFLICT。
        let error = dispatch_err(
            &rt,
            "plugins.remove",
            json!({
                "scope": scope,
                "pluginId": "rm-plugin",
                "expectedVersion": "9.9.9",
            }),
            Some("key-rm-stale"),
        )
        .await;
        assert_eq!(error.code, "VERSION_CONFLICT");

        // remove：默认保留配置。
        let (_, receipt) = rt
            .dispatch(
                "plugins.remove",
                json!({
                    "scope": scope,
                    "pluginId": "rm-plugin",
                    "expectedVersion": "1.0.0",
                }),
                Some("key-rm-1"),
                None,
            )
            .await
            .unwrap();
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        assert_eq!(operation["result"]["removed"], true);
        assert_eq!(operation["result"]["configurationRetained"], true);
        assert_eq!(
            operation["result"]["retainedResources"],
            json!(["configuration"])
        );

        let (_, listed) = rt
            .dispatch("plugins.list", json!({"scope": scope}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
        let install_dir = root_dir
            .join("v1")
            .join("plugins")
            .join(layer_slug(&layer_key("project", "proj-rm")))
            .join("rm-plugin");
        assert!(!install_dir.exists(), "install resources are reclaimed");

        // 再装回（同版本号）：保留的配置恢复。
        let (_, receipt) = install(
            &rt,
            &scope,
            "rm-plugin",
            &source.display().to_string(),
            "1.0.0",
            Value::Null,
            "key-rm-reinstall",
        )
        .await
        .unwrap();
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "succeeded");
        let (_, got) = rt
            .dispatch(
                "plugins.get",
                json!({"scope": scope, "pluginId": "rm-plugin"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["configuration"]["token"], "<redacted>");
        assert_eq!(
            got["plugin"]["configurationRevision"],
            json!(config_revision),
            "the retained configuration revision survives re-install"
        );

        // removeConfiguration=true：连暂存一起清掉。
        rt.dispatch(
            "plugins.remove",
            json!({
                "scope": scope,
                "pluginId": "rm-plugin",
                "expectedVersion": "1.0.0",
                "removeConfiguration": true,
            }),
            Some("key-rm-2"),
            None,
        )
        .await
        .unwrap();
        // 等待 remove 完成。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, listed) = rt
                .dispatch("plugins.list", json!({"scope": scope}), None, None)
                .await
                .unwrap();
            if listed["items"].as_array().map(Vec::len) == Some(0) {
                break;
            }
            assert!(Instant::now() < deadline, "remove never completed");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let (_, receipt) = install(
            &rt,
            &scope,
            "rm-plugin",
            &source.display().to_string(),
            "1.0.0",
            Value::Null,
            "key-rm-3",
        )
        .await
        .unwrap();
        wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        let (_, got) = rt
            .dispatch(
                "plugins.get",
                json!({"scope": scope, "pluginId": "rm-plugin"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            got["plugin"]["configurationRevision"],
            json!(""),
            "removeConfiguration=true cleared the stash"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn invoke_rejects_before_execution_and_fails_honestly_after_accept() {
        let (tmp, root_dir) = fresh_home("plug-invoke");
        let (rt, _iid) = open_runtime(&root_dir);
        let workspace = seed_project(&rt, &root_dir, "proj-inv");
        let scope = json!({"kind": "project", "id": "proj-inv"});
        let source = seed_plugin_source(&workspace, "inv-plugin", "1.0.0", sample_operations());
        install(
            &rt,
            &scope,
            "inv-plugin",
            &source.display().to_string(),
            "1.0.0",
            Value::Null,
            "key-inv-install",
        )
        .await
        .unwrap();

        // 未声明操作 → NOT_FOUND（插件执行前拒绝）。
        let error = dispatch_err(
            &rt,
            "plugins.invoke",
            json!({
                "scope": scope,
                "pluginId": "inv-plugin",
                "operationId": "not-declared",
                "schemaId": "agentcpm-pd.plugin.inv-plugin.op.not-declared.input/1",
                "arguments": {},
            }),
            Some("key-inv-unknown"),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");

        // schemaId 不匹配 → SCHEMA_MISMATCH。
        let error = dispatch_err(
            &rt,
            "plugins.invoke",
            json!({
                "scope": scope,
                "pluginId": "inv-plugin",
                "operationId": "analyze",
                "schemaId": "agentcpm-pd.plugin.inv-plugin.op.analyze.result/1",
                "arguments": {"text": "hello"},
            }),
            Some("key-inv-schema"),
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // 参数不合 schema → SCHEMA_MISMATCH（带详情）。
        let error = dispatch_err(
            &rt,
            "plugins.invoke",
            json!({
                "scope": scope,
                "pluginId": "inv-plugin",
                "operationId": "analyze",
                "schemaId": "agentcpm-pd.plugin.inv-plugin.op.analyze.input/1",
                "arguments": {"text": 42},
            }),
            Some("key-inv-args"),
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // 禁用插件 → UNAVAILABLE（基线 503 语义）。
        let error = dispatch_err(
            &rt,
            "plugins.invoke",
            json!({
                "scope": scope,
                "pluginId": "inv-plugin",
                "operationId": "analyze",
                "schemaId": "agentcpm-pd.plugin.inv-plugin.op.analyze.input/1",
                "arguments": {"text": "hello"},
            }),
            Some("key-inv-disabled"),
        )
        .await;
        assert_eq!(error.code, "UNAVAILABLE");

        // 启用后：受理 → 诚实失败（无执行桥）。
        rt.dispatch(
            "plugins.transition",
            json!({
                "scope": scope,
                "pluginId": "inv-plugin",
                "action": "enable",
                "expectedGeneration": "absent",
            }),
            Some("key-inv-enable"),
            None,
        )
        .await
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, got) = rt
                .dispatch(
                    "plugins.get",
                    json!({"scope": scope, "pluginId": "inv-plugin"}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["plugin"]["enabled"] == true {
                break;
            }
            assert!(Instant::now() < deadline, "enable never landed");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let (_, receipt) = rt
            .dispatch(
                "plugins.invoke",
                json!({
                    "scope": scope,
                    "pluginId": "inv-plugin",
                    "operationId": "analyze",
                    "schemaId": "agentcpm-pd.plugin.inv-plugin.op.analyze.input/1",
                    "arguments": {"text": "hello"},
                }),
                Some("key-inv-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], "operation");
        let operation = wait_operation(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(operation["state"], "failed");
        assert_eq!(operation["result"]["code"], "PLUGIN_EXECUTION_UNAVAILABLE");

        // 无 dispatch key → 拒绝（idempotentSubmit 合同）。
        let result = rt
            .dispatch(
                "plugins.invoke",
                json!({
                    "scope": scope,
                    "pluginId": "inv-plugin",
                    "operationId": "analyze",
                    "schemaId": "agentcpm-pd.plugin.inv-plugin.op.analyze.input/1",
                    "arguments": {"text": "hello"},
                }),
                None,
                None,
            )
            .await;
        assert!(result.is_err(), "missing idempotencyKey must reject");
        drop(tmp);
    }

    #[tokio::test]
    async fn layered_scopes_are_independent() {
        let (tmp, root_dir) = fresh_home("plug-layers");
        let (rt, _iid) = open_runtime(&root_dir);
        seed_project(&rt, &root_dir, "proj-layers");
        // global 锚点是实例 home：源目录必须落在 home 内（工作区不属于
        // home，会正确地被 PATH_OUTSIDE_SCOPE 拒绝）。
        let source = seed_plugin_source(&root_dir, "layer-plugin", "1.0.0", json!([]));

        // global 层与 project 层互不串扰。
        install(
            &rt,
            &json!({"kind": "global"}),
            "layer-plugin",
            &source.display().to_string(),
            "1.0.0",
            Value::Null,
            "key-layer-global",
        )
        .await
        .unwrap();
        let (_, global_list) = rt
            .dispatch(
                "plugins.list",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(global_list["items"].as_array().map(Vec::len), Some(1));
        let (_, project_list) = rt
            .dispatch(
                "plugins.list",
                json!({"scope": {"kind": "project", "id": "proj-layers"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(project_list["items"].as_array().map(Vec::len), Some(0));

        // global 里的 unknown scope 仍按 schema 校验拒绝（id 为空）。
        let error = dispatch_err(
            &rt,
            "plugins.list",
            json!({"scope": {"kind": "project", "id": ""}}),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        drop(tmp);
    }
}
