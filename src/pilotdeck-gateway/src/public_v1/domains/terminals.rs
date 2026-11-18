//! `terminals.*` — PTY lifecycle, control leases, ordered input, byte-cursor
//! output, and the close chain (t015 PR-3).
//!
//! Ownership follows the brief's topology: the Gateway process owns every
//! live pty (master fd + retained output ring + child supervisor in
//! [`super::terminals_pty`]); attach processes are clients. Durable terminal
//! records live in the `terminals` SQLite table; the live runtime overlays
//! them (offsets, exit facts) while the handle exists. A terminal whose
//! `serverEpoch` (the boot's `meta.generation`) no longer matches the
//! running Gateway is reported as `lost` on read — the master fd died with
//! the old process, so the kernel already hung up the session.
//!
//! Contract anchors (`state/cli-v1-operations.json`, terminals.*): the
//! `Terminal` object shape, `ControlLease` fields, the input
//! dedup/gap/conflict matrix, the output frame types
//! (`bytes`/`gap`/`exit`/`heartbeat`), and the close semantics (close is
//! explicit; detach, EOF input, and Ctrl-C are not close; force only ever
//! explicit). The lease token is a sensitive short-lived credential: only
//! its SHA-256 is persisted, and it never appears in logs or events.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_LIMIT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::terminals_pty::{
    self, ExitFacts, TerminalHandle, live_terminal, live_terminal_count,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// Error codes the terminals contract introduces beyond the shared set.
pub(crate) const CODE_SHELL_UNAVAILABLE: &str = "SHELL_UNAVAILABLE";
pub(crate) const CODE_PATH_ESCAPE: &str = "PATH_ESCAPE";
pub(crate) const CODE_CONTROL_CONFLICT: &str = "CONTROL_CONFLICT";
pub(crate) const CODE_LEASE_EXPIRED: &str = "LEASE_EXPIRED";
pub(crate) const CODE_INPUT_SEQUENCE_GAP: &str = "INPUT_SEQUENCE_GAP";
pub(crate) const CODE_INPUT_SEQUENCE_CONFLICT: &str = "INPUT_SEQUENCE_CONFLICT";
pub(crate) const CODE_TERMINAL_CLOSED: &str = "TERMINAL_CLOSED";
pub(crate) const CODE_CURSOR_INVALID: &str = "CURSOR_INVALID";

/// Control-lease lifetime. Attach renews around 2/3 of the window; expiry
/// only costs input rights (the pty keeps running) — the contract's
/// "租约到期或 release 只失去输入权" — so a short window keeps the takeover
/// latency bounded without endangering terminals.
const LEASE_TTL_SECS: i64 = 60;

/// Defensive cap on concurrently live (not yet exited) terminals. The
/// contract lists RESOURCE_LIMIT; the value is this implementation's
/// choice (the TS baseline had no explicit cap).
const MAX_LIVE_TERMINALS: usize = 32;

/// Retention of idempotency records, mirroring `run.submit`'s 7 days.
const IDEMPOTENCY_RETENTION_SECS: i64 = 7 * 24 * 3600;

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "terminals",
        "CREATE TABLE IF NOT EXISTS terminals (
            terminal_id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            working_directory TEXT,
            shell_profile_id TEXT,
            server_epoch TEXT NOT NULL,
            state TEXT NOT NULL,
            control_epoch INTEGER NOT NULL DEFAULT 0,
            controller_id TEXT,
            lease_token_hash TEXT,
            lease_expires_ms INTEGER,
            last_accepted_input_seq INTEGER NOT NULL DEFAULT 0,
            last_input_digest TEXT,
            output_epoch TEXT NOT NULL,
            earliest_output_offset INTEGER NOT NULL DEFAULT 0,
            latest_output_offset INTEGER NOT NULL DEFAULT 0,
            exit_code INTEGER,
            exit_signal TEXT,
            term_rows INTEGER NOT NULL,
            term_columns INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    );
    reg.schema(
        "terminal_close_operations",
        "CREATE TABLE IF NOT EXISTS terminal_close_operations (
            operation_id TEXT PRIMARY KEY,
            terminal_id TEXT NOT NULL,
            state TEXT NOT NULL,
            revision TEXT NOT NULL,
            result_json TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    );
    reg.handler("terminals.create", |rt, input, key, _rev| {
        Box::pin(terminals_create(rt, input, key))
    });
    reg.handler("terminals.list", |rt, input, _key, _rev| {
        Box::pin(terminals_list(rt, input))
    });
    reg.handler("terminals.get", |rt, input, _key, _rev| {
        Box::pin(terminals_get(rt, input))
    });
    reg.handler("terminals.control", |rt, input, _key, _rev| {
        Box::pin(terminals_control(rt, input))
    });
    reg.handler("terminals.input", |rt, input, _key, _rev| {
        Box::pin(terminals_input(rt, input))
    });
    reg.handler("terminals.resize", |rt, input, _key, _rev| {
        Box::pin(terminals_resize(rt, input))
    });
    // The dispatch handler serves the unary (non-follow) read and the
    // schema-validation path; the follow stream is the `StreamSource`
    // registered right after.
    reg.handler("terminals.output", |rt, input, _key, _rev| {
        Box::pin(terminals_output_unary(rt, input))
    });
    reg.handler("terminals.close", |rt, input, key, _rev| {
        Box::pin(terminals_close(rt, input, key))
    });
    crate::public_v1::ws::register_stream_source(
        "terminals.output",
        super::terminals_stream::terminal_output_source,
    );
}

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct StoredTerminal {
    pub terminal_id: String,
    pub project_id: String,
    pub working_directory: Option<String>,
    pub shell_profile_id: Option<String>,
    pub server_epoch: String,
    pub state: String,
    pub control_epoch: i64,
    pub controller_id: Option<String>,
    pub lease_expires_ms: Option<i64>,
    pub last_accepted_input_seq: i64,
    pub output_epoch: String,
    pub earliest_output_offset: i64,
    pub latest_output_offset: i64,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<String>,
    pub rows: i64,
    pub columns: i64,
}

fn terminal_err(code: &str, message: impl Into<String>) -> ContractError {
    ContractError::new(code, message, RetrySpec::never(), EffectStatus::NotApplied)
}

