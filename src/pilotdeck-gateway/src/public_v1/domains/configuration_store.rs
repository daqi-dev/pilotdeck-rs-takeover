//! Storage layer for the `configuration.*` domain (`cli.task.config`, S3).
//!
//! The v1 configuration contract is a scoped key-value layer store
//! (`instance < project < session` overlays) with per-layer stored revisions,
//! one instance-wide effective generation, and durable async-operation
//! records for `configuration.reload`. It is a redesign of the TS baseline's
//! `pilotdeck.yaml` document model (source decision "重设计承接…不继承原配置
//! 字段混用为新合同"), so it owns its tables rather than reusing
//! `config/` (the old line is the YAML loader for a different contract).
//!
//! Physical placement: the tables live in the v1 instance's own SQLite
//! database (`<AGENTCPM_PD_HOME>/v1/state.sqlite`, 007 §8) alongside the S2
//! core tables. `V1Store`'s connection is private to `store.rs`, and the
//! substrate's frozen `DomainHandler` signature has no per-domain state
//! slot, so this module opens its own short-lived `rusqlite` connection per
//! operation (`ConfigDb::open`). The file already runs `WAL` +
//! `busy_timeout` (set at store creation, and re-asserted here), and every
//! write runs in an `IMMEDIATE` transaction, so concurrent store/domain
//! writers serialize on SQLite's own locks instead of deadlocking.
//!
//! DDL is registered twice on purpose: once through
//! `DomainRegistry::schema` (the substrate hook `V1Store::init_schema`
//! executes when the database is created) and once idempotently in
//! `ConfigDb::open` as defense in depth for databases created by builds
//! without this domain compiled in.

use std::collections::BTreeMap;
use std::path::Path;

use agentcpm_pd_protocol::errors::{CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec};
use rusqlite::OptionalExtension;
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::public_v1::store::now_rfc3339;

/// One stored layer entry: the raw JSON value plus the layer revision at
/// which this value was written (the `sourceRevision` the resolve contract
/// reports).
#[derive(Clone, Debug)]
pub(super) struct StoredEntry {
    pub(super) value: Value,
    pub(super) write_revision: String,
}

pub(super) type LayerMap = BTreeMap<String, StoredEntry>;

/// Instance-wide effective state (what the runtime has applied). The failed
/// apply's target revision and reason live only in the database row
/// (`target_revision`/`last_error` columns); this projection carries what
/// callers need.
#[derive(Clone, Debug)]
pub(super) struct EffectiveState {
    pub(super) generation: String,
    pub(super) reload_state: String,
    /// layer id -> stored revision incorporated into `generation`.
    pub(super) applied: BTreeMap<String, String>,
}

/// Durable record of one async `configuration.reload` submission, shaped so
/// a future `operations.get` handler can project it into the contract's
/// `Operation` object without re-parsing domain state.
#[derive(Clone, Debug)]
pub(super) struct OperationRecord {
    pub(super) operation_id: String,
    pub(super) operation_type: String,
    pub(super) scope_json: Value,
    pub(super) state: String,
    pub(super) revision: String,
    pub(super) cancel_requested: bool,
    pub(super) effect_status: String,
    pub(super) progress: Vec<Value>,
    pub(super) result: Option<Value>,
    pub(super) error: Option<Value>,
}

/// The implicit revision of a layer that has never been written. Opaque like
/// every revision (`Rev`: server-defined, not client-incremented).
pub(super) const BASE_REVISION: &str = "cfg-0";
/// The implicit effective generation before any apply has happened.
pub(super) const BASE_GENERATION: &str = "cfggen-0";

pub(super) const DDL_LAYER_ROWS: &str = "CREATE TABLE IF NOT EXISTS v1_config_layer_rows (
    layer TEXT NOT NULL,
    cfg_key TEXT NOT NULL,
    value_json TEXT NOT NULL,
    write_revision TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (layer, cfg_key)
)";
pub(super) const DDL_LAYER_STATE: &str = "CREATE TABLE IF NOT EXISTS v1_config_layer_state (
    layer TEXT PRIMARY KEY,
    stored_revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";
