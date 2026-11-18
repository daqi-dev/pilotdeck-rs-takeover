//! `notifications.events` — the business-notification event stream (t019
//! T3).
//!
//! TS baseline (cc7c4e6): the notification surface was the push pipeline in
//! `services/notification-orchestrator.js` — event kinds `action_required` /
//! `stop` / `error` (plus plain `info`), a 20 s dedupe window keyed on the
//! event identity, preference filtering, then web-push delivery. Its run
//! hooks (`notifyRunStopped` / `notifyRunFailed`) and the always-on event
//! aggregation (`services/always-on-events.js`) existed as the producer API;
//! in the fixed baseline only the settings push-subscription confirmation
//! actually fed the orchestrator.
//!
//! v1 re-positioning (007 §7, contract acceptance): the host is the CLI
//! host, `notifications.events` is the stream the host adapts for display —
//! "事件可供宿主适配不代表浏览器Push已送达" and no WebPush authentication is
//! ever exposed. The event source is derived, not pushed: the shared
//! `events` table already records the run/interaction lifecycle
//! (`interaction.requested`, `run.completed`, `run.cancelled`,
//! `run.interrupted`, `run.result_unknown`, `run.failed`), and those rows
//! map deterministically onto the baseline's notification families:
//!
//! * `interaction.requested` → `action_required` (subject = the interaction),
//! * `run.failed` → `error` (subject = the run),
//! * `run.completed` / `run.cancelled` / `run.interrupted` /
//!   `run.result_unknown` → `stop` (subject = the run).
//!
//! Derivation happens inside the stream source / unary read (`INSERT OR
//! IGNORE` with a stable `notificationId` derived from the source event
//! identity), so it is idempotent across re-derives, concurrent
//! subscriptions and gateway restarts, and delivery order follows the source
//! event order (each derive batch runs under one immediate SQLite
//! transaction). Retention is 30 days; a resume cursor below the retained
//! window answers `CURSOR_EXPIRED` with a restart-query gap — there is no
//! notifications snapshot operation, so the recovery is to re-subscribe
//! from the earliest retained event.
//!
//! The unary form (007 §6.4 "同一个操作以 follow=false 作为一元调用时，返回一
//! 页数据"): the result schema is one notification object, so the page is
//! the next notification after `afterCursor`; when none is pending the
//! call long-polls up to [`UNARY_WAIT`] and then answers `DEADLINE_EXCEEDED`
//! (the error list's deadline code — an empty page is not expressible in the
//! all-strings result schema). This choice is recorded in the task report.

use std::sync::Arc;
use std::time::Duration;