fn storage_err(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message,
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn map_sql(err: rusqlite::Error) -> ContractError {
    storage_err(format!("sqlite storage error: {err}"))
}

/// The boot generation: `serve_v1_gateway` mints a fresh one per start, so
/// a stored `serverEpoch` that differs marks the terminal as `lost` (its
/// master fd died with the old process).
fn current_server_epoch(store: &V1Store) -> String {
    store.meta().generation.clone()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| now_rfc3339())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn is_effective_terminal_state(state: &str) -> bool {
    matches!(state, "exited" | "failed" | "lost")
}

/// The state a client observes: `lost` for every non-terminal row whose
/// epoch died with a previous Gateway boot.
fn effective_state(row: &StoredTerminal, current_epoch: &str) -> String {
    if !is_effective_terminal_state(&row.state) && row.server_epoch != current_epoch {
        return "lost".to_owned();
    }
    row.state.clone()
}

fn terminal_to_json(
    row: &StoredTerminal,
    current_epoch: &str,
    live: Option<&Arc<TerminalHandle>>,
) -> Value {
    let mut state = effective_state(row, current_epoch);
    let mut earliest = row.earliest_output_offset;
    let mut latest = row.latest_output_offset;
    let mut exit_code = row.exit_code;
    let mut exit_signal = row.exit_signal.clone();
    if let Some(handle) = live {
        let (ring_earliest, ring_latest) = handle.ring_positions();
        earliest = ring_earliest as i64;
        latest = ring_latest as i64;
        if let Some(facts) = handle.exit_facts() {
            state = "exited".to_owned();
            exit_code = facts.exit_code.map(i64::from);
            exit_signal = facts.exit_signal.clone();
        }
    }
    json!({
        "terminalId": row.terminal_id,
        "projectId": row.project_id,
        "serverEpoch": row.server_epoch,
        "state": state,
        "controlEpoch": row.control_epoch,
        "controllerId": row.controller_id,
        "leaseExpiresAt": row.lease_expires_ms.map(ms_to_rfc3339),
        "lastAcceptedInputSeq": row.last_accepted_input_seq,
        "outputEpoch": row.output_epoch,
        "earliestOutputOffset": earliest,
        "latestOutputOffset": latest,
        "exitCode": exit_code,
        "exitSignal": exit_signal,
        "rows": row.rows,
        "columns": row.columns,
    })
}

fn fetch_terminal_row(
    conn: &Connection,
    terminal_id: &str,
) -> Result<Option<StoredTerminal>, ContractError> {
    let row = conn
        .query_row(
            "SELECT terminal_id, project_id, working_directory, shell_profile_id, server_epoch,
                    state, control_epoch, controller_id, lease_expires_ms, last_accepted_input_seq,
                    output_epoch, earliest_output_offset, latest_output_offset, exit_code,
                    exit_signal, term_rows, term_columns
             FROM terminals WHERE terminal_id = ?1",
            params![terminal_id],
            |r| {
                Ok(StoredTerminal {
                    terminal_id: r.get(0)?,
                    project_id: r.get(1)?,
                    working_directory: r.get(2)?,
                    shell_profile_id: r.get(3)?,
                    server_epoch: r.get(4)?,
                    state: r.get(5)?,
                    control_epoch: r.get(6)?,
                    controller_id: r.get(7)?,
                    lease_expires_ms: r.get(8)?,
                    last_accepted_input_seq: r.get(9)?,
                    output_epoch: r.get(10)?,
                    earliest_output_offset: r.get(11)?,
                    latest_output_offset: r.get(12)?,
                    exit_code: r.get(13)?,
                    exit_signal: r.get(14)?,
                    rows: r.get(15)?,
                    columns: r.get(16)?,
                })
            },
        )
        .optional()
        .map_err(map_sql)?;
    Ok(row)
}

/// Appends one domain event to the shared `events` table (keyed by
/// `terminal_id`), waking the run-event pumps exactly like every other
/// writer. Mirrors the store's two-step cursor insert.
fn append_terminal_event(
    conn: &Connection,
    resource_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    let placeholder = format!(
        "tmp-term-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', ?3, 'agentcpm-pd.terminal.event/1', ?4, ?4, ?5)",
        params![
            placeholder,
            resource_id,
            event_type,
            payload_str,
            now_rfc3339()
        ],
    )
    .map_err(map_sql)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![crate::public_v1::store::format_cursor(seq), seq],
    )
    .map_err(map_sql)?;
    Ok(())
}

fn next_domain_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(map_sql)?;
    Ok(format!("rev-{next:08}"))
}

