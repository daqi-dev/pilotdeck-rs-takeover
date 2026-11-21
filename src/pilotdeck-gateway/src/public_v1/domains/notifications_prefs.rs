//! `notifications.get_preferences` / `set_preferences` — the v1
//! notification-preference domain (t019 T2).
//!
//! TS 语义参考（cc7c4e6）：`ui/server/routes/settings.js:211-227` +
//! `database/init.sql:55-80` 的 `user_notification_preferences` 单行
//! `preferences_json`。v1 重定位（007 CLI 语义）：偏好是 scope 化的
//! `(eventType, deliveryChannelId, enabled)` 列表，条件写
//! （`expectedRevision` CAS，创建用 `absent`），重启保持（SQLite 落盘）。
//! 「偏好读取、修改、重启保持，并影响事件投递选择」——投递侧
//! （`notifications.events`，T3 的 stream）读同一张表；本域只提供读写面。
//!
//! `deliveryChannels` 是已配置渠道的投递可用性视图（复用 `channels.rs`
//! 的存储行）：`available` 仅对 `ready` 状态为真（「禁用后不选择该渠道
//! 投递新事件」「不伪报在线」）。偏好里禁用某渠道 = 之后新事件不再向
//! 该渠道投递；已发出的在途消息不追溯撤回（合同 acceptance 原文语义）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::channels::parse_v1_scope;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// The absent-scope revision baseline (no preference row was ever written).
const ABSENT_REVISION: &str = "nrev-00000000";

const DDL_PREFERENCES: &str = "CREATE TABLE IF NOT EXISTS v1_notification_preferences (
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL DEFAULT '',
    preferences_json TEXT NOT NULL,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (scope_kind, scope_id)
);";

const DDL_META: &str = "CREATE TABLE IF NOT EXISTS v1_notifications_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_notification_preferences", DDL_PREFERENCES);
    reg.schema("v1_notifications_meta", DDL_META);
    reg.handler("notifications.get_preferences", |rt, input, _key, _rev| {
        Box::pin(notifications_get_preferences(rt, input))
    });
    reg.handler("notifications.set_preferences", |rt, input, _key, _rev| {
        Box::pin(notifications_set_preferences(rt, input))
    });
}

