//! `schedules.*` — the v1 scheduling domain, layered over the ported cron
//! engine (`crate::runtime::cron`, the TS-baseline port).
//!
//! Architecture (t019 T1, brief `_tmp/20261118-s3/briefs/channels-schedules.md` §1):
//!
//! * The v1 schedule identity (`scheduleId`, caller-chosen), the per-schedule
//!   `enabled` flag, and the v1 string `revision` live in domain-owned SQLite
//!   tables (`v1_schedules`, `v1_schedule_executions`, `v1_domain_operations`)
//!   inside the shared `<home>/v1/state.sqlite`. The old line has none of
//!   these concepts (brief §1 gaps 1–3), so the mapping lives here.
//! * The firing machinery is the ported engine, constructed with
//!   `CronManagerOptions::pilot_home` pointed at the v1 dataRoot:
//!   `CronManager`/`CronRuntime` own the task store
//!   (`<home>/cron/projects/<id>/tasks.json`), the schedule computation, the
//!   scheduler thread, and the fire path. Nothing inside `runtime/cron` is
//!   modified.
//! * Scheduled fires run the full old-line path (scheduler tick → `CronFire`
//!   claim → gateway), and the v1 bridge (`V1CronGateway`) turns one fired
//!   task into exactly one v1 run through the S2 admission machine
//!   (`V1Store::submit_run` + `start_session_worker`). A busy session is
//!   rejected with the old-line admission-fence code `session_busy`
//!   (`SessionAdmissionCronGateway` semantics preserved), which surfaces as
//!   `RESOURCE_BUSY` for explicit triggers.
//! * `schedules.trigger` does not clone the task (the old `runTaskNow`
//!   semantics); it wraps one execution: the executionId *is* the cron
//!   `runId` handed to a private `CronFire` instance, so the claim/post-run
//!   bookkeeping and `stop_execution` keep working against the same
//!   identifiers.
//! * Domain state is a per-dataRoot singleton (`domain_for`); the registry
//!   holds it strongly so the scheduler thread outlives the dispatch that
//!   started it (the gateway process owns the scheduling loop). The loop
//!   starts both at gateway startup (`serve_v1_gateway` calls
//!   [`ensure_started`]) and lazily on the first `schedules.*` call.
//!
//! Registered v1 choices (contract does not specify them; recorded in the
//! task report too):
//!
//! * Old-line project key: `/<v1 projectId>` (absolute, cwd-stable, and its
//!   `metadata::project_id` sanitization is exactly the v1 id, so the
//!   on-disk task store directory is the v1 id).
//! * Missed triggers: gateway downtime keeps the old-line behavior (a due
//!   task fires on the next tick); a disabled period skips missed
//!   occurrences (re-enabling re-arms from now, in the recorded timezone).
//! * `trigger` on a disabled schedule rejects with `INVALID_ARGUMENT`; a
//!   sessionless schedule accepts but every execution fails to start
//!   (`NO_TARGET_SESSION`, `runId: null`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::{ResponseOutcome, RunState};
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetryMode, RetrySpec,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::StoredRun;
use crate::runtime::cron::{
    ComputedRunAt, CronAgentGateway, CronCreateSchedule, CronError, CronFire, CronFireDeps,
    CronManager, CronManagerConfig, CronManagerOptions, CronTaskStore, CronTurnEventSink,
    CronTurnSubmit, CronZoneResolverCache, NoopCronLogger, compute_next_run_at,
    is_valid_cron_timezone, resolve_cron_paths,
};

/// The old-line project key for a v1 project id. It must be absolute
/// (`requireProjectKey` resolves relative keys against the process cwd, so a
/// bare v1 id would key different `cron/projects/<dir>` stores per cwd) and
/// its `metadata::project_id` sanitization must be exactly the v1 id, which
/// the v1 id charset (`$defs.Id`: `[A-Za-z0-9._~-]`) guarantees.
pub(crate) fn cron_project_key(project_id: &str) -> String {
    format!("/{project_id}")
}

/// The v1-side revision prefix (`srev-…`). Deliberately distinct from the
/// core store's `rev-…` space: schedule revisions only order within this
/// domain.
const REV_PREFIX: &str = "srev-";

const DDL_SCHEDULES: &str = "CREATE TABLE IF NOT EXISTS v1_schedules (
    schedule_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL DEFAULT '',
    project_id TEXT NOT NULL,
    session_id TEXT,
    kind TEXT NOT NULL,
    expression TEXT NOT NULL,
    timezone TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    message TEXT NOT NULL,
    channel_id TEXT,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

const DDL_EXECUTIONS: &str = "CREATE TABLE IF NOT EXISTS v1_schedule_executions (
    execution_id TEXT PRIMARY KEY,
    schedule_id TEXT NOT NULL,
    run_id TEXT,
    origin TEXT NOT NULL,
    scheduled_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    state TEXT NOT NULL,
    error_code TEXT
);";

/// Shared with `background_tasks.rs`: the async-operation records the
/// `retry=idempotentSubmit` operations of both domains accept against.
pub(crate) const DDL_DOMAIN_OPERATIONS: &str = "CREATE TABLE IF NOT EXISTS v1_domain_operations (
    operation_id TEXT PRIMARY KEY,
    method_id TEXT NOT NULL,
    idempotency_key TEXT,
    input_digest TEXT NOT NULL,
    state TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    completion_json TEXT,
    rejection_code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