impl V1Store {
    /// Reads one terminal row (no live overlay — callers overlay through
    /// [`terminal_to_json`]).
    pub(crate) fn terminal_row(&self, terminal_id: &str) -> Result<StoredTerminal, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        fetch_terminal_row(&conn, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })
    }

    /// Lists the project's terminals in `terminal_id` order, continuing
    /// after `cursor` (the last id of the previous page).
    pub(crate) fn terminal_rows_for_project(
        &self,
        project_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredTerminal>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let mut stmt = conn
            .prepare(
                "SELECT terminal_id, project_id, working_directory, shell_profile_id, server_epoch,
                        state, control_epoch, controller_id, lease_expires_ms, last_accepted_input_seq,
                        output_epoch, earliest_output_offset, latest_output_offset, exit_code,
                        exit_signal, term_rows, term_columns
                 FROM terminals WHERE project_id = ?1 AND (?2 IS NULL OR terminal_id > ?2)
                 ORDER BY terminal_id LIMIT ?3",
            )
            .map_err(map_sql)?;
        let rows = stmt
            .query_map(params![project_id, cursor, limit as i64], |r| {
                Ok(StoredTerminal {
                    terminal_id: r.get(0)?,
                    project_id: r.get(1)?,
                    working_directory: r.get(2)?,
                    shell_profile_id: r.get(3)?,
                    server_epoch: r.get(4)?,
                    state: r.get(5)?,
                    control_epoch: r.get(6)?,
                    controller_id: r.get(7)?,
                    lease_expires_ms: r.get(8)?,
                    last_accepted_input_seq: r.get(9)?,
                    output_epoch: r.get(10)?,
                    earliest_output_offset: r.get(11)?,
                    latest_output_offset: r.get(12)?,
                    exit_code: r.get(13)?,
                    exit_signal: r.get(14)?,
                    rows: r.get(15)?,
                    columns: r.get(16)?,
                })
            })
            .map_err(map_sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql)?;
        Ok(rows)
    }

    /// Inserts a freshly spawned terminal row and its `terminals.changed`
    /// event atomically.
    pub(crate) fn terminal_insert(&self, terminal: &StoredTerminal) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql)?;
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO terminals (terminal_id, project_id, working_directory, shell_profile_id,
                server_epoch, state, control_epoch, controller_id, lease_token_hash, lease_expires_ms,
                last_accepted_input_seq, last_input_digest, output_epoch, earliest_output_offset,
                latest_output_offset, exit_code, exit_signal, term_rows, term_columns, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL, 0, NULL, ?8, 0, 0, NULL, NULL, ?9, ?10, ?11, ?11)",
            params![
                terminal.terminal_id,
                terminal.project_id,
                terminal.working_directory,
                terminal.shell_profile_id,
                terminal.server_epoch,
                terminal.state,
                terminal.control_epoch,
                terminal.output_epoch,
                terminal.rows,
                terminal.columns,
                now
            ],
        )
        .map_err(map_sql)?;
        append_terminal_event(
            &tx,
            &terminal.terminal_id,
            "terminals.changed",
            &json!({"terminalId": terminal.terminal_id, "state": terminal.state}),
        )?;
        tx.commit().map_err(map_sql)?;
        self.event_notifier().notify_waiters();
        Ok(())
    }

    /// Applies a control-lease transition. Returns the updated row plus the
    /// raw lease token (only the hash is persisted).
    pub(crate) fn terminal_control_lease(
        &self,
        terminal_id: &str,
        action: &str,
        expected_control_epoch: i64,
        lease_token: Option<&str>,
    ) -> Result<(StoredTerminal, String), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql)?;
        let row = fetch_terminal_row(&tx, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })?;
        let epoch = current_server_epoch(self);
        if effective_state(&row, &epoch) != row.state {
            return Err(terminal_err(
                CODE_CONTROL_CONFLICT,
                format!(
                    "terminal `{terminal_id}` is {} and accepts no control transitions",
                    effective_state(&row, &epoch)
                ),
            ));
        }
        if is_effective_terminal_state(&row.state) {
            return Err(terminal_err(
                CODE_CONTROL_CONFLICT,
                format!(
                    "terminal `{terminal_id}` is `{}` and accepts no control transitions",
                    row.state
                ),
            ));
        }
        if row.control_epoch != expected_control_epoch {
            return Err(terminal_err(
                CODE_CONTROL_CONFLICT,
                format!(
                    "expectedControlEpoch `{expected_control_epoch}` does not match `{}`",
                    row.control_epoch
                ),
            ));
        }
        let lease_alive = row
            .lease_expires_ms
            .is_some_and(|expires| now_ms() < expires);
        let token_hash = row.lease_token_hash.as_deref();
        let token_matches = token_hash
            .zip(lease_token)
            .is_some_and(|(stored, presented)| stored == &sha256_hex(presented.as_bytes()));

        let new_epoch = row.control_epoch + 1;
        let fresh_token = uuid::Uuid::new_v4().to_string();
        let new_expires = now_ms() + LEASE_TTL_SECS * 1000;
        let new_hash = sha256_hex(fresh_token.as_bytes());

        let updated = match action {
            "acquire" => {
                if lease_alive {
                    return Err(terminal_err(
                        CODE_CONTROL_CONFLICT,
                        format!(
                            "terminal `{terminal_id}` already has an active controller; use takeover to take it over"
                        ),
                    ));
                }
                ("acquire", new_epoch, Some(new_hash), Some(new_expires))
            }
            "takeover" => ("takeover", new_epoch, Some(new_hash), Some(new_expires)),
            "renew" => {
                if lease_token.is_none() {
                    return Err(terminal_err(
                        CODE_INVALID_ARGUMENT,
                        "renew requires leaseToken",
                    ));
                }
                if !lease_alive {
                    return Err(terminal_err(
                        CODE_LEASE_EXPIRED,
                        format!("lease on terminal `{terminal_id}` expired; acquire again"),
                    ));
                }
                if !token_matches {
                    return Err(terminal_err(
                        CODE_CONTROL_CONFLICT,
                        "leaseToken does not match the active controller",
                    ));
                }
                (
                    "renew",
                    row.control_epoch,
                    token_hash.map(ToOwned::to_owned),
                    Some(new_expires),
                )
            }
            "release" => {
                if lease_token.is_none() {
                    return Err(terminal_err(
                        CODE_INVALID_ARGUMENT,
                        "release requires leaseToken",
                    ));
                }
                if !lease_alive {
                    return Err(terminal_err(
                        CODE_LEASE_EXPIRED,
                        format!("lease on terminal `{terminal_id}` expired; acquire again"),
                    ));
                }
                if !token_matches {
                    return Err(terminal_err(
                        CODE_CONTROL_CONFLICT,
                        "leaseToken does not match the active controller",
                    ));
                }
                ("release", new_epoch, None, None)
            }
            other => {
                return Err(terminal_err(
                    CODE_INVALID_ARGUMENT,
                    format!("unsupported control action `{other}`"),
                ));
            }
        };
        let (action_name, control_epoch, hash, expires) = updated;
        tx.execute(
            "UPDATE terminals SET control_epoch = ?1, lease_token_hash = ?2, lease_expires_ms = ?3, updated_at = ?4 WHERE terminal_id = ?5",
            params![control_epoch, hash, expires, now_rfc3339(), terminal_id],
        )
        .map_err(map_sql)?;
        append_terminal_event(
            &tx,
            terminal_id,
            "terminals.controlChanged",
            &json!({"terminalId": terminal_id, "controlEpoch": control_epoch, "action": action_name}),
        )?;
        let mut row = fetch_terminal_row(&tx, terminal_id)?.expect("updated terminal");
        row.control_epoch = control_epoch;
        row.lease_expires_ms = expires;
        tx.commit().map_err(map_sql)?;
        self.event_notifier().notify_waiters();
        let token = if action_name == "renew" {
            lease_token.expect("renew was validated above").to_owned()
        } else {
            fresh_token
        };
        Ok((row, token))
    }

    /// Validates and records one ordered input acceptance. Returns whether
    /// the input is a same-bytes replay (`deduplicated`) of the immediately
    /// previous sequence number.
    pub(crate) fn terminal_accept_input(
        &self,
        terminal_id: &str,
        server_epoch: &str,
        control_epoch: i64,
        lease_token: &str,
        input_seq: i64,
        bytes: &[u8],
    ) -> Result<bool, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql)?;
        let row = fetch_terminal_row(&tx, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })?;
        let gateway_epoch = current_server_epoch(self);
        let effective = effective_state(&row, &gateway_epoch);
        if effective == "lost" {
            return Err(terminal_err(
                CODE_TERMINAL_CLOSED,
                format!(
                    "terminal `{terminal_id}` was lost after a gateway restart (serverEpoch `{server_epoch}` no longer matches `{gateway_epoch}`); inputs are never replayed across server epochs"
                ),
            ));
        }
        if is_effective_terminal_state(&row.state) {
            return Err(terminal_err(
                CODE_TERMINAL_CLOSED,
                format!(
                    "terminal `{terminal_id}` is `{}` and accepts no input",
                    row.state
                ),
            ));
        }
        if row.server_epoch != server_epoch {
            return Err(terminal_err(
                CODE_TERMINAL_CLOSED,
                format!(
                    "serverEpoch `{server_epoch}` does not match `{}`; the gateway restarted and this terminal is lost",
                    row.server_epoch
                ),
            ));
        }
        if row.control_epoch != control_epoch {
            return Err(terminal_err(
                CODE_CONTROL_CONFLICT,
                format!(
                    "controlEpoch `{control_epoch}` does not match `{}`",
                    row.control_epoch
                ),
            ));
        }
        let lease_alive = row
            .lease_expires_ms
            .is_some_and(|expires| now_ms() < expires);
        if !lease_alive {
            return Err(terminal_err(
                CODE_LEASE_EXPIRED,
                format!("lease on terminal `{terminal_id}` expired; acquire again"),
            ));
        }
        if row.lease_token_hash.as_deref() != Some(&sha256_hex(lease_token.as_bytes())) {
            return Err(terminal_err(
                CODE_CONTROL_CONFLICT,
                "leaseToken does not match the active controller",
            ));
        }

        let last = row.last_accepted_input_seq;
        let digest = sha256_hex(bytes);
        if input_seq == last {
            if row.last_input_digest.as_deref() == Some(&digest) {
                return Ok(true);
            }
            return Err(terminal_err(
                CODE_INPUT_SEQUENCE_CONFLICT,
                format!("inputSeq `{input_seq}` was already accepted with different bytes"),
            ));
        }
        if input_seq < last {
            return Err(terminal_err(
                CODE_INPUT_SEQUENCE_CONFLICT,
                format!(
                    "inputSeq `{input_seq}` is behind the accepted watermark `{last}`; replay of an old sequence is a conflict"
                ),
            ));
        }
        if input_seq > last + 1 {
            return Err(terminal_err(
                CODE_INPUT_SEQUENCE_GAP,
                format!(
                    "inputSeq `{input_seq}` leaves a gap after the accepted watermark `{last}`; inputs must be contiguous"
                ),
            ));
        }
        tx.execute(
            "UPDATE terminals SET last_accepted_input_seq = ?1, last_input_digest = ?2, updated_at = ?3 WHERE terminal_id = ?4",
            params![input_seq, digest, now_rfc3339(), terminal_id],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(false)
    }

    /// Validates and records a resize.
    pub(crate) fn terminal_resize(
        &self,
        terminal_id: &str,
        control_epoch: i64,
        lease_token: &str,
        rows: i64,
        columns: i64,
    ) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql)?;
        let row = fetch_terminal_row(&tx, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })?;
        let epoch = current_server_epoch(self);
        if effective_state(&row, &epoch) != row.state || is_effective_terminal_state(&row.state) {
            return Err(terminal_err(
                CODE_TERMINAL_CLOSED,
                format!(
                    "terminal `{terminal_id}` is `{}` and accepts no resize",
                    effective_state(&row, &epoch)
                ),
            ));
        }
        if row.control_epoch != control_epoch {
            return Err(terminal_err(
                CODE_CONTROL_CONFLICT,
                format!(
                    "controlEpoch `{control_epoch}` does not match `{}`",
                    row.control_epoch
                ),
            ));
        }
        let lease_alive = row
            .lease_expires_ms
            .is_some_and(|expires| now_ms() < expires);
        if !lease_alive {
            return Err(terminal_err(
                CODE_LEASE_EXPIRED,
                format!("lease on terminal `{terminal_id}` expired; acquire again"),
            ));
        }
        if row.lease_token_hash.as_deref() != Some(&sha256_hex(lease_token.as_bytes())) {
            return Err(terminal_err(
                CODE_CONTROL_CONFLICT,
                "leaseToken does not match the active controller",
            ));
        }
        tx.execute(
            "UPDATE terminals SET term_rows = ?1, term_columns = ?2, updated_at = ?3 WHERE terminal_id = ?4",
            params![rows, columns, now_rfc3339(), terminal_id],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(())
    }

    /// Durable exit finalization (the supervisor's `on_exit`): records the
    /// exit facts and ring offsets, and emits `terminals.changed`.
    pub(crate) fn terminal_finalize_exit(
        &self,
        terminal_id: &str,
        facts: &ExitFacts,
        ring_earliest: u64,
        ring_latest: u64,
    ) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql)?;
        tx.execute(
            "UPDATE terminals SET state = 'exited', exit_code = ?1, exit_signal = ?2,
                    earliest_output_offset = ?3, latest_output_offset = ?4, updated_at = ?5
             WHERE terminal_id = ?6",
            params![
                facts.exit_code,
                facts.exit_signal,
                ring_earliest as i64,
                ring_latest as i64,
                now_rfc3339(),
                terminal_id
            ],
        )
        .map_err(map_sql)?;
        append_terminal_event(
            &tx,
            terminal_id,
            "terminals.changed",
            &json!({
                "terminalId": terminal_id,
                "state": "exited",
                "exitCode": facts.exit_code,
                "exitSignal": facts.exit_signal,
            }),
        )?;
        tx.commit().map_err(map_sql)?;
        self.event_notifier().notify_waiters();
        Ok(())
    }

    /// Marks every non-terminal row from a foreign server epoch as `lost`
    /// (read-time reconciliation entry point used by `terminals.get`'s
    /// fresh-state check when the live handle is also gone).
    pub(crate) fn terminal_row_effective(
        &self,
        terminal_id: &str,
    ) -> Result<Option<StoredTerminal>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        fetch_terminal_row(&conn, terminal_id)
    }

    /// Records and progresses one `terminals.close` operation.
    pub(crate) fn terminal_close_operation(
        &self,
        operation_id: &str,
        terminal_id: &str,
        state: &str,
        result: Option<&Value>,
    ) -> Result<String, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let now = now_rfc3339();
        let revision = next_domain_rev(&conn)?;
        conn.execute(
            "INSERT INTO terminal_close_operations (operation_id, terminal_id, state, revision, result_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(operation_id) DO UPDATE SET state = ?3, result_json = ?5, updated_at = ?6",
            params![
                operation_id,
                terminal_id,
                state,
                revision,
                result.and_then(|r| serde_json::to_string(r).ok()),
                now
            ],
        )
        .map_err(map_sql)?;
        Ok(revision)
    }

    /// The recorded `(state, revision)` of a close operation, for the
    /// idempotent replay's receipt.
    pub(crate) fn terminal_close_operation_state(
        &self,
        operation_id: &str,
    ) -> Option<(String, String)> {
        let conn = self.conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT state, revision FROM terminal_close_operations WHERE operation_id = ?1",
            params![operation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// Idempotency intake shared by `terminals.create` and
    /// `terminals.close` (the `idempotency_records` table `run.submit`
    /// uses): same key + same input replays the stored resource id; same
    /// key + different input is `IDEMPOTENCY_CONFLICT`.
    pub(crate) fn terminal_idempotency_replay(
        &self,
        method_id: &str,
        key: &str,
        digest: &str,
    ) -> Result<Option<String>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT input_digest, resource_json FROM idempotency_records
                 WHERE idempotency_key = ?1 AND method_id = ?2",
                params![key, method_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(map_sql)?;
        let Some((stored_digest, resource_json)) = existing else {
            return Ok(None);
        };
        if stored_digest != digest {
            return Err(terminal_err(
                CODE_IDEMPOTENCY_CONFLICT,
                format!("idempotencyKey `{key}` already used for {method_id} with different input"),
            ));
        }
        let resource: Value = serde_json::from_str(&resource_json).unwrap_or(Value::Null);
        Ok(resource
            .get("terminalId")
            .or(resource.get("operationId"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }

    pub(crate) fn terminal_idempotency_record(
        &self,
        method_id: &str,
        key: &str,
        digest: &str,
        resource: &Value,
    ) -> Result<(), ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let retained_until = (chrono::Utc::now()
            + chrono::Duration::seconds(IDEMPOTENCY_RETENTION_SECS))
        .to_rfc3339();
        conn.execute(
            "INSERT OR REPLACE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
             VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, NULL, NULL, ?5, ?6, 'doNotReplay')",
            params![
                key,
                method_id,
                digest,
                serde_json::to_string(resource).unwrap_or_default(),
                now_rfc3339(),
                retained_until
            ],
        )
        .map_err(map_sql)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn digest_of(method_id: &str, input: &Value) -> String {
    agentcpm_pd_protocol::envelope::compute_input_digest(method_id, input)
}

/// Resolves the terminal's working directory inside the project's
/// authorized root: an absolute path must canonicalize inside the root, a
/// relative one joins it. `None` means the project root itself.
fn resolve_working_directory(
    store: &Arc<V1Store>,
    project: &Value,
    working_directory: Option<&Value>,
) -> Result<String, ContractError> {
    let root_id = project["root"]["rootId"].as_str().unwrap_or_default();
    let project_root = store.resolve_authorized_root(root_id)?;
    let canonical_root = project_root.canonicalize().map_err(|e| {
        terminal_err(
            CODE_INVALID_ARGUMENT,
            format!(
                "project root `{}` is not usable: {e}",
                project_root.display()
            ),
        )
    })?;
    let target = match working_directory {
        None => canonical_root,
        Some(raw) => {
            let path = crate::public_v1::domains::workspace_dirs::decode_byte_path(raw)?;
            let joined = if path.is_absolute() {
                path
            } else {
                canonical_root.join(path)
            };
            let canonical = joined.canonicalize().map_err(|e| {
                terminal_err(
                    CODE_INVALID_ARGUMENT,
                    format!(
                        "working directory `{}` does not exist: {e}",
                        joined.display()
                    ),
                )
            })?;
            if !canonical.starts_with(&canonical_root) {
                return Err(terminal_err(
                    CODE_PATH_ESCAPE,
                    format!(
                        "working directory `{}` escapes the project root `{}`",
                        canonical.display(),
                        canonical_root.display()
                    ),
                ));
            }
            canonical
        }
    };
    if !target.is_dir() {
        return Err(terminal_err(
            CODE_INVALID_ARGUMENT,
            format!(
                "working directory `{}` is not a directory",
                target.display()
            ),
        ));
    }
    Ok(target.to_string_lossy().into_owned())
}

async fn terminals_create(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let digest = digest_of("terminals.create", &input);

    // Same-request dedup (contract acceptance): the key replays the same
    // terminal's current state.
    if let Some(key) = idempotency_key.as_deref() {
        if let Some(existing_id) =
            store.terminal_idempotency_replay("terminals.create", key, &digest)?
        {
            let row = store.terminal_row(&existing_id)?;
            let live = live_terminal(&existing_id);
            let epoch = current_server_epoch(store);
            return Ok((
                ResponseOutcome::Succeeded,
                json!({"terminal": terminal_to_json(&row, &epoch, live.as_ref())}),
            ));
        }
    }

    // The project must exist; its authorized root bounds the working dir.
    let project = store.get_project(&project_id)?["project"].clone();
    if let Some(profile) = input["shellProfileId"].as_str().filter(|p| !p.is_empty()) {
        // No shell-profile registry exists in this round; a declared
        // profile that cannot be resolved must fail the create outright
        // (contract: SHELL_UNAVAILABLE, never a fake running terminal).
        return Err(terminal_err(
            CODE_SHELL_UNAVAILABLE,
            format!(
                "shellProfileId `{profile}` cannot be resolved; no shell profiles are configured on this instance"
            ),
        ));
    }
    let working_directory = resolve_working_directory(
        store,
        &project,
        input.get("workingDirectory").filter(|v| !v.is_null()),
    )?;

    let rows = input["rows"]
        .as_i64()
        .unwrap_or(24)
        .clamp(1, i64::from(u16::MAX));
    let columns = input["columns"]
        .as_i64()
        .unwrap_or(80)
        .clamp(1, i64::from(u16::MAX));

    // Defensive live-terminal cap before any spawn side effect.
    if live_terminal_count() >= MAX_LIVE_TERMINALS {
        return Err(terminal_err(
            CODE_RESOURCE_LIMIT,
            format!("this instance already hosts {MAX_LIVE_TERMINALS} live terminals"),
        ));
    }

    let terminal_id = store.next_id();
    let output_epoch = store.next_id();
    // Spawn failure answers SHELL_UNAVAILABLE synchronously with no row —
    // the contract forbids leaving a fake `running` behind.
    let spawned = terminals_pty::spawn_shell(
        &terminal_id,
        std::path::Path::new(&working_directory),
        rows as u16,
        columns as u16,
    )
    .map_err(|e| {
        terminal_err(
            CODE_SHELL_UNAVAILABLE,
            format!("failed to spawn the interactive shell in `{working_directory}`: {e}"),
        )
    })?;

    let row = StoredTerminal {
        terminal_id: terminal_id.clone(),
        project_id,
        working_directory: Some(working_directory),
        shell_profile_id: None,
        server_epoch: current_server_epoch(store),
        state: "running".to_owned(),
        control_epoch: 0,
        controller_id: None,
        lease_expires_ms: None,
        last_accepted_input_seq: 0,
        output_epoch,
        earliest_output_offset: 0,
        latest_output_offset: 0,
        exit_code: None,
        exit_signal: None,
        rows,
        columns,
    };
    store.terminal_insert(&row)?;
    if let Some(key) = idempotency_key.as_deref() {
        store.terminal_idempotency_record(
            "terminals.create",
            key,
            &digest,
            &json!({"kind": "terminal", "terminalId": terminal_id}),
        )?;
    }

    let finalize_store = store.clone();
    let finalize_handle = spawned.handle.clone();
    let finalize_id = terminal_id.clone();
    let live_handle = spawned.handle.clone();
    terminals_pty::supervise(spawned.handle, spawned.child, move |facts: ExitFacts| {
        let (earliest, latest) = finalize_handle.ring_positions();
        let _ = finalize_store.terminal_finalize_exit(&finalize_id, &facts, earliest, latest);
    });

    Ok((
        ResponseOutcome::Succeeded,
        json!({"terminal": terminal_to_json(&row, &row.server_epoch, Some(&live_handle))}),
    ))
}

async fn terminals_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let page = input.get("page").cloned().unwrap_or(json!({}));
    let limit = page["limit"].as_u64().unwrap_or(100).clamp(1, 500) as usize;
    let cursor = page["cursor"].as_str().map(ToOwned::to_owned);
    let rows = store.terminal_rows_for_project(&project_id, cursor.as_deref(), limit)?;
    let epoch = current_server_epoch(store);
    let terminals: Vec<Value> = rows
        .iter()
        .map(|row| {
            let live = live_terminal(&row.terminal_id);
            terminal_to_json(row, &epoch, live.as_ref())
        })
        .collect();
    let next_cursor = if terminals.len() >= limit {
        rows.last().map(|row| row.terminal_id.clone())
    } else {
        None
    };
    let mut result = json!({"terminals": terminals});
    if let Some(cursor) = next_cursor {
        result["nextCursor"] = json!(cursor);
    }
    Ok((ResponseOutcome::Succeeded, result))
}

async fn terminals_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let terminal_id = input["terminalId"].as_str().unwrap_or_default();
    let row = store.terminal_row(terminal_id)?;
    let live = live_terminal(terminal_id);
    Ok((
        ResponseOutcome::Succeeded,
        json!({"terminal": terminal_to_json(&row, &current_server_epoch(store), live.as_ref())}),
    ))
}

async fn terminals_control(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let terminal_id = input["terminalId"].as_str().unwrap_or_default().to_owned();
    let action = input["action"].as_str().unwrap_or_default().to_owned();
    let expected = input["expectedControlEpoch"].as_i64().unwrap_or(-1);
    let lease_token = input["leaseToken"].as_str().map(ToOwned::to_owned);
    let (row, token) =
        store.terminal_control_lease(&terminal_id, &action, expected, lease_token.as_deref())?;
    let live = live_terminal(&terminal_id);
    let lease = json!({
        "terminalId": terminal_id,
        "controlEpoch": row.control_epoch,
        "leaseToken": token,
        "expiresAt": row.lease_expires_ms.map(ms_to_rfc3339).unwrap_or_default(),
        "lastAcceptedInputSeq": row.last_accepted_input_seq,
        "previousAcceptedWatermark": row.last_accepted_input_seq,
    });
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "terminal": terminal_to_json(&row, &current_server_epoch(store), live.as_ref()),
            "lease": lease,
        }),
    ))
}

