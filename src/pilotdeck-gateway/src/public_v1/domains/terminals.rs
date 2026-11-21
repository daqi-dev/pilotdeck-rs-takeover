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

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_LIMIT,
    CODE_RESULT_UNKNOWN, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::{
    ProjectWorkspace, decode_byte_path, lexical_join_under, os_from_bytes, require_within,
    resolve_project,
};
use crate::public_v1::domains::operations_ops::LongOperationOutcome;
use crate::public_v1::domains::terminals_pty::{
    self, ExitFacts, TerminalHandle, TerminalRegistry, WriteInputError, is_closed_pty_error,
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
    pub lease_token_hash: Option<String>,
    pub last_accepted_input_seq: i64,
    pub last_input_digest: Option<String>,
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
        .unwrap_or_else(now_rfc3339)
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
    if state != "lost"
        && let Some(handle) = live
    {
        let (ring_earliest, ring_latest) = handle.ring_positions();
        earliest = ring_earliest as i64;
        latest = ring_latest as i64;
        if let Some(facts) = handle.exit_facts() {
            state = "exited".to_owned();
            exit_code = facts.exit_code.map(i64::from);
            exit_signal = facts.exit_signal.clone();
        }
    }
    // `controllerId` / `leaseExpiresAt` are optional in the Terminal contract
    // but typed `string` (never null): a terminal without a control lease
    // (freshly created, lease expired, or lease lost on restart) omits both
    // instead of serializing a null that fails the schema.
    let mut value = json!({
        "terminalId": row.terminal_id,
        "projectId": row.project_id,
        "serverEpoch": row.server_epoch,
        "state": state,
        "controlEpoch": row.control_epoch,
        "lastAcceptedInputSeq": row.last_accepted_input_seq,
        "outputEpoch": row.output_epoch,
        "earliestOutputOffset": earliest,
        "latestOutputOffset": latest,
        "exitCode": exit_code,
        "exitSignal": exit_signal,
        "rows": row.rows,
        "columns": row.columns,
    });
    if let Some(controller_id) = &row.controller_id {
        value["controllerId"] = json!(controller_id);
    }
    if let Some(expires_ms) = row.lease_expires_ms {
        value["leaseExpiresAt"] = json!(ms_to_rfc3339(expires_ms));
    }
    value
}

fn fetch_terminal_row(
    conn: &Connection,
    terminal_id: &str,
) -> Result<Option<StoredTerminal>, ContractError> {
    let row = conn
        .query_row(
            "SELECT terminal_id, project_id, working_directory, shell_profile_id, server_epoch,
                    state, control_epoch, controller_id, lease_expires_ms, lease_token_hash,
                    last_accepted_input_seq, last_input_digest,
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
                    lease_token_hash: r.get(9)?,
                    last_accepted_input_seq: r.get(10)?,
                    last_input_digest: r.get(11)?,
                    output_epoch: r.get(12)?,
                    earliest_output_offset: r.get(13)?,
                    latest_output_offset: r.get(14)?,
                    exit_code: r.get(15)?,
                    exit_signal: r.get(16)?,
                    rows: r.get(17)?,
                    columns: r.get(18)?,
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
                        state, control_epoch, controller_id, lease_expires_ms, lease_token_hash,
                        last_accepted_input_seq, last_input_digest,
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
                    lease_token_hash: r.get(9)?,
                    last_accepted_input_seq: r.get(10)?,
                    last_input_digest: r.get(11)?,
                    output_epoch: r.get(12)?,
                    earliest_output_offset: r.get(13)?,
                    latest_output_offset: r.get(14)?,
                    exit_code: r.get(15)?,
                    exit_signal: r.get(16)?,
                    rows: r.get(17)?,
                    columns: r.get(18)?,
                })
            })
            .map_err(map_sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql)?;
        Ok(rows)
    }

    /// Inserts a freshly spawned terminal row and its `terminals.changed`
    /// event atomically.
    #[cfg(test)]
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
            .is_some_and(|(stored, presented)| stored == sha256_hex(presented.as_bytes()));

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
}

#[derive(Clone, Debug)]
enum CreateCommitOutcome {
    Created,
    Replayed(String),
}

#[derive(Clone, Debug)]
enum CloseAcceptOutcome {
    Deduplicated {
        operation_id: String,
        state: String,
        revision: String,
    },
    Created {
        operation_id: String,
        row: Box<StoredTerminal>,
    },
}

fn validate_input_row(
    row: &StoredTerminal,
    gateway_epoch: &str,
    server_epoch: &str,
    control_epoch: i64,
    lease_token: &str,
    input_seq: i64,
    bytes: &[u8],
) -> Result<bool, ContractError> {
    let terminal_id = row.terminal_id.as_str();
    if input_seq < 1 {
        return Err(terminal_err(
            CODE_INVALID_ARGUMENT,
            format!("inputSeq must be >= 1, got {input_seq}"),
        ));
    }
    let effective = effective_state(row, gateway_epoch);
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
    Ok(false)
}