const DDL_META: &str = "CREATE TABLE IF NOT EXISTS v1_schedules_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_schedules", DDL_SCHEDULES);
    reg.schema("v1_schedule_executions", DDL_EXECUTIONS);
    reg.schema("v1_domain_operations", DDL_DOMAIN_OPERATIONS);
    reg.schema("v1_schedules_meta", DDL_META);
    reg.handler("schedules.list", |rt, input, _key, _rev| {
        Box::pin(schedules_list(rt, input))
    });
    reg.handler("schedules.history", |rt, input, _key, _rev| {
        Box::pin(schedules_history(rt, input))
    });
    reg.handler("schedules.configure", |rt, input, _key, _rev| {
        Box::pin(schedules_configure(rt, input))
    });
    reg.handler("schedules.set_enabled", |rt, input, _key, _rev| {
        Box::pin(schedules_set_enabled(rt, input))
    });
    reg.handler("schedules.remove", |rt, input, key, _rev| {
        Box::pin(schedules_remove(rt, input, key))
    });
    reg.handler("schedules.trigger", |rt, input, key, _rev| {
        Box::pin(schedules_trigger(rt, input, key))
    });
    reg.handler("schedules.stop_execution", |rt, input, key, _rev| {
        Box::pin(schedules_stop_execution(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Storage (domain-owned tables in the shared state DB)
// ---------------------------------------------------------------------------

struct ScheduleRow {
    schedule_id: String,
    task_id: String,
    project_id: String,
    session_id: Option<String>,
    kind: String,
    expression: String,
    timezone: String,
    enabled: bool,
    message: String,
    channel_id: Option<String>,
    revision: String,
}

#[derive(Clone)]
struct ExecutionRow {
    execution_id: String,
    schedule_id: String,
    run_id: Option<String>,
    state: String,
    error_code: Option<String>,
    scheduled_at: String,
    started_at: Option<String>,
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

fn invalid_arg(message: impl Into<String>) -> ContractError {
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

fn revision_conflict(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

/// The contract's `RESOURCE_BUSY` (no protocol constant exists for it yet).
fn resource_busy(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "RESOURCE_BUSY",
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn cron_to_contract(error: CronError) -> ContractError {
    let message = String::from_utf16_lossy(error.message().units());
    // The ported engine's input-validation rejections (the CronRuntime.ts
    // message texts) map to INVALID_ARGUMENT; everything else is a storage
    // or wiring failure.
    if message.contains("must be scheduled in the future")
        || message.contains("does not produce a valid future run time")
        || message.contains("Invalid Cron timezone")
    {
        invalid_arg(message)
    } else {
        ContractError::new(
            CODE_UNAVAILABLE,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    }
}

/// The domain's SQLite handle: a second connection to the shared
/// `<home>/v1/state.sqlite` (WAL). The core store keeps its own connection;
/// both use `busy_timeout`, and this domain's transactions are short.
pub(crate) struct DomainDb {
    conn: Mutex<Connection>,
}

impl DomainDb {
    pub(crate) fn open(
        home: &std::path::Path,
        extra_ddl: &[&'static str],
    ) -> Result<Self, ContractError> {
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
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;",
        )
        .map_err(|e| sql_err("pragma v1 state db", e))?;
        for ddl in extra_ddl {
            conn.execute_batch(ddl)
                .map_err(|e| sql_err("domain ddl", e))?;
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

    /// The next domain revision (`srev-…`), from the domain's own counter.
    fn next_revision(&self) -> Result<String, ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(|e| sql_err("begin rev", e))?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM v1_schedules_meta WHERE key = 'rev_counter'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_err("read rev counter", e))?;
        let next = current.unwrap_or(0) + 1;
        tx.execute(
            "INSERT INTO v1_schedules_meta (key, value) VALUES ('rev_counter', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![next.to_string()],
        )
        .map_err(|e| sql_err("write rev counter", e))?;
        tx.commit().map_err(|e| sql_err("commit rev", e))?;
        Ok(format!("{REV_PREFIX}{next:08}"))
    }

    fn current_revision(&self) -> Result<String, ContractError> {
        self.with_conn("read revision", |conn| {
            let current: Option<String> = conn
                .query_row(
                    "SELECT value FROM v1_schedules_meta WHERE key = 'rev_counter'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(current.unwrap_or_else(|| format!("{REV_PREFIX}00000000")))
        })
    }

    fn shared_store_revision(&self) -> Result<String, ContractError> {
        self.with_conn("read store revision", |conn| {
            let has_meta: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
                [],
                |r| r.get(0),
            )?;
            if has_meta {
                let current: Option<i64> = conn
                    .query_row(
                        "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(n) = current {
                    return Ok(format!("rev-{n:08}"));
                }
            }
            let current: Option<String> = conn
                .query_row(
                    "SELECT value FROM v1_schedules_meta WHERE key = 'rev_counter'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(current.unwrap_or_else(|| format!("{REV_PREFIX}00000000")))
        })
    }

    // ---- schedules -------------------------------------------------------

    fn schedule(&self, schedule_id: &str) -> Result<Option<ScheduleRow>, ContractError> {
        self.with_conn("read schedule", |conn| {
            conn.query_row(
                "SELECT schedule_id, task_id, project_id, session_id, kind, expression,
                        timezone, enabled, message, channel_id, revision
                 FROM v1_schedules WHERE schedule_id = ?1",
                params![schedule_id],
                row_to_schedule,
            )
            .optional()
        })
    }

    fn schedule_by_task(&self, task_id: &str) -> Result<Option<ScheduleRow>, ContractError> {
        self.with_conn("read schedule by task", |conn| {
            conn.query_row(
                "SELECT schedule_id, task_id, project_id, session_id, kind, expression,
                        timezone, enabled, message, channel_id, revision
                 FROM v1_schedules WHERE task_id = ?1",
                params![task_id],
                row_to_schedule,
            )
            .optional()
        })
    }

    fn schedules(&self, project_id: Option<&str>) -> Result<Vec<ScheduleRow>, ContractError> {
        self.with_conn("list schedules", |conn| {
            let mut stmt = conn.prepare(
                "SELECT schedule_id, task_id, project_id, session_id, kind, expression,
                        timezone, enabled, message, channel_id, revision
                 FROM v1_schedules
                 WHERE (?1 IS NULL OR project_id = ?1)
                 ORDER BY created_at ASC, schedule_id ASC",
            )?;
            let rows = stmt
                .query_map(params![project_id], row_to_schedule)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    fn upsert_schedule_cas(
        &self,
        row: &ScheduleRow,
        expected_revision: Option<&str>,
    ) -> Result<bool, ContractError> {
        self.with_conn("write schedule", |conn| {
            let now = now_rfc3339();
            let changed = match expected_revision {
                None => conn.execute(
                    "INSERT INTO v1_schedules (schedule_id, task_id, project_id, session_id, kind,
                        expression, timezone, enabled, message, channel_id, revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
                     ON CONFLICT(schedule_id) DO NOTHING",
                    params![
                        row.schedule_id,
                        row.task_id,
                        row.project_id,
                        row.session_id,
                        row.kind,
                        row.expression,
                        row.timezone,
                        if row.enabled { 1 } else { 0 },
                        row.message,
                        row.channel_id,
                        row.revision,
                        now,
                    ],
                )?,
                Some(expected) => conn.execute(
                    "UPDATE v1_schedules
                     SET task_id = ?2, project_id = ?3, session_id = ?4, kind = ?5,
                         expression = ?6, timezone = ?7, enabled = ?8, message = ?9,
                         channel_id = ?10, revision = ?11, updated_at = ?12
                     WHERE schedule_id = ?1 AND revision = ?13",
                    params![
                        row.schedule_id,
                        row.task_id,
                        row.project_id,
                        row.session_id,
                        row.kind,
                        row.expression,
                        row.timezone,
                        if row.enabled { 1 } else { 0 },
                        row.message,
                        row.channel_id,
                        row.revision,
                        now,
                        expected,
                    ],
                )?,
            };
            Ok(changed == 1)
        })
    }

    fn delete_schedule_cas(
        &self,
        schedule_id: &str,
        expected_revision: &str,
    ) -> Result<bool, ContractError> {
        self.with_conn("delete schedule", |conn| {
            let changed = conn.execute(
                "DELETE FROM v1_schedules WHERE schedule_id = ?1 AND revision = ?2",
                params![schedule_id, expected_revision],
            )?;
            Ok(changed == 1)
        })
    }

    // ---- executions ------------------------------------------------------

    /// Creates the execution row unless it already exists (the trigger path
    /// pre-creates with `origin=trigger`; the bridge arrives afterwards).
    fn begin_execution(
        &self,
        schedule_id: &str,
        execution_id: &str,
        origin: &str,
    ) -> Result<(), ContractError> {
        self.with_conn("begin execution", |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO v1_schedule_executions
                    (execution_id, schedule_id, run_id, origin, scheduled_at, started_at, finished_at, state, error_code)
                 VALUES (?1, ?2, NULL, ?3, ?4, NULL, NULL, 'pending', NULL)",
                params![execution_id, schedule_id, origin, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    fn execution(&self, execution_id: &str) -> Result<Option<ExecutionRow>, ContractError> {
        self.with_conn("read execution", |conn| {
            conn.query_row(
                "SELECT execution_id, schedule_id, run_id, state, error_code, scheduled_at, started_at
                 FROM v1_schedule_executions WHERE execution_id = ?1",
                params![execution_id],
                row_to_execution,
            )
            .optional()
        })
    }

    fn executions_for(&self, schedule_id: &str) -> Result<Vec<ExecutionRow>, ContractError> {
        self.with_conn("list executions", |conn| {
            let mut stmt = conn.prepare(
                "SELECT execution_id, schedule_id, run_id, state, error_code, scheduled_at, started_at
                 FROM v1_schedule_executions
                 WHERE schedule_id = ?1
                 ORDER BY scheduled_at DESC, rowid DESC",
            )?;
            let rows = stmt
                .query_map(params![schedule_id], row_to_execution)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Non-terminal executions of one schedule (the `set_enabled` / `remove`
    /// in-flight view).
    fn running_executions(&self, schedule_id: &str) -> Result<Vec<ExecutionRow>, ContractError> {
        Ok(self
            .executions_for(schedule_id)?
            .into_iter()
            .filter(|exec| exec.state == "pending" || exec.state == "running")
            .collect())
    }

    fn mark_execution_running(
        &self,
        execution_id: &str,
        run_id: &str,
    ) -> Result<(), ContractError> {
        self.with_conn("mark execution running", |conn| {
            conn.execute(
                "UPDATE v1_schedule_executions
                 SET run_id = ?2, started_at = COALESCE(started_at, ?3), state = 'running'
                 WHERE execution_id = ?1",
                params![execution_id, run_id, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// Terminalizes a pending/running execution; a row already terminal (a
    /// racing `stop_execution` won) is left untouched.
    fn finish_execution(
        &self,
        execution_id: &str,
        state: &str,
        error_code: Option<&str>,
    ) -> Result<(), ContractError> {
        self.with_conn("finish execution", |conn| {
            conn.execute(
                "UPDATE v1_schedule_executions
                 SET state = ?2, error_code = ?3, finished_at = ?4
                 WHERE execution_id = ?1 AND state IN ('pending', 'running')",
                params![execution_id, state, error_code, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// Crash recovery: executions still pending/running at domain creation
    /// belong to a previous gateway process (the core store already
    /// interrupted their runs); they terminalize as `interrupted`.
    fn recover_orphaned_executions(&self) -> Result<(), ContractError> {
        self.with_conn("recover executions", |conn| {
            conn.execute(
                "UPDATE v1_schedule_executions
                 SET state = 'interrupted', error_code = 'GATEWAY_RESTART', finished_at = ?1
                 WHERE state IN ('pending', 'running')",
                params![now_rfc3339()],
            )?;
            Ok(())
        })
    }

    // ---- project/session scope checks (shared core tables) ---------------

    fn project_exists(&self, project_id: &str) -> Result<bool, ContractError> {
        self.with_conn("check project", |conn| {
            let found: Option<String> = conn
                .query_row(
                    "SELECT project_id FROM projects WHERE project_id = ?1",
                    params![project_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(found.is_some())
        })
    }

    fn session_belongs_to_project(
        &self,
        session_id: &str,
        project_id: &str,
    ) -> Result<bool, ContractError> {
        self.with_conn("check session", |conn| {
            let found: Option<String> = conn
                .query_row(
                    "SELECT project_id FROM sessions WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(found.as_deref() == Some(project_id))
        })
    }

    /// The S2 admission-fence view: the session has an active run or queued
    /// runs.
    fn session_active_or_queued(&self, session_id: &str) -> Result<bool, ContractError> {
        self.with_conn("check session busy", |conn| {
            let active: Option<Option<String>> = conn
                .query_row(
                    "SELECT active_run_id FROM sessions WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(active) = active else {
                return Ok(false);
            };
            // rb7 P1（t010 后续）：pin 指向终态（或缺失）run 时不算忙——与
            // `store.rs::pinned_run_is_live`（submit_run / run.continue /
            // resume_queue 同款）统一判定，否则 stale pin 让会话被永久
            // 判忙，定时触发一直 RESOURCE_BUSY。
            let active_run_live = match active {
                Some(run_id) => {
                    crate::public_v1::store::pinned_run_is_live(conn, Some(run_id.as_str()))?
                }
                None => false,
            };
            if active_run_live {
                return Ok(true);
            }
            let queued: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
                params![session_id],
                |r| r.get(0),
            )?;
            Ok(queued != 0)
        })
    }

    // ---- domain events (durable, in the shared events table) -------------

    /// The same placeholder→`cur-<seq>` pattern the core store uses
    /// (`store.rs::append_event_in_tx`), through this domain's connection.
    pub(super) fn append_event(
        &self,
        resource_id: &str,
        session_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), ContractError> {
        self.with_conn("append domain event", |conn| {
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

fn row_to_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleRow> {
    Ok(ScheduleRow {
        schedule_id: row.get(0)?,
        task_id: row.get(1)?,
        project_id: row.get(2)?,
        session_id: row.get(3)?,
        kind: row.get(4)?,
        expression: row.get(5)?,
        timezone: row.get(6)?,
        enabled: row.get::<_, i64>(7)? != 0,
        message: row.get(8)?,
        channel_id: row.get(9)?,
        revision: row.get(10)?,
    })
}

fn row_to_execution(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExecutionRow> {
    Ok(ExecutionRow {
        execution_id: row.get(0)?,
        schedule_id: row.get(1)?,
        run_id: row.get(2)?,
        state: row.get(3)?,
        error_code: row.get(4)?,
        scheduled_at: row.get(5)?,
        started_at: row.get(6)?,
    })
}

// ---------------------------------------------------------------------------
// Async-operation records (shared with background_tasks.rs)
// ---------------------------------------------------------------------------

/// What an idempotency lookup found for a `retry=idempotentSubmit` operation.
pub(crate) enum DomainOperationLookup {
    /// No record: proceed with validation and accept.
    Fresh,
    /// A previously accepted operation: replay its receipt.
    Replay(Value),
    /// The key was already used with different input.
    Conflict,
    /// The key was already used and the request was definitively rejected.
    Rejected(String),
}

pub(crate) struct AcceptedDomainOperation {
    pub(crate) operation_id: String,
    pub(crate) receipt: Value,
}

/// Idempotency gate for a `retry=idempotentSubmit` domain operation: the same
/// `idempotencyKey` with identical input replays the stored receipt; with
/// different input it answers `IDEMPOTENCY_CONFLICT`; a definitive
/// rejection replays its rejection code.
pub(crate) fn lookup_domain_operation(
    db: &DomainDb,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
) -> Result<DomainOperationLookup, ContractError> {
    let Some(key) = idempotency_key else {
        return Ok(DomainOperationLookup::Fresh);
    };
    let digest = compute_input_digest(method_id, input);
    let existing = db.with_conn("lookup operation", |conn| {
        conn.query_row(
            "SELECT input_digest, receipt_json, rejection_code FROM v1_domain_operations
             WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, method_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
    })?;
    match existing {
        None => Ok(DomainOperationLookup::Fresh),
        Some((stored_digest, receipt_json, rejection)) => {
            if stored_digest != digest {
                return Ok(DomainOperationLookup::Conflict);
            }
            if let Some(code) = rejection {
                return Ok(DomainOperationLookup::Rejected(code));
            }
            let mut receipt: Value =
                serde_json::from_str(&receipt_json).unwrap_or_else(|_| json!({}));
            if let Some(obj) = receipt.as_object_mut() {
                obj.insert("deduplicated".into(), json!(true));
            }
            Ok(DomainOperationLookup::Replay(receipt))
        }
    }
}

/// Accepts (records) one operation after its pre-accept validation passed.
pub(crate) fn record_domain_operation_accept(
    db: &DomainDb,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    operation_id: &str,
    receipt: &Value,
) -> Result<(), ContractError> {
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    db.with_conn("insert operation", |conn| {
        conn.execute(
            "INSERT INTO v1_domain_operations
                (operation_id, method_id, idempotency_key, input_digest, state, receipt_json,
                 completion_json, rejection_code, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, NULL, NULL, ?6, ?6)",
            params![
                operation_id,
                method_id,
                idempotency_key,
                digest,
                serde_json::to_string(receipt).unwrap_or_else(|_| "{}".into()),
                now,
            ],
        )?;
        Ok(())
    })
}

/// Records a definitive pre-accept rejection under the caller's key (only
/// when one was supplied), so a retry of the same key + input replays the
/// rejection instead of silently re-validating.
pub(crate) fn record_domain_operation_rejection(
    db: &DomainDb,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    code: &str,
) -> Result<(), ContractError> {
    let Some(key) = idempotency_key else {
        return Ok(());
    };
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    db.with_conn("record operation rejection", |conn| {
        conn.execute(
            "INSERT OR REPLACE INTO v1_domain_operations
                (operation_id, method_id, idempotency_key, input_digest, state, receipt_json,
                 completion_json, rejection_code, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'rejected', '{}', NULL, ?5, ?6, ?6)",
            params![
                // Method-scoped so one key rejecting under two different
                // operations does not clobber the other's record.
                format!("rejected-{method_id}-{key}"),
                method_id,
                key,
                digest,
                code,
                now,
            ],
        )?;
        Ok(())
    })
}

/// Marks the operation running (with an `operation.started` event).
pub(crate) fn mark_domain_operation_running(
    db: &DomainDb,
    operation_id: &str,
    session_id: &str,
) -> Result<(), ContractError> {
    db.with_conn("mark operation running", |conn| {
        conn.execute(
            "UPDATE v1_domain_operations SET state = 'running', updated_at = ?2 WHERE operation_id = ?1",
            params![operation_id, now_rfc3339()],
        )?;
        Ok(())
    })?;
    db.append_event(
        operation_id,
        session_id,
        "operation.started",
        &json!({"operationId": operation_id}),
    )
}

/// Terminalizes the operation (with `operation.completed` /
/// `operation.failed`).
pub(crate) fn finish_domain_operation(
    db: &DomainDb,
    operation_id: &str,
    session_id: &str,
    state: &str,
    completion: &Value,
) -> Result<(), ContractError> {
    db.with_conn("finish operation", |conn| {
        conn.execute(
            "UPDATE v1_domain_operations
             SET state = ?2, completion_json = ?3, updated_at = ?4
             WHERE operation_id = ?1",
            params![
                operation_id,
                state,
                serde_json::to_string(completion).unwrap_or_else(|_| "{}".into()),
                now_rfc3339(),
            ],
        )?;
        Ok(())
    })?;
    let event = if state == "succeeded" {
        "operation.completed"
    } else {
        "operation.failed"
    };
    db.append_event(
        operation_id,
        session_id,
        event,
        &json!({"operationId": operation_id, "result": completion}),
    )
}

/// The shared accept flow of the domain's async operations: idempotency
/// lookup first (a replay answers with the stored receipt even if the
/// world has moved on), then pre-accept validation. A failed validation
/// records the rejection under the caller's key only when it is definitive
/// (`retry: never`); retryable rejections (e.g. `RESOURCE_BUSY`) must not
/// pin the key to a stale failure.
pub(crate) fn accept_domain_operation(
    db: &DomainDb,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    new_operation_id: impl FnOnce() -> String,
    validate: impl FnOnce() -> Result<(), ContractError>,
) -> Result<AcceptedDomainOperation, ContractError> {
    match lookup_domain_operation(db, method_id, input, idempotency_key)? {
        DomainOperationLookup::Replay(receipt) => {
            return Ok(AcceptedDomainOperation {
                operation_id: receipt["operationId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                receipt,
            });
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
        "acceptedRevision": db.shared_store_revision()?,
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
    Ok(AcceptedDomainOperation {
        operation_id,
        receipt,
    })
}

// ---------------------------------------------------------------------------
// The per-dataRoot domain singleton
// ---------------------------------------------------------------------------

pub(crate) struct SchedulesDomain {
    home: PathBuf,
    rt: Arc<PublicV1Runtime>,
    handle: tokio::runtime::Handle,
    manager: Arc<CronManager>,
    gateway: Arc<V1CronGateway>,
    pub(crate) db: DomainDb,
    schedule_locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

fn domains_registry() -> &'static Mutex<HashMap<PathBuf, Arc<SchedulesDomain>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<SchedulesDomain>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn domain_for(rt: &Arc<PublicV1Runtime>) -> Result<Arc<SchedulesDomain>, ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&home) {
        return Ok(existing.clone());
    }
    let domain = Arc::new(SchedulesDomain::new(rt.clone())?);
    // Link the gateway's weak self-reference now that the Arc exists.
    *domain
        .gateway
        .domain
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&domain);
    domain
        .manager
        .bind_agent_gateway(domain.gateway.clone() as Arc<dyn CronAgentGateway>)
        .map_err(cron_to_contract)?;
    domain.manager.start().map_err(cron_to_contract)?;
    // The registry holds the ONLY strong reference on purpose: the scheduler
    // thread and every in-flight fire keep the CronRuntime alive through
    // `SchedulerDeps::runtime` (a Weak), so with only Weak handles here the
    // domain — and with it the scheduler thread — would be dropped the moment
    // the dispatch that started it returns, and no later idle poll would
    // ever run. A strong Arc here ties the scheduling machinery to the
    // process lifetime, exactly what a background scheduler needs (the
    // gateway owns this state for its whole run; `shutdown_for_tests` is the
    // one deliberate teardown path).
    registry.insert(home, domain.clone());
    Ok(domain)
}

/// Integrator hook: starts the scheduling machinery (engine recovery +
/// scheduler loop) for a runtime's dataRoot. The standalone gateway's
/// `serve_v1_gateway` calls this at startup so schedules configured before a
/// restart resume firing without waiting for the first `schedules.*` call;
/// the domain also starts lazily on the first dispatch.
pub fn ensure_started(rt: &Arc<PublicV1Runtime>) -> Result<(), ContractError> {
    domain_for(rt).map(|_| ())
}

/// Stops the scheduling machinery for one dataRoot and forgets the domain
/// (the scheduler threads are joined). Test hygiene; production shutdown
/// ends the process instead.
#[cfg(test)]
pub(crate) fn shutdown_for_tests(home: &std::path::Path) {
    let domain = domains_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(home);
    if let Some(domain) = domain {
        let _ = domain.manager.stop();
    }
}

impl SchedulesDomain {
    fn new(rt: Arc<PublicV1Runtime>) -> Result<Self, ContractError> {
        let home = rt.store().home().to_path_buf();
        let db = DomainDb::open(
            &home,
            &[
                DDL_SCHEDULES,
                DDL_EXECUTIONS,
                DDL_DOMAIN_OPERATIONS,
                DDL_META,
            ],
        )?;
        db.recover_orphaned_executions()?;
        let manager = CronManager::new(CronManagerOptions {
            config: CronManagerConfig::default(),
            pilot_home: home.to_string_lossy().into_owned(),
            session_overrides: None,
            now: None,
            uuid: None,
            logger: None,
            scheduler_idle_poll_ms: None,
        })
        .map_err(cron_to_contract)?;
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            ContractError::new(
                CODE_UNAVAILABLE,
                "the schedules domain requires a tokio runtime",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        Ok(Self {
            home,
            rt,
            handle,
            manager,
            gateway: Arc::new(V1CronGateway {
                domain: Mutex::new(Weak::new()),
            }),
            db,
            schedule_locks: Mutex::new(HashMap::new()),
        })
    }

    async fn lock_schedule(&self, schedule_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .schedule_locks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            locks
                .get(schedule_id)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(schedule_id.to_owned(), Arc::downgrade(&lock));
                    lock
                })
        };
        lock.lock_owned().await
    }

    /// `CronManagerConfig::default().run_timeout_minutes` (60) is private to
    /// the manager; the mirror keeps the bridge and trigger fire on the same
    /// bound as the engine's own fires.
    fn manager_run_timeout_minutes(&self) -> f64 {
        60.0
    }

    fn run_timeout(&self) -> Duration {
        Duration::from_millis((self.manager_run_timeout_minutes() * 60_000.0) as u64)
    }

    /// The old-line task store of one v1 project (read side for
    /// `nextRunAt`/status; the same file the runtimes mutate).
    fn task_store(&self, project_id: &str) -> Result<CronTaskStore, ContractError> {
        let key = cron_project_key(project_id);
        let home = self.home.to_string_lossy().into_owned();
        resolve_cron_paths(&home, &key)
            .map(CronTaskStore::new)
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("resolve cron paths for `{project_id}`: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })
    }

    /// The current `nextRunAt` of the old-line task behind a schedule row
    /// (`None` when disabled, task-less, or already fired-and-deleted).
    fn task_next_run_at(
        &self,
        row: &ScheduleRow,
        cache: &mut HashMap<String, HashMap<String, String>>,
    ) -> Option<String> {
        if !row.enabled || row.task_id.is_empty() {
            return None;
        }
        let tasks = cache.entry(row.project_id.clone()).or_insert_with(|| {
            match self.task_store(&row.project_id) {
                Ok(store) => store
                    .list_tasks()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|task| {
                        let id = task
                            .get("taskId")
                            .and_then(JsValue::as_string)?
                            .to_utf8()
                            .ok()?;
                        let next = task
                            .get("nextRunAt")
                            .and_then(JsValue::as_string)
                            .map(|text| String::from_utf16_lossy(text.units()));
                        Some((id, next?))
                    })
                    .collect(),
                Err(_) => HashMap::new(),
            }
        });
        tasks.get(&row.task_id).cloned()
    }

    fn schedule_json(&self, row: &ScheduleRow, next_run_at: Option<String>) -> Value {
        json!({
            "scheduleId": row.schedule_id,
            "projectId": row.project_id,
            "sessionId": row.session_id,
            "kind": row.kind,
            "expression": row.expression,
            "timezone": row.timezone,
            "enabled": row.enabled,
            "nextRunAt": next_run_at,
            "revision": row.revision,
        })
    }

    /// Deletes the old-line task of a row (a missing task is a no-op, e.g. a
    /// fired once task the engine already deleted).
    fn delete_old_task(&self, row: &ScheduleRow) -> Result<(), ContractError> {
        if row.task_id.is_empty() {
            return Ok(());
        }
        let mut input = JsObject::new();
        input.insert(
            "taskId".into(),
            JsValue::String(JsString::from(row.task_id.as_str())),
        );
        input.insert(
            "projectKey".into(),
            JsValue::String(JsString::from(cron_project_key(&row.project_id))),
        );
        self.manager
            .delete_task(&JsValue::Object(input))
            .map_err(cron_to_contract)?;
        Ok(())
    }

    /// Creates (or re-creates) the old-line task for a schedule row and
    /// returns its taskId. The v1 `expression` maps `once`→`schedule.runAt`
    /// and `cron`→`schedule.expression`.
    fn create_old_task(&self, row: &ScheduleRow) -> Result<String, ContractError> {
        let mut schedule = JsObject::new();
        match row.kind.as_str() {
            "once" => {
                schedule.insert("type".into(), JsValue::from("once"));
                schedule.insert(
                    "runAt".into(),
                    JsValue::String(JsString::from(row.expression.as_str())),
                );
            }
            _ => {
                schedule.insert("type".into(), JsValue::from("cron"));
                schedule.insert(
                    "expression".into(),
                    JsValue::String(JsString::from(row.expression.as_str())),
                );
                schedule.insert(
                    "timezone".into(),
                    JsValue::String(JsString::from(row.timezone.as_str())),
                );
            }
        }
        let mut input = JsObject::new();
        input.insert(
            "projectKey".into(),
            JsValue::String(JsString::from(cron_project_key(&row.project_id))),
        );
        input.insert(
            "message".into(),
            JsValue::String(JsString::from(row.message.as_str())),
        );
        input.insert("schedule".into(), JsValue::Object(schedule));
        input.insert(
            "timezone".into(),
            JsValue::String(JsString::from(row.timezone.as_str())),
        );
        if let Some(session_id) = &row.session_id {
            input.insert(
                "sessionKey".into(),
                JsValue::String(JsString::from(session_id.as_str())),
            );
        }
        if let Some(channel_id) = &row.channel_id {
            input.insert(
                "channelKey".into(),
                JsValue::String(JsString::from(channel_id.as_str())),
            );
        }
        let created = self
            .manager
            .create_task(&JsValue::Object(input))
            .map_err(cron_to_contract)?;
        let task_id = created
            .get("task")
            .and_then(|task| task.get("taskId"))
            .and_then(JsValue::as_string)
            .map(|id| String::from_utf16_lossy(id.units()))
            .unwrap_or_default();
        if task_id.is_empty() {
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                "cron engine created a task without a taskId",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Ok(task_id)
    }

    // ------------------------------------------------------------------
    // The execution engine: one schedule execution = one v1 run.
    // ------------------------------------------------------------------

    /// Submits the schedule's message as a v1 run in the target session and
    /// waits for its terminal state (bounded by the cron run timeout).
    /// Returns `(state, error_code, run_id)`.
    async fn execute_schedule_run(
        self: &Arc<Self>,
        row: &ScheduleRow,
        execution_id: &str,
    ) -> (String, Option<String>, Option<String>) {
        let Some(session_id) = row.session_id.clone() else {
            return (
                "failed".to_owned(),
                Some("NO_TARGET_SESSION".to_owned()),
                None,
            );
        };
        let input = json!({
            "sessionId": session_id,
            "input": [{"kind": "text", "text": row.message}],
        });
        let run_id = match self.rt.store().submit_run(&input, None) {
            Err(error) => return ("failed".to_owned(), Some(error.code), None),
            Ok(crate::public_v1::store::SubmitDisposition::Deduplicated { .. }) => {
                return (
                    "failed".to_owned(),
                    Some("SUBMIT_DEDUPLICATED".to_owned()),
                    None,
                );
            }
            Ok(crate::public_v1::store::SubmitDisposition::NewRun {
                run_id,
                start_immediately: false,
                ..
            }) => {
                // Admission fence (the `SessionAdmissionCronGateway`
                // `session_busy` semantics): the session already has an
                // active or queued run, so this execution is rejected, not
                // queued. The just-queued run is unwound first.
                let _ = self.rt.store().cancel_run(&run_id, None);
                return ("failed".to_owned(), Some("session_busy".to_owned()), None);
            }
            Ok(crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. }) => run_id,
        };
        let _ = self.db.mark_execution_running(execution_id, &run_id);
        self.rt.start_session_worker(run_id.clone());
        let deadline = Instant::now() + self.run_timeout();
        loop {
            let store = self.rt.store();
            let notifier = store.state_notifier();
            let notified = notifier.notified();
            match self.rt.store().get_run(&run_id) {
                Ok(run) if run.terminal => return terminal_execution_state(&run, run_id),
                Ok(_) => {}
                Err(_) => {
                    return (
                        "failed".to_owned(),
                        Some("RUN_LOOKUP_FAILED".to_owned()),
                        Some(run_id),
                    );
                }
            }
            if Instant::now() >= deadline {
                // The old-line gateway applies `run_timeout_minutes` to the
                // turn; the v1 bridge does the same to the run.
                if let Ok(outcome) = self.rt.store().cancel_run(&run_id, None)
                    && outcome.should_notify_worker
                {
                    self.rt.signal_run_cancel(&run_id);
                }
                return (
                    "failed".to_owned(),
                    Some("turn_timeout".to_owned()),
                    Some(run_id),
                );
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }

    /// Issues one stop for an execution: through the old-line stop path when
    /// the engine has it as an active run (so the fire records `stopped`
    /// and the post-run update runs), otherwise a direct v1 run cancel.
    fn request_execution_stop(&self, row: &ScheduleRow, exec: &ExecutionRow) {
        let key = cron_project_key(&row.project_id);
        if let Some(runtime) = self.manager.loaded_runtime(&key) {
            let cron_run = JsString::from(exec.execution_id.as_str());
            if runtime.active_run(&cron_run).is_some() {
                let mut input = JsObject::new();
                input.insert(
                    "taskId".into(),
                    JsValue::String(JsString::from(row.task_id.as_str())),
                );
                input.insert("runId".into(), JsValue::String(cron_run));
                input.insert("projectKey".into(), JsValue::String(JsString::from(key)));
                let _ = self.manager.stop_task(&JsValue::Object(input));
                return;
            }
        }
        if let Some(run_id) = &exec.run_id
            && let Ok(outcome) = self.rt.store().cancel_run(run_id, None)
            && outcome.should_notify_worker
        {
            self.rt.signal_run_cancel(run_id);
        }
    }

    /// Waits (bounded) for an execution to reach a terminal state.
    async fn wait_execution_terminal(
        self: &Arc<Self>,
        execution_id: &str,
        bound: Duration,
    ) -> Option<ExecutionRow> {
        let deadline = Instant::now() + bound;
        loop {
            if let Ok(Some(exec)) = self.db.execution(execution_id)
                && exec.state != "pending"
                && exec.state != "running"
            {
                return Some(exec);
            }
            if Instant::now() >= deadline {
                return self.db.execution(execution_id).ok().flatten();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Runs one trigger execution through a private `CronFire` on a
    /// dedicated thread (its `submit_turn` blocks; a tokio worker must not).
    fn fire_trigger_execution(
        self: &Arc<Self>,
        row: &ScheduleRow,
        execution_id: &str,
    ) -> Result<(), ContractError> {
        let Some(runtime) = self
            .manager
            .loaded_runtime(&cron_project_key(&row.project_id))
        else {
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!(
                    "cron runtime for project `{}` is not loaded",
                    row.project_id
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        let Some(task) = self
            .task_store(&row.project_id)?
            .get_task(&row.task_id)
            .ok()
            .flatten()
        else {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("cron task `{}` vanished", row.task_id),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        let fire = CronFire::new(CronFireDeps {
            gateway: self.gateway.clone() as Arc<dyn CronAgentGateway>,
            store: self.task_store(&row.project_id)?,
            now: Arc::new(wall_clock_ms),
            register_active_run: {
                let runtime = runtime.clone();
                Arc::new(move |run| runtime.register_active_run(run))
            },
            unregister_active_run: {
                let runtime = runtime.clone();
                Arc::new(move |run_id| runtime.unregister_active_run(run_id))
            },
            get_active_run: {
                let runtime = runtime.clone();
                Arc::new(move |run_id| runtime.active_run(run_id))
            },
            run_timeout_ms: self.manager_run_timeout_minutes() * 60_000.0,
            default_timezone: "UTC".to_owned(),
            // The ported `releaseTaskSession` cleans the old-line session
            // override registry; the v1 domain runs the engine with a
            // private override registry nothing consumes, so a no-op is
            // registered as a difference in the task report.
            release_task_session: Arc::new(|_: &JsValue| {}),
            on_result_delivery: None,
            on_turn_event: None,
            logger: Arc::new(NoopCronLogger),
            zone_cache: self.manager.zone_resolver_cache().clone(),
        });
        let run_id = JsString::from(execution_id);
        let spawned = std::thread::Builder::new()
            .name("v1-schedule-trigger".into())
            .spawn(move || {
                // The fire never rejects in production (its failures fold
                // into run records); the result is deliberately dropped and
                // the trigger's wait observes the execution row instead.
                let _ = fire.run_task(&task, &run_id);
            });
        match spawned {
            Ok(_join_handle) => Ok(()),
            Err(error) => Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!("spawn trigger thread: {error}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )),
        }
    }
}

fn terminal_execution_state(
    run: &StoredRun,
    run_id: String,
) -> (String, Option<String>, Option<String>) {
    let code = run.outcome_code.clone();
    match run.state {
        RunState::Succeeded => ("succeeded".to_owned(), None, Some(run_id)),
        RunState::Cancelled => ("cancelled".to_owned(), None, Some(run_id)),
        RunState::Interrupted => (
            "interrupted".to_owned(),
            Some(code.unwrap_or_else(|| "run_interrupted".to_owned())),
            Some(run_id),
        ),
        RunState::ResultUnknown => (
            "result_unknown".to_owned(),
            Some(code.unwrap_or_else(|| "result_unknown".to_owned())),
            Some(run_id),
        ),
        _ => (
            "failed".to_owned(),
            Some(code.unwrap_or_else(|| "run_failed".to_owned())),
            Some(run_id),
        ),
    }
}

fn wall_clock_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// The cron ↔ v1 bridge
// ---------------------------------------------------------------------------

/// The `CronAgentGateway` the v1 domain binds into the ported engine: one
/// fired cron task becomes one v1 run (S2 admission machine), the run's
/// terminal state classifies the cron outcome, and `abort_turn` maps to a v1
/// run cancel. `submit_turn` runs on the engine's dedicated `cron-fire`
/// thread; the actual work is spawned onto the v1 runtime's tokio driver and
/// the thread only waits on a channel, so no runtime worker is blocked.
struct V1CronGateway {
    domain: Mutex<Weak<SchedulesDomain>>,
}

impl CronAgentGateway for V1CronGateway {
    fn submit_turn(
        &self,
        input: &CronTurnSubmit,
        _on_event: CronTurnEventSink<'_>,
    ) -> Result<(), CronError> {
        let Some(domain) = self
            .domain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
        else {
            return Err(CronError::error(JsString::from(
                "v1 schedules domain is shut down",
            )));
        };
        let execution_id = String::from_utf16_lossy(input.run_id.units());
        let session_key = String::from_utf16_lossy(input.session_key.units());
        let task_id = session_key
            .strip_prefix("cron:")
            .map(str::to_owned)
            .unwrap_or(session_key);
        let Some(row) = domain
            .db
            .schedule_by_task(&task_id)
            .map_err(|error| CronError::error(JsString::from(error.message)))?
        else {
            return Err(CronError::gateway(
                "v1_schedule_missing",
                JsString::from(format!("cron task `{task_id}` has no v1 schedule row")),
            ));
        };
        let _ = domain
            .db
            .begin_execution(&row.schedule_id, &execution_id, "scheduled");
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let runner = domain.clone();
        let exec_row = clone_schedule_row(&row);
        // The `async move` block takes ownership; the thread below keeps
        // using the id to read the finished execution back.
        let execution_id_task = execution_id.clone();
        domain.handle.spawn(async move {
            let (state, code, _run_id) = runner
                .execute_schedule_run(&exec_row, &execution_id_task)
                .await;
            let _ = runner
                .db
                .finish_execution(&execution_id_task, &state, code.as_deref());
            let _ = tx.send(());
        });
        let _ = rx.recv();
        match domain.db.execution(&execution_id) {
            Ok(Some(exec)) => match exec.state.as_str() {
                "succeeded" => Ok(()),
                "cancelled" => Err(CronError::gateway(
                    "agent_aborted",
                    JsString::from("v1 run cancelled"),
                )),
                state => Err(CronError::gateway(
                    exec.error_code.as_deref().unwrap_or("run_failed"),
                    JsString::from(format!("v1 run ended `{state}`")),
                )),
            },
            _ => Err(CronError::error(JsString::from(
                "v1 execution record missing after run",
            ))),
        }
    }

    fn abort_turn(
        &self,
        _session_key: &JsString,
        run_id: Option<&JsString>,
        _reason: Option<&JsString>,
    ) -> Result<(), CronError> {
        let Some(domain) = self
            .domain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
        else {
            return Ok(());
        };
        if let Some(cron_run) = run_id {
            let execution_id = String::from_utf16_lossy(cron_run.units());
            if let Ok(Some(exec)) = domain.db.execution(&execution_id)
                && let Some(run_id) = &exec.run_id
                && let Ok(outcome) = domain.rt.store().cancel_run(run_id, None)
                && outcome.should_notify_worker
            {
                domain.rt.signal_run_cancel(run_id);
            }
        }
        Ok(())
    }

    fn close_session(
        &self,
        _session_key: &JsString,
        _reason: Option<&JsString>,
    ) -> Result<(), CronError> {
        // v1 sessions are durable; cron task sessions need no teardown.
        Ok(())
    }
}

fn clone_schedule_row(row: &ScheduleRow) -> ScheduleRow {
    ScheduleRow {
        schedule_id: row.schedule_id.clone(),
        task_id: row.task_id.clone(),
        project_id: row.project_id.clone(),
        session_id: row.session_id.clone(),
        kind: row.kind.clone(),
        expression: row.expression.clone(),
        timezone: row.timezone.clone(),
        enabled: row.enabled,
        message: row.message.clone(),
        channel_id: row.channel_id.clone(),
        revision: row.revision.clone(),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn str_field(input: &Value, name: &str) -> Result<String, ContractError> {
    input
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid_arg(format!("`{name}` is required")))
}

fn opt_str(input: &Value, name: &str) -> Option<String> {
    input
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_limit(input: &Value) -> Result<usize, ContractError> {
    Ok(input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize)
}

/// Offset cursors (`<prefix>-<n>`); anything else is INVALID_ARGUMENT.
fn parse_offset_cursor(cursor: Option<&str>, prefix: &str) -> Result<usize, ContractError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let rest = if prefix.ends_with('-') {
        cursor.strip_prefix(prefix)
    } else {
        cursor
            .strip_prefix(prefix)
            .and_then(|s| s.strip_prefix('-'))
    };
    rest.and_then(|rest| rest.parse::<usize>().ok())
        .ok_or_else(|| invalid_arg(format!("malformed cursor `{cursor}`")))
}

async fn schedules_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let project_filter = opt_str(&input, "projectId");
    let cursor = parse_offset_cursor(opt_str(&input, "cursor").as_deref(), "sl")?;
    let limit = parse_limit(&input)?;
    let rows = domain.db.schedules(project_filter.as_deref())?;
    let snapshot_revision = domain.db.current_revision()?;
    let mut tasks_cache: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut items = Vec::new();
    let mut next_cursor = None;
    for (index, row) in rows.iter().enumerate().skip(cursor) {
        if items.len() >= limit {
            next_cursor = Some(format!("sl-{index}"));
            break;
        }
        let next_run_at = domain.task_next_run_at(row, &mut tasks_cache);
        items.push(domain.schedule_json(row, next_run_at));
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({"items": items, "nextCursor": next_cursor, "snapshotRevision": snapshot_revision}),
    ))
}

async fn schedules_history(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let schedule_id = str_field(&input, "scheduleId")?;
    let cursor = parse_offset_cursor(opt_str(&input, "cursor").as_deref(), "sh")?;
    let limit = parse_limit(&input)?;
    let Some(row) = domain.db.schedule(&schedule_id)? else {
        return Err(not_found("schedule", &schedule_id).into());
    };
    let executions = domain.db.executions_for(&row.schedule_id)?;
    let snapshot_revision = domain.db.current_revision()?;
    let mut items = Vec::new();
    let mut next_cursor = None;
    for (index, exec) in executions.iter().enumerate().skip(cursor) {
        if items.len() >= limit {
            next_cursor = Some(format!("sh-{index}"));
            break;
        }
        items.push(execution_json(exec));
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({"items": items, "nextCursor": next_cursor, "snapshotRevision": snapshot_revision}),
    ))
}

fn execution_json(exec: &ExecutionRow) -> Value {
    json!({
        "executionId": exec.execution_id,
        "runId": exec.run_id,
        "scheduledAt": exec.scheduled_at,
        "startedAt": exec.started_at,
        "state": exec.state,
        "errorCode": exec.error_code,
    })
}

fn validate_schedule_expression(
    kind: &str,
    expression: &str,
    timezone: &str,
) -> Result<(), ContractError> {
    if !is_valid_cron_timezone(timezone) {
        return Err(invalid_arg(format!("invalid timezone `{timezone}`")));
    }
    let zone_cache = CronZoneResolverCache::default();
    let now_ms = chrono::Utc::now().timestamp_millis() as f64;
    match kind {
        "once" => {
            let computed = compute_next_run_at(
                &CronCreateSchedule::Once {
                    run_at: JsString::from(expression),
                },
                now_ms,
                timezone,
                &zone_cache,
            )
            .map_err(|_| invalid_arg(format!("invalid once timestamp `{expression}`")))?;
            match computed {
                ComputedRunAt::Valid(epoch_ms) if epoch_ms > now_ms => Ok(()),
                ComputedRunAt::Valid(_) => {
                    Err(invalid_arg("once schedule must be scheduled in the future"))
                }
                ComputedRunAt::Undefined | ComputedRunAt::Invalid => Err(invalid_arg(format!(
                    "invalid once timestamp `{expression}`"
                ))),
            }
        }
        "cron" => {
            let computed = compute_next_run_at(
                &CronCreateSchedule::Cron {
                    expression: JsString::from(expression),
                    timezone: Some(JsString::from(timezone)),
                },
                now_ms,
                timezone,
                &zone_cache,
            )
            .map_err(|_| invalid_arg(format!("invalid cron expression `{expression}`")))?;
            match computed {
                ComputedRunAt::Valid(_) => Ok(()),
                ComputedRunAt::Undefined | ComputedRunAt::Invalid => Err(invalid_arg(format!(
                    "cron expression `{expression}` does not produce a valid future run time"
                ))),
            }
        }
        _ => Err(invalid_arg(format!(
            "kind must be `once` or `cron`, got `{kind}`"
        ))),
    }
}

/// `schedules.configure` — create (`expectedRevision == "absent"`) or replace
/// the scheduling generation of one schedule. Validates expression & timezone
/// unconditionally (even when `enabled=false`) before touching the existing
/// task, creates any replacement task before deleting the old one, and commits
/// atomically under a per-schedule mutex + SQLite revision CAS.
async fn schedules_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let schedule_id = str_field(&input, "scheduleId")?;
    let project_id = str_field(&input, "projectId")?;
    let kind = str_field(&input, "kind")?;
    let expression = str_field(&input, "expression")?;
    let timezone = str_field(&input, "timezone")?;
    let message = str_field(&input, "message")?;
    let expected_revision = str_field(&input, "expectedRevision")?;
    let session_id = opt_str(&input, "sessionId");
    let channel_id = opt_str(&input, "channelId");
    let enabled = input
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if !domain.db.project_exists(&project_id)? {
        return Err(invalid_arg(format!("project `{project_id}` is not registered")).into());
    }
    if let Some(session_id) = &session_id
        && !domain
            .db
            .session_belongs_to_project(session_id, &project_id)?
    {
        return Err(invalid_arg(format!(
            "session `{session_id}` does not belong to project `{project_id}`"
        ))
        .into());
    }
    validate_schedule_expression(&kind, &expression, &timezone)?;

    let _schedule_guard = domain.lock_schedule(&schedule_id).await;
    let existing = domain.db.schedule(&schedule_id)?;
    let expected_db_rev = match (expected_revision.as_str(), &existing) {
        ("absent", Some(_)) => {
            return Err(revision_conflict(format!(
                "schedule `{schedule_id}` already exists; pass its revision to update it"
            ))
            .into());
        }
        ("absent", None) => None,
        (revision, Some(row)) if *revision == row.revision => Some(row.revision.clone()),
        (revision, Some(row)) => {
            return Err(revision_conflict(format!(
                "expectedRevision `{revision}` does not match `{}`",
                row.revision
            ))
            .into());
        }
        (revision, None) => {
            return Err(revision_conflict(format!(
                "schedule `{schedule_id}` does not exist (expectedRevision `{revision}`)"
            ))
            .into());
        }
    };

    // 先创建新任务（如启用），DB CAS 提交成功后才删除旧任务，避免验证失败或竞态删毁原任务。
    let task_id = if enabled {
        let candidate = ScheduleRow {
            schedule_id: schedule_id.clone(),
            task_id: String::new(),
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            kind: kind.clone(),
            expression: expression.clone(),
            timezone: timezone.clone(),
            enabled,
            message: message.clone(),
            channel_id: channel_id.clone(),
            revision: String::new(),
        };
        domain.create_old_task(&candidate)?
    } else {
        String::new()
    };
    let revision = domain.db.next_revision()?;
    let row = ScheduleRow {
        schedule_id: schedule_id.clone(),
        task_id,
        project_id,
        session_id,
        kind,
        expression,
        timezone,
        enabled,
        message,
        channel_id,
        revision: revision.clone(),
    };
    if !domain
        .db
        .upsert_schedule_cas(&row, expected_db_rev.as_deref())?
    {
        if !row.task_id.is_empty() {
            let _ = domain.delete_old_task(&row);
        }
        return Err(revision_conflict(format!(
            "concurrent update conflict on schedule `{schedule_id}`"
        ))
        .into());
    }
    if let Some(old) = &existing {
        let _ = domain.delete_old_task(old);
    }
    let _ = domain.db.append_event(
        &schedule_id,
        row.session_id.as_deref().unwrap_or(""),
        "schedule.changed",
        &json!({
            "scheduleId": schedule_id,
            "projectId": row.project_id,
            "enabled": row.enabled,
            "revision": revision,
        }),
    );
    let next_run_at = domain.task_next_run_at(&row, &mut HashMap::new());
    Ok((
        ResponseOutcome::Succeeded,
        json!({"schedule": domain.schedule_json(&row, next_run_at)}),
    ))
}

/// `schedules.set_enabled` — pause (delete the old-line task so nothing is
/// due; in-flight executions keep running and are reported) or resume
/// (re-arm from now: missed occurrences are skipped, the recorded timezone
/// applies). Resuming a once schedule whose time already passed arms
/// nothing (`nextRunAt: null`) instead of failing.
async fn schedules_set_enabled(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let schedule_id = str_field(&input, "scheduleId")?;
    let enabled = input
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid_arg("`enabled` is required"))?;
    let expected_revision = str_field(&input, "expectedRevision")?;

    let _schedule_guard = domain.lock_schedule(&schedule_id).await;
    let Some(mut row) = domain.db.schedule(&schedule_id)? else {
        return Err(not_found("schedule", &schedule_id).into());
    };
    if expected_revision != row.revision {
        return Err(revision_conflict(format!(
            "expectedRevision `{expected_revision}` does not match `{}`",
            row.revision
        ))
        .into());
    }
    let running = domain
        .db
        .running_executions(&schedule_id)?
        .into_iter()
        .map(|exec| exec.execution_id)
        .collect::<Vec<_>>();

    if row.enabled != enabled {
        let old_task_row = if !enabled {
            Some(clone_schedule_row(&row))
        } else {
            None
        };
        let mut created_task_id: Option<String> = None;
        if enabled {
            // Arm only when there is no live task (disabled rows keep their
            // last taskId for history; a fired once task is already deleted).
            let live_task = !row.task_id.is_empty()
                && domain
                    .task_store(&row.project_id)?
                    .get_task(&row.task_id)
                    .ok()
                    .flatten()
                    .is_some();
            if !live_task {
                let candidate = ScheduleRow {
                    task_id: String::new(),
                    enabled: true,
                    ..clone_schedule_row(&row)
                };
                match domain.create_old_task(&candidate) {
                    Ok(task_id) => {
                        created_task_id = Some(task_id.clone());
                        row.task_id = task_id;
                    }
                    Err(error) => {
                        // A once schedule in the past stays unarmed; other
                        // failures (storage, invalid cron) propagate.
                        if error.message.contains("must be scheduled in the future") {
                            row.task_id = String::new();
                        } else {
                            return Err(error.into());
                        }
                    }
                }
            }
        } else {
            row.task_id = String::new();
        }
        row.enabled = enabled;
        row.revision = domain.db.next_revision()?;
        if !domain
            .db
            .upsert_schedule_cas(&row, Some(&expected_revision))?
        {
            if created_task_id.is_some() {
                let _ = domain.delete_old_task(&row);
            }
            return Err(revision_conflict(format!(
                "concurrent update conflict on schedule `{schedule_id}`"
            ))
            .into());
        }
        if let Some(old) = old_task_row {
            let _ = domain.delete_old_task(&old);
        }
        let _ = domain.db.append_event(
            &schedule_id,
            row.session_id.as_deref().unwrap_or(""),
            "schedule.changed",
            &json!({
                "scheduleId": schedule_id,
                "enabled": enabled,
                "revision": row.revision,
            }),
        );
    }
    let next_run_at = domain.task_next_run_at(&row, &mut HashMap::new());
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "schedule": domain.schedule_json(&row, next_run_at),
            "runningExecutionIds": running,
        }),
    ))
}

/// How long `remove {stopRunning}` / `stop_execution` waits for a requested
/// cancellation to become terminal before completing with the observed
/// state (the v1 cancel itself is a synchronous state transition).
const STOP_WAIT: Duration = Duration::from_secs(15);

/// `schedules.remove` — async. Default deletes future triggering only;
/// `stopRunning: true` issues a stop per in-flight execution and waits for
/// the cancellations before completing.
async fn schedules_remove(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let schedule_id = str_field(&input, "scheduleId")?;
    let expected_revision = str_field(&input, "expectedRevision")?;
    let stop_running = input
        .get("stopRunning")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut validated_row: Option<ScheduleRow> = None;
    let operation = accept_domain_operation(
        &domain.db,
        "schedules.remove",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || {
            let Some(row) = domain.db.schedule(&schedule_id)? else {
                return Err(not_found("schedule", &schedule_id));
            };
            if expected_revision != row.revision {
                return Err(revision_conflict(format!(
                    "expectedRevision `{expected_revision}` does not match `{}`",
                    row.revision
                )));
            }
            validated_row = Some(row);
            Ok(())
        },
    )?;
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    let row = match validated_row {
        Some(row) => row,
        None => domain
            .db
            .schedule(&schedule_id)?
            .ok_or_else(|| not_found("schedule", &schedule_id))?,
    };
    let runner = domain.clone();
    let schedule_id = schedule_id.clone();
    let operation_id = operation.operation_id.clone();
    let session_for_events = row.session_id.clone().unwrap_or_default();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, &session_for_events);
        let mut cancelled = Vec::new();
        let mut retained = Vec::new();
        if stop_running {
            let running = runner
                .db
                .running_executions(&schedule_id)
                .unwrap_or_default();
            for exec in &running {
                runner.request_execution_stop(&row, exec);
            }
            for exec in running {
                match runner
                    .wait_execution_terminal(&exec.execution_id, STOP_WAIT)
                    .await
                {
                    Some(finished) if finished.state == "cancelled" => {
                        cancelled.push(finished.execution_id)
                    }
                    Some(finished) => retained.push(finished.execution_id),
                    None => retained.push(exec.execution_id),
                }
            }
        } else {
            retained.extend(
                runner
                    .db
                    .running_executions(&schedule_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|exec| exec.execution_id),
            );
        }
        {
            let _schedule_guard = runner.lock_schedule(&schedule_id).await;
            if runner
                .db
                .delete_schedule_cas(&schedule_id, &row.revision)
                .unwrap_or(false)
            {
                let _ = runner.delete_old_task(&row);
            }
        }
        let completion = json!({
            "scheduleId": schedule_id,
            "removed": true,
            "cancelledExecutionIds": cancelled,
            "retainedExecutionIds": retained,
        });
        let _ = finish_domain_operation(
            &runner.db,
            &operation_id,
            &session_for_events,
            "succeeded",
            &completion,
        );
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// `schedules.trigger` — async. One request is exactly one execution; the
/// executionId doubles as the cron `runId` of a private `CronFire` instance
/// so the engine's claim and post-run bookkeeping (which preserves the
/// pending `nextRunAt`) apply, without the old `runTaskNow` clone. Busy
/// sessions reject with `RESOURCE_BUSY` (the old-line admission fence).
async fn schedules_trigger(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let schedule_id = str_field(&input, "scheduleId")?;
    let expected_revision = str_field(&input, "expectedRevision")?;

    let mut validated_row: Option<ScheduleRow> = None;
    let operation = accept_domain_operation(
        &domain.db,
        "schedules.trigger",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || {
            let Some(row) = domain.db.schedule(&schedule_id)? else {
                return Err(not_found("schedule", &schedule_id));
            };
            // The disabled gate precedes the revision CAS: a disabled schedule
            // rejects with INVALID_ARGUMENT regardless of the presented
            // revision (`set_enabled` advances the revision, so checking the
            // CAS first would mask the disabled rejection with a conflict on
            // every stale-revision trigger of a paused schedule).
            if !row.enabled {
                return Err(invalid_arg(format!(
                    "schedule `{}` is disabled; call schedules.set_enabled before triggering",
                    row.schedule_id
                )));
            }
            if expected_revision != row.revision {
                return Err(revision_conflict(format!(
                    "expectedRevision `{expected_revision}` does not match `{}`",
                    row.revision
                )));
            }
            if !domain.db.running_executions(&row.schedule_id)?.is_empty() {
                return Err(resource_busy(format!(
                    "schedule `{}` already has a running execution",
                    row.schedule_id
                )));
            }
            // The old-line task must not be mid-claim (a just-fired task whose
            // execution row has not landed yet).
            if !row.task_id.is_empty()
                && let Ok(Some(task)) = domain.task_store(&row.project_id)?.get_task(&row.task_id)
                && task.get("status").and_then(JsValue::as_string)
                    == Some(&JsString::from("running"))
            {
                return Err(resource_busy(format!(
                    "schedule `{}` is currently executing",
                    row.schedule_id
                )));
            }
            // Session admission pre-check: an active or queued run in the
            // target session rejects the trigger up front (`session_busy`
            // semantics).
            if let Some(session_id) = &row.session_id
                && domain.db.session_active_or_queued(session_id)?
            {
                return Err(resource_busy(format!(
                    "session `{session_id}` already has a run; schedules.trigger rejects instead of queueing"
                )));
            }
            validated_row = Some(row);
            Ok(())
        },
    )?;
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    let row = match validated_row {
        Some(row) => row,
        None => domain
            .db
            .schedule(&schedule_id)?
            .ok_or_else(|| not_found("schedule", &schedule_id))?,
    };
    let execution_id = domain.rt.store().next_id();
    let _ = domain
        .db
        .begin_execution(&schedule_id, &execution_id, "trigger");
    let runner = domain.clone();
    let schedule_id = schedule_id.clone();
    let operation_id = operation.operation_id.clone();
    let session_for_events = row.session_id.clone().unwrap_or_default();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, &session_for_events);
        let has_live_task = !row.task_id.is_empty()
            && runner
                .task_store(&row.project_id)
                .ok()
                .and_then(|store| store.get_task(&row.task_id).ok().flatten())
                .is_some();
        if has_live_task {
            // Engine path: one `CronFire` over the same store, gateway, and
            // active-run registry the scheduler uses, with the executionId as
            // the cron runId. A fire whose claim fails leaves the execution
            // pending; the wait below then reports it as failed.
            let fired = runner.fire_trigger_execution(&row, &execution_id);
            if let Err(error) = fired {
                let _ =
                    runner
                        .db
                        .finish_execution(&execution_id, "failed", Some(error.code.as_str()));
            }
        } else {
            // No old-line task (e.g. a fired once schedule re-triggered
            // manually): run the v1 execution directly.
            let (state, code, _run_id) = runner.execute_schedule_run(&row, &execution_id).await;
            let _ = runner
                .db
                .finish_execution(&execution_id, &state, code.as_deref());
        }
        // The run starts within milliseconds of the accept, so a first wait
        // bounds how long the claim can take to materialize; a still-pending
        // execution after it means the fire's claim lost its race (no run
        // record at all), which fails instead of waiting the run timeout.
        let mut finished = runner
            .wait_execution_terminal(&execution_id, Duration::from_secs(60))
            .await;
        if finished
            .as_ref()
            .is_some_and(|exec| exec.state == "pending")
        {
            let _ = runner
                .db
                .finish_execution(&execution_id, "failed", Some("claim_conflict"));
            finished = runner.db.execution(&execution_id).ok().flatten();
        } else if finished
            .as_ref()
            .is_some_and(|exec| exec.state == "running")
        {
            finished = runner
                .wait_execution_terminal(
                    &execution_id,
                    runner.run_timeout() + Duration::from_secs(30),
                )
                .await;
        }
        let (run_id, execution_state) = match finished {
            Some(exec) => (exec.run_id, exec.state),
            None => (None, "pending".to_owned()),
        };
        let completion = json!({
            "scheduleId": schedule_id,
            "executionId": execution_id,
            "runId": run_id,
            "executionState": execution_state,
            "artifactIds": [],
        });
        let state = if execution_state == "succeeded" {
            "succeeded"
        } else {
            "failed"
        };
        let _ = finish_domain_operation(
            &runner.db,
            &operation_id,
            &session_for_events,
            state,
            &completion,
        );
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// `schedules.stop_execution` — async. Rejects executions that do not belong
/// to the schedule, stops the current run without touching the schedule, and
/// returns the terminal state when the execution already ended.
async fn schedules_stop_execution(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let schedule_id = str_field(&input, "scheduleId")?;
    let execution_id = str_field(&input, "executionId")?;

    let mut validated_row: Option<ScheduleRow> = None;
    let mut validated_exec: Option<ExecutionRow> = None;
    let operation = accept_domain_operation(
        &domain.db,
        "schedules.stop_execution",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || {
            let Some(row) = domain.db.schedule(&schedule_id)? else {
                return Err(not_found("schedule", &schedule_id));
            };
            let Some(exec) = domain.db.execution(&execution_id)? else {
                return Err(not_found("execution", &execution_id));
            };
            if exec.schedule_id != schedule_id {
                return Err(not_found(
                    "execution for schedule",
                    &format!("{execution_id} (schedule {schedule_id})"),
                ));
            }
            validated_row = Some(row);
            validated_exec = Some(exec);
            Ok(())
        },
    )?;
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    let row = match validated_row {
        Some(row) => row,
        None => domain
            .db
            .schedule(&schedule_id)?
            .ok_or_else(|| not_found("schedule", &schedule_id))?,
    };
    let exec = match validated_exec {
        Some(exec) => exec,
        None => domain
            .db
            .execution(&execution_id)?
            .ok_or_else(|| not_found("execution", &execution_id))?,
    };
    let runner = domain.clone();
    let schedule_id = schedule_id.clone();
    let execution_id = execution_id.clone();
    let operation_id = operation.operation_id.clone();
    let session_for_events = row.session_id.clone().unwrap_or_default();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, &session_for_events);
        let finished = if exec.state != "pending" && exec.state != "running" {
            Some(exec.clone())
        } else {
            runner.request_execution_stop(&row, &exec);
            runner
                .wait_execution_terminal(&execution_id, STOP_WAIT)
                .await
        };
        let (run_id, execution_state) = match finished {
            Some(finished) => (finished.run_id, finished.state),
            None => (exec.run_id.clone(), exec.state.clone()),
        };
        let completion = json!({
            "scheduleId": schedule_id,
            "executionId": execution_id,
            "executionState": execution_state,
            "runId": run_id,
        });
        let state = if execution_state == "cancelled" || execution_state == "succeeded" {
            "succeeded"
        } else {
            "failed"
        };
        let _ = finish_domain_operation(
            &runner.db,
            &operation_id,
            &session_for_events,
            state,
            &completion,
        );
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;

    struct Fixture {
        _tmp: TempDir,
        rt: Arc<PublicV1Runtime>,
        home: PathBuf,
        instance_id: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            shutdown_for_tests(&self.home);
        }
    }

    async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        let instance_id = rt.store().meta().instance_id.clone();
        Fixture {
            _tmp: tmp,
            rt,
            home,
            instance_id,
        }
    }

    fn open_runtime(root_dir: &std::path::Path) -> Arc<PublicV1Runtime> {
        let (file, _token) =
            crate::public_v1::bootstrap::load_local_instance_and_token(root_dir, None).unwrap();
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

    async fn setup_project_session(fx: &Fixture) {
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": fx.instance_id,
                    "projectId": "proj-1",
                    "name": "Schedules",
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": "root-1", "relativePath": "w"}
                }),
                None,
                None,
            )
            .await
            .unwrap();
        for session_id in ["sess-1", "sess-2"] {
            fx.rt
                .dispatch(
                    "session.create",
                    json!({"instanceId": fx.instance_id, "sessionId": session_id, "projectId": "proj-1"}),
                    None,
                    None,
                )
                .await
                .unwrap();
        }
    }

    fn configure_input(_fx: &Fixture, schedule_id: &str, kind: &str, expression: &str) -> Value {
        json!({
            "scheduleId": schedule_id,
            "projectId": "proj-1",
            "sessionId": "sess-1",
            "kind": kind,
            "expression": expression,
            "timezone": "UTC",
            "message": "hello schedule",
            "expectedRevision": "absent",
        })
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> ContractError {
        match rt.dispatch(method, input, None, None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    /// Polls `schedules.history` until the newest execution of `schedule_id`
    /// is terminal and returns it.
    async fn wait_for_execution(rt: &Arc<PublicV1Runtime>, schedule_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, history) = rt
                .dispatch(
                    "schedules.history",
                    json!({"scheduleId": schedule_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if let Some(first) = history["items"].as_array().and_then(|items| items.first())
                && first["state"] != "pending"
                && first["state"] != "running"
            {
                return first.clone();
            }
            assert!(Instant::now() < deadline, "execution never became terminal");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Polls `schedules.history` until the newest execution is running and
    /// returns its JSON row.
    async fn wait_until_running(rt: &Arc<PublicV1Runtime>, schedule_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, history) = rt
                .dispatch(
                    "schedules.history",
                    json!({"scheduleId": schedule_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if let Some(first) = history["items"].as_array().and_then(|items| items.first())
                && first["state"] == "running"
            {
                return first.clone();
            }
            assert!(Instant::now() < deadline, "triggered run never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn configure_list_and_history_round_trip() {
        let fx = fixture("sched-configure").await;
        setup_project_session(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-1", "cron", "*/5 * * * *"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(created["schedule"]["scheduleId"], "sched-1");
        assert_eq!(created["schedule"]["enabled"], true);
        assert!(
            created["schedule"]["nextRunAt"].as_str().is_some(),
            "an armed cron schedule exposes its next due time"
        );
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();
        assert!(revision.starts_with("srev-"));

        let (_, listed) = fx
            .rt
            .dispatch("schedules.list", json!({"projectId": "proj-1"}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["items"][0]["scheduleId"], "sched-1");
        assert_eq!(listed["items"][0]["revision"], revision);
        assert!(listed["snapshotRevision"].as_str().is_some());

        let (_, history) = fx
            .rt
            .dispatch(
                "schedules.history",
                json!({"scheduleId": "sched-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            history["items"].as_array().map(Vec::len),
            Some(0),
            "nothing fired yet"
        );

        // Invalid timezone is rejected before any write.
        let mut bad = configure_input(&fx, "sched-tz", "cron", "* * * * *");
        bad["timezone"] = json!("Mars/Olympus");
        let error = dispatch_err(&fx.rt, "schedules.configure", bad).await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        // And nothing was written for the rejected id.
        let error = dispatch_err(
            &fx.rt,
            "schedules.history",
            json!({"scheduleId": "sched-tz"}),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn revision_gate_rejects_stale_and_absent_mismatches() {
        let fx = fixture("sched-revision").await;
        setup_project_session(&fx).await;

        fx.rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-1", "cron", "*/5 * * * *"),
                None,
                None,
            )
            .await
            .unwrap();

        let error = dispatch_err(
            &fx.rt,
            "schedules.configure",
            configure_input(&fx, "sched-1", "cron", "*/10 * * * *"),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT", "absent on existing");

        let mut stale = configure_input(&fx, "sched-1", "cron", "*/10 * * * *");
        stale["expectedRevision"] = json!("srev-00009999");
        let error = dispatch_err(&fx.rt, "schedules.configure", stale).await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        let mut unknown = configure_input(&fx, "sched-other", "cron", "*/10 * * * *");
        unknown["expectedRevision"] = json!("srev-00000001");
        let error = dispatch_err(&fx.rt, "schedules.configure", unknown).await;
        assert_eq!(
            error.code, "REVISION_CONFLICT",
            "unknown schedule with revision"
        );
    }

    #[tokio::test]
    async fn once_schedule_must_be_in_the_future() {
        let fx = fixture("sched-once-past").await;
        setup_project_session(&fx).await;

        let error = dispatch_err(
            &fx.rt,
            "schedules.configure",
            configure_input(&fx, "sched-past", "once", "2000-01-01T00:00:00.000Z"),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        assert!(
            error.message.contains("future"),
            "engine's once-in-the-past rejection surfaces: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn unknown_project_and_foreign_session_reject() {
        let fx = fixture("sched-scope").await;
        setup_project_session(&fx).await;

        let mut input = configure_input(&fx, "sched-x", "cron", "*/5 * * * *");
        input["projectId"] = json!("proj-none");
        let error = dispatch_err(&fx.rt, "schedules.configure", input).await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        let mut input = configure_input(&fx, "sched-y", "cron", "*/5 * * * *");
        input["sessionId"] = json!("sess-unknown");
        let error = dispatch_err(&fx.rt, "schedules.configure", input).await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn set_enabled_pauses_and_resumes() {
        let fx = fixture("sched-enabled").await;
        setup_project_session(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-1", "cron", "*/5 * * * *"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

        let (_, disabled) = fx
            .rt
            .dispatch(
                "schedules.set_enabled",
                json!({
                    "scheduleId": "sched-1",
                    "enabled": false,
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(disabled["schedule"]["enabled"], false);
        assert_eq!(
            disabled["schedule"]["nextRunAt"],
            Value::Null,
            "a paused schedule is not armed"
        );
        assert_eq!(
            disabled["runningExecutionIds"].as_array().map(Vec::len),
            Some(0)
        );
        let new_revision = disabled["schedule"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(revision, new_revision);

        let (_, resumed) = fx
            .rt
            .dispatch(
                "schedules.set_enabled",
                json!({
                    "scheduleId": "sched-1",
                    "enabled": true,
                    "expectedRevision": new_revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(resumed["schedule"]["enabled"], true);
        assert!(
            resumed["schedule"]["nextRunAt"].as_str().is_some(),
            "resuming re-arms from now"
        );

        let error = dispatch_err(
            &fx.rt,
            "schedules.set_enabled",
            json!({
                "scheduleId": "sched-1",
                "enabled": false,
                "expectedRevision": revision,
            }),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        let error = dispatch_err(
            &fx.rt,
            "schedules.set_enabled",
            json!({
                "scheduleId": "sched-missing",
                "enabled": false,
                "expectedRevision": revision,
            }),
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn trigger_executes_once_and_records_history() {
        let fx = fixture("sched-trigger").await;
        setup_project_session(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-1", "cron", "0 3 * * *"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();
        let next_before = created["schedule"]["nextRunAt"]
            .as_str()
            .unwrap()
            .to_owned();

        let trigger_input = json!({
            "scheduleId": "sched-1",
            "expectedRevision": revision,
        });
        let (_, receipt) = fx
            .rt
            .dispatch(
                "schedules.trigger",
                trigger_input.clone(),
                Some("key-trigger-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], "operation");
        assert_eq!(receipt["durable"], true);
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

        let execution = wait_for_execution(&fx.rt, "sched-1").await;
        assert_eq!(execution["state"], "succeeded");
        let run_id = execution["runId"].as_str().unwrap().to_owned();

        // The associated run is queryable through the run domain.
        let (_, run) = fx
            .rt
            .dispatch(
                "run.get",
                json!({"instanceId": fx.instance_id, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(run["run"]["state"], "succeeded");
        assert_eq!(run["run"]["sessionId"], "sess-1");

        // The regular next trigger is untouched by the explicit trigger.
        let (_, listed) = fx
            .rt
            .dispatch("schedules.list", json!({}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["items"][0]["nextRunAt"], next_before);

        // Idempotent replay: same key + same input → the same operation.
        let (_, replay) = fx
            .rt
            .dispatch(
                "schedules.trigger",
                trigger_input,
                Some("key-trigger-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["operationId"], operation_id);
        assert_eq!(replay["deduplicated"], true);
    }

    #[tokio::test]
    async fn trigger_on_disabled_schedule_rejects() {
        let fx = fixture("sched-trigger-disabled").await;
        setup_project_session(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-1", "cron", "*/5 * * * *"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();
        fx.rt
            .dispatch(
                "schedules.set_enabled",
                json!({
                    "scheduleId": "sched-1",
                    "enabled": false,
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();

        let error = fx
            .rt
            .dispatch(
                "schedules.trigger",
                json!({"scheduleId": "sched-1", "expectedRevision": revision}),
                Some("key-trigger-disabled-1"),
                None,
            )
            .await;
        match error {
            Err(DispatchError::Application(error)) => assert_eq!(error.code, "INVALID_ARGUMENT"),
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn trigger_rejects_busy_session_with_resource_busy() {
        let fx = fixture("sched-busy").await;
        setup_project_session(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-1", "cron", "0 3 * * *"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

        // Occupy the session with a long run (the offline driver sleeps).
        let (_, submitted) = fx
            .rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": fx.instance_id,
                    "sessionId": "sess-1",
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=60000]"}]
                }),
                Some("key-occupy-1"),
                None,
            )
            .await
            .unwrap();
        let occupying_run = submitted["runId"].as_str().unwrap().to_owned();

        let error = fx
            .rt
            .dispatch(
                "schedules.trigger",
                json!({"scheduleId": "sched-1", "expectedRevision": revision}),
                Some("key-trigger-busy-1"),
                None,
            )
            .await;
        match error {
            Err(DispatchError::Application(error)) => assert_eq!(error.code, "RESOURCE_BUSY"),
            other => panic!("expected RESOURCE_BUSY, got {other:?}"),
        }

        // No execution row was created for the rejected trigger.
        let (_, history) = fx
            .rt
            .dispatch(
                "schedules.history",
                json!({"scheduleId": "sched-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(history["items"].as_array().map(Vec::len), Some(0));

        // Cleanup: cancel the occupying run so the fixture can shut down.
        let _ = fx
            .rt
            .dispatch(
                "run.cancel",
                json!({"instanceId": fx.instance_id, "runId": occupying_run}),
                Some("key-cleanup-occupy"),
                None,
            )
            .await;
    }

    #[tokio::test]
    async fn trigger_then_stop_execution_cancels_and_keeps_schedule() {
        let fx = fixture("sched-stop").await;
        setup_project_session(&fx).await;

        let mut input = configure_input(&fx, "sched-1", "cron", "0 3 * * *");
        input["message"] = json!("[pd-driver:sleep=60000] still running");
        let (_, created) = fx
            .rt
            .dispatch("schedules.configure", input, None, None)
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

        let (_, receipt) = fx
            .rt
            .dispatch(
                "schedules.trigger",
                json!({"scheduleId": "sched-1", "expectedRevision": revision}),
                Some("key-stop-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["state"], "queued");

        let running = wait_until_running(&fx.rt, "sched-1").await;
        let execution_id = running["executionId"].as_str().unwrap().to_owned();

        let (_, stop_receipt) = fx
            .rt
            .dispatch(
                "schedules.stop_execution",
                json!({
                    "scheduleId": "sched-1",
                    "executionId": execution_id,
                }),
                Some("key-stop-exec-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(stop_receipt["kind"], "operation");

        let execution = wait_for_execution(&fx.rt, "sched-1").await;
        assert_eq!(execution["state"], "cancelled");

        // The schedule survives the stop, still armed and enabled.
        let (_, listed) = fx
            .rt
            .dispatch("schedules.list", json!({}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["items"][0]["enabled"], true);
        assert!(listed["items"][0]["nextRunAt"].as_str().is_some());

        // An execution that does not exist for this schedule is rejected.
        let error = fx
            .rt
            .dispatch(
                "schedules.stop_execution",
                json!({
                    "scheduleId": "sched-1",
                    "executionId": "exec-does-not-exist",
                }),
                Some("key-stop-exec-none"),
                None,
            )
            .await;
        match error {
            Err(DispatchError::Application(error)) => assert_eq!(error.code, "NOT_FOUND"),
            other => panic!("expected NOT_FOUND, got {other:?}"),
        }
    }

    /// The scheduled-fire path: the engine's own scheduler tick fires the due
    /// task, the bridge turns it into a v1 run (the offline driver completes
    /// it), and the fired once schedule is deleted by the engine (not
    /// re-armed).
    #[tokio::test]
    async fn scheduler_tick_fires_due_task_into_v1_run() {
        let fx = fixture("sched-tick").await;
        setup_project_session(&fx).await;

        let run_at = wall_clock_ms() + 100.0;
        let iso = pilotdeck_transport::system_clock::format_epoch_iso(run_at as i64);
        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-once", "once", &iso),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(created["schedule"]["nextRunAt"].as_str().is_some());

        let domain = domain_for(&fx.rt).unwrap();
        let store = domain.task_store("proj-1").unwrap();
        assert_eq!(store.list_tasks().unwrap().len(), 1);

        // Wait past the due time, then drive one scheduler tick
        // deterministically (the loop's own 60s poll is not waited out).
        tokio::time::sleep(Duration::from_millis(250)).await;
        domain
            .manager
            .run_tick_once(Some(&cron_project_key("proj-1")))
            .unwrap();

        let execution = wait_for_execution(&fx.rt, "sched-once").await;
        assert_eq!(execution["state"], "succeeded");
        assert!(execution["runId"].as_str().is_some());
        assert!(
            execution["scheduledAt"].as_str().is_some(),
            "history distinguishes the scheduled instant"
        );
        assert!(
            execution["startedAt"].as_str().is_some(),
            "history records when the run started"
        );

        // Post-run: the engine deleted the fired once task. The deletion is
        // the fire thread's post-run update, which trails the v1 execution's
        // terminal visibility (the bridge unblocks `submit_turn` before the
        // engine's finally block runs), so poll for it with the shared test
        // bound instead of asserting an ordering the engine never promised.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !store.list_tasks().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "fired once task is removed");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // The schedule stays listed, unarmed, and is not re-armed.
        let (_, listed) = fx
            .rt
            .dispatch("schedules.list", json!({}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["items"][0]["nextRunAt"], Value::Null);

        // A second tick does not re-fire it.
        domain
            .manager
            .run_tick_once(Some(&cron_project_key("proj-1")))
            .unwrap();
        let (_, history) = fx
            .rt
            .dispatch(
                "schedules.history",
                json!({"scheduleId": "sched-once"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(history["items"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn disabled_schedule_does_not_fire_on_tick() {
        let fx = fixture("sched-disabled-tick").await;
        setup_project_session(&fx).await;

        let run_at = wall_clock_ms() + 100.0;
        let iso = pilotdeck_transport::system_clock::format_epoch_iso(run_at as i64);
        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-once", "once", &iso),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

        fx.rt
            .dispatch(
                "schedules.set_enabled",
                json!({
                    "scheduleId": "sched-once",
                    "enabled": false,
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();

        let domain = domain_for(&fx.rt).unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        domain
            .manager
            .run_tick_once(Some(&cron_project_key("proj-1")))
            .unwrap();

        let (_, history) = fx
            .rt
            .dispatch(
                "schedules.history",
                json!({"scheduleId": "sched-once"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            history["items"].as_array().map(Vec::len),
            Some(0),
            "a paused schedule does not fire"
        );
    }

    #[tokio::test]
    async fn remove_defaults_to_retaining_and_stops_when_asked() {
        let fx = fixture("sched-remove").await;
        setup_project_session(&fx).await;

        // Part A: default remove only stops future triggering; the running
        // execution is retained (it keeps running in sess-1).
        let mut input = configure_input(&fx, "sched-1", "cron", "0 3 * * *");
        input["message"] = json!("[pd-driver:sleep=60000] retained");
        let (_, created) = fx
            .rt
            .dispatch("schedules.configure", input, None, None)
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();
        fx.rt
            .dispatch(
                "schedules.trigger",
                json!({"scheduleId": "sched-1", "expectedRevision": revision}),
                Some("key-remove-trig-1"),
                None,
            )
            .await
            .unwrap();
        let retained_run = wait_until_running(&fx.rt, "sched-1").await;
        let retained_run_id = retained_run["runId"].as_str().unwrap().to_owned();

        let (_, remove_receipt) = fx
            .rt
            .dispatch(
                "schedules.remove",
                json!({
                    "scheduleId": "sched-1",
                    "expectedRevision": revision,
                }),
                Some("key-remove-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(remove_receipt["kind"], "operation");
        let remove_operation_id = remove_receipt["operationId"].as_str().unwrap().to_owned();

        // The remove is an async operation: the row deletion happens on the
        // spawned driver, so poll history until the schedule is gone (the
        // same effect-visibility waiting the e2e scenarios use).
        let gone = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if matches!(
                    fx.rt
                        .dispatch(
                            "schedules.history",
                            json!({"scheduleId": "sched-1"}),
                            None,
                            None,
                        )
                        .await,
                    Err(DispatchError::Application(error))
                        if error.code == "NOT_FOUND"
                ) {
                    break true;
                }
                assert!(
                    Instant::now() < deadline,
                    "schedule `sched-1` still present after remove `{remove_operation_id}`"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        assert!(gone, "removed schedule is gone");

        // The retained run is still active (the default remove stopped no
        // execution); cancel it directly for fixture hygiene.
        let (_, retained) = fx
            .rt
            .dispatch(
                "run.get",
                json!({"instanceId": fx.instance_id, "runId": retained_run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(retained["run"]["state"], "running");
        let _ = fx
            .rt
            .dispatch(
                "run.cancel",
                json!({"instanceId": fx.instance_id, "runId": retained_run_id}),
                Some("key-cleanup-retained"),
                None,
            )
            .await;

        // Part B: `stopRunning: true` cancels the in-flight execution of the
        // removed schedule (sess-2 stays free).
        let mut input = configure_input(&fx, "sched-2", "cron", "0 3 * * *");
        input["sessionId"] = json!("sess-2");
        input["message"] = json!("[pd-driver:sleep=60000] cancelled");
        let (_, created) = fx
            .rt
            .dispatch("schedules.configure", input, None, None)
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();
        fx.rt
            .dispatch(
                "schedules.trigger",
                json!({"scheduleId": "sched-2", "expectedRevision": revision}),
                Some("key-remove-trig-2"),
                None,
            )
            .await
            .unwrap();
        let running = wait_until_running(&fx.rt, "sched-2").await;
        let stopped_run_id = running["runId"].as_str().unwrap().to_owned();

        fx.rt
            .dispatch(
                "schedules.remove",
                json!({
                    "scheduleId": "sched-2",
                    "stopRunning": true,
                    "expectedRevision": revision,
                }),
                Some("key-remove-2"),
                None,
            )
            .await
            .unwrap();

        // The stop was requested and takes effect on the run.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, run) = fx
                .rt
                .dispatch(
                    "run.get",
                    json!({"instanceId": fx.instance_id, "runId": stopped_run_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if run["run"]["terminal"] == json!(true) {
                assert_eq!(run["run"]["state"], "cancelled");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stopRunning never cancelled the run"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn sessionless_schedule_fails_to_start() {
        let fx = fixture("sched-sessionless").await;
        setup_project_session(&fx).await;

        let mut input = configure_input(&fx, "sched-free", "cron", "0 3 * * *");
        input["sessionId"] = Value::Null;
        let (_, created) = fx
            .rt
            .dispatch("schedules.configure", input, None, None)
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();
        assert_eq!(created["schedule"]["sessionId"], Value::Null);

        let (_, _receipt) = fx
            .rt
            .dispatch(
                "schedules.trigger",
                json!({"scheduleId": "sched-free", "expectedRevision": revision}),
                Some("key-sessionless-1"),
                None,
            )
            .await
            .unwrap();

        let execution = wait_for_execution(&fx.rt, "sched-free").await;
        assert_eq!(execution["state"], "failed");
        assert_eq!(execution["errorCode"], "NO_TARGET_SESSION");
        assert_eq!(execution["runId"], Value::Null);
    }

    #[tokio::test]
    async fn idempotency_key_conflict_replays_rejection_not_accept() {
        let fx = fixture("sched-idem").await;
        setup_project_session(&fx).await;

        // A definitively rejected trigger (unknown schedule) replays its
        // rejection under the same key + identical input, and does not poison
        // the key for other schedules.
        let input = json!({"scheduleId": "sched-none", "expectedRevision": "srev-00000001"});
        let error = fx
            .rt
            .dispatch("schedules.trigger", input.clone(), Some("key-idem-1"), None)
            .await;
        match error {
            Err(DispatchError::Application(error)) => assert_eq!(error.code, "NOT_FOUND"),
            other => panic!("expected NOT_FOUND, got {other:?}"),
        }
        let error = fx
            .rt
            .dispatch("schedules.trigger", input, Some("key-idem-1"), None)
            .await;
        match error {
            Err(DispatchError::Application(error)) => {
                assert_eq!(error.code, "NOT_FOUND", "replay re-rejects definitively")
            }
            other => panic!("expected the replay to reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn trigger_and_remove_idempotent_replay_short_circuits_cleanly() {
        let fx = fixture("sched-replay-short").await;
        setup_project_session(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-idem", "cron", "*/10 * * * *"),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["schedule"]["revision"].as_str().unwrap().to_owned();

        let trig_req = json!({"scheduleId": "sched-idem", "expectedRevision": revision});
        let (_, trig1) = fx
            .rt
            .dispatch(
                "schedules.trigger",
                trig_req.clone(),
                Some("k-trig-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(trig1["deduplicated"], json!(false));
        let first_exec = wait_for_execution(&fx.rt, "sched-idem").await;

        // 触发完成、执行行终态后同 key 重放：不得产生第二条 execution 或二次运行。
        let (_, trig2) = fx
            .rt
            .dispatch("schedules.trigger", trig_req, Some("k-trig-1"), None)
            .await
            .unwrap();
        assert_eq!(trig2["deduplicated"], json!(true));
        assert_eq!(trig2["operationId"], trig1["operationId"]);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (_, hist) = fx
            .rt
            .dispatch(
                "schedules.history",
                json!({"scheduleId": "sched-idem"}),
                None,
                None,
            )
            .await
            .unwrap();
        let items = hist["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["executionId"], first_exec["executionId"]);

        // remove 完成后同 key 重放：不得误报 NOT_FOUND，应原样返回 deduplicated=true。
        let rem_req = json!({"scheduleId": "sched-idem", "expectedRevision": revision});
        let (_, rem1) = fx
            .rt
            .dispatch("schedules.remove", rem_req.clone(), Some("k-rem-1"), None)
            .await
            .unwrap();
        let op_id = rem1["operationId"].as_str().unwrap().to_owned();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, got) = fx
                .rt
                .dispatch("operations.get", json!({"operationId": op_id}), None, None)
                .await
                .unwrap();
            if got["operation"]["state"] == json!("succeeded") {
                break;
            }
            assert!(Instant::now() < deadline, "remove never succeeded");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let (_, rem2) = fx
            .rt
            .dispatch("schedules.remove", rem_req, Some("k-rem-1"), None)
            .await
            .unwrap();
        assert_eq!(rem2["deduplicated"], json!(true));
        assert_eq!(rem2["operationId"], rem1["operationId"]);
    }

    #[tokio::test]
    async fn configure_validates_disabled_expressions_preserves_old_task_and_cas_serializes() {
        let fx = fixture("sched-cfg-cas").await;
        setup_project_session(&fx).await;

        // 1) enabled=false 也必须提前拒绝非法 cron 与过期 once，不落库。
        let mut bad_disabled_cron = configure_input(&fx, "sched-bad", "cron", "not-a-cron");
        bad_disabled_cron["enabled"] = json!(false);
        let err = dispatch_err(&fx.rt, "schedules.configure", bad_disabled_cron).await;
        assert_eq!(err.code, "INVALID_ARGUMENT");

        let mut past_disabled_once =
            configure_input(&fx, "sched-bad2", "once", "2000-01-01T00:00:00.000Z");
        past_disabled_once["enabled"] = json!(false);
        let err = dispatch_err(&fx.rt, "schedules.configure", past_disabled_once).await;
        assert_eq!(err.code, "INVALID_ARGUMENT");

        // 2) 非法更新不得删毁原调度任务。
        let (_, created) = fx
            .rt
            .dispatch(
                "schedules.configure",
                configure_input(&fx, "sched-keep", "cron", "*/5 * * * *"),
                None,
                None,
            )
            .await
            .unwrap();
        let rev1 = created["schedule"]["revision"].as_str().unwrap().to_owned();
        let mut bad_update = configure_input(&fx, "sched-keep", "cron", "bad-expr");
        bad_update["expectedRevision"] = json!(rev1);
        let err = dispatch_err(&fx.rt, "schedules.configure", bad_update).await;
        assert_eq!(err.code, "INVALID_ARGUMENT");
        let domain = domain_for(&fx.rt).unwrap();
        let kept_row = domain.db.schedule("sched-keep").unwrap().unwrap();
        assert!(!kept_row.task_id.is_empty());
        assert!(
            domain
                .task_store("proj-1")
                .unwrap()
                .get_task(&kept_row.task_id)
                .unwrap()
                .is_some(),
            "failed update must preserve the existing armed cron task"
        );

        // 3) 并发 CAS 创建/更新同一 scheduleId：恰好 1 个成功、无孤儿 cron task。
        let req_a = configure_input(&fx, "sched-race", "cron", "*/5 * * * *");
        let req_b = configure_input(&fx, "sched-race", "cron", "*/10 * * * *");
        let (res_a, res_b) = tokio::join!(
            fx.rt.dispatch("schedules.configure", req_a, None, None),
            fx.rt.dispatch("schedules.configure", req_b, None, None),
        );
        let mut ok_count = 0usize;
        let mut conflict_count = 0usize;
        for res in [res_a, res_b] {
            match res {
                Ok(_) => ok_count += 1,
                Err(DispatchError::Application(err)) if err.code == "REVISION_CONFLICT" => {
                    conflict_count += 1;
                }
                other => panic!("unexpected configure outcome: {other:?}"),
            }
        }
        assert_eq!(ok_count, 1);
        assert_eq!(conflict_count, 1);
        let tasks = domain.task_store("proj-1").unwrap().list_tasks().unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "only sched-keep and the winning sched-race task may exist in tasks.json"
        );
    }
}