async fn terminals_input(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let terminal_id = input["terminalId"].as_str().unwrap_or_default().to_owned();
    let server_epoch = input["serverEpoch"].as_str().unwrap_or_default().to_owned();
    let control_epoch = input["controlEpoch"].as_i64().unwrap_or(-1);
    let lease_token = input["leaseToken"].as_str().unwrap_or_default().to_owned();
    let input_seq = input["inputSeq"].as_i64().unwrap_or(0);
    let bytes = decode_input_bytes(input["bytesBase64"].as_str().unwrap_or_default())?;

    let Some(handle) = live_terminal(&terminal_id) else {
        let row = store.terminal_row(&terminal_id)?;
        let epoch = current_server_epoch(store);
        return Err(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!(
                "terminal `{terminal_id}` has no live pty (state `{}`)",
                effective_state(&row, &epoch)
            ),
        ));
    };

    // Single-writer lane: validation, byte write, and ack are serialized per
    // terminal so two racing inputs cannot interleave on the master.
    let guard = handle.write_lock.lock().await;
    if !handle.inputs_open() {
        return Err(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!("terminal `{terminal_id}` stopped accepting input (close in progress)"),
        ));
    }
    let deduplicated = store.terminal_accept_input(
        &terminal_id,
        &server_epoch,
        control_epoch,
        &lease_token,
        input_seq,
        &bytes,
    )?;
    if !deduplicated {
        if let Err(e) = handle.write_input(&bytes).await {
            // The terminal died between acceptance and delivery; the ack
            // must not claim success.
            return Err(terminal_err(
                CODE_TERMINAL_CLOSED,
                format!("terminal `{terminal_id}` no longer accepts input: {e}"),
            ));
        }
    }
    drop(guard);
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "terminalId": terminal_id,
            "controlEpoch": control_epoch,
            "acceptedThroughSeq": input_seq,
            "acceptedBytes": bytes.len(),
            "deduplicated": deduplicated,
            "ackMeaning": "acceptedByServer",
        }),
    ))
}