fn sql_err(context: &str, error: rusqlite::Error) -> ContractError {
    ContractError::new(
        agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
        format!("{context}: {error}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// This domain's own connection to the shared state DB (the credentials /
/// channels domains keep theirs; transactions are short and WAL keeps them
/// concurrent-safe).
struct NotificationsDb {
    conn: std::sync::Mutex<Connection>,
}

impl NotificationsDb {
    fn open(home: &std::path::Path) -> Result<Self, ContractError> {
        let dir = home.join("v1");
        std::fs::create_dir_all(&dir).map_err(|e| {
            ContractError::new(
                agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
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
        for ddl in [DDL_PREFERENCES, DDL_META] {
            conn.execute_batch(ddl)
                .map_err(|e| sql_err("notifications ddl", e))?;
        }
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
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

    fn preferences_row(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<(String, String)>, ContractError> {
        self.with_conn("read notification preferences", |conn| {
            conn.query_row(
                "SELECT preferences_json, revision FROM v1_notification_preferences
                 WHERE scope_kind = ?1 AND scope_id = ?2",
                params![kind, id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
        })
    }

    /// The conditional full write as ONE transaction on ONE connection (the
    /// `credentials.rs:347` single-transaction CAS precedent): read the
    /// stored row, gate on `expectedRevision`, bump the counter and write
    /// the row all inside the same `BEGIN…COMMIT`, so two concurrent
    /// `set_preferences` calls cannot both pass the same `expectedRevision`
    /// and silently overwrite each other. A gate failure rolls the whole
    /// transaction back — zero side effects, not even the counter bump.
    /// The absent sentinel semantics are unchanged (162fafc47): a missing
    /// row matches `"absent"` or the absent baseline revision, and `absent`
    /// never matches an existing row.
    fn write_preferences_cas(
        &self,
        kind: &str,
        id: &str,
        preferences: &[Value],
        expected_revision: &str,
    ) -> Result<String, ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin preferences cas tx", e))?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT revision FROM v1_notification_preferences
                 WHERE scope_kind = ?1 AND scope_id = ?2",
                params![kind, id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| sql_err("read notification preferences", e))?;
        let current = existing.as_deref().unwrap_or(ABSENT_REVISION);
        let matches = match &existing {
            None => expected_revision == "absent" || expected_revision == ABSENT_REVISION,
            Some(revision) => expected_revision == revision.as_str(),
        };
        if !matches {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!("expectedRevision `{expected_revision}` does not match `{current}`"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let counter: i64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM v1_notifications_meta WHERE key = 'rev_counter'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_err("read notifications revision", e))?
            .unwrap_or(0);
        let next = counter + 1;
        tx.execute(
            "INSERT INTO v1_notifications_meta (key, value) VALUES ('rev_counter', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![next.to_string()],
        )
        .map_err(|e| sql_err("write notifications revision", e))?;
        let revision = format!("nrev-{next:08}");
        let preferences_json = serde_json::to_string(&Value::Array(preferences.to_vec()))
            .unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT INTO v1_notification_preferences (scope_kind, scope_id,
                preferences_json, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(scope_kind, scope_id) DO UPDATE SET
                preferences_json = excluded.preferences_json,
                revision = excluded.revision,
                updated_at = excluded.updated_at",
            params![kind, id, preferences_json, revision, now_rfc3339()],
        )
        .map_err(|e| sql_err("write notification preferences", e))?;
        tx.commit()
            .map_err(|e| sql_err("commit preferences cas tx", e))?;
        Ok(revision)
    }

    /// The delivery-adapter view over the channels store (all scopes — the
    /// delivery adapters are instance resources).
    fn delivery_channels(&self) -> Result<Vec<Value>, ContractError> {
        self.with_conn("list delivery channels", |conn| {
            let mut stmt = conn.prepare(
                "SELECT channel_id, enabled, state, reason_code FROM v1_channels
                 ORDER BY created_at ASC, channel_id ASC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .map(|(channel_id, enabled, state, reason_code)| {
                    let available = enabled && state == "ready";
                    let reason_code = if available {
                        None
                    } else if !enabled {
                        Some("CHANNEL_DISABLED".to_owned())
                    } else if state == "failed" {
                        Some(reason_code.unwrap_or_else(|| "ASSEMBLY_FAILED".to_owned()))
                    } else {
                        Some("NOT_READY".to_owned())
                    };
                    json!({
                        "channelId": channel_id,
                        "available": available,
                        "reasonCode": reason_code,
                    })
                })
                .collect())
        })
    }

    fn append_event(
        &self,
        resource_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), ContractError> {
        self.with_conn("append notifications event", |conn| {
            let placeholder =
                format!("tmp-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
            conn.execute(
                "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
                 VALUES (?1, ?2, '', ?3, 'agentcpm-pd.event/1', ?4, ?4, ?5)",
                params![
                    placeholder,
                    resource_id,
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

fn db_registry() -> &'static std::sync::Mutex<HashMap<PathBuf, Weak<NotificationsDb>>> {
    static REGISTRY: OnceLock<std::sync::Mutex<HashMap<PathBuf, Weak<NotificationsDb>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn db_for(rt: &Arc<PublicV1Runtime>) -> Result<Arc<NotificationsDb>, ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut registry = db_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.retain(|_, weak| weak.strong_count() > 0);
    if let Some(existing) = registry.get(&home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let db = Arc::new(NotificationsDb::open(&home)?);
    registry.insert(home, Arc::downgrade(&db));
    Ok(db)
}

#[cfg(test)]
pub(crate) fn shutdown_for_tests(home: &std::path::Path) {
    let mut registry = db_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.remove(home);
}

fn validate_preferences(preferences: &[Value]) -> Result<(), ContractError> {
    let invalid = |msg: String| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            msg,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    for (idx, item) in preferences.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(invalid(format!("preferences[{idx}] must be an object")));
        };
        for key in obj.keys() {
            if !matches!(key.as_str(), "eventType" | "deliveryChannelId" | "enabled") {
                return Err(invalid(format!(
                    "preferences[{idx}] contains unknown field `{key}`"
                )));
            }
        }
        if obj
            .get("eventType")
            .and_then(Value::as_str)
            .is_none_or(|s| s.is_empty())
        {
            return Err(invalid(format!(
                "preferences[{idx}].eventType must be a non-empty string"
            )));
        }
        if obj
            .get("deliveryChannelId")
            .and_then(Value::as_str)
            .is_none_or(|s| s.is_empty())
        {
            return Err(invalid(format!(
                "preferences[{idx}].deliveryChannelId must be a non-empty string"
            )));
        }
        if obj.get("enabled").and_then(Value::as_bool).is_none() {
            return Err(invalid(format!(
                "preferences[{idx}].enabled must be a boolean"
            )));
        }
    }
    Ok(())
}

/// `notifications.get_preferences` — the read view: the scope's stored
/// preference list (empty until written), its revision, and the delivery
/// adapters' availability.
async fn notifications_get_preferences(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_v1_scope(&rt, &input)?;
    let (kind, id) = scope.storage_parts();
    let rt_blocking = rt.clone();
    let kind_owned = kind.to_owned();
    let id_owned = id.clone();
    let (row, delivery_channels) = tokio::task::spawn_blocking(move || {
        let db = db_for(&rt_blocking)?;
        let row = db.preferences_row(&kind_owned, &id_owned)?;
        let delivery_channels = db.delivery_channels()?;
        Ok::<_, ContractError>((row, delivery_channels))
    })
    .await
    .map_err(|e| {
        ContractError::new(
            agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
            format!("notifications get task failed: {e}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })??;

    let (preferences, revision) = match row {
        Some((preferences_json, revision)) => (
            serde_json::from_str::<Value>(&preferences_json)
                .ok()
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default(),
            revision,
        ),
        None => (Vec::new(), ABSENT_REVISION.to_owned()),
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "revision": revision,
            "preferences": preferences,
            "deliveryChannels": delivery_channels,
        }),
    ))
}

/// `notifications.set_preferences` — the conditional full write. The stored
/// list replaces the previous one atomically; a disabled entry means later
/// events skip that delivery channel.
async fn notifications_set_preferences(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_v1_scope(&rt, &input)?;
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                "`expectedRevision` is required",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?
        .to_owned();
    let preferences = input
        .get("preferences")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                "`preferences` is required",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
    validate_preferences(&preferences)?;

    let (kind, id) = scope.storage_parts();
    let rt_blocking = rt.clone();
    let kind_owned = kind.to_owned();
    let id_owned = id.clone();
    let revision = tokio::task::spawn_blocking(move || {
        let db = db_for(&rt_blocking)?;
        let revision =
            db.write_preferences_cas(&kind_owned, &id_owned, &preferences, &expected_revision)?;
        let _ = db.append_event(
            "notification-preferences",
            "notifications.preferencesChanged",
            &json!({"scope": {"kind": kind_owned, "id": id_owned}, "revision": revision}),
        );
        Ok::<_, ContractError>(revision)
    })
    .await
    .map_err(|e| {
        ContractError::new(
            agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
            format!("notifications set task failed: {e}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })??;
    rt.store().event_notifier().notify_waiters();
    Ok((
        ResponseOutcome::Succeeded,
        json!({"revision": revision, "applied": true, "generation": Value::Null}),
    ))
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
    use std::sync::Arc;

    struct Fixture {
        _tmp: TempDir,
        rt: Arc<PublicV1Runtime>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            shutdown_for_tests(self.rt.store().home());
            crate::public_v1::domains::channels::shutdown_for_tests(self.rt.store().home());
        }
    }

    async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        Fixture { _tmp: tmp, rt }
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

    async fn setup_project(fx: &Fixture) {
        let instance_id = fx.rt.store().meta().instance_id.clone();
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": instance_id,
                    "projectId": "proj-1",
                    "name": "Notifications",
                    "environmentId": "local",
                    "root": {"instanceId": instance_id, "rootId": "root-1", "relativePath": "w"}
                }),
                None,
                None,
            )
            .await
            .unwrap();
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> ContractError {
        match rt.dispatch(method, input, None, None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    fn set_input(preferences: Value, expected_revision: &str) -> Value {
        json!({
            "scope": {"kind": "global"},
            "preferences": preferences,
            "expectedRevision": expected_revision,
        })
    }

    #[tokio::test]
    async fn preferences_round_trip_with_revision_gate_and_restart() {
        let fx = fixture("notif-roundtrip").await;
        setup_project(&fx).await;

        // A fresh scope reads the absent baseline and an empty list.
        let (_, initial) = fx
            .rt
            .dispatch(
                "notifications.get_preferences",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(initial["revision"], "nrev-00000000");
        assert_eq!(initial["preferences"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            initial["deliveryChannels"].as_array().map(Vec::len),
            Some(0)
        );

        // Create (absent) then read back; scopes stay separate.
        let preferences = json!([
            {"eventType": "run.completed", "deliveryChannelId": "ch-telegram", "enabled": true},
            {"eventType": "run.failed", "deliveryChannelId": "ch-telegram", "enabled": false},
        ]);
        let (_, written) = fx
            .rt
            .dispatch(
                "notifications.set_preferences",
                set_input(preferences.clone(), "absent"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(written["applied"], true);
        assert_eq!(written["generation"], Value::Null);
        let revision = written["revision"].as_str().unwrap().to_owned();
        assert!(revision.starts_with("nrev-"));

        let (_, read) = fx
            .rt
            .dispatch(
                "notifications.get_preferences",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(read["revision"], revision);
        assert_eq!(read["preferences"], preferences);
        let (_, project) = fx
            .rt
            .dispatch(
                "notifications.get_preferences",
                json!({"scope": {"kind": "project", "id": "proj-1"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(project["revision"], "nrev-00000000");

        // The revision gate: absent-on-existing and stale both reject.
        let error = dispatch_err(
            &fx.rt,
            "notifications.set_preferences",
            set_input(json!([]), "absent"),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");
        let error = dispatch_err(
            &fx.rt,
            "notifications.set_preferences",
            set_input(json!([]), "nrev-99999999"),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        // A stored-invalid scope rejects before any read.
        let error = dispatch_err(
            &fx.rt,
            "notifications.get_preferences",
            json!({"scope": {"kind": "session", "id": "sess-none"}}),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // Reopen the store (a gateway restart): the preference survives.
        let home = fx.rt.store().home().to_path_buf();
        let rt2 = open_runtime(&home);
        let (_, persisted) = rt2
            .dispatch(
                "notifications.get_preferences",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(persisted["revision"], revision);
        assert_eq!(persisted["preferences"], preferences);

        // The schema stays honest about malformed input.
        let error = dispatch_err(
            &fx.rt,
            "notifications.set_preferences",
            json!({
                "scope": {"kind": "global"},
                "preferences": "not-an-array",
                "expectedRevision": revision,
            }),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn delivery_channels_track_the_channel_states() {
        let fx = fixture("notif-delivery").await;
        setup_project(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "channels.configure",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-email",
                    "adapterType": "email",
                    "schemaId": crate::public_v1::domains::channels::channel_schema_id("email"),
                    "configuration": {"enabled": true},
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let channel_revision = created["revision"].as_str().unwrap().to_owned();

        // Enabled but never assembled: offered, not available.
        let (_, read) = fx
            .rt
            .dispatch(
                "notifications.get_preferences",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let delivery = read["deliveryChannels"].as_array().unwrap().clone();
        assert_eq!(delivery.len(), 1);
        assert_eq!(delivery[0]["channelId"], "ch-email");
        assert_eq!(delivery[0]["available"], false);
        assert_eq!(delivery[0]["reasonCode"], "NOT_READY");

        // Assemble the channel (transition enable): available.
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.transition",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-email",
                    "action": "enable",
                    "expectedGeneration": "absent",
                }),
                Some("key-notif-enable"),
                None,
            )
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, read) = fx
                .rt
                .dispatch(
                    "notifications.get_preferences",
                    json!({"scope": {"kind": "global"}}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if read["deliveryChannels"][0]["available"] == json!(true) {
                assert_eq!(read["deliveryChannels"][0]["reasonCode"], Value::Null);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "channel never became ready: {}",
                read
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let _ = receipt;

        // Disabling the configuration withdraws the delivery option.
        fx.rt
            .dispatch(
                "channels.configure",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-email",
                    "adapterType": "email",
                    "schemaId": crate::public_v1::domains::channels::channel_schema_id("email"),
                    "configuration": {"enabled": false},
                    "expectedRevision": channel_revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let (_, read) = fx
            .rt
            .dispatch(
                "notifications.get_preferences",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(read["deliveryChannels"][0]["available"], false);
        assert_eq!(
            read["deliveryChannels"][0]["reasonCode"],
            "CHANNEL_DISABLED"
        );
    }

    #[tokio::test]
    async fn concurrent_set_preferences_admit_exactly_one_write_per_revision() {
        let fx = fixture("notif-cas-race").await;
        setup_project(&fx).await;
        let home = fx.rt.store().home().to_path_buf();

        // Four writers race on the same `expectedRevision: "absent"` from
        // four separate connections (each request opens its own). The
        // read-gate-write runs as ONE transaction, so exactly one writer
        // wins; every loser sees REVISION_CONFLICT (its read caught the
        // winner's committed row) or a transient UNAVAILABLE (SQLite
        // BUSY_SNAPSHOT when its read snapshot predates the winner's
        // commit). No silent overwrite is possible.
        let mut handles = Vec::new();
        for tag in ["ch-a", "ch-b", "ch-c", "ch-d"] {
            let home = home.clone();
            handles.push(std::thread::spawn(move || {
                let db = NotificationsDb::open(&home).unwrap();
                db.write_preferences_cas(
                    "global",
                    "",
                    &[json!({
                        "eventType": "run.completed",
                        "deliveryChannelId": tag,
                        "enabled": true,
                    })],
                    "absent",
                )
            }));
        }
        let mut winners = Vec::new();
        for handle in handles {
            match handle.join().unwrap() {
                Ok(revision) => winners.push(revision),
                Err(error) => assert!(
                    error.code == "REVISION_CONFLICT" || error.code == "UNAVAILABLE",
                    "loser got `{}`",
                    error.code
                ),
            }
        }
        assert_eq!(winners.len(), 1, "exactly one writer may win the CAS");
        assert_eq!(
            winners[0], "nrev-00000001",
            "losing transactions must not burn a revision"
        );

        // The stored row is the winner's list, at the winner's revision.
        let (_, read) = fx
            .rt
            .dispatch(
                "notifications.get_preferences",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(read["revision"], winners[0]);
        let stored = read["preferences"].as_array().unwrap().clone();
        assert_eq!(stored.len(), 1);
        let winner_tag = stored[0]["deliveryChannelId"].as_str().unwrap();
        assert!(["ch-a", "ch-b", "ch-c", "ch-d"].contains(&winner_tag));
    }

    #[tokio::test]
    async fn db_singleton_is_reused_and_preferences_notify_and_validate() {
        let fx = fixture("notif-singleton").await;
        let db1 = db_for(&fx.rt).unwrap();
        let db2 = db_for(&fx.rt).unwrap();
        assert!(
            Arc::ptr_eq(&db1, &db2),
            "per-dataRoot singleton must reuse connection"
        );

        // Malformed item schema rejects.
        let bad = dispatch_err(
            &fx.rt,
            "notifications.set_preferences",
            json!({
                "scope": {"kind": "global"},
                "preferences": [{"eventType": "run.completed", "deliveryChannelId": "ch-a", "enabled": true, "extra": 1}],
                "expectedRevision": "absent",
            }),
        )
        .await;
        assert_eq!(bad.code, "INVALID_ARGUMENT");

        // Register waiter BEFORE set_preferences to confirm notify_waiters fires.
        let notifier = fx.rt.store().event_notifier();
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        fx.rt
            .dispatch(
                "notifications.set_preferences",
                json!({
                    "scope": {"kind": "global"},
                    "preferences": [{"eventType": "run.completed", "deliveryChannelId": "ch-a", "enabled": false}],
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .expect("set_preferences must notify waiters");
    }
}