use agentcpm_pd_protocol::contract::{EventEnvelope, ResponseOutcome};
use agentcpm_pd_protocol::errors::{
    CODE_CURSOR_EXPIRED, CODE_INVALID_ARGUMENT, CODE_UNAVAILABLE, ContractError, EffectStatus,
    RetrySpec,
};
use agentcpm_pd_protocol::wire::{
    JSONRPC_VERSION, JsonRpcNotification, SubEventNotification, WIRE_SUB_EVENT,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::public_v1::PublicV1State;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;

/// The stream's own cursor prefix (`ncur-<seq>`); distinct from the shared
/// events table's `cur-<seq>` space because the seq here is the
/// notifications table's own monotonic row id.
const CURSOR_PREFIX: &str = "ncur-";

/// Notification retention. Mirrors the scale of the shared events table's
/// retention; a consumer that resumes after this window gets
/// `CURSOR_EXPIRED` and restarts from the earliest retained event.
const RETENTION_DAYS: i64 = 30;

/// How long the unary (non-follow) read waits for the next notification
/// before answering `DEADLINE_EXCEEDED`.
const UNARY_WAIT: Duration = Duration::from_secs(25);

/// The delivery state of a recorded notification: available for the host
/// to adapt (explicitly not "pushed" — the contract's boundary).
const DELIVERY_STATE: &str = "available";

/// The wire event type of every delivered frame (the operation's declared
/// event); the notification's own family rides in `data.eventType`.
const EVENT_TYPE: &str = "notification.available";

/// The source event types that derive notifications, mapped to their
/// families.
fn family_of(event_type: &str) -> Option<&'static str> {
    match event_type {
        "interaction.requested" => Some("action_required"),
        "run.failed" => Some("error"),
        "run.completed" | "run.cancelled" | "run.interrupted" | "run.result_unknown" => {
            Some("stop")
        }
        _ => None,
    }
}

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "notifications",
        "CREATE TABLE IF NOT EXISTS notifications (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            notification_id TEXT NOT NULL UNIQUE,
            event_type TEXT NOT NULL,
            subject_id TEXT NOT NULL,
            subject_project_id TEXT,
            subject_session_id TEXT NOT NULL,
            run_id TEXT NOT NULL,
            summary TEXT NOT NULL,
            delivery_state TEXT NOT NULL,
            occurred_at TEXT NOT NULL,
            source_seq INTEGER NOT NULL
        );",
    );
    // The dispatch handler serves the unary (non-follow) poll read; the
    // follow stream is the StreamSource registered right after
    // (`sub.open` admits the operation because the handler marks it
    // implemented, then resolves the factory).
    reg.handler("notifications.events", |rt, input, _key, _rev| {
        Box::pin(notifications_events_unary(rt, input))
    });
    crate::public_v1::ws::register_stream_source("notifications.events", notifications_source);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn invalid_arg(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn storage_err(message: String) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {message}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn sql_err(error: rusqlite::Error) -> ContractError {
    storage_err(error.to_string())
}

// ---------------------------------------------------------------------------
// Filter, cursor, derivation
// ---------------------------------------------------------------------------

/// One subscription's or poll's scope and family filter.
#[derive(Clone, Debug)]
struct NotificationFilter {
    scope_kind: String,
    scope_id: Option<String>,
    event_types: Vec<String>,
}

fn parse_filter(input: &Value) -> Result<NotificationFilter, ContractError> {
    let scope = input
        .get("scope")
        .ok_or_else(|| invalid_arg("`scope` is required"))?;
    let kind = scope
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let id = scope
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);
    match kind.as_str() {
        "global" => {
            if id.is_some() {
                return Err(invalid_arg(
                    "scope.id must be null when scope.kind is `global`",
                ));
            }
        }
        "project" | "session" => {
            if id.is_none() {
                return Err(invalid_arg(format!(
                    "scope.id is required when scope.kind is `{kind}`"
                )));
            }
        }
        other => {
            return Err(invalid_arg(format!(
                "scope.kind must be global, project or session, got `{other}`"
            )));
        }
    }
    let event_types = input
        .get("eventTypes")
        .and_then(Value::as_array)
        .map(|types| {
            types
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(NotificationFilter {
        scope_kind: kind,
        scope_id: id,
        event_types,
    })
}

fn cursor_of(seq: i64) -> String {
    format!("{CURSOR_PREFIX}{seq}")
}

fn parse_cursor(cursor: &str) -> Option<i64> {
    cursor
        .strip_prefix(CURSOR_PREFIX)
        .and_then(|rest| rest.parse::<i64>().ok())
}

/// The earliest retained notification seq, or `None` when nothing is
/// retained (no retention floor to enforce).
fn earliest_retained(conn: &Connection) -> Result<Option<i64>, ContractError> {
    conn.query_row("SELECT MIN(seq) FROM notifications", [], |row| row.get(0))
        .map_err(sql_err)
}

/// Resolves the start position for a subscription / poll: the default
/// starts at the beginning of the retained log; an `afterCursor` older than
/// the retained window answers CURSOR_EXPIRED (open-time rejection, the
/// run.events position); a malformed cursor answers the same with the
/// restart-query recovery policy.
fn resolve_start(conn: &Connection, after_cursor: Option<&str>) -> Result<i64, ContractError> {
    let Some(cursor) = after_cursor else {
        return Ok(0);
    };
    let Some(seq) = parse_cursor(cursor) else {
        let mut error = ContractError::new(
            CODE_CURSOR_EXPIRED,
            format!("invalid cursor `{cursor}`"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        );
        error.details = Some(gap_details(cursor, None));
        return Err(error);
    };
    let earliest = earliest_retained(conn)?;
    // Like run.events' `seq >= earliest - 1`: a cursor pointing at the last
    // pruned item is fine (delivery continues strictly after it).
    if let Some(earliest) = earliest
        && seq + 1 < earliest
    {
        let mut error = ContractError::new(
            CODE_CURSOR_EXPIRED,
            format!(
                "afterCursor `{cursor}` is older than the earliest retained notification; \
                 re-subscribe from the earliest retained cursor"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        );
        error.details = Some(gap_details(cursor, earliest));
        return Err(error);
    }
    Ok(seq)
}

fn gap_details(requested: &str, earliest: Option<i64>) -> serde_json::Map<String, Value> {
    json!({
        "gap": {
            "kind": "gap",
            "requestedCursor": requested,
            "earliestCursor": earliest.map(|seq| cursor_of(seq)),
            "snapshotRevision": "notifications",
            "recovery": "restart_query",
        }
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// One derived notification row.
struct StoredNotification {
    seq: i64,
    notification_id: String,
    event_type: String,
    subject_id: String,
    run_id: String,
    summary: String,
    delivery_state: String,
    occurred_at: String,
}

impl StoredNotification {
    /// The contract result object of `notifications.events`.
    fn to_result_json(&self) -> Value {
        json!({
            "cursor": cursor_of(self.seq),
            "notificationId": self.notification_id,
            "eventType": self.event_type,
            "occurredAt": self.occurred_at,
            "subjectId": self.subject_id,
            "summary": self.summary,
            "deliveryState": self.delivery_state,
        })
    }
}

fn notification_matches(filter: &NotificationFilter, row: &StoredNotification) -> bool {
    if !filter.event_types.is_empty()
        && !filter
            .event_types
            .iter()
            .any(|wanted| *wanted == row.event_type)
    {
        return false;
    }
    match filter.scope_kind.as_str() {
        "project" => row.subject_project_id.as_deref() == filter.scope_id.as_deref(),
        "session" => filter.scope_id.as_deref() == Some(row.subject_session_id.as_str()),
        _ => true,
    }
}

/// Derives notifications from the shared `events` table and prunes past
/// retention. Idempotent: the source event's identity decides the
/// `notificationId`, so re-derivation inserts nothing, and the whole scan
/// runs under one immediate transaction (a concurrent derive serializes and
/// re-covers the same range as no-ops).
fn derive_notifications(store: &Arc<V1Store>) -> Result<bool, ContractError> {
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let last_derived: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(source_seq), 0) FROM notifications",
            [],
            |row| row.get(0),
        )
        .map_err(sql_err)?;

    let mut stmt = tx
        .prepare(
            "SELECT seq, event_type, resource_id, session_id, payload_json, occurred_at
             FROM events WHERE seq > ?1 ORDER BY seq",
        )
        .map_err(sql_err)?;
    let sources: Vec<(i64, String, String, String, String, String)> = stmt
        .query_map(params![last_derived], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);

    let mut inserted = false;
    for (source_seq, event_type, resource_id, session_id, payload_json, occurred_at) in sources {
        let Some(family) = family_of(&event_type) else {
            continue;
        };
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or_else(|_| json!({}));
        // The subject is what the notification is about: the interaction
        // for action_required (its run rides the envelope), the run for
        // the run families.
        let subject_id = if family == "action_required" {
            payload
                .get("interactionId")
                .and_then(Value::as_str)
                .unwrap_or(&resource_id)
                .to_owned()
        } else {
            resource_id.clone()
        };
        let summary = match family {
            "action_required" => {
                let kind = payload
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("interaction");
                format!("Run {resource_id} is waiting for a {kind} (interaction {subject_id})")
            }
            "error" => match payload.get("errorCode").and_then(Value::as_str) {
                Some(code) => format!("Run {resource_id} failed ({code})"),
                None => format!("Run {resource_id} failed"),
            },
            _ => {
                let verb = match event_type.as_str() {
                    "run.completed" => "completed",
                    "run.cancelled" => "cancelled",
                    "run.interrupted" => "interrupted",
                    _ => "ended with an unknown result",
                };
                format!("Run {resource_id} {verb}")
            }
        };
        // The stable notification identity: family + subject. A business
        // event that somehow lands twice derives the same id and the second
        // insert is ignored — the contract's notificationId dedupe.
        let notification_id = format!("ntf-{family}:{subject_id}");
        let project_id: Option<String> = tx
            .query_row(
                "SELECT project_id FROM runs WHERE run_id = ?1",
                params![resource_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let changed = tx
            .execute(
                "INSERT OR IGNORE INTO notifications
                    (notification_id, event_type, subject_id, subject_project_id,
                     subject_session_id, run_id, summary, delivery_state, occurred_at, source_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    notification_id,
                    family,
                    subject_id,
                    project_id,
                    session_id,
                    resource_id,
                    summary,
                    DELIVERY_STATE,
                    occurred_at,
                    source_seq,
                ],
            )
            .map_err(sql_err)?;
        inserted |= changed != 0;
    }

    if inserted {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(RETENTION_DAYS)).to_rfc3339();
        tx.execute(
            "DELETE FROM notifications WHERE occurred_at < ?1",
            params![cutoff],
        )
        .map_err(sql_err)?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(inserted)
}

/// Reads retained notifications strictly after `after_seq`, filtered by
/// `filter`, in delivery order.
fn notifications_after(
    store: &Arc<V1Store>,
    after_seq: i64,
    filter: &NotificationFilter,
    limit: usize,
) -> Result<Vec<StoredNotification>, ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let mut stmt = conn
        .prepare(
            "SELECT seq, notification_id, event_type, subject_id, run_id, summary,
                    delivery_state, occurred_at
             FROM notifications WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
        )
        .map_err(sql_err)?;
    let rows: Vec<StoredNotification> = stmt
        .query_map(params![after_seq, limit as i64], |row| {
            Ok(StoredNotification {
                seq: row.get(0)?,
                notification_id: row.get(1)?,
                event_type: row.get(2)?,
                subject_id: row.get(3)?,
                run_id: row.get(4)?,
                summary: row.get(5)?,
                delivery_state: row.get(6)?,
                occurred_at: row.get(7)?,
            })
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    Ok(rows
        .into_iter()
        .filter(|row| notification_matches(filter, row))
        .collect())
}

// ---------------------------------------------------------------------------
// The unary (non-follow) read: poll the next notification
// ---------------------------------------------------------------------------

async fn notifications_events_unary(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let filter = parse_filter(&input)?;
    let after_cursor = input
        .get("afterCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty());
    let store = rt.store().clone();
    let after_seq = {
        let conn = store.conn.lock().expect("sqlite mutex");
        resolve_start(&conn, after_cursor)?
    };

    let deadline = tokio::time::Instant::now() + UNARY_WAIT;
    loop {
        derive_notifications(&store)?;
        let found = notifications_after(&store, after_seq, &filter, 1)?;
        if let Some(next) = found.first() {
            return Ok((ResponseOutcome::Succeeded, next.to_result_json()));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ContractError::new(
                "DEADLINE_EXCEEDED",
                "no notification is pending for this scope; subscribe with follow=true to \
                 observe the stream instead of polling",
                RetrySpec::backoff_read(Some(1000)),
                EffectStatus::NotApplied,
            )
            .into());
        }
        let notifier = store.event_notifier();
        let notified = notifier.notified();
        let wait = tokio::time::sleep_until(deadline);
        tokio::select! {
            _ = notified => {}
            _ = wait => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The follow stream source
// ---------------------------------------------------------------------------

/// The `notifications.events` data plane for the subscription pump.
struct NotificationsSource {
    store: Arc<V1Store>,
    instance_id: String,
    sub_id: String,
    filter: NotificationFilter,
    last_seq: i64,
}

/// The `sub.open` admission: scope validation and start-position resolution
/// before any subscription id becomes visible.
fn notifications_source(
    state: &Arc<PublicV1State>,
    input: &Value,
    sub_id: &str,
) -> Result<Box<dyn crate::public_v1::ws::StreamSource>, ContractError> {
    let store = state.runtime.store().clone();
    let filter = parse_filter(input)?;
    let after_cursor = input
        .get("afterCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty());
    let last_seq = {
        let conn = store.conn.lock().expect("sqlite mutex");
        resolve_start(&conn, after_cursor)?
    };
    Ok(Box::new(NotificationsSource {
        instance_id: store.meta().instance_id.clone(),
        store,
        sub_id: sub_id.to_owned(),
        filter,
        last_seq,
    }))
}

impl crate::public_v1::ws::StreamSource for NotificationsSource {
    fn fetch_next(
        &mut self,
        batch: usize,
    ) -> Result<crate::public_v1::ws::StreamFetch, ContractError> {
        derive_notifications(&self.store)?;
        let rows = notifications_after(&self.store, self.last_seq, &self.filter, batch)?;
        let mut frames = Vec::with_capacity(rows.len());
        for row in &rows {
            self.last_seq = row.seq;
            let data = row.to_result_json();
            let notification = JsonRpcNotification {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                method: WIRE_SUB_EVENT.to_owned(),
                params: serde_json::to_value(SubEventNotification {
                    subscription_id: self.sub_id.clone(),
                    event: EventEnvelope {
                        version: "agentcpm-pd.event/1".to_owned(),
                        instance_id: self.instance_id.clone(),
                        run_id: Some(row.run_id.clone()),
                        operation_id: None,
                        subscription_id: Some(self.sub_id.clone()),
                        cursor: cursor_of(row.seq),
                        event_type: EVENT_TYPE.to_owned(),
                        data: data.as_object().cloned().unwrap_or_default(),
                    },
                })
                .unwrap_or(Value::Null),
            };
            let text = serde_json::to_string(&notification).unwrap_or_default();
            frames.push(crate::public_v1::ws::StreamFrame {
                estimated_bytes: text.len() + 128,
                text,
                cursor: cursor_of(row.seq),
            });
        }
        // The notification log never reaches a terminal state: the
        // subscription stays open until the consumer closes it.
        Ok(crate::public_v1::ws::StreamFetch {
            frames,
            stream_ended: false,
        })
    }

    fn wake(&self) -> Arc<Notify> {
        self.store.event_notifier()
    }

    fn oversized_recovery_hint(&self) -> &'static str {
        "re-subscribe with --after-cursor set to the last delivered notification cursor"
    }
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
    use std::path::PathBuf;

    struct Fixture {
        _tmp: TempDir,
        rt: Arc<PublicV1Runtime>,
        home: PathBuf,
        instance_id: String,
    }

    fn fixture(tag: &str) -> Fixture {
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

    async fn seed_project_session(fx: &Fixture, tag: &str) -> (String, String) {
        let (project_id, session_id) = (format!("proj-{tag}"), format!("sess-{tag}"));
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": fx.instance_id,
                    "projectId": project_id,
                    "name": "Notifications tests",
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": "root", "relativePath": ""}
                }),
                None,
                None,
            )
            .await
            .unwrap();
        fx.rt
            .dispatch(
                "session.create",
                json!({"instanceId": fx.instance_id, "sessionId": session_id, "projectId": project_id}),
                None,
                None,
            )
            .await
            .unwrap();
        (project_id, session_id)
    }

    /// Submits a run and waits for it to become terminal (the offline driver
    /// executes it through the session worker `run.submit` starts).
    async fn run_to_terminal(fx: &Fixture, session_id: &str, text: &str, key: &str) -> String {
        let run_id = submit_run(fx, session_id, text, key).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let (_, page) = fx
                .rt
                .dispatch(
                    "run.get",
                    json!({"instanceId": fx.instance_id, "runId": run_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if page["run"]["terminal"] == true {
                return run_id;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run `{run_id}` never became terminal"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn submit_run(fx: &Fixture, session_id: &str, text: &str, key: &str) -> String {
        let (_, submitted) = fx
            .rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": fx.instance_id,
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": text}]
                }),
                Some(key),
                None,
            )
            .await
            .unwrap();
        submitted["runId"].as_str().unwrap().to_owned()
    }

    /// Resumes a session queue a failed run paused (`run_failed`).
    async fn resume_paused_queue(fx: &Fixture, session_id: &str) {
        let (_, page) = fx
            .rt
            .dispatch(
                "queue.get",
                json!({"instanceId": fx.instance_id, "sessionId": session_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let queue = &page["queue"];
        if queue["paused"] != true {
            return;
        }
        fx.rt
            .dispatch(
                "queue.resume",
                json!({
                    "instanceId": fx.instance_id,
                    "sessionId": session_id,
                    "expectedRevision": queue["revision"],
                    "acknowledgedPauseReason": queue["pauseReason"],
                }),
                None,
                None,
            )
            .await
            .unwrap();
    }

    async fn wait_notification(fx: &Fixture, filter_input: Value, after_seq: i64) -> Option<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let result = fx
                .rt
                .dispatch(
                    "notifications.events",
                    {
                        let mut input = filter_input.clone();
                        if after_seq > 0 {
                            input["afterCursor"] = json!(cursor_of(after_seq));
                        }
                        input
                    },
                    None,
                    None,
                )
                .await;
            match result {
                Ok((_, notification)) => return Some(notification),
                Err(DispatchError::Application(error)) if error.code == "DEADLINE_EXCEEDED" => {}
                Err(other) => panic!("unexpected notifications.events error: {other:?}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expected notification never arrived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn run_lifecycle_derives_the_three_families() {
        let fx = fixture("ntf-families");
        let (project_id, session_id) = seed_project_session(&fx, "fam").await;

        // A failed run derives an `error` notification.
        let failed_run = run_to_terminal(&fx, &session_id, "[pd-driver:fail]", "k-fail").await;
        let notification = wait_notification(&fx, json!({"scope": {"kind": "global"}}), 0)
            .await
            .expect("error notification");
        assert_eq!(notification["eventType"], "error");
        assert_eq!(notification["subjectId"], failed_run);
        assert_eq!(notification["deliveryState"], "available");
        assert!(
            notification["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("failed"))
        );
        assert!(
            notification["notificationId"]
                .as_str()
                .is_some_and(|id| id.starts_with("ntf-error:"))
        );
        assert!(
            notification["cursor"]
                .as_str()
                .is_some_and(|cursor| cursor.starts_with("ncur-"))
        );
        let error_cursor = notification["cursor"].as_str().unwrap().to_owned();
        let error_seq = parse_cursor(&error_cursor).unwrap();

        // The failed run paused its session queue; resume before the next.
        resume_paused_queue(&fx, &session_id).await;

        // A completed run derives a `stop` notification; the family filter
        // and the cursor skip the already-delivered one.
        let done_run = run_to_terminal(&fx, &session_id, "plain text", "k-done").await;
        let notification = wait_notification(
            &fx,
            json!({"scope": {"kind": "global"}, "eventTypes": ["stop"]}),
            error_seq,
        )
        .await
        .expect("stop notification");
        assert_eq!(notification["eventType"], "stop");
        assert_eq!(notification["subjectId"], done_run);

        // An approval-waiting run derives `action_required`; the subject is
        // the interaction (the run itself is not the subject). The run
        // waits for the interaction answer, so only the notification is
        // awaited, not the run's terminal state.
        let approval_run = submit_run(&fx, &session_id, "[pd-driver:approval]", "k-appr").await;
        let notification = wait_notification(
            &fx,
            json!({"scope": {"kind": "session", "id": session_id}, "eventTypes": ["action_required"]}),
            0,
        )
        .await
        .expect("action_required notification");
        assert_eq!(notification["eventType"], "action_required");
        assert!(
            notification["subjectId"]
                .as_str()
                .is_some_and(|subject| subject != approval_run)
        );
        assert!(
            notification["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("approval"))
        );

        // Project scope filters by the runs' project.
        let notification = wait_notification(
            &fx,
            json!({"scope": {"kind": "project", "id": project_id}}),
            0,
        )
        .await
        .expect("project-scoped notification");
        assert_eq!(notification["eventType"], "error");

        // Scope misuse rejects.
        for bad in [
            json!({"scope": {"kind": "global", "id": "x"}}),
            json!({"scope": {"kind": "project"}}),
            json!({"scope": {"kind": "session"}}),
        ] {
            let error = fx
                .rt
                .dispatch("notifications.events", bad, None, None)
                .await
                .unwrap_err();
            match error {
                DispatchError::Application(error) => assert_eq!(error.code, CODE_INVALID_ARGUMENT),
                other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn derivation_is_idempotent_and_dedupes_notification_ids() {
        let fx = fixture("ntf-dedupe");
        let (_project_id, session_id) = seed_project_session(&fx, "dedupe").await;
        run_to_terminal(&fx, &session_id, "[pd-driver:fail]", "k-dedupe").await;

        // Two consecutive derives insert exactly one row.
        derive_notifications(&fx.rt.store().clone()).unwrap();
        derive_notifications(&fx.rt.store().clone()).unwrap();
        let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notifications", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "one failed run = one notification");
        let (notification_id, event_type): (String, String) = conn
            .query_row(
                "SELECT notification_id, event_type FROM notifications",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(conn);
        assert_eq!(event_type, "error");
        assert!(notification_id.starts_with("ntf-error:"));
    }

    #[test]
    fn expired_and_malformed_cursors_reject_with_a_gap() {
        let fx = fixture("ntf-cursor");
        // Seed retained rows, then prune the early ones: a cursor below the
        // retained window answers CURSOR_EXPIRED with the gap descriptor
        // (the open-time position run.events holds); a malformed cursor
        // answers the same with the restart-query recovery.
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            for index in 0..3 {
                conn.execute(
                    "INSERT INTO notifications
                        (notification_id, event_type, subject_id, subject_project_id,
                         subject_session_id, run_id, summary, delivery_state, occurred_at, source_seq)
                     VALUES (?1, 'stop', ?2, NULL, '', ?2, 'row', 'available', ?3, ?4)",
                    params![
                        format!("ntf-cursor:{index}"),
                        format!("run-{index}"),
                        format!("2026-11-{index:02}T00:00:00+00:00"),
                        index
                    ],
                )
                .unwrap();
            }
            conn.execute("DELETE FROM notifications WHERE seq <= 2", [])
                .unwrap();
            // Rows retained from seq 3: a cursor at 0 points before the window.
            let error = resolve_start(&conn, Some("ncur-0")).unwrap_err();
            assert_eq!(error.code, CODE_CURSOR_EXPIRED);
            let gap = error.details.as_ref().expect("gap details");
            assert_eq!(
                gap["gap"]["earliestCursor"],
                json!(cursor_of(3)),
                "the gap names the earliest retained cursor: {gap}"
            );
            assert_eq!(gap["gap"]["recovery"], "restart_query");
            // A cursor at the last pruned position still resumes.
            assert_eq!(resolve_start(&conn, Some("ncur-2")).unwrap(), 2);
            // A malformed cursor answers the same code.
            let error = resolve_start(&conn, Some("not-a-cursor")).unwrap_err();
            assert_eq!(error.code, CODE_CURSOR_EXPIRED);
            assert!(
                error
                    .details
                    .as_ref()
                    .is_some_and(|details| details.contains_key("gap"))
            );
            // The default starts at the beginning of the retained log.
            assert_eq!(resolve_start(&conn, None).unwrap(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unary_poll_answers_deadline_when_nothing_is_pending() {
        let fx = fixture("ntf-deadline");
        let error = fx
            .rt
            .dispatch(
                "notifications.events",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match error {
            DispatchError::Application(error) => {
                assert_eq!(error.code, "DEADLINE_EXCEEDED");
                assert_eq!(error.effect, EffectStatus::NotApplied);
            }
            other => panic!("expected DEADLINE_EXCEEDED, got {other:?}"),
        }
    }

    #[test]
    fn family_mapping_covers_exactly_the_lifecycle_events() {
        assert_eq!(family_of("interaction.requested"), Some("action_required"));
        assert_eq!(family_of("run.failed"), Some("error"));
        for event in [
            "run.completed",
            "run.cancelled",
            "run.interrupted",
            "run.result_unknown",
        ] {
            assert_eq!(family_of(event), Some("stop"));
        }
        assert_eq!(family_of("run.started"), None);
        assert_eq!(family_of("run.queued"), None);
        assert_eq!(family_of("run.output.delta"), None);
        assert_eq!(family_of("integration.documentChanged"), None);
    }
}