fn decode_input_bytes(encoded: &str) -> Result<Vec<u8>, ContractError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| {
            terminal_err(
                CODE_INVALID_ARGUMENT,
                format!("bytesBase64 is not valid base64: {e}"),
            )
        })
}

async fn terminals_resize(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let terminal_id = input["terminalId"].as_str().unwrap_or_default().to_owned();
    let control_epoch = input["controlEpoch"].as_i64().unwrap_or(-1);
    let lease_token = input["leaseToken"].as_str().unwrap_or_default().to_owned();
    let rows = input["rows"].as_i64().unwrap_or(0);
    let columns = input["columns"].as_i64().unwrap_or(0);
    let Some(handle) = live_terminal(&terminal_id) else {
        let row = store.terminal_row(&terminal_id)?;
        let epoch = current_server_epoch(store);
        return Err(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!(
                "terminal `{terminal_id}` has no live pty (state `{}`)",
                effective_state(&row, &epoch)
            ),
        ));
    };
    store.terminal_resize(&terminal_id, control_epoch, &lease_token, rows, columns)?;
    handle
        .set_winsize(rows as u16, columns as u16)
        .map_err(|e| {
            terminal_err(
                CODE_TERMINAL_CLOSED,
                format!("terminal `{terminal_id}` could not be resized: {e}"),
            )
        })?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"rows": rows, "columns": columns}),
    ))
}