pub(super) const DDL_EFFECTIVE: &str = "CREATE TABLE IF NOT EXISTS v1_config_effective (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    generation TEXT NOT NULL,
    reload_state TEXT NOT NULL,
    applied_json TEXT NOT NULL,
    target_revision TEXT,
    last_error TEXT,
    updated_at TEXT NOT NULL
)";
pub(super) const DDL_COUNTERS: &str = "CREATE TABLE IF NOT EXISTS v1_config_counters (
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
)";
pub(super) const DDL_OPERATIONS: &str = "CREATE TABLE IF NOT EXISTS v1_config_operations (
    operation_id TEXT PRIMARY KEY,
    operation_type TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    state TEXT NOT NULL,
    revision TEXT NOT NULL,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    effect_status TEXT NOT NULL DEFAULT 'known',
    progress_json TEXT NOT NULL DEFAULT '[]',
    result_json TEXT,
    error_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn domain_ddl() -> Vec<(&'static str, &'static str)> {
    vec![
        ("v1_config_layer_rows", DDL_LAYER_ROWS),
        ("v1_config_layer_state", DDL_LAYER_STATE),
        ("v1_config_effective", DDL_EFFECTIVE),
        ("v1_config_counters", DDL_COUNTERS),
        ("v1_config_operations", DDL_OPERATIONS),
    ]
}

pub(super) fn storage_err(msg: String) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {msg}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

pub(super) fn storage_err_from(err: rusqlite::Error) -> ContractError {
    storage_err(err.to_string())
}

/// A short-lived connection to the v1 state database for one operation.
pub(super) struct ConfigDb {
    conn: Connection,
}

impl ConfigDb {
    /// Opens `<home>/v1/state.sqlite` with the same durability pragmas the
    /// store asserts and runs the (idempotent) domain DDL.
    pub(super) fn open(home: &Path) -> Result<Self, ContractError> {
        let db_path = home.join("v1").join("state.sqlite");
        let conn = Connection::open(&db_path)
            .map_err(|e| storage_err(format!("open {}: {e}", db_path.display())))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| storage_err(format!("sqlite pragma: {e}")))?;
        for (_table, ddl) in domain_ddl() {
            conn.execute_batch(ddl).map_err(storage_err_from)?;
        }
        Ok(Self { conn })
    }

    /// Runs `f` inside one `IMMEDIATE` transaction (all writes serialize on
    /// the SQLite write lock; the upgrade-deadlock window of deferred
    /// transactions never opens).
    pub(super) fn with_tx<T>(
        &mut self,
        f: impl FnOnce(&Connection) -> Result<T, ContractError>,
    ) -> Result<T, ContractError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_err_from)?;
        let out = f(&tx)?;
        tx.commit().map_err(storage_err_from)?;
        Ok(out)
    }

    /// Begins one `IMMEDIATE` transaction for the multi-step
    /// `configuration.reload` flow (which must commit its rejection record
    /// even when it answers with an error).
    pub(super) fn begin(&mut self) -> Result<ConfigTx<'_>, ContractError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_err_from)?;
        Ok(ConfigTx { tx })
    }

    // -----------------------------------------------------------------
    // Layer reads
    // -----------------------------------------------------------------

    /// All entries of one layer, ordered by key (deterministic reads).
    pub(super) fn read_layer(&self, layer: &str) -> Result<LayerMap, ContractError> {
        read_layer(&self.conn, layer)
    }

    /// The layer's stored revision (`BASE_REVISION` until first written).
    pub(super) fn layer_revision(&self, layer: &str) -> Result<String, ContractError> {
        layer_revision(&self.conn, layer)
    }

    // -----------------------------------------------------------------
    // Effective (applied) state
    // -----------------------------------------------------------------

    pub(super) fn effective(&self) -> Result<EffectiveState, ContractError> {
        effective_row(&self.conn)
    }

    // -----------------------------------------------------------------
    // Test support: raw connection access for store-surgery fixtures.
    // -----------------------------------------------------------------
    #[cfg(test)]
    pub(super) fn conn(&self) -> &Connection {
        &self.conn
    }

    #[cfg(test)]
    pub(super) fn reload_operation(&self, operation_id: &str) -> Option<OperationRecord> {
        read_operation(&self.conn, operation_id).ok().flatten()
    }
}

/// A manual `IMMEDIATE` transaction for the `configuration.reload` flow
/// (which commits its rejection record even when answering with an error).
pub(super) struct ConfigTx<'a> {
    tx: rusqlite::Transaction<'a>,
}