fn validate_resize_row(
    row: &StoredTerminal,
    gateway_epoch: &str,
    terminal_id: &str,
    control_epoch: i64,
    lease_token: &str,
) -> Result<(), ContractError> {
    if effective_state(row, gateway_epoch) != row.state || is_effective_terminal_state(&row.state) {
        return Err(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!(
                "terminal `{terminal_id}` is `{}` and accepts no resize",
                effective_state(row, gateway_epoch)
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
    Ok(())
}

fn append_operation_queued_in_tx(
    conn: &Connection,
    operation_id: &str,
    operation_type: &str,
) -> Result<(), ContractError> {
    let placeholder = format!("tmp-op-{operation_id}");
    let payload_str = serde_json::to_string(
        &json!({"operationId": operation_id, "operationType": operation_type}),
    )
    .unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', 'operation.queued', 'agentcpm-pd.operation.event/1', ?3, ?3, ?4)",
        params![placeholder, operation_id, payload_str, now_rfc3339()],
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

fn idempotency_extract_id(method_id: &str, resource_json: &str) -> Option<String> {
    let resource: Value = serde_json::from_str(resource_json).unwrap_or(Value::Null);
    let field = match method_id {
        "terminals.close" => "operationId",
        _ => "terminalId",
    };
    resource
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

impl V1Store {
    /// Atomically commits a freshly spawned `terminals.create` row, its
    /// `terminals.changed` event, and optional idempotency record in a single
    /// transaction (rolling back cleanly if an idempotency replay or conflict
    /// raced in).
    fn terminal_create_commit(
        &self,
        terminal: &StoredTerminal,
        idempotency_key: Option<&str>,
        digest: &str,
    ) -> Result<CreateCommitOutcome, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let now = now_rfc3339();
        if let Some(key) = idempotency_key {
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT input_digest, resource_json FROM idempotency_records
                     WHERE idempotency_key = ?1 AND method_id = 'terminals.create'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(map_sql)?;
            if let Some((stored_digest, resource_json)) = existing {
                if stored_digest != digest {
                    return Err(terminal_err(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for terminals.create with different input"
                        ),
                    ));
                }
                if let Some(existing_id) =
                    idempotency_extract_id("terminals.create", &resource_json)
                {
                    return Ok(CreateCommitOutcome::Replayed(existing_id));
                }
            }
            let retained_until = (chrono::Utc::now()
                + chrono::Duration::seconds(IDEMPOTENCY_RETENTION_SECS))
            .to_rfc3339();
            let resource = json!({"kind": "terminal", "terminalId": terminal.terminal_id});
            tx.execute(
                "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'terminals.create', ?2, 'accepted', 'receiptFound', ?3, NULL, NULL, ?4, ?5, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&resource).unwrap_or_default(),
                    now,
                    retained_until
                ],
            )
            .map_err(map_sql)?;
        }

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
        Ok(CreateCommitOutcome::Created)
    }

    /// Validates ordered input acceptance WITHOUT advancing `last_accepted_input_seq`.
    /// Returns `Ok(true)` if the input is an idempotent same-bytes replay of the
    /// immediately previous sequence number, or `Ok(false)` when ready to write.
    pub(crate) fn terminal_validate_input(
        &self,
        terminal_id: &str,
        server_epoch: &str,
        control_epoch: i64,
        lease_token: &str,
        input_seq: i64,
        bytes: &[u8],
    ) -> Result<bool, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let row = fetch_terminal_row(&conn, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })?;
        let gateway_epoch = current_server_epoch(self);
        validate_input_row(
            &row,
            &gateway_epoch,
            server_epoch,
            control_epoch,
            lease_token,
            input_seq,
            bytes,
        )
    }

    /// Commits `last_accepted_input_seq` AFTER the PTY master write succeeded.
    pub(crate) fn terminal_commit_input(
        &self,
        terminal_id: &str,
        input_seq: i64,
        bytes: &[u8],
    ) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let digest = sha256_hex(bytes);
        let updated = tx
            .execute(
                "UPDATE terminals
                 SET last_accepted_input_seq = ?1, last_input_digest = ?2, updated_at = ?3
                 WHERE terminal_id = ?4 AND last_accepted_input_seq = ?5",
                params![input_seq, digest, now_rfc3339(), terminal_id, input_seq - 1],
            )
            .map_err(map_sql)?;
        if updated != 1 {
            return Err(ContractError::result_unknown(
                None,
                format!(
                    "terminal `{terminal_id}` input watermark shifted before committing seq {input_seq}"
                ),
            ));
        }
        tx.commit().map_err(map_sql)?;
        Ok(())
    }

    /// Validates and records one ordered input acceptance in a single step
    /// (retained for store-level unit tests).
    #[cfg(test)]
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
        let dedup = validate_input_row(
            &row,
            &gateway_epoch,
            server_epoch,
            control_epoch,
            lease_token,
            input_seq,
            bytes,
        )?;
        if dedup {
            return Ok(true);
        }
        let digest = sha256_hex(bytes);
        tx.execute(
            "UPDATE terminals SET last_accepted_input_seq = ?1, last_input_digest = ?2, updated_at = ?3 WHERE terminal_id = ?4",
            params![input_seq, digest, now_rfc3339(), terminal_id],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(false)
    }

    /// Validates lease and terminal state for resize WITHOUT modifying SQLite.
    pub(crate) fn terminal_validate_resize(
        &self,
        terminal_id: &str,
        control_epoch: i64,
        lease_token: &str,
    ) -> Result<(), ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let row = fetch_terminal_row(&conn, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })?;
        let epoch = current_server_epoch(self);
        validate_resize_row(&row, &epoch, terminal_id, control_epoch, lease_token)
    }

    /// Persists resize dimensions AFTER the PTY ioctl succeeded.
    pub(crate) fn terminal_commit_resize(
        &self,
        terminal_id: &str,
        control_epoch: i64,
        lease_token: &str,
        rows: u16,
        columns: u16,
    ) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let row = fetch_terminal_row(&tx, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })?;
        let epoch = current_server_epoch(self);
        validate_resize_row(&row, &epoch, terminal_id, control_epoch, lease_token)?;
        tx.execute(
            "UPDATE terminals SET term_rows = ?1, term_columns = ?2, updated_at = ?3 WHERE terminal_id = ?4",
            params![i64::from(rows), i64::from(columns), now_rfc3339(), terminal_id],
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

    /// Reads the stored terminal row for effective-state checks.
    pub(crate) fn terminal_row_effective(
        &self,
        terminal_id: &str,
    ) -> Result<Option<StoredTerminal>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        fetch_terminal_row(&conn, terminal_id)
    }

    /// Atomically checks `terminals.close` idempotency or accepts a new close
    /// operation directly in the unified `operations` table.
    fn terminal_close_accept(
        &self,
        terminal_id: &str,
        expected_epoch: &str,
        mode: &str,
        idempotency_key: Option<&str>,
        digest: &str,
    ) -> Result<CloseAcceptOutcome, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;

        if let Some(key) = idempotency_key {
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT input_digest, resource_json FROM idempotency_records
                     WHERE idempotency_key = ?1 AND method_id = 'terminals.close'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(map_sql)?;
            if let Some((stored_digest, resource_json)) = existing {
                if stored_digest != digest {
                    return Err(terminal_err(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for terminals.close with different input"
                        ),
                    ));
                }
                let operation_id = idempotency_extract_id("terminals.close", &resource_json)
                    .ok_or_else(|| {
                        storage_err("terminals.close idempotency record missing operationId")
                    })?;
                let (state, revision): (String, String) = tx
                    .query_row(
                        "SELECT state, revision FROM operations WHERE operation_id = ?1",
                        params![operation_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(map_sql)?
                    .ok_or_else(|| {
                        storage_err(format!(
                            "close operation `{operation_id}` missing from operations table"
                        ))
                    })?;
                return Ok(CloseAcceptOutcome::Deduplicated {
                    operation_id,
                    state,
                    revision,
                });
            }
        }

        // Validation precedes any durable acceptance record: a rejected close
        // (unknown terminal, wrong epoch, bad mode) must never poison a retry
        // of the same idempotencyKey.
        let row = fetch_terminal_row(&tx, terminal_id)?.ok_or_else(|| {
            terminal_err(
                CODE_NOT_FOUND,
                format!("terminal `{terminal_id}` not found"),
            )
        })?;
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

        let operation_id = self.next_id();
        let revision = next_domain_rev(&tx)?;
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO operations (operation_id, operation_type, scope_kind, project_id, session_id,
                state, revision, cancel_requested, cancellable, effect_status, progress_json,
                created_at, updated_at)
             VALUES (?1, 'terminals.close', 'project', ?2, NULL, 'queued', ?3, 0, 1, 'known', '[]', ?4, ?4)",
            params![operation_id, row.project_id, revision, now],
        )
        .map_err(map_sql)?;
        append_operation_queued_in_tx(&tx, &operation_id, "terminals.close")?;

        if let Some(key) = idempotency_key {
            let retained_until = (chrono::Utc::now()
                + chrono::Duration::seconds(IDEMPOTENCY_RETENTION_SECS))
            .to_rfc3339();
            let resource = json!({
                "kind": "operation",
                "operationId": operation_id,
                "terminalId": terminal_id,
            });
            tx.execute(
                "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'terminals.close', ?2, 'accepted', 'receiptFound', ?3, NULL, NULL, ?4, ?5, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&resource).unwrap_or_default(),
                    now,
                    retained_until
                ],
            )
            .map_err(map_sql)?;
        }

        tx.commit().map_err(map_sql)?;
        self.event_notifier().notify_waiters();
        Ok(CloseAcceptOutcome::Created {
            operation_id,
            row: Box::new(row),
        })
    }

    fn operation_state_and_revision(
        &self,
        operation_id: &str,
    ) -> Result<(String, String), ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        conn.query_row(
            "SELECT state, revision FROM operations WHERE operation_id = ?1",
            params![operation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(map_sql)?
        .ok_or_else(|| storage_err(format!("operation `{operation_id}` not found")))
    }

    /// Idempotency intake for `terminals.create`: same key + same input replays
    /// the stored resource id; same key + different input is `IDEMPOTENCY_CONFLICT`.
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
        Ok(idempotency_extract_id(method_id, &resource_json))
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

struct IdemLockGuard {
    map_key: (String, String),
    permit: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for IdemLockGuard {
    fn drop(&mut self) {
        drop(self.permit.take());
        if let Ok(mut map) = idem_locks().lock()
            && map
                .get(&self.map_key)
                .is_some_and(|w| w.strong_count() == 0)
        {
            map.remove(&self.map_key);
        }
    }
}

type IdemLockMap = StdMutex<HashMap<(String, String), Weak<tokio::sync::Mutex<()>>>>;

fn idem_locks() -> &'static IdemLockMap {
    static LOCKS: OnceLock<IdemLockMap> = OnceLock::new();
    LOCKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

async fn acquire_idem_lock(method_id: &str, key: Option<&str>) -> Option<IdemLockGuard> {
    let key = key?;
    let map_key = (method_id.to_owned(), key.to_owned());
    let mutex = {
        let mut map = idem_locks().lock().expect("idem locks");
        if let Some(existing) = map.get(&map_key).and_then(Weak::upgrade) {
            existing
        } else {
            let fresh = Arc::new(tokio::sync::Mutex::new(()));
            map.insert(map_key.clone(), Arc::downgrade(&fresh));
            fresh
        }
    };
    let permit = mutex.lock_owned().await;
    Some(IdemLockGuard {
        map_key,
        permit: Some(permit),
    })
}

fn digest_of(method_id: &str, input: &Value) -> String {
    agentcpm_pd_protocol::envelope::compute_input_digest(method_id, input)
}

fn parse_dimension(value: &Value, field: &str) -> Result<u16, ContractError> {
    let raw = value.as_i64().ok_or_else(|| {
        terminal_err(
            CODE_INVALID_ARGUMENT,
            format!("{field} must be an integer within 1..={}", u16::MAX),
        )
    })?;
    if raw < 1 {
        return Err(terminal_err(
            CODE_INVALID_ARGUMENT,
            format!("{field} must be >= 1, got {raw}"),
        ));
    }
    u16::try_from(raw).map_err(|_| {
        terminal_err(
            CODE_INVALID_ARGUMENT,
            format!("{field} must be <= {}, got {raw}", u16::MAX),
        )
    })
}

fn lexical_normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(pre) => out.push(pre.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    out
}

fn ancestor_or_symlink_escapes_workspace(workspace: &Path, joined: &Path) -> bool {
    if let Ok(meta) = std::fs::symlink_metadata(joined)
        && meta.file_type().is_symlink()
        && let Ok(target) = std::fs::read_link(joined)
    {
        let resolved_target = if target.is_absolute() {
            lexical_normalize_absolute(&target)
        } else if let Some(parent) = joined.parent() {
            lexical_normalize_absolute(&parent.join(target))
        } else {
            return true;
        };
        if !resolved_target.starts_with(workspace) {
            return true;
        }
    }
    let mut cur = joined.parent();
    while let Some(anc) = cur {
        if anc == workspace || !anc.starts_with(workspace) {
            break;
        }
        if let Ok(resolved) = std::fs::canonicalize(anc) {
            return !resolved.starts_with(workspace);
        }
        cur = anc.parent();
    }
    false
}

fn verify_workspace_directory(root: &Path, target: &Path) -> Result<(), ContractError> {
    #[cfg(unix)]
    {
        crate::project_files::open_directory_beneath(root, target).map_err(|e| {
            if crate::project_files::nofollow_loop(&e) {
                terminal_err(
                    CODE_PATH_ESCAPE,
                    "working directory symlink resolved outside the project workspace",
                )
            } else {
                terminal_err(
                    CODE_INVALID_ARGUMENT,
                    format!(
                        "working directory `{}` is not a usable directory: {e}",
                        target.display()
                    ),
                )
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        if !target.is_dir() {
            return Err(terminal_err(
                CODE_INVALID_ARGUMENT,
                format!(
                    "working directory `{}` is not a directory",
                    target.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Resolves the terminal's working directory inside the project's canonical
/// `workspace` (`root.relativePath` honored, symlinked/virtual roots refused).
/// Preserves raw `PathBuf` bytes losslessly and enforces lexical confinement
/// before any `canonicalize()` call so out-of-workspace probes cannot leak
/// host filesystem existence.
fn resolve_working_directory(
    project: &ProjectWorkspace,
    working_directory: Option<&Value>,
) -> Result<PathBuf, ContractError> {
    let Some(raw) = working_directory else {
        verify_workspace_directory(&project.root, &project.workspace)?;
        return Ok(project.workspace.clone());
    };
    let raw_bytes = decode_byte_path(raw)?;
    let os_str = os_from_bytes(&raw_bytes);
    let req_path = PathBuf::from(&os_str);
    let joined = if req_path.is_absolute() {
        let normalized = lexical_normalize_absolute(&req_path);
        if !normalized.starts_with(&project.workspace) {
            return Err(terminal_err(
                CODE_PATH_ESCAPE,
                "working directory escapes the project workspace",
            ));
        }
        normalized
    } else {
        lexical_join_under(&project.workspace, &raw_bytes)?
    };
    let canonical = match std::fs::canonicalize(&joined) {
        Ok(canonical) => canonical,
        Err(e) => {
            if ancestor_or_symlink_escapes_workspace(&project.workspace, &joined) {
                return Err(terminal_err(
                    CODE_PATH_ESCAPE,
                    "working directory resolves outside the project workspace",
                ));
            }
            return Err(terminal_err(
                CODE_INVALID_ARGUMENT,
                format!(
                    "working directory `{}` does not exist: {e}",
                    joined.display()
                ),
            ));
        }
    };
    require_within(&project.workspace, &canonical)?;
    verify_workspace_directory(&project.workspace, &canonical)?;
    Ok(canonical)
}

fn encode_working_dir_for_db(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Some(s) = path.to_str() {
            Some(s.to_owned())
        } else {
            let encoded =
                crate::public_v1::domains::files::byte_path_json(path.as_os_str().as_bytes());
            serde_json::to_string(&encoded).ok()
        }
    }
    #[cfg(not(unix))]
    {
        path.to_str().map(str::to_owned)
    }
}

async fn terminals_create(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store();
    let project_id = input["projectId"].as_str().unwrap_or_default().to_owned();
    let digest = digest_of("terminals.create", &input);
    let _idem_guard = acquire_idem_lock("terminals.create", idempotency_key.as_deref()).await;

    // Same-request dedup (contract acceptance): the key replays the same
    // terminal's current state.
    if let Some(key) = idempotency_key.as_deref()
        && let Some(existing_id) =
            store.terminal_idempotency_replay("terminals.create", key, &digest)?
    {
        let row = store.terminal_row(&existing_id)?;
        let live = rt.terminal_registry().live_terminal(&existing_id);
        let epoch = current_server_epoch(store);
        return Ok((
            ResponseOutcome::Succeeded,
            json!({"terminal": terminal_to_json(&row, &epoch, live.as_ref())}),
        ));
    }

    // The project must exist with a host-declared workspace; its canonical
    // workspace bounds the working dir.
    let project = resolve_project(store, &project_id).map_err(ContractError::from)?;
    if let Some(profile) = input["shellProfileId"].as_str().filter(|p| !p.is_empty()) {
        // No shell-profile registry exists in this round; a declared
        // profile that cannot be resolved must fail the create outright
        // (contract: SHELL_UNAVAILABLE, never a fake running terminal).
        return Err(DispatchError::Application(terminal_err(
            CODE_SHELL_UNAVAILABLE,
            format!(
                "shellProfileId `{profile}` cannot be resolved; no shell profiles are configured on this instance"
            ),
        )));
    }
    let rows = parse_dimension(&input["rows"], "rows")?;
    let columns = parse_dimension(&input["columns"], "columns")?;
    let working_directory = resolve_working_directory(
        &project,
        input.get("workingDirectory").filter(|v| !v.is_null()),
    )?;

    // Reserve live-terminal quota atomically before any spawn side effect.
    let slot_guard = rt
        .terminal_registry()
        .try_reserve_live_slot(MAX_LIVE_TERMINALS)
        .ok_or_else(|| {
            terminal_err(
                CODE_RESOURCE_LIMIT,
                format!("this instance already hosts {MAX_LIVE_TERMINALS} live terminals"),
            )
        })?;

    let terminal_id = store.next_id();
    let output_epoch = store.next_id();
    // Spawn failure answers SHELL_UNAVAILABLE synchronously with no row —
    // the contract forbids leaving a fake `running` behind.
    let spawned = terminals_pty::spawn_shell(&terminal_id, &working_directory, rows, columns)
        .map_err(|e| {
            terminal_err(
                CODE_SHELL_UNAVAILABLE,
                format!(
                    "failed to spawn the interactive shell in `{}`: {e}",
                    working_directory.display()
                ),
            )
        })?;
    let mut spawn_guard = crate::process_group::GroupGuard(spawned.handle.leader_pgid());

    let row = StoredTerminal {
        terminal_id: terminal_id.clone(),
        project_id,
        working_directory: encode_working_dir_for_db(&working_directory),
        shell_profile_id: None,
        server_epoch: current_server_epoch(store),
        state: "running".to_owned(),
        control_epoch: 0,
        controller_id: None,
        lease_expires_ms: None,
        lease_token_hash: None,
        last_accepted_input_seq: 0,
        last_input_digest: None,
        output_epoch,
        earliest_output_offset: 0,
        latest_output_offset: 0,
        exit_code: None,
        exit_signal: None,
        rows: i64::from(rows),
        columns: i64::from(columns),
    };

    match store.terminal_create_commit(&row, idempotency_key.as_deref(), &digest) {
        Ok(CreateCommitOutcome::Created) => {
            store.event_notifier().notify_waiters();
            spawn_guard.disarm();
            let live_handle = spawned.handle.clone();
            slot_guard.commit(spawned.handle.clone());
            let finalize_store = store.clone();
            let finalize_handle = live_handle.clone();
            let finalize_id = terminal_id.clone();
            let finalize_reg = rt.terminal_registry().clone();
            terminals_pty::supervise(
                finalize_reg,
                spawned.handle,
                spawned.child,
                move |facts: ExitFacts| {
                    let (earliest, latest) = finalize_handle.ring_positions();
                    let _ = finalize_store.terminal_finalize_exit(
                        &finalize_id,
                        &facts,
                        earliest,
                        latest,
                    );
                },
            );
            Ok((
                ResponseOutcome::Succeeded,
                json!({"terminal": terminal_to_json(&row, &row.server_epoch, Some(&live_handle))}),
            ))
        }
        Ok(CreateCommitOutcome::Replayed(existing_id)) => {
            spawn_guard.disarm();
            terminals_pty::abort_spawned(spawned).await;
            drop(slot_guard);
            let existing_row = store.terminal_row(&existing_id)?;
            let live = rt.terminal_registry().live_terminal(&existing_id);
            let epoch = current_server_epoch(store);
            Ok((
                ResponseOutcome::Succeeded,
                json!({"terminal": terminal_to_json(&existing_row, &epoch, live.as_ref())}),
            ))
        }
        Err(err) => {
            spawn_guard.disarm();
            terminals_pty::abort_spawned(spawned).await;
            drop(slot_guard);
            Err(err.into())
        }
    }
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
            let live = rt.terminal_registry().live_terminal(&row.terminal_id);
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
    let live = rt.terminal_registry().live_terminal(terminal_id);
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
    let live = rt.terminal_registry().live_terminal(&terminal_id);
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

const INPUT_LOCK_TIMEOUT: Duration = Duration::from_secs(3);

fn map_write_input_error(
    terminal_id: &str,
    total_bytes: usize,
    inputs_still_open: bool,
    err: WriteInputError,
) -> ContractError {
    match err {
        WriteInputError::ClosedBeforeWrite(e) => terminal_err(
            CODE_TERMINAL_CLOSED,
            format!("terminal `{terminal_id}` no longer accepts input: {e}"),
        ),
        WriteInputError::TimedOut { wrote_bytes: 0 } => {
            if !inputs_still_open {
                terminal_err(
                    CODE_TERMINAL_CLOSED,
                    format!("terminal `{terminal_id}` stopped accepting input"),
                )
            } else {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("terminal `{terminal_id}` input timed out before writing bytes"),
                    RetrySpec::backoff_read(Some(100)),
                    EffectStatus::NotApplied,
                )
            }
        }
        WriteInputError::TimedOut { wrote_bytes }
        | WriteInputError::FailedAfterPartialWrite { wrote_bytes, .. } => {
            ContractError::result_unknown(
                None,
                format!(
                    "terminal `{terminal_id}` wrote {wrote_bytes} of {total_bytes} bytes before failing; reconcile watermark before retry"
                ),
            )
        }
        WriteInputError::FailedCleanly(e) => {
            if !inputs_still_open || is_closed_pty_error(&e) {
                terminal_err(
                    CODE_TERMINAL_CLOSED,
                    format!("terminal `{terminal_id}` no longer accepts input: {e}"),
                )
            } else {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("terminal `{terminal_id}` input write failed: {e}"),
                    RetrySpec::backoff_read(Some(100)),
                    EffectStatus::NotApplied,
                )
            }
        }
    }
}

fn map_winsize_error(terminal_id: &str, err: agentcpm_pd_pty::PtyError) -> ContractError {
    match &err {
        agentcpm_pd_pty::PtyError::Io(io_err)
            if io_err.kind() == std::io::ErrorKind::InvalidInput
                || io_err.raw_os_error() == Some(libc::EINVAL) =>
        {
            terminal_err(
                CODE_INVALID_ARGUMENT,
                format!("terminal `{terminal_id}` rejected invalid window dimensions: {io_err}"),
            )
        }
        agentcpm_pd_pty::PtyError::Io(io_err)
            if is_closed_pty_error(&err)
                || matches!(io_err.raw_os_error(), Some(libc::ENOTTY | libc::ENODEV)) =>
        {
            terminal_err(
                CODE_TERMINAL_CLOSED,
                format!("terminal `{terminal_id}` could not be resized: {io_err}"),
            )
        }
        other => storage_err(format!("terminal `{terminal_id}` resize failed: {other}")),
    }
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

    let Some(handle) = rt.terminal_registry().live_terminal(&terminal_id) else {
        let row = store.terminal_row(&terminal_id)?;
        let epoch = current_server_epoch(store);
        return Err(DispatchError::Application(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!(
                "terminal `{terminal_id}` has no live pty (state `{}`)",
                effective_state(&row, &epoch)
            ),
        )));
    };

    // Single-writer lane with bounded lock wait so a stalled child cannot
    // permanently block subsequent controllers (e.g. takeover + Ctrl-C).
    let guard = tokio::time::timeout(INPUT_LOCK_TIMEOUT, handle.write_lock.lock())
        .await
        .map_err(|_| {
            if !handle.inputs_open() {
                terminal_err(
                    CODE_TERMINAL_CLOSED,
                    format!("terminal `{terminal_id}` stopped accepting input (close in progress)"),
                )
            } else {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("terminal `{terminal_id}` input writer is busy"),
                    RetrySpec::backoff_read(Some(100)),
                    EffectStatus::NotApplied,
                )
            }
        })?;
    if !handle.inputs_open() {
        return Err(DispatchError::Application(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!("terminal `{terminal_id}` stopped accepting input (close in progress)"),
        )));
    }

    // Validate lease/sequence BEFORE writing to PTY, and only commit the
    // accepted watermark AFTER PTY write succeeds so failed writes never
    // advance the watermark and cause silent byte loss on retry.
    let deduplicated = store.terminal_validate_input(
        &terminal_id,
        &server_epoch,
        control_epoch,
        &lease_token,
        input_seq,
        &bytes,
    )?;
    if !deduplicated {
        if let Err(err) = handle.write_input(&bytes).await {
            return Err(DispatchError::Application(map_write_input_error(
                &terminal_id,
                bytes.len(),
                handle.inputs_open(),
                err,
            )));
        }
        store.terminal_commit_input(&terminal_id, input_seq, &bytes)?;
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
    let rows = parse_dimension(&input["rows"], "rows")?;
    let columns = parse_dimension(&input["columns"], "columns")?;

    let Some(handle) = rt.terminal_registry().live_terminal(&terminal_id) else {
        let row = store.terminal_row(&terminal_id)?;
        let epoch = current_server_epoch(store);
        return Err(DispatchError::Application(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!(
                "terminal `{terminal_id}` has no live pty (state `{}`)",
                effective_state(&row, &epoch)
            ),
        )));
    };

    let _resize_guard = handle.resize_lock.lock().await;
    if !handle.inputs_open() {
        return Err(DispatchError::Application(terminal_err(
            CODE_TERMINAL_CLOSED,
            format!("terminal `{terminal_id}` stopped accepting resize (close in progress)"),
        )));
    }
    store.terminal_validate_resize(&terminal_id, control_epoch, &lease_token)?;
    handle
        .set_winsize(rows, columns)
        .map_err(|e| map_winsize_error(&terminal_id, e))?;
    store.terminal_commit_resize(&terminal_id, control_epoch, &lease_token, rows, columns)?;
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
    let live = rt.terminal_registry().live_terminal(terminal_id);
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
        let earliest = row.earliest_output_offset.max(0) as u64;
        if offset < earliest {
            gap_frame(&row, offset, earliest)
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
        }
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

fn resolve_terminal_close_outcome(
    registry: &TerminalRegistry,
    store: &V1Store,
    terminal_id: &str,
    current_epoch: &str,
    cancelled: bool,
) -> LongOperationOutcome {
    if let Ok(Some(row)) = store.terminal_row_effective(terminal_id)
        && is_effective_terminal_state(&effective_state(&row, current_epoch))
    {
        let live = registry.live_terminal(terminal_id);
        let terminal = terminal_to_json(&row, current_epoch, live.as_ref());
        return LongOperationOutcome::Succeeded {
            result: json!({"terminal": terminal}),
            result_schema_id: None,
        };
    }
    if cancelled {
        return LongOperationOutcome::Cancelled;
    }
    LongOperationOutcome::Failed {
        code: CODE_RESULT_UNKNOWN.to_owned(),
        message: format!("terminal `{terminal_id}` did not reach a terminal state within 10s"),
    }
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
    let _idem_guard = acquire_idem_lock("terminals.close", idempotency_key.as_deref()).await;

    match store.terminal_close_accept(
        &terminal_id,
        &expected_epoch,
        &mode,
        idempotency_key.as_deref(),
        &digest,
    )? {
        CloseAcceptOutcome::Deduplicated {
            operation_id,
            state,
            revision,
        } => Ok((
            ResponseOutcome::Accepted,
            json!({
                "kind": "operation",
                "operationId": operation_id,
                "durable": true,
                "acceptedRevision": revision,
                "deduplicated": true,
                "state": state,
            }),
        )),
        CloseAcceptOutcome::Created { operation_id, row } => {
            store.mark_operation_running(&operation_id)?;
            let epoch = current_server_epoch(store);
            let effective = effective_state(&row, &epoch);
            let live = rt.terminal_registry().live_terminal(&terminal_id);
            let already_done = effective != row.state || is_effective_terminal_state(&row.state);

            if already_done {
                let outcome = resolve_terminal_close_outcome(
                    rt.terminal_registry(),
                    store,
                    &terminal_id,
                    &epoch,
                    false,
                );
                store.finish_long_operation(&operation_id, outcome)?;
                let (state, revision) = store.operation_state_and_revision(&operation_id)?;
                return Ok((
                    ResponseOutcome::Accepted,
                    json!({
                        "kind": "operation",
                        "operationId": operation_id,
                        "durable": true,
                        "acceptedRevision": revision,
                        "deduplicated": false,
                        "state": state,
                    }),
                ));
            }

            if let Some(handle) = live.as_ref() {
                handle.request_close(mode == "graceful");
            }
            let (state, revision) = store.operation_state_and_revision(&operation_id)?;

            let completion_store = store.clone();
            let completion_reg = rt.terminal_registry().clone();
            let completion_id = terminal_id.clone();
            let completion_op = operation_id.clone();
            tokio::spawn(async move {
                let completion_epoch = current_server_epoch(&completion_store);
                let wake = completion_reg
                    .live_terminal(&completion_id)
                    .map(|h| h.notify.clone());
                let event_wake = completion_store.event_notifier();
                let mut cancelled = false;

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
                    if completion_store
                        .operation_cancel_requested(&completion_op)
                        .unwrap_or(false)
                    {
                        cancelled = true;
                        break;
                    }
                    match &wake {
                        Some(n) => {
                            tokio::select! {
                                _ = n.notified() => {}
                                _ = event_wake.notified() => {}
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                        }
                        None => {
                            tokio::select! {
                                _ = event_wake.notified() => {}
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                        }
                    }
                }
                let outcome = resolve_terminal_close_outcome(
                    &completion_reg,
                    &completion_store,
                    &completion_id,
                    &completion_epoch,
                    cancelled,
                );
                let _ = completion_store.finish_long_operation(&completion_op, outcome);
            });

            Ok((
                ResponseOutcome::Accepted,
                json!({
                    "kind": "operation",
                    "operationId": operation_id,
                    "durable": true,
                    "acceptedRevision": revision,
                    "deduplicated": false,
                    "state": state,
                }),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (store-level contract cases; the pty lifecycle itself is covered by
// the e2e suite — the local build freeze forbids spawning here).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::domains::automation::tests::{dispatch_ok, fixture};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    fn test_store(tag: &str) -> Arc<V1Store> {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
        let _iid = file.instance_id.clone();
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
            lease_token_hash: None,
            last_input_digest: None,
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

    #[tokio::test]
    async fn resolve_working_directory_confines_to_project_workspace_and_preserves_byte_path() {
        let fx = fixture("term-wd-confine").await;
        let root = fx._tmp.0.join("auth-root");
        let proj_a_dir = root.join("proj-a");
        let proj_b_dir = root.join("proj-b");
        let external_dir = fx._tmp.0.join("external");
        std::fs::create_dir_all(proj_a_dir.join("sub")).unwrap();
        std::fs::create_dir_all(&proj_b_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            use std::os::unix::fs::symlink;
            let non_utf8_name = std::ffi::OsStr::from_bytes(b"dir-\xff\xfe");
            std::fs::create_dir_all(proj_a_dir.join(non_utf8_name)).unwrap();
            symlink(&external_dir, proj_a_dir.join("escape_link")).unwrap();
            symlink(
                external_dir.join("missing_leaf"),
                proj_a_dir.join("dangling_escape"),
            )
            .unwrap();
        }

        fx.rt
            .store()
            .declare_authorized_root("root-wd", &root)
            .unwrap();
        for (pid, rel) in [("proj-a", "proj-a"), ("proj-b", "proj-b")] {
            fx.rt
                .store()
                .register_project(&json!({
                    "instanceId": fx.instance_id,
                    "projectId": pid,
                    "name": pid,
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": "root-wd", "relativePath": rel},
                }))
                .unwrap();
        }
        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": "proj-virtual",
                "name": "Virtual",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "missing-root", "relativePath": ""},
            }))
            .unwrap();

        let resolved_a = resolve_project(fx.rt.store(), "proj-a")
            .map_err(ContractError::from)
            .unwrap();
        let canonical_a = std::fs::canonicalize(&proj_a_dir).unwrap();
        let canonical_b = std::fs::canonicalize(&proj_b_dir).unwrap();
        assert_eq!(resolved_a.workspace, canonical_a);

        // Omitting workingDirectory lands in proj-a, NOT the parent authorized root.
        assert_eq!(
            resolve_working_directory(&resolved_a, None).unwrap(),
            canonical_a
        );

        // Passing sibling directory name stays scoped under proj-a (does not escape to proj-b).
        let err = resolve_working_directory(
            &resolved_a,
            Some(&json!({"encoding": "utf8", "value": "proj-b"})),
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_INVALID_ARGUMENT);

        // Relative and absolute escapes return PATH_ESCAPE.
        for candidate in [
            json!({"encoding": "utf8", "value": "../proj-b"}),
            json!({"encoding": "utf8", "value": canonical_b.to_string_lossy()}),
            json!({"encoding": "utf8", "value": "/etc/shadow"}),
            json!({"encoding": "utf8", "value": "/etc/definitely_nonexistent_pd_path"}),
            json!({"encoding": "utf8", "value": "escape_link"}),
            json!({"encoding": "utf8", "value": "escape_link/nonexistent"}),
            json!({"encoding": "utf8", "value": "dangling_escape"}),
        ] {
            let err = resolve_working_directory(&resolved_a, Some(&candidate)).unwrap_err();
            assert_eq!(
                err.code, CODE_PATH_ESCAPE,
                "candidate `{candidate}` should fail with PATH_ESCAPE, got `{}`",
                err.code
            );
        }

        // Non-UTF-8 BytePath survives losslessly as raw PathBuf bytes.
        #[cfg(unix)]
        {
            use base64::Engine;
            use std::os::unix::ffi::OsStrExt;
            let raw = base64::engine::general_purpose::STANDARD.encode(b"dir-\xff\xfe");
            let resolved = resolve_working_directory(
                &resolved_a,
                Some(&json!({"encoding": "base64", "value": raw})),
            )
            .unwrap();
            assert_eq!(
                resolved.file_name().unwrap().as_bytes(),
                b"dir-\xff\xfe",
                "non-UTF-8 bytes must not suffer U+FFFD replacement"
            );
        }

        // Virtual project (undeclared authorized root) is rejected.
        let v_err = resolve_project(fx.rt.store(), "proj-virtual")
            .map_err(ContractError::from)
            .err()
            .expect("virtual project must fail");
        assert_eq!(v_err.code, CODE_NOT_FOUND);

        // Symlinked authorized root (swapped after declaration) is rejected by resolve_project.
        #[cfg(unix)]
        {
            let link_root = fx._tmp.0.join("symlinked-root");
            std::fs::create_dir_all(&link_root).unwrap();
            fx.rt
                .store()
                .declare_authorized_root("root-sym", &link_root)
                .unwrap();
            fx.rt
                .store()
                .register_project(&json!({
                    "instanceId": fx.instance_id,
                    "projectId": "proj-sym",
                    "name": "Sym",
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": "root-sym", "relativePath": "proj-a"},
                }))
                .unwrap();
            std::fs::remove_dir(&link_root).unwrap();
            std::os::unix::fs::symlink(&root, &link_root).unwrap();
            let sym_err = resolve_project(fx.rt.store(), "proj-sym")
                .map_err(ContractError::from)
                .err()
                .expect("symlinked root must fail");
            assert_eq!(sym_err.code, CODE_UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn close_integrates_with_unified_operations_replays_correctly_and_fails_on_timeout() {
        let fx = fixture("term-close-ops").await;
        let epoch = fx.rt.store().meta().generation.clone();
        seed_row(fx.rt.store(), "term-close-1", &epoch);
        fx.rt
            .store()
            .terminal_finalize_exit(
                "term-close-1",
                &ExitFacts {
                    exit_code: Some(0),
                    exit_signal: None,
                },
                0,
                0,
            )
            .unwrap();

        let close_input = json!({
            "terminalId": "term-close-1",
            "expectedServerEpoch": epoch,
            "mode": "graceful",
        });
        let (outcome1, receipt1) = fx
            .rt
            .dispatch(
                "terminals.close",
                close_input.clone(),
                Some("close-k1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome1, ResponseOutcome::Accepted);
        assert_eq!(receipt1["kind"], "operation");
        assert_eq!(receipt1["deduplicated"], false);
        let op_id = receipt1["operationId"].as_str().unwrap().to_owned();
        assert_ne!(op_id, "term-close-1");
        assert_eq!(receipt1["state"], "succeeded");

        // Same-key replay must return the REAL operationId (not terminalId) + real state/revision.
        let (outcome2, receipt2) = fx
            .rt
            .dispatch("terminals.close", close_input, Some("close-k1"), None)
            .await
            .unwrap();
        assert_eq!(outcome2, ResponseOutcome::Accepted);
        assert_eq!(receipt2["operationId"], op_id);
        assert_eq!(receipt2["deduplicated"], true);
        assert_eq!(receipt2["state"], receipt1["state"]);
        assert_eq!(receipt2["acceptedRevision"], receipt1["acceptedRevision"]);

        // Same key with different input conflicts.
        let err = fx
            .rt
            .dispatch(
                "terminals.close",
                json!({
                    "terminalId": "term-close-1",
                    "expectedServerEpoch": epoch,
                    "mode": "force",
                }),
                Some("close-k1"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(c) => assert_eq!(c.code, CODE_IDEMPOTENCY_CONFLICT),
            other => panic!("unexpected error: {other:?}"),
        }

        // Visible through unified operations.get / operations.list / operations.events.
        let op_get = dispatch_ok(
            &fx.rt,
            "operations.get",
            json!({"operationId": op_id}),
            None,
        )
        .await;
        assert_eq!(op_get["operation"]["operationId"], op_id);
        assert_eq!(op_get["operation"]["operationType"], "terminals.close");
        assert_eq!(op_get["operation"]["state"], "succeeded");
        assert_eq!(
            op_get["operation"]["result"]["terminal"]["terminalId"],
            "term-close-1"
        );
        OperationRegistry::global()
            .validate_result("terminals.get", &op_get["operation"]["result"])
            .expect("completionResult.terminal passes Terminal schema");

        let listed = dispatch_ok(
            &fx.rt,
            "operations.list",
            json!({"scope": {"kind": "project", "projectId": "proj-t"}}),
            None,
        )
        .await;
        assert!(
            listed["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["operationId"] == op_id)
        );

        let events = dispatch_ok(
            &fx.rt,
            "operations.events",
            json!({"operationId": op_id}),
            None,
        )
        .await;
        assert!(!events["frame"]["events"].as_array().unwrap().is_empty());

        // Non-terminal timeout resolution must yield Failed(RESULT_UNKNOWN), never Succeeded.
        seed_row(fx.rt.store(), "term-stuck", &epoch);
        let timeout_outcome = resolve_terminal_close_outcome(
            fx.rt.terminal_registry(),
            fx.rt.store(),
            "term-stuck",
            &epoch,
            false,
        );
        match timeout_outcome {
            LongOperationOutcome::Failed { code, .. } => assert_eq!(code, CODE_RESULT_UNKNOWN),
            _ => panic!("non-terminal timeout must fail with RESULT_UNKNOWN"),
        }
        assert!(matches!(
            resolve_terminal_close_outcome(
                fx.rt.terminal_registry(),
                fx.rt.store(),
                "term-stuck",
                &epoch,
                true,
            ),
            LongOperationOutcome::Cancelled
        ));

        // Explicit operations.cancel cancels an in-flight close operation.
        let (in_flight_outcome, in_flight_receipt) = fx
            .rt
            .dispatch(
                "terminals.close",
                json!({
                    "terminalId": "term-stuck",
                    "expectedServerEpoch": epoch,
                    "mode": "graceful",
                }),
                Some("close-stuck-cancel"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(in_flight_outcome, ResponseOutcome::Accepted);
        let stuck_op_id = in_flight_receipt["operationId"]
            .as_str()
            .unwrap()
            .to_owned();
        let cancelled = dispatch_ok(
            &fx.rt,
            "operations.cancel",
            json!({"operationId": stuck_op_id}),
            Some("cancel-close-op"),
        )
        .await;
        assert_eq!(cancelled["receipt"]["disposition"], "requested");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let cur = dispatch_ok(
                &fx.rt,
                "operations.get",
                json!({"operationId": stuck_op_id}),
                None,
            )
            .await;
            if cur["operation"]["state"] == "cancelled" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cancelled close operation did not reach cancelled state: {cur}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn input_validation_does_not_advance_watermark_and_maps_partial_write_unknown() {
        let store = test_store("term-input-two-phase");
        let epoch = store.meta().generation.clone();
        seed_row(&store, "term-6", &epoch);
        let (row, token) = store
            .terminal_control_lease("term-6", "acquire", 0, None)
            .unwrap();
        let ce = row.control_epoch;

        // Validate seq=1 without committing: watermark stays 0, so retrying seq=1
        // after a clean PTY write failure is NOT deduplicated.
        assert!(
            !store
                .terminal_validate_input("term-6", &epoch, ce, &token, 1, b"echo 1\n")
                .unwrap()
        );
        assert_eq!(
            store
                .terminal_row("term-6")
                .unwrap()
                .last_accepted_input_seq,
            0
        );
        assert!(
            !store
                .terminal_validate_input("term-6", &epoch, ce, &token, 1, b"echo 1\n")
                .unwrap()
        );

        // Once committed after write succeeds, replay deduplicates cleanly.
        store
            .terminal_commit_input("term-6", 1, b"echo 1\n")
            .unwrap();
        assert!(
            store
                .terminal_validate_input("term-6", &epoch, ce, &token, 1, b"echo 1\n")
                .unwrap()
        );

        // Partial write timeout/failure maps to RESULT_UNKNOWN + EffectStatus::Unknown.
        let partial_err = map_write_input_error(
            "term-6",
            10,
            true,
            WriteInputError::TimedOut { wrote_bytes: 4 },
        );
        assert_eq!(partial_err.code, CODE_RESULT_UNKNOWN);
        assert_eq!(partial_err.effect, EffectStatus::Unknown);

        let clean_timeout = map_write_input_error(
            "term-6",
            10,
            true,
            WriteInputError::TimedOut { wrote_bytes: 0 },
        );
        assert_eq!(clean_timeout.code, CODE_UNAVAILABLE);
        assert_eq!(clean_timeout.effect, EffectStatus::NotApplied);

        let closed_err = map_write_input_error(
            "term-6",
            10,
            false,
            WriteInputError::ClosedBeforeWrite(agentcpm_pd_pty::PtyError::Io(
                std::io::Error::from_raw_os_error(libc::EIO),
            )),
        );
        assert_eq!(closed_err.code, CODE_TERMINAL_CLOSED);
        assert_eq!(closed_err.effect, EffectStatus::NotApplied);
    }

    #[tokio::test]
    async fn resize_rejects_u16_overflow_and_keeps_sqlite_untouched_until_ioctl_succeeds() {
        let fx = fixture("term-resize-bounds").await;
        let declared_root = fx._tmp.0.join("root-resize");
        std::fs::create_dir_all(declared_root.join("ws")).unwrap();
        fx.rt
            .store()
            .declare_authorized_root("root-resize", &declared_root)
            .unwrap();
        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": "proj-resize",
                "name": "Resize",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "root-resize", "relativePath": "ws"},
            }))
            .unwrap();

        // parse_dimension rejects 0, > u16::MAX, and > i64::MAX.
        assert_eq!(
            parse_dimension(&json!(0), "rows").unwrap_err().code,
            CODE_INVALID_ARGUMENT
        );
        assert_eq!(
            parse_dimension(&json!(65536), "rows").unwrap_err().code,
            CODE_INVALID_ARGUMENT
        );
        assert_eq!(
            parse_dimension(&json!(100_000), "columns")
                .unwrap_err()
                .code,
            CODE_INVALID_ARGUMENT
        );
        assert_eq!(
            parse_dimension(&json!(u64::MAX), "rows").unwrap_err().code,
            CODE_INVALID_ARGUMENT
        );

        // terminals.create rejects out-of-range dimensions via INVALID_ARGUMENT.
        let create_err = fx
            .rt
            .dispatch(
                "terminals.create",
                json!({"projectId": "proj-resize", "rows": 65536, "columns": 80}),
                Some("resize-create-bad"),
                None,
            )
            .await
            .unwrap_err();
        match create_err {
            DispatchError::Application(c) => assert_eq!(c.code, CODE_INVALID_ARGUMENT),
            other => panic!("unexpected error: {other:?}"),
        }

        // Create a real terminal, acquire lease, test resize overflow & closed mapping.
        let created = dispatch_ok(
            &fx.rt,
            "terminals.create",
            json!({"projectId": "proj-resize", "rows": 24, "columns": 80}),
            Some("resize-create-ok"),
        )
        .await;
        let tid = created["terminal"]["terminalId"]
            .as_str()
            .unwrap()
            .to_owned();
        let epoch = created["terminal"]["serverEpoch"]
            .as_str()
            .unwrap()
            .to_owned();
        let lease = dispatch_ok(
            &fx.rt,
            "terminals.control",
            json!({"terminalId": tid, "action": "acquire", "expectedControlEpoch": 0}),
            None,
        )
        .await;
        let token = lease["lease"]["leaseToken"].as_str().unwrap().to_owned();

        let resize_err = fx
            .rt
            .dispatch(
                "terminals.resize",
                json!({
                    "terminalId": tid,
                    "controlEpoch": 1,
                    "leaseToken": token,
                    "rows": 65536,
                    "columns": 100_000,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        match resize_err {
            DispatchError::Application(c) => assert_eq!(c.code, CODE_INVALID_ARGUMENT),
            other => panic!("unexpected error: {other:?}"),
        }
        let row = fx.rt.store().terminal_row(&tid).unwrap();
        assert_eq!((row.rows, row.columns), (24, 80));

        // Verify map_winsize_error separates InvalidInput vs EIO/ENXIO.
        let inval_err = map_winsize_error(
            &tid,
            agentcpm_pd_pty::PtyError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "zero",
            )),
        );
        assert_eq!(inval_err.code, CODE_INVALID_ARGUMENT);
        let eio_err = map_winsize_error(
            &tid,
            agentcpm_pd_pty::PtyError::Io(std::io::Error::from_raw_os_error(libc::EIO)),
        );
        assert_eq!(eio_err.code, CODE_TERMINAL_CLOSED);

        // Close terminal and verify resize does not touch SQLite.
        let _ = fx
            .rt
            .dispatch(
                "terminals.close",
                json!({"terminalId": tid, "expectedServerEpoch": epoch, "mode": "force"}),
                Some("resize-close"),
                None,
            )
            .await
            .unwrap();
        let closed_resize_err = fx
            .rt
            .dispatch(
                "terminals.resize",
                json!({
                    "terminalId": tid,
                    "controlEpoch": 1,
                    "leaseToken": token,
                    "rows": 40,
                    "columns": 120,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        match closed_resize_err {
            DispatchError::Application(c) => assert_eq!(c.code, CODE_TERMINAL_CLOSED),
            other => panic!("unexpected error: {other:?}"),
        }
        let row_after = fx.rt.store().terminal_row(&tid).unwrap();
        assert_eq!((row_after.rows, row_after.columns), (24, 80));

        // Wait until exited so no child process outlives test.
        for _ in 0..100 {
            if fx.rt.store().terminal_row(&tid).unwrap().state == "exited" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn create_rolls_back_on_db_error_and_serializes_concurrent_idempotency() {
        let fx = fixture("term-create-rb").await;
        let declared_root = fx._tmp.0.join("root-rb");
        std::fs::create_dir_all(declared_root.join("ws")).unwrap();
        fx.rt
            .store()
            .declare_authorized_root("root-rb", &declared_root)
            .unwrap();
        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": "proj-rb",
                "name": "Rollback",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "root-rb", "relativePath": "ws"},
            }))
            .unwrap();

        let live_before = fx.rt.terminal_registry().live_terminal_count();
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            conn.execute_batch(
                "CREATE TRIGGER fail_terminal_insert BEFORE INSERT ON terminals
                 BEGIN SELECT RAISE(ABORT, 'simulated disk error'); END;",
            )
            .unwrap();
        }

        let create_input = json!({"projectId": "proj-rb", "rows": 24, "columns": 80});
        let err = fx
            .rt
            .dispatch(
                "terminals.create",
                create_input.clone(),
                Some("k-rb-1"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(c) => assert_eq!(c.code, CODE_UNAVAILABLE),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(fx.rt.terminal_registry().live_terminal_count(), live_before);
        assert!(
            fx.rt
                .store()
                .terminal_idempotency_replay(
                    "terminals.create",
                    "k-rb-1",
                    &digest_of("terminals.create", &create_input)
                )
                .unwrap()
                .is_none(),
            "failed DB commit must not poison idempotency_records"
        );

        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            conn.execute_batch("DROP TRIGGER fail_terminal_insert;")
                .unwrap();
        }

        // Concurrent same-key creates converge to the exact same terminalId and 1 live slot.
        let (res_a, res_b) = tokio::join!(
            fx.rt.dispatch(
                "terminals.create",
                create_input.clone(),
                Some("k-rb-1"),
                None
            ),
            fx.rt
                .dispatch("terminals.create", create_input, Some("k-rb-1"), None)
        );
        let (_, val_a) = res_a.unwrap();
        let (_, val_b) = res_b.unwrap();
        let tid = val_a["terminal"]["terminalId"].as_str().unwrap().to_owned();
        assert_eq!(val_b["terminal"]["terminalId"], tid);
        let epoch = val_a["terminal"]["serverEpoch"]
            .as_str()
            .unwrap()
            .to_owned();

        let _ = fx
            .rt
            .dispatch(
                "terminals.close",
                json!({"terminalId": tid, "expectedServerEpoch": epoch, "mode": "force"}),
                Some("k-rb-close"),
                None,
            )
            .await
            .unwrap();
        for _ in 0..100 {
            if fx.rt.store().terminal_row(&tid).unwrap().state == "exited" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