/// Unary `terminals.output` (non-follow): one frame — the next retained
/// bytes, a gap when the cursor fell behind, the exit facts once the
/// terminal ended, or a heartbeat frame when caught up on a live terminal.
/// The follow stream is the `StreamSource` in `terminals_stream.rs`.
async fn terminals_output_unary(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let terminal_id = input["terminalId"].as_str().unwrap_or_default();
    let row = store.terminal_row(terminal_id)?;
    let live = live_terminal(terminal_id);
    let offset = read_cursor(&input, &row, live.as_ref())?;
    let frame = if let Some(handle) = live {
        let (earliest, _latest) = handle.ring_positions();
        if offset < earliest {
            gap_frame(&row, offset, earliest)
        } else {
            let chunk = handle
                .copy_output(offset, terminals_pty::OUTPUT_FRAME_BYTES)
                .unwrap_or_default();
            if chunk.is_empty() {
                if let Some(facts) = handle.exit_facts() {
                    exit_frame(&row, offset, &facts)
                } else {
                    heartbeat_frame(&row, offset)
                }
            } else {
                let next = offset + chunk.len() as u64;
                bytes_frame(&row, offset, next, &chunk)
            }
        }
    } else {
        // No live pty: a terminal whose row is terminal reports its exit
        // facts; a non-terminal row here means the gateway restarted and
        // the terminal was lost.
        exit_frame(
            &row,
            offset,
            &ExitFacts {
                exit_code: row.exit_code.map(|c| c as i32),
                exit_signal: row.exit_signal.clone(),
            },
        )
    };
    Ok((ResponseOutcome::Succeeded, frame))
}