impl ConfigTx<'_> {
    pub(super) fn conn(&self) -> &Connection {
        &self.tx
    }

    pub(super) fn commit(self) -> Result<(), ContractError> {
        self.tx.commit().map_err(storage_err_from)
    }
}

/// Every layer that holds any state (rows or a stored revision), sorted.
pub(super) fn all_layers(conn: &Connection) -> Result<Vec<String>, ContractError> {
    let mut stmt = conn
        .prepare(
            "SELECT layer FROM v1_config_layer_rows
             UNION SELECT layer FROM v1_config_layer_state",
        )
        .map_err(storage_err_from)?;
    let mut layers: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(storage_err_from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_err_from)?;
    layers.sort();
    Ok(layers)
}

pub(super) fn read_layer(conn: &Connection, layer: &str) -> Result<LayerMap, ContractError> {
    let mut stmt = conn
        .prepare(
            "SELECT cfg_key, value_json, write_revision FROM v1_config_layer_rows
             WHERE layer = ?1 ORDER BY cfg_key ASC",
        )
        .map_err(storage_err_from)?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map(params![layer], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(storage_err_from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_err_from)?;
    let mut map = LayerMap::new();
    for (key, value_json, write_revision) in rows {
        let value = serde_json::from_str(&value_json).unwrap_or(Value::Null);
        map.insert(
            key,
            StoredEntry {
                value,
                write_revision,
            },
        );
    }
    Ok(map)
}

pub(super) fn layer_revision(conn: &Connection, layer: &str) -> Result<String, ContractError> {
    let rev: Option<String> = conn
        .query_row(
            "SELECT stored_revision FROM v1_config_layer_state WHERE layer = ?1",
            params![layer],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage_err_from)?;
    Ok(rev.unwrap_or_else(|| BASE_REVISION.to_owned()))
}

pub(super) fn effective_row(conn: &Connection) -> Result<EffectiveState, ContractError> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT generation, reload_state, applied_json
             FROM v1_config_effective WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(storage_err_from)?;
    Ok(match row {
        Some((generation, reload_state, applied_json)) => {
            let applied = serde_json::from_str::<Value>(&applied_json)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            EffectiveState {
                generation,
                reload_state,
                applied: applied
                    .into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_owned())))
                    .collect(),
            }
        }
        None => EffectiveState {
            generation: BASE_GENERATION.to_owned(),
            reload_state: "current".to_owned(),
            applied: BTreeMap::new(),
        },
    })
}

/// Reads one stored entry without materializing the whole layer (used by the
/// secret-preservation rules).
pub(super) fn read_entry(
    conn: &Connection,
    layer: &str,
    key: &str,
) -> Result<Option<StoredEntry>, ContractError> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT value_json, write_revision FROM v1_config_layer_rows
             WHERE layer = ?1 AND cfg_key = ?2",
            params![layer, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_err_from)?;
    Ok(row.map(|(value_json, write_revision)| StoredEntry {
        value: serde_json::from_str(&value_json).unwrap_or(Value::Null),
        write_revision,
    }))
}