/// The byte offset to read from: the request's cursor (validated against the
/// output epoch and the latest offset) or the earliest retained byte.
pub(crate) fn read_cursor(
    input: &Value,
    row: &StoredTerminal,
    live: Option<&Arc<TerminalHandle>>,
) -> Result<u64, ContractError> {
    let Some(raw) = input.get("cursor").filter(|v| !v.is_null()) else {
        return Ok(live
            .map(|h| h.ring_positions().0)
            .unwrap_or(row.earliest_output_offset as u64));
    };
    let epoch = raw["outputEpoch"].as_str().unwrap_or_default();
    let offset = raw["byteOffset"].as_u64().unwrap_or(u64::MAX);
    if epoch != row.output_epoch {
        return Err(terminal_err(
            CODE_CURSOR_INVALID,
            format!(
                "cursor outputEpoch `{epoch}` does not match this terminal's `{}`",
                row.output_epoch
            ),
        ));
    }
    let latest = live
        .map(|h| h.ring_positions().1)
        .unwrap_or(row.latest_output_offset.max(row.earliest_output_offset) as u64);
    if offset > latest {
        return Err(terminal_err(
            CODE_CURSOR_INVALID,
            format!("cursor byteOffset `{offset}` is ahead of the output stream (`{latest}`)"),
        ));
    }
    Ok(offset)
}

fn cursor_json(row: &StoredTerminal, offset: u64) -> Value {
    json!({"outputEpoch": row.output_epoch, "byteOffset": offset})
}

/// The stream-cursor string form (`"<outputEpoch>:<byteOffset>"`), used as
/// the event envelope cursor and the `sub.end` resume cursor.
pub(crate) fn stream_cursor(row: &StoredTerminal, offset: u64) -> String {
    format!("{}:{offset}", row.output_epoch)
}

pub(crate) fn bytes_frame(row: &StoredTerminal, from: u64, next: u64, bytes: &[u8]) -> Value {
    use base64::Engine;
    json!({
        "type": "bytes",
        "cursor": cursor_json(row, from),
        "nextCursor": cursor_json(row, next),
        "bytesBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

pub(crate) fn gap_frame(row: &StoredTerminal, requested: u64, earliest: u64) -> Value {
    json!({
        "type": "gap",
        "cursor": cursor_json(row, requested),
        "nextCursor": cursor_json(row, earliest),
        "earliestAvailableCursor": cursor_json(row, earliest),
    })
}

pub(crate) fn exit_frame(row: &StoredTerminal, offset: u64, facts: &ExitFacts) -> Value {
    json!({
        "type": "exit",
        "cursor": cursor_json(row, offset),
        "nextCursor": cursor_json(row, offset),
        "exitCode": facts.exit_code,
        "exitSignal": facts.exit_signal,
    })
}

fn heartbeat_frame(row: &StoredTerminal, offset: u64) -> Value {
    json!({
        "type": "heartbeat",
        "cursor": cursor_json(row, offset),
        "nextCursor": cursor_json(row, offset),
    })
}

async fn terminals_close(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let terminal_id = input["terminalId"].as_str().unwrap_or_default().to_owned();
    let expected_epoch = input["expectedServerEpoch"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mode = input["mode"].as_str().unwrap_or("graceful").to_owned();
    let digest = digest_of("terminals.close", &input);

    // Validation precedes any durable acceptance record: a rejected close
    // (unknown terminal, wrong epoch, bad mode) must never poison a retry
    // of the same idempotencyKey.
    let row = store.terminal_row(&terminal_id)?;
    let epoch = current_server_epoch(store);
    let effective = effective_state(&row, &epoch);
    if row.server_epoch != expected_epoch {
        return Err(terminal_err(
            CODE_INVALID_ARGUMENT,
            format!(
                "expectedServerEpoch `{expected_epoch}` does not match `{}`",
                row.server_epoch
            ),
        ));
    }
    if mode != "graceful" && mode != "force" {
        return Err(terminal_err(
            CODE_INVALID_ARGUMENT,
            format!("unsupported close mode `{mode}`"),
        ));
    }

    // Same-key dedup replays the recorded operation id and never re-runs the
    // kill chain (the terminal's state is whatever the first attempt left).
    let mut deduplicated = false;
    let operation_id = if let Some(key) = idempotency_key.as_deref() {
        if let Some(existing) =
            store.terminal_idempotency_replay("terminals.close", key, &digest)?
        {
            deduplicated = true;
            existing
        } else {
            let operation_id = store.next_id();
            store.terminal_close_operation(&operation_id, &terminal_id, "queued", None)?;
            store.terminal_idempotency_record(
                "terminals.close",
                key,
                &digest,
                &json!({"kind": "operation", "operationId": operation_id, "terminalId": terminal_id}),
            )?;
            operation_id
        }
    } else {
        store.next_id()
    };

    if deduplicated {
        let (state, revision) = store
            .terminal_close_operation_state(&operation_id)
            .unwrap_or_else(|| ("running".to_owned(), "rev-00000001".to_owned()));
        return Ok((
            ResponseOutcome::Accepted,
            json!({
                "kind": "operation",
                "operationId": operation_id,
                "durable": true,
                "acceptedRevision": revision,
                "deduplicated": true,
                "state": state,
            }),
        ));
    }

    let revision = store.terminal_close_operation(&operation_id, &terminal_id, "running", None)?;

    // The kill chain: reject input first, then group-signal the session. A
    // terminal already in a terminal state (or lost) has nothing to kill —
    // the operation completes immediately.
    let live = live_terminal(&terminal_id);
    let already_done = effective != row.state || is_effective_terminal_state(&row.state);
    if let Some(handle) = live.as_ref().filter(|_| !already_done) {
        handle.request_close(mode == "graceful");
    }

    let completion_store = store.clone();
    let completion_id = terminal_id.clone();
    let completion_op = operation_id.clone();
    tokio::spawn(async move {
        // Bounded convergence: wait for the durable row to reach a terminal
        // state (the supervisor finalizes exits; a lost terminal — foreign
        // server epoch — counts as terminal here too), then complete the
        // receipt.
        let completion_epoch = current_server_epoch(&completion_store);
        for _ in 0..200 {
            let done = completion_store
                .terminal_row_effective(&completion_id)
                .ok()
                .flatten()
                .is_some_and(|row| {
                    is_effective_terminal_state(&effective_state(&row, &completion_epoch))
                });
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let terminal = completion_store
            .terminal_row_effective(&completion_id)
            .ok()
            .flatten()
            .map(|row| {
                let live = live_terminal(&completion_id);
                terminal_to_json(&row, &completion_epoch, live.as_ref())
            })
            .unwrap_or(Value::Null);
        let _ = completion_store.terminal_close_operation(
            &completion_op,
            &completion_id,
            "succeeded",
            Some(&json!({"terminal": terminal})),
        );
    });

    Ok((
        ResponseOutcome::Accepted,
        json!({
            "kind": "operation",
            "operationId": operation_id,
            "durable": true,
            "acceptedRevision": revision,
            "deduplicated": false,
            "state": "running",
        }),
    ))
}

// ---------------------------------------------------------------------------
// Tests (store-level contract cases; the pty lifecycle itself is covered by
// the e2e suite — the local build freeze forbids spawning here).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    fn test_store(tag: &str) -> Arc<V1Store> {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
        let iid = file.instance_id.clone();
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
        V1Store::open_and_recover(&root_dir, meta).unwrap()
    }

    fn seed_row(store: &V1Store, terminal_id: &str, epoch: &str) -> StoredTerminal {
        let row = StoredTerminal {
            terminal_id: terminal_id.to_owned(),
            project_id: "proj-t".to_owned(),
            working_directory: None,
            shell_profile_id: None,
            server_epoch: epoch.to_owned(),
            state: "running".to_owned(),
            control_epoch: 0,
            controller_id: None,
            lease_expires_ms: None,
            last_accepted_input_seq: 0,
            output_epoch: "oe-1".to_owned(),
            earliest_output_offset: 0,
            latest_output_offset: 0,
            exit_code: None,
            exit_signal: None,
            rows: 24,
            columns: 80,
        };
        store.terminal_insert(&row).unwrap();
        row
    }

    #[test]
    fn terminal_object_matches_the_contract_result_schema() {
        let store = test_store("term-json");
        let row = seed_row(&store, "term-1", &store.meta().generation);
        let value = terminal_to_json(&row, &store.meta().generation, None);
        let result = json!({"terminal": value});
        OperationRegistry::global()
            .validate_result("terminals.get", &result)
            .expect("the Terminal object passes the contract schema");
    }

    #[test]
    fn foreign_epoch_rows_read_as_lost() {
        let store = test_store("term-lost");
        let row = seed_row(&store, "term-2", "gen-old-boot");
        let effective = effective_state(&row, &store.meta().generation);
        assert_eq!(effective, "lost");
        // A closed/terminal row keeps its own state whatever the epoch.
        let mut exited = row.clone();
        exited.state = "exited".to_owned();
        assert_eq!(effective_state(&exited, "gen-other"), "exited");
    }

    #[test]
    fn lease_transitions_follow_the_contract_matrix() {
        let store = test_store("term-lease");
        let epoch = store.meta().generation.clone();
        seed_row(&store, "term-3", &epoch);

        // acquire with a stale expectedControlEpoch conflicts.
        let err = store
            .terminal_control_lease("term-3", "acquire", 7, None)
            .unwrap_err();
        assert_eq!(err.code, CODE_CONTROL_CONFLICT);

        // First acquire succeeds and bumps the epoch.
        let (row, token) = store
            .terminal_control_lease("term-3", "acquire", 0, None)
            .unwrap();
        assert_eq!(row.control_epoch, 1);
        assert!(!token.is_empty());

        // A second acquire while the lease is alive conflicts.
        let err = store
            .terminal_control_lease("term-3", "acquire", 1, None)
            .unwrap_err();
        assert_eq!(err.code, CODE_CONTROL_CONFLICT);

        // Renew with a wrong token conflicts; with the right one it extends.
        let err = store
            .terminal_control_lease("term-3", "renew", 1, Some("bogus"))
            .unwrap_err();
        assert_eq!(err.code, CODE_CONTROL_CONFLICT);
        let (row, renewed) = store
            .terminal_control_lease("term-3", "renew", 1, Some(&token))
            .unwrap();
        assert_eq!(renewed, token);
        assert_eq!(row.control_epoch, 1);
        assert!(row.lease_expires_ms.unwrap_or(0) > now_ms());

        // Takeover bumps the epoch even though the lease is alive.
        let (row, taken) = store
            .terminal_control_lease("term-3", "takeover", 1, None)
            .unwrap();
        assert_eq!(row.control_epoch, 2);
        assert_ne!(taken, token);

        // The old token can no longer renew.
        let err = store
            .terminal_control_lease("term-3", "renew", 2, Some(&token))
            .unwrap_err();
        assert_eq!(err.code, CODE_CONTROL_CONFLICT);

        // Release with the new token clears the lease and bumps the epoch.
        let (row, _) = store
            .terminal_control_lease("term-3", "release", 2, Some(&taken))
            .unwrap();
        assert_eq!(row.control_epoch, 3);
        assert!(row.lease_expires_ms.is_none());
    }

    #[test]
    fn input_sequence_matrix_accepts_dedups_and_rejects_gaps() {
        let store = test_store("term-input");
        let epoch = store.meta().generation.clone();
        seed_row(&store, "term-4", &epoch);
        let (row, token) = store
            .terminal_control_lease("term-4", "acquire", 0, None)
            .unwrap();
        let ce = row.control_epoch;

        // Contiguous input is accepted.
        assert!(
            !store
                .terminal_accept_input("term-4", &epoch, ce, &token, 1, b"ls\n")
                .unwrap()
        );

        // Same seq + same bytes deduplicates.
        assert!(
            store
                .terminal_accept_input("term-4", &epoch, ce, &token, 1, b"ls\n")
                .unwrap()
        );

        // Same seq + different bytes conflicts.
        let err = store
            .terminal_accept_input("term-4", &epoch, ce, &token, 1, b"rm\n")
            .unwrap_err();
        assert_eq!(err.code, CODE_INPUT_SEQUENCE_CONFLICT);

        // Skipping ahead is a gap.
        let err = store
            .terminal_accept_input("term-4", &epoch, ce, &token, 3, b"x")
            .unwrap_err();
        assert_eq!(err.code, CODE_INPUT_SEQUENCE_GAP);

        // A stale epoch/token pair is refused before the sequence logic.
        let err = store
            .terminal_accept_input("term-4", "gen-other", ce, &token, 2, b"x")
            .unwrap_err();
        assert_eq!(err.code, CODE_TERMINAL_CLOSED);

        // The accepted watermark advanced to 1, not past it.
        let row = store.terminal_row("term-4").unwrap();
        assert_eq!(row.last_accepted_input_seq, 1);
    }

    #[test]
    fn frame_builders_match_the_contract_frame_types() {
        let store = test_store("term-frames");
        let row = seed_row(&store, "term-5", &store.meta().generation);
        for (frame, kind) in [
            (bytes_frame(&row, 0, 4, b"data"), "bytes"),
            (gap_frame(&row, 0, 4096), "gap"),
            (
                exit_frame(
                    &row,
                    4096,
                    &ExitFacts {
                        exit_code: Some(0),
                        exit_signal: None,
                    },
                ),
                "exit",
            ),
        ] {
            assert_eq!(frame["type"], kind);
            assert!(frame["cursor"]["outputEpoch"].is_string());
            assert!(frame["nextCursor"]["byteOffset"].is_u64());
            OperationRegistry::global()
                .validate_result("terminals.output", &frame)
                .unwrap_or_else(|e| panic!("`{kind}` frame must pass the schema: {e:?}"));
        }
    }
}