/// Draws the next value of a named counter (`revision`, `generation`).
/// Must run inside a write transaction.
pub(super) fn next_counter(conn: &Connection, name: &str) -> Result<i64, ContractError> {
    conn.execute(
        "INSERT INTO v1_config_counters (name, value) VALUES (?1, 1)
         ON CONFLICT(name) DO UPDATE SET value = value + 1",
        params![name],
    )
    .map_err(storage_err_from)?;
    conn.query_row(
        "SELECT value FROM v1_config_counters WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )
    .map_err(storage_err_from)
}

pub(super) fn next_revision(conn: &Connection) -> Result<String, ContractError> {
    Ok(format!("cfg-{}", next_counter(conn, "revision")?))
}

pub(super) fn next_generation(conn: &Connection) -> Result<String, ContractError> {
    Ok(format!("cfggen-{}", next_counter(conn, "generation")?))
}

/// Persists a whole layer entry (upsert) with the write revision stamped on
/// the entry (the resolve contract's `sourceRevision`).
pub(super) fn write_entry(
    conn: &Connection,
    layer: &str,
    key: &str,
    value: &Value,
    revision: &str,
) -> Result<(), ContractError> {
    let value_json = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    conn.execute(
        "INSERT INTO v1_config_layer_rows (layer, cfg_key, value_json, write_revision, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(layer, cfg_key) DO UPDATE SET value_json = excluded.value_json,
             write_revision = excluded.write_revision, updated_at = excluded.updated_at",
        params![layer, key, value_json, revision, now_rfc3339()],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// Removes one entry from a layer. Deleting an overlay entry re-exposes the
/// lower layer's value; it never tombstones across layers.
pub(super) fn delete_entry(conn: &Connection, layer: &str, key: &str) -> Result<(), ContractError> {
    conn.execute(
        "DELETE FROM v1_config_layer_rows WHERE layer = ?1 AND cfg_key = ?2",
        params![layer, key],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// Bumps a layer's stored revision (called inside the update transaction).
pub(super) fn set_layer_revision(
    conn: &Connection,
    layer: &str,
    revision: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "INSERT INTO v1_config_layer_state (layer, stored_revision, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(layer) DO UPDATE SET stored_revision = excluded.stored_revision,
             updated_at = excluded.updated_at",
        params![layer, revision, now_rfc3339()],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

fn applied_to_json(applied: &BTreeMap<String, String>) -> String {
    let mut map = serde_json::Map::new();
    for (layer, revision) in applied {
        map.insert(layer.clone(), json!(revision));
    }
    serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| "{}".into())
}

/// Records a successful apply: new instance-wide effective generation that
/// incorporates every existing layer's current stored revision.
pub(super) fn apply_success(
    conn: &Connection,
    generation: &str,
    applied: &BTreeMap<String, String>,
) -> Result<(), ContractError> {
    let applied_json = applied_to_json(applied);
    conn.execute(
        "INSERT INTO v1_config_effective (id, generation, reload_state, applied_json, target_revision, last_error, updated_at)
         VALUES (1, ?1, 'current', ?2, NULL, NULL, ?3)
         ON CONFLICT(id) DO UPDATE SET generation = excluded.generation, reload_state = 'current',
             applied_json = excluded.applied_json, target_revision = NULL, last_error = NULL,
             updated_at = excluded.updated_at",
        params![generation, applied_json, now_rfc3339()],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// Records a failed apply: the previous generation stays effective and the
/// failure (target revision + reason) is observable through resolve.
pub(super) fn apply_failure(
    conn: &Connection,
    target_revision: &str,
    reason: &str,
) -> Result<(), ContractError> {
    let row = effective_row(conn)?;
    let applied_json = applied_to_json(&row.applied);
    conn.execute(
        "INSERT INTO v1_config_effective (id, generation, reload_state, applied_json, target_revision, last_error, updated_at)
         VALUES (1, ?1, 'failed', ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET reload_state = 'failed', target_revision = excluded.target_revision,
             last_error = excluded.last_error, updated_at = excluded.updated_at",
        params![row.generation, applied_json, target_revision, reason, now_rfc3339()],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// The snapshot of layer revisions an apply would incorporate.
pub(super) fn current_layer_revisions(
    conn: &Connection,
    layers: &[String],
) -> Result<BTreeMap<String, String>, ContractError> {
    let mut out = BTreeMap::new();
    for layer in layers {
        out.insert(layer.clone(), layer_revision(conn, layer)?);
    }
    Ok(out)
}

/// Appends the `configuration.changed` domain event to the shared event log
/// (the same `events` table the S2 core writes). The payload carries only
/// revisions and changed key names — never values, so secrets cannot leak
/// through the event stream.
pub(super) fn append_config_event(
    conn: &Connection,
    resource_id: &str,
    session_id: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    static SEQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let placeholder = format!("tmpcfg-{nanos}-{seq}");
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, 'configuration.changed', 'agentcpm-pd.configuration.event/1', ?4, ?4, ?5)",
        params![placeholder, resource_id, session_id, payload_str, now_rfc3339()],
    )
    .map_err(storage_err_from)?;
    let seq = conn.last_insert_rowid();
    let cursor = crate::public_v1::store::format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// Persists the operation record of one `configuration.reload` submission.
pub(super) fn write_operation(
    conn: &Connection,
    record: &OperationRecord,
) -> Result<(), ContractError> {
    let progress_json = serde_json::to_string(&record.progress).unwrap_or_else(|_| "[]".into());
    let result_json = record
        .result
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into()));
    let error_json = record
        .error
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into()));
    let scope_json = serde_json::to_string(&record.scope_json).unwrap_or_else(|_| "{}".into());
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO v1_config_operations (operation_id, operation_type, scope_json, state, revision,
            cancel_requested, effect_status, progress_json, result_json, error_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
        params![
            record.operation_id,
            record.operation_type,
            scope_json,
            record.state,
            record.revision,
            record.cancel_requested as i64,
            record.effect_status,
            progress_json,
            result_json,
            error_json,
            now,
        ],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// Test/未来 `operations.get` 投影用的读取面:今天只由本域测试消费,保持
/// cfg(test) 以免非测试构建出现死代码;operations.* 域落地后由其 owner
/// 移出该门控(缝点,见任务报告)。
#[cfg(test)]
fn read_operation(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<OperationRecord>, ContractError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        String,
        String,
        String,
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT operation_type, scope_json, state, revision, cancel_requested, effect_status,
                progress_json, result_json, error_json
             FROM v1_config_operations WHERE operation_id = ?1",
            params![operation_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )
        .optional()
        .map_err(storage_err_from)?;
    Ok(row.map(
        |(
            operation_type,
            scope_json,
            state,
            revision,
            cancel_requested,
            effect_status,
            progress_json,
            result_json,
            error_json,
        )| OperationRecord {
            operation_id: operation_id.to_owned(),
            operation_type,
            scope_json: serde_json::from_str(&scope_json).unwrap_or(Value::Null),
            state,
            revision,
            cancel_requested: cancel_requested != 0,
            effect_status,
            progress: serde_json::from_str(&progress_json).unwrap_or_default(),
            result: result_json.and_then(|s| serde_json::from_str(&s).ok()),
            error: error_json.and_then(|s| serde_json::from_str(&s).ok()),
        },
    ))
}

/// Idempotency bookkeeping for `configuration.reload`, mirroring the
/// `run.submit` record shape so `requests.lookup` serves both.
pub(super) struct IdempotencyRecord {
    pub(super) input_digest: String,
    pub(super) request_state: String,
    pub(super) receipt_json: Option<String>,
    pub(super) rejection_code: Option<String>,
}

pub(super) fn read_idempotency(
    conn: &Connection,
    key: &str,
) -> Result<Option<IdempotencyRecord>, ContractError> {
    let row: Option<(String, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT input_digest, request_state, receipt_json, rejection_code
             FROM idempotency_records
             WHERE idempotency_key = ?1 AND method_id = 'configuration.reload'",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(storage_err_from)?;
    Ok(row.map(
        |(input_digest, request_state, receipt_json, rejection_code)| IdempotencyRecord {
            input_digest,
            request_state,
            receipt_json,
            rejection_code,
        },
    ))
}

/// Records a definitively rejected submission (e.g. REVISION_CONFLICT) so a
/// replay of the same key answers the same rejection instead of re-executing.
pub(super) fn record_rejection(
    conn: &Connection,
    key: &str,
    digest: &str,
    rejection_code: &str,
    now: &str,
) -> Result<(), ContractError> {
    let retained_until = retention_expiry_rfc3339(now);
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest,
            request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, 'configuration.reload', ?2, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?3, ?4, ?5, 'correctRejectedInputWithNewKey')",
        params![key, digest, rejection_code, now, retained_until],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// Records an accepted submission with its receipt.
pub(super) fn record_acceptance(
    conn: &Connection,
    key: &str,
    digest: &str,
    resource: &Value,
    receipt: &Value,
    now: &str,
) -> Result<(), ContractError> {
    let retained_until = retention_expiry_rfc3339(now);
    conn.execute(
        "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest,
            request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, 'configuration.reload', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
        params![
            key,
            digest,
            serde_json::to_string(resource).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(receipt).unwrap_or_else(|_| "null".into()),
            now,
            retained_until,
        ],
    )
    .map_err(storage_err_from)?;
    Ok(())
}

/// The store's retention window (24h), computed the same way
/// `store.rs::retention_expiry_rfc3339` does (that helper is private; the
/// arithmetic is reproduced here). A `match` (not `map().unwrap_or_else`)
/// keeps no `Result` closure involved at all.
fn retention_expiry_rfc3339(now: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(now) {
        Ok(parsed) => (parsed + chrono::Duration::hours(24)).to_rfc3339(),
        Err(_) => now.to_owned(),
    }
}

/// Canonical JSON for digests: object keys sorted at every level, no
/// whitespace — the same normalizing rules the protocol's
/// `compute_input_digest` applies.
pub(super) fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into()),
                        canonical_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".into()),
    }
}
