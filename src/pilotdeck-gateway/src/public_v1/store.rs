//! SQLite WAL authoritative storage, startup crash recovery (`D9`), and `runs/registry.json` reconciliation for `public_v1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentcpm_pd_protocol::contract::{OPERATION_VERSION, ResponseOutcome, RunState};
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_CURSOR_EXPIRED, CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT,
    CODE_NOT_FOUND, CODE_QUEUE_PAUSED, CODE_REVISION_CONFLICT, ContractError, EffectStatus,
    RetrySpec,
};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceMeta {
    pub instance_id: String,
    pub owner_id: String,
    pub product: String,
    pub protocol_version: String,
    pub node_id: String,
    pub generation: String,
    pub server_revision: String,
    pub initialization_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthDiagnostic {
    pub code: String,
    pub severity: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StoredEvent {
    pub seq: i64,
    pub cursor: String,
    pub resource_id: String,
    pub session_id: String,
    pub event_type: String,
    pub schema_id: String,
    pub payload: Value,
    pub data: Map<String, Value>,
    pub occurred_at: String,
}

#[derive(Clone, Debug)]
pub struct StoredRun {
    pub run_id: String,
    pub session_id: String,
    pub project_id: String,
    pub state: RunState,
    pub revision: String,
    pub cancel_requested: bool,
    pub waiting_for: Vec<String>,
    pub effect_status: String,
    pub config_revision: String,
    pub effective_generation: String,
    pub final_cursor: Option<String>,
    pub continued_from: Option<String>,
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub input: Value,
    pub instance_id: String,
    pub terminal: bool,
    pub queue_paused: bool,
}

impl StoredRun {
    pub fn to_contract_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("runId".into(), json!(self.run_id));
        obj.insert("sessionId".into(), json!(self.session_id));
        obj.insert("projectId".into(), json!(self.project_id));
        obj.insert("state".into(), serde_json::to_value(self.state).unwrap());
        obj.insert("revision".into(), json!(self.revision));
        obj.insert("cancelRequested".into(), json!(self.cancel_requested));
        obj.insert("waitingFor".into(), json!(self.waiting_for));
        obj.insert("effectStatus".into(), json!(self.effect_status));
        obj.insert(
            "config".into(),
            json!({
                "configRevision": self.config_revision,
                "effectiveGeneration": self.effective_generation,
            }),
        );
        obj.insert("finalCursor".into(), json!(self.final_cursor));
        obj.insert("continuedFrom".into(), json!(self.continued_from));
        if let Some(code) = &self.outcome_code {
            obj.insert("outcomeCode".into(), json!(code));
        }
        obj.insert("result".into(), json!(self.result));
        obj.insert("instanceId".into(), json!(self.instance_id));
        obj.insert("terminal".into(), json!(self.terminal));
        obj.insert("queuePaused".into(), json!(self.queue_paused));
        Value::Object(obj)
    }
}

#[derive(Clone, Debug)]
pub struct StoredInteraction {
    pub interaction_id: String,
    pub kind: String,
    pub run_id: String,
    pub project_id: String,
    pub session_id: String,
    pub revision: String,
    pub state: String,
    pub resolution: String,
    pub expires_at: Option<String>,
    pub response_schema_id: Option<String>,
    pub response_schema: Value,
    pub prompt: Vec<Value>,
    pub choices: Vec<String>,
    pub lease_owner: Option<String>,
    pub answer: Option<Value>,
    pub policy_revision: Option<String>,
    pub instance_id: String,
}

impl StoredInteraction {
    pub fn to_contract_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("interactionId".into(), json!(self.interaction_id));
        obj.insert("kind".into(), json!(self.kind));
        obj.insert("runId".into(), json!(self.run_id));
        obj.insert("projectId".into(), json!(self.project_id));
        obj.insert("sessionId".into(), json!(self.session_id));
        obj.insert("revision".into(), json!(self.revision));
        obj.insert("state".into(), json!(self.state));
        obj.insert("resolution".into(), json!(self.resolution));
        obj.insert("expiresAt".into(), json!(self.expires_at));
        if let Some(id) = &self.response_schema_id {
            obj.insert("responseSchemaId".into(), json!(id));
        }
        if !self.prompt.is_empty() {
            obj.insert("prompt".into(), json!(self.prompt));
        }
        obj.insert("leaseOwner".into(), json!(self.lease_owner));
        obj.insert("instanceId".into(), json!(self.instance_id));
        obj.insert("choices".into(), json!(self.choices));
        Value::Object(obj)
    }
}

#[derive(Clone, Debug)]
pub enum SubmitDisposition {
    NewRun {
        receipt: Value,
        run_id: String,
        session_id: String,
        start_immediately: bool,
    },
    Deduplicated {
        receipt: Value,
    },
}

#[derive(Clone, Debug)]
pub struct CancelRunOutcome {
    pub receipt: Value,
    pub should_notify_worker: bool,
    pub transitioned_immediately: bool,
}

#[derive(Clone, Debug)]
pub struct InteractionResolveOutcome {
    pub interaction: StoredInteraction,
    pub policy_revision: Option<String>,
    pub decision_or_answer: Value,
    pub should_wake_run: bool,
}

#[derive(Clone, Debug)]
pub enum EventsQueryResult {
    Page(Value),
    Gap(Value),
}

pub struct V1Store {
    conn: Mutex<Connection>,
    home: PathBuf,
    meta: InstanceMeta,
    id_gen: IdGenerator,
    event_notify: Arc<Notify>,
    state_notify: Arc<Notify>,
    diagnostics: Mutex<Vec<HealthDiagnostic>>,
}

#[derive(Clone, Debug)]
pub struct RunTransition<'a> {
    pub new_state: RunState,
    pub outcome_code: Option<&'a str>,
    pub effect_status: Option<&'a str>,
    pub result_artifact: Option<&'a Value>,
    pub event_type: &'a str,
    pub event_payload: &'a Value,
    pub pause_session_reason: Option<&'a str>,
}

impl<'a> RunTransition<'a> {
    pub fn new(new_state: RunState, event_type: &'a str, event_payload: &'a Value) -> Self {
        Self {
            new_state,
            outcome_code: None,
            effect_status: None,
            result_artifact: None,
            event_type,
            event_payload,
            pause_session_reason: None,
        }
    }

    pub fn outcome_code(mut self, code: &'a str) -> Self {
        self.outcome_code = Some(code);
        self
    }

    pub fn result_artifact(mut self, artifact: &'a Value) -> Self {
        self.result_artifact = Some(artifact);
        self
    }

    pub fn pause_session_reason(mut self, reason: &'a str) -> Self {
        self.pause_session_reason = Some(reason);
        self
    }
}

impl V1Store {
    /// Opens `<home>/v1/state.sqlite` in WAL mode, runs schema migrations, applies crash
    /// recovery for interrupted active runs, and reconciles optional `runs/registry.json`.
    pub fn open_and_recover(home: &Path, meta: InstanceMeta) -> Result<Arc<Self>, ContractError> {
        let v1_dir = home.join("v1");
        fs::create_dir_all(&v1_dir).map_err(|e| storage_err(format!("mkdir v1: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&v1_dir, fs::Permissions::from_mode(0o700));
        }

        let db_path = v1_dir.join("state.sqlite");
        let conn = Connection::open(&db_path)
            .map_err(|e| storage_err(format!("open {}: {e}", db_path.display())))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            PRAGMA busy_timeout = 5000;
            PRAGMA foreign_keys = ON;
            ",
        )
        .map_err(|e| storage_err(format!("sqlite pragma: {e}")))?;

        init_schema(&conn)?;

        let node = NodeId::new(meta.node_id.clone()).unwrap_or_else(|_| NodeId::local());
        let store = Arc::new(Self {
            conn: Mutex::new(conn),
            home: home.to_path_buf(),
            meta: meta.clone(),
            id_gen: IdGenerator::new(node),
            event_notify: Arc::new(Notify::new()),
            state_notify: Arc::new(Notify::new()),
            diagnostics: Mutex::new(Vec::new()),
        });

        store.sync_meta_and_recover()?;
        store.reconcile_legacy_runs_registry()?;
        Ok(store)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn meta(&self) -> &InstanceMeta {
        &self.meta
    }

    pub fn next_id(&self) -> String {
        self.id_gen.next_id()
    }

    pub fn event_notifier(&self) -> Arc<Notify> {
        self.event_notify.clone()
    }

    pub fn state_notifier(&self) -> Arc<Notify> {
        self.state_notify.clone()
    }

    pub fn diagnostics(&self) -> Vec<HealthDiagnostic> {
        self.diagnostics.lock().expect("diagnostics mutex").clone()
    }

    fn sync_meta_and_recover(&self) -> Result<(), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;

        let existing_iid: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key = 'instance_id'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        if let Some(existing) = existing_iid
            && existing != self.meta.instance_id
        {
            return Err(ContractError::instance_mismatch(
                &existing,
                &self.meta.instance_id,
            ));
        }

        let pairs = [
            ("instance_id", self.meta.instance_id.as_str()),
            ("owner_id", self.meta.owner_id.as_str()),
            ("node_id", self.meta.node_id.as_str()),
            ("generation", self.meta.generation.as_str()),
            ("server_revision", self.meta.server_revision.as_str()),
            (
                "initialization_revision",
                self.meta.initialization_revision.as_str(),
            ),
        ];
        for (k, v) in pairs {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )
            .map_err(map_sql_err)?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('earliest_cursor_seq', '1')",
            [],
        )
        .map_err(map_sql_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('rev_counter', '1')",
            [],
        )
        .map_err(map_sql_err)?;

        // Crash recovery (D9 / A8): running or cancelling -> interrupted (or result_unknown);
        // waiting -> interrupted; queued stays queued; pause affected session queues with `gateway_restart`.
        let mut active_stmt = tx
            .prepare(
                "SELECT run_id, session_id, effect_status FROM runs WHERE state IN ('running', 'cancelling', 'waiting')",
            )
            .map_err(map_sql_err)?;
        let active_rows: Vec<(String, String, String)> = active_stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(map_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_err)?;
        drop(active_stmt);

        let now = now_rfc3339();
        let mut touched_sessions = std::collections::BTreeSet::new();
        for (run_id, session_id, effect_status) in active_rows {
            touched_sessions.insert(session_id.clone());
            let new_state = if effect_status == "unknown" {
                "result_unknown"
            } else {
                "interrupted"
            };
            let rev = next_rev_in_tx(&tx)?;
            let ev_type = if new_state == "result_unknown" {
                "run.result_unknown"
            } else {
                "run.interrupted"
            };
            let cursor = append_event_in_tx(
                &tx,
                &self.meta.instance_id,
                &run_id,
                &session_id,
                ev_type,
                "agentcpm-pd.run.event/1",
                &json!({"runId": run_id, "state": new_state, "reason": "gateway_restart"}),
                &now,
            )?;
            tx.execute(
                "UPDATE runs SET state = ?1, terminal = 1, revision = ?2, outcome_code = 'GATEWAY_RESTART', final_cursor = ?3, queue_paused = 1, waiting_for_json = '[]', updated_at = ?4 WHERE run_id = ?5",
                params![new_state, rev, cursor, now, run_id],
            )
            .map_err(map_sql_err)?;
            tx.execute("DELETE FROM run_queue WHERE run_id = ?1", params![run_id])
                .map_err(map_sql_err)?;
        }

        // Also pause any session that still has queued runs waiting on startup
        let mut q_stmt = tx
            .prepare("SELECT DISTINCT session_id FROM run_queue")
            .map_err(map_sql_err)?;
        let q_sessions: Vec<String> = q_stmt
            .query_map([], |row| row.get(0))
            .map_err(map_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_err)?;
        drop(q_stmt);
        for sid in q_sessions {
            touched_sessions.insert(sid);
        }

        for sid in touched_sessions {
            let qrev = next_rev_in_tx(&tx)?;
            tx.execute(
                "UPDATE sessions SET active_run_id = NULL, queue_paused = 1, pause_reason = 'gateway_restart', queue_revision = ?1 WHERE session_id = ?2",
                params![qrev, sid],
            )
            .map_err(map_sql_err)?;
        }

        // Pending interactions from crashed runs expire (`007 §7.4` line 348)
        let pending_ia_rev = next_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE interactions SET state = 'expired', resolution = 'expired', revision = ?1 WHERE state = 'pending'",
            params![pending_ia_rev],
        )
        .map_err(map_sql_err)?;

        tx.commit().map_err(map_sql_err)?;
        Ok(())
    }

    fn reconcile_legacy_runs_registry(&self) -> Result<(), ContractError> {
        let legacy_path = self.home.join("runs").join("registry.json");
        if !legacy_path.exists() {
            return Ok(());
        }
        let raw = match fs::read_to_string(&legacy_path) {
            Ok(s) => s,
            Err(e) => {
                self.diagnostics
                    .lock()
                    .expect("diag")
                    .push(HealthDiagnostic {
                        code: "LEGACY_REGISTRY_UNREADABLE".to_owned(),
                        severity: "warning".to_owned(),
                        message: format!("failed to read {}: {e}", legacy_path.display()),
                        resource_id: None,
                    });
                return Ok(());
            }
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            self.diagnostics
                .lock()
                .expect("diag")
                .push(HealthDiagnostic {
                    code: "LEGACY_REGISTRY_MALFORMED".to_owned(),
                    severity: "warning".to_owned(),
                    message: format!(
                        "{} is invalid JSON; SQLite remains authoritative",
                        legacy_path.display()
                    ),
                    resource_id: None,
                });
            return Ok(());
        };

        let conn = self.conn.lock().expect("sqlite mutex");
        let runs_map = parsed.get("runs").and_then(Value::as_object);
        let mut diags = self.diagnostics.lock().expect("diag");
        if let Some(map) = runs_map {
            for (run_id, entry) in map {
                let sql_state: Option<String> = conn
                    .query_row(
                        "SELECT state FROM runs WHERE run_id = ?1",
                        params![run_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(map_sql_err)?;
                let legacy_state = entry.get("state").and_then(Value::as_str);
                if sql_state.as_deref() != legacy_state {
                    diags.push(HealthDiagnostic {
                        code: "REGISTRY_RECONCILE_MISMATCH".to_owned(),
                        severity: "warning".to_owned(),
                        message: format!(
                            "runs/registry.json disagrees with SQLite for run `{run_id}` (sqlite={sql_state:?}, legacy={legacy_state:?}); SQLite kept as authority"
                        ),
                        resource_id: Some(run_id.clone()),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn set_earliest_cursor_seq_for_test(&self, earliest_seq: i64) -> Result<(), ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('earliest_cursor_seq', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![earliest_seq.to_string()],
        )
        .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn register_project(&self, input: &Value) -> Result<Value, ContractError> {
        let env_id = input
            .get("environmentId")
            .and_then(Value::as_str)
            .unwrap_or("local");
        if env_id != "local" {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("unsupported environmentId `{env_id}`; only `local` is supported in S1+S2"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let project_id = input["projectId"].as_str().unwrap_or_default();
        let name = input["name"].as_str().unwrap_or_default();
        let root = &input["root"];
        let root_inst = root.get("instanceId").and_then(Value::as_str).unwrap_or("");
        if root_inst != self.meta.instance_id {
            return Err(ContractError::instance_mismatch(
                &self.meta.instance_id,
                root_inst,
            ));
        }
        let expected_absent = input.get("expectedAbsent").and_then(Value::as_bool);

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT project_id FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;

        let _ = expected_absent;
        if exists.is_some() {
            return Err(ContractError::new(
                CODE_ALREADY_EXISTS,
                format!("project `{project_id}` is already registered"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let rev = next_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let root_json = serde_json::to_string(root).unwrap_or_else(|_| "{}".into());
        tx.execute(
            "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
             VALUES (?1, ?2, ?3, 'registered', 'local', ?4, ?5)",
            params![project_id, name, root_json, rev, now],
        )
        .map_err(map_sql_err)?;
        tx.commit().map_err(map_sql_err)?;

        Ok(json!({
            "project": {
                "projectId": project_id,
                "name": name,
                "root": root,
                "revision": rev,
                "registrationState": "registered",
                "environmentId": "local",
            }
        }))
    }

    pub fn get_project(&self, project_id: &str) -> Result<Value, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let row: Option<(String, String, String, String, String, String)> = conn
            .query_row(
                "SELECT project_id, name, root_json, registration_state, environment_id, revision
                 FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sql_err)?;
        let Some((pid, name, root_json, reg_state, env_id, rev)) = row else {
            return Err(not_found("project", project_id));
        };
        let root: Value = serde_json::from_str(&root_json).unwrap_or(Value::Null);
        Ok(json!({
            "project": {
                "projectId": pid,
                "name": name,
                "root": root,
                "revision": rev,
                "registrationState": reg_state,
                "environmentId": env_id,
            }
        }))
    }

    pub fn list_projects(&self, page: Option<&Value>) -> Result<Value, ContractError> {
        let (offset, limit) = parse_page(page)?;
        let conn = self.conn.lock().expect("sqlite mutex");
        let snap_rev = current_rev(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT project_id, name, root_json, registration_state, environment_id, revision
                 FROM projects ORDER BY created_at ASC, project_id ASC",
            )
            .map_err(map_sql_err)?;
        let all: Vec<Value> = stmt
            .query_map([], |r| {
                let root_json: String = r.get(2)?;
                let root: Value = serde_json::from_str(&root_json).unwrap_or(Value::Null);
                Ok(json!({
                    "projectId": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "root": root,
                    "revision": r.get::<_, String>(5)?,
                    "registrationState": r.get::<_, String>(3)?,
                    "environmentId": r.get::<_, String>(4)?,
                }))
            })
            .map_err(map_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_err)?;
        let (items, page_info) = paginate_vec(all, offset, limit, &snap_rev)?;
        Ok(json!({"items": items, "pageInfo": page_info}))
    }

    pub fn create_session(&self, input: &Value) -> Result<Value, ContractError> {
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let project_id = input["projectId"].as_str().unwrap_or_default();
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(session_id);
        let tags: Vec<String> = input
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;
        let proj_exists: Option<String> = tx
            .query_row(
                "SELECT project_id FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        if proj_exists.is_none() {
            return Err(not_found("project", project_id));
        }

        let exists: Option<String> = tx
            .query_row(
                "SELECT session_id FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        if exists.is_some() {
            return Err(ContractError::new(
                CODE_ALREADY_EXISTS,
                format!("session `{session_id}` already exists"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let rev = next_rev_in_tx(&tx)?;
        let qrev = next_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT INTO sessions (session_id, project_id, name, tags_json, revision, active_run_id, session_state, queue_paused, pause_reason, queue_revision, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'open', 0, NULL, ?6, ?7)",
            params![session_id, project_id, name, tags_json, rev, qrev, now],
        )
        .map_err(map_sql_err)?;
        tx.commit().map_err(map_sql_err)?;

        Ok(json!({
            "session": {
                "sessionId": session_id,
                "projectId": project_id,
                "name": name,
                "tags": tags,
                "revision": rev,
                "activeRunId": null,
                "sessionState": "open",
            }
        }))
    }

    pub fn get_session(&self, session_id: &str) -> Result<Value, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let session = fetch_session_json(&conn, session_id)?
            .ok_or_else(|| not_found("session", session_id))?;
        let snap_cursor: Option<String> = conn
            .query_row(
                "SELECT cursor FROM events WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        Ok(json!({
            "session": session,
            "snapshotCursor": snap_cursor,
        }))
    }

    pub fn list_sessions(
        &self,
        project_id: &str,
        state_filter: Option<&str>,
        page: Option<&Value>,
    ) -> Result<Value, ContractError> {
        let (offset, limit) = parse_page(page)?;
        let conn = self.conn.lock().expect("sqlite mutex");
        let snap_rev = current_rev(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, project_id, name, tags_json, revision, active_run_id, session_state
                 FROM sessions WHERE project_id = ?1 ORDER BY created_at ASC, session_id ASC",
            )
            .map_err(map_sql_err)?;
        let all: Vec<Value> = stmt
            .query_map(params![project_id], |r| {
                let s_state: String = r.get(6)?;
                Ok((
                    s_state,
                    json!({
                        "sessionId": r.get::<_, String>(0)?,
                        "projectId": r.get::<_, String>(1)?,
                        "name": r.get::<_, String>(2)?,
                        "tags": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap_or(json!([])),
                        "revision": r.get::<_, String>(4)?,
                        "activeRunId": r.get::<_, Option<String>>(5)?,
                        "sessionState": r.get::<_, String>(6)?,
                    }),
                ))
            })
            .map_err(map_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_err)?
            .into_iter()
            .filter(|(s_state, _)| match state_filter {
                Some("all") | None => true,
                Some(want) => s_state == want,
            })
            .map(|(_, val)| val)
            .collect();
        let (items, page_info) = paginate_vec(all, offset, limit, &snap_rev)?;
        Ok(json!({"items": items, "pageInfo": page_info}))
    }

    pub fn get_queue(
        &self,
        session_id: &str,
        page: Option<&Value>,
    ) -> Result<Value, ContractError> {
        let (offset, limit) = parse_page(page)?;
        let conn = self.conn.lock().expect("sqlite mutex");
        build_queue_json(&conn, session_id, offset, limit)
    }

    pub fn resume_queue(
        &self,
        session_id: &str,
        expected_revision: &str,
        acknowledged_reason: &str,
    ) -> Result<(Value, Option<String>), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;
        let row: Option<(bool, Option<String>, String, Option<String>)> = tx
            .query_row(
                "SELECT queue_paused, pause_reason, queue_revision, active_run_id FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(map_sql_err)?;
        let Some((paused, pause_reason, qrev, active_run_id)) = row else {
            return Err(not_found("session", session_id));
        };
        if qrev != expected_revision {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!("queue expectedRevision `{expected_revision}` does not match `{qrev}`"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        if !paused {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("session `{session_id}` queue is not paused"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let actual_reason = pause_reason.unwrap_or_default();
        if actual_reason != acknowledged_reason {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!(
                    "acknowledgedPauseReason `{acknowledged_reason}` does not match `{actual_reason}`"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        let new_qrev = next_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET queue_paused = 0, pause_reason = NULL, queue_revision = ?1 WHERE session_id = ?2",
            params![new_qrev, session_id],
        )
        .map_err(map_sql_err)?;
        tx.execute(
            "UPDATE runs SET queue_paused = 0 WHERE session_id = ?1 AND terminal = 0",
            params![session_id],
        )
        .map_err(map_sql_err)?;

        let next_run_id: Option<String> = if active_run_id.is_none() {
            tx.query_row(
                "SELECT run_id FROM run_queue WHERE session_id = ?1 ORDER BY position ASC LIMIT 1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sql_err)?
        } else {
            None
        };

        // P1 (wp4 review): the resumed head run becomes the session's active
        // run inside the same transaction, so `run.submit` during its
        // execution observes a non-empty active slot and stays queued —
        // without this the resumed run ran with `active_run_id` NULL and a
        // concurrent submit could start a second run in the same session.
        if let Some(run_id) = next_run_id.as_deref() {
            tx.execute(
                "UPDATE sessions SET active_run_id = ?1 WHERE session_id = ?2",
                params![run_id, session_id],
            )
            .map_err(map_sql_err)?;
        }

        tx.commit().map_err(map_sql_err)?;
        let queue_json = build_queue_json(&conn, session_id, 0, 100)?;
        drop(conn);
        self.state_notify.notify_waiters();
        Ok((queue_json, next_run_id))
    }

    pub fn submit_run(
        &self,
        input: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<SubmitDisposition, ContractError> {
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let digest = compute_input_digest("run.submit", input);
        let now = now_rfc3339();
        let retained_until = retention_expiry_rfc3339();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;

        if let Some(key) = idempotency_key {
            let existing: Option<(String, String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, request_state, receipt_json, rejection_code
                     FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = 'run.submit'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(map_sql_err)?;

            if let Some((stored_digest, req_state, receipt_json, rej_code)) = existing {
                if stored_digest != digest {
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for run.submit with different input"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if req_state == "accepted"
                    && let Some(raw_receipt) = receipt_json
                    && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw_receipt)
                {
                    if let Some(obj) = receipt.as_object_mut() {
                        obj.insert("deduplicated".into(), json!(true));
                    }
                    return Ok(SubmitDisposition::Deduplicated { receipt });
                }
                return Err(ContractError::new(
                    rej_code.unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
                    "request previously rejected definitively",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }

        let sess_row: Option<(String, bool, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT project_id, queue_paused, pause_reason, active_run_id FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(map_sql_err)?;
        let Some((project_id, paused, pause_reason, active_run_id)) = sess_row else {
            if let Some(key) = idempotency_key {
                record_rejection_in_tx(
                    &tx,
                    key,
                    "run.submit",
                    &digest,
                    CODE_NOT_FOUND,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(map_sql_err)?;
            }
            return Err(not_found("session", session_id));
        };

        if paused {
            if let Some(key) = idempotency_key {
                record_rejection_in_tx(
                    &tx,
                    key,
                    "run.submit",
                    &digest,
                    CODE_QUEUE_PAUSED,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(map_sql_err)?;
            }
            return Err(ContractError::new(
                CODE_QUEUE_PAUSED,
                format!(
                    "session `{session_id}` queue is paused ({}); call queue.resume first",
                    pause_reason.as_deref().unwrap_or("paused")
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        let run_id = self.id_gen.next_id();
        let rev = next_rev_in_tx(&tx)?;
        // P1 (wp4 review): a new run may only start when the session has no
        // active run AND no queued run — `active_run_id` alone was NULL while
        // a resumed run executed (queue.resume used to leave it unset),
        // letting the same session start two concurrent runs.
        let queued_exists: i64 = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(map_sql_err)?;
        let start_immediately = active_run_id.is_none() && queued_exists == 0;
        let input_json = serde_json::to_string(input).unwrap_or_else(|_| "{}".into());
        let config_json = json!({
            "configRevision": self.meta.initialization_revision,
            "effectiveGeneration": self.meta.generation,
        });
        let config_str = serde_json::to_string(&config_json).unwrap();

        let cursor = append_event_in_tx(
            &tx,
            &self.meta.instance_id,
            &run_id,
            session_id,
            "run.queued",
            "agentcpm-pd.run.event/1",
            &json!({"runId": run_id, "sessionId": session_id, "state": "queued"}),
            &now,
        )?;

        tx.execute(
            "INSERT INTO runs (run_id, session_id, project_id, state, revision, cancel_requested, waiting_for_json, effect_status, config_json, final_cursor, continued_from, outcome_code, result_json, input_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'queued', ?4, 0, '[]', 'known', ?5, ?6, NULL, NULL, NULL, ?7, ?8, ?9)",
            params![run_id, session_id, project_id, rev, config_str, cursor, input_json, now, now],
        )
        .map_err(map_sql_err)?;

        let max_pos: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(position), 0) FROM run_queue WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(map_sql_err)?;
        tx.execute(
            "INSERT INTO run_queue (session_id, position, run_id, disposition) VALUES (?1, ?2, ?3, 'normal')",
            params![session_id, max_pos + 1, run_id],
        )
        .map_err(map_sql_err)?;

        if start_immediately {
            tx.execute(
                "UPDATE sessions SET active_run_id = ?1 WHERE session_id = ?2",
                params![run_id, session_id],
            )
            .map_err(map_sql_err)?;
        }

        let receipt = json!({
            "kind": "run",
            "runId": run_id,
            "durable": true,
            "acceptedRevision": rev,
            "deduplicated": false,
            "state": "queued",
        });

        if let Some(key) = idempotency_key {
            let resource_val = json!({"kind": "run", "runId": run_id});
            tx.execute(
                "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'run.submit', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&resource_val).unwrap(),
                    serde_json::to_string(&receipt).unwrap(),
                    now,
                    retained_until,
                ],
            )
            .map_err(map_sql_err)?;
        }

        tx.commit().map_err(map_sql_err)?;
        self.event_notify.notify_waiters();
        self.state_notify.notify_waiters();

        Ok(SubmitDisposition::NewRun {
            receipt,
            run_id,
            session_id: session_id.to_owned(),
            start_immediately,
        })
    }

    pub fn transition_run_state(
        &self,
        run_id: &str,
        tr: RunTransition<'_>,
    ) -> Result<(StoredRun, Option<String>), ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;
        let existing = fetch_run_in_tx(&tx, run_id)?.ok_or_else(|| not_found("run", run_id))?;

        // P2 (wp4 review): terminal states are absorbing. Without this guard
        // a late writer (e.g. the offline driver waking after `run.cancel`
        // already terminalized a waiting run) appended a second terminal
        // event past the durable one, and a worker racing a queued-run cancel
        // could resurrect `terminal` back to 0.
        if existing.terminal {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "run `{run_id}` is already terminal ({})",
                    run_state_str(existing.state)
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let rev = next_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let terminal = is_terminal_run_state(tr.new_state);
        let eff = tr.effect_status.unwrap_or(&existing.effect_status);
        let res_json = tr
            .result_artifact
            .map(|v| serde_json::to_string(v).unwrap());
        let cursor = append_event_in_tx(
            &tx,
            &self.meta.instance_id,
            run_id,
            &existing.session_id,
            tr.event_type,
            "agentcpm-pd.run.event/1",
            tr.event_payload,
            &now,
        )?;
        let state_str = run_state_str(tr.new_state);
        let paused_flag = tr.pause_session_reason.is_some();

        tx.execute(
            "UPDATE runs SET state = ?1, revision = ?2, effect_status = ?3, final_cursor = ?4, outcome_code = ?5, result_json = COALESCE(?6, result_json), terminal = ?7, queue_paused = ?8, updated_at = ?9 WHERE run_id = ?10",
            params![
                state_str,
                rev,
                eff,
                cursor,
                tr.outcome_code,
                res_json,
                if terminal { 1 } else { 0 },
                if paused_flag { 1 } else { 0 },
                now,
                run_id,
            ],
        )
        .map_err(map_sql_err)?;

        let mut next_queued_run: Option<String> = None;
        if terminal {
            tx.execute("DELETE FROM run_queue WHERE run_id = ?1", params![run_id])
                .map_err(map_sql_err)?;

            if let Some(reason) = tr.pause_session_reason {
                let qrev = next_rev_in_tx(&tx)?;
                tx.execute(
                    "UPDATE sessions SET active_run_id = NULL, queue_paused = 1, pause_reason = ?1, queue_revision = ?2 WHERE session_id = ?3",
                    params![reason, qrev, existing.session_id],
                )
                .map_err(map_sql_err)?;
            } else {
                next_queued_run = tx
                    .query_row(
                        "SELECT run_id FROM run_queue WHERE session_id = ?1 ORDER BY position ASC LIMIT 1",
                        params![existing.session_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(map_sql_err)?;
                tx.execute(
                    "UPDATE sessions SET active_run_id = ?1 WHERE session_id = ?2",
                    params![next_queued_run, existing.session_id],
                )
                .map_err(map_sql_err)?;
            }
        }

        let updated = fetch_run_in_tx(&tx, run_id)?.expect("updated run present");
        tx.commit().map_err(map_sql_err)?;
        self.event_notify.notify_waiters();
        self.state_notify.notify_waiters();
        Ok((updated, next_queued_run))
    }

    pub fn append_run_progress_event(
        &self,
        run_id: &str,
        session_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<String, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;
        let now = now_rfc3339();
        let cursor = append_event_in_tx(
            &tx,
            &self.meta.instance_id,
            run_id,
            session_id,
            event_type,
            "agentcpm-pd.run.event/1",
            payload,
            &now,
        )?;
        tx.execute(
            "UPDATE runs SET final_cursor = ?1, updated_at = ?2 WHERE run_id = ?3",
            params![cursor, now, run_id],
        )
        .map_err(map_sql_err)?;
        tx.commit().map_err(map_sql_err)?;
        self.event_notify.notify_waiters();
        Ok(cursor)
    }

    pub fn get_run(&self, run_id: &str) -> Result<StoredRun, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        fetch_run_in_tx(&conn, run_id)?.ok_or_else(|| not_found("run", run_id))
    }

    pub fn list_runs(
        &self,
        scope: Option<&Value>,
        states: Option<&Vec<String>>,
        page: Option<&Value>,
    ) -> Result<Value, ContractError> {
        let (offset, limit) = parse_page(page)?;
        let conn = self.conn.lock().expect("sqlite mutex");
        let snap_rev = current_rev(&conn)?;
        let mut stmt = conn
            .prepare("SELECT run_id FROM runs ORDER BY created_at ASC, run_id ASC")
            .map_err(map_sql_err)?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(map_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_err)?;
        drop(stmt);

        let mut items = Vec::new();
        for rid in ids {
            if let Some(run) = fetch_run_in_tx(&conn, &rid)? {
                let scope_match = match scope.and_then(|s| s.get("kind")).and_then(Value::as_str) {
                    Some("project") => {
                        scope
                            .and_then(|s| s.get("projectId"))
                            .and_then(Value::as_str)
                            == Some(run.project_id.as_str())
                    }
                    Some("session") => {
                        scope
                            .and_then(|s| s.get("sessionId"))
                            .and_then(Value::as_str)
                            == Some(run.session_id.as_str())
                    }
                    _ => true,
                };
                let state_str = run_state_str(run.state).to_owned();
                let state_match = match states {
                    Some(st) if !st.is_empty() => st.contains(&state_str),
                    _ => true,
                };
                if scope_match && state_match {
                    items.push(run.to_contract_json());
                }
            }
        }
        let (sliced, page_info) = paginate_vec(items, offset, limit, &snap_rev)?;
        Ok(json!({"items": sliced, "pageInfo": page_info}))
    }

    pub fn cancel_run(
        &self,
        run_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<CancelRunOutcome, ContractError> {
        let lookup_input = json!({"instanceId": self.meta.instance_id, "runId": run_id});
        let digest = compute_input_digest("run.cancel", &lookup_input);
        let now = now_rfc3339();
        let retained_until = retention_expiry_rfc3339();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;

        if let Some(key) = idempotency_key {
            let existing: Option<(String, String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, request_state, receipt_json, rejection_code
                     FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = 'run.cancel'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(map_sql_err)?;
            if let Some((stored_digest, req_state, receipt_json, rej_code)) = existing {
                if stored_digest != digest {
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for run.cancel with different runId"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if req_state == "accepted"
                    && let Some(raw_receipt) = receipt_json
                    && let Ok(receipt) = serde_json::from_str::<Value>(&raw_receipt)
                {
                    return Ok(CancelRunOutcome {
                        receipt,
                        should_notify_worker: false,
                        transitioned_immediately: false,
                    });
                }
                return Err(ContractError::new(
                    rej_code.unwrap_or_else(|| CODE_NOT_FOUND.to_owned()),
                    "cancel request previously rejected",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }

        let Some(run) = fetch_run_in_tx(&tx, run_id)? else {
            if let Some(key) = idempotency_key {
                record_rejection_in_tx(
                    &tx,
                    key,
                    "run.cancel",
                    &digest,
                    CODE_NOT_FOUND,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(map_sql_err)?;
            }
            return Err(not_found("run", run_id));
        };

        let (disposition, final_state, new_rev, notify, immediate) = if run.terminal {
            (
                "alreadyTerminal",
                run.state,
                run.revision.clone(),
                false,
                false,
            )
        } else if run.state == RunState::Queued {
            let rev = next_rev_in_tx(&tx)?;
            let cursor = append_event_in_tx(
                &tx,
                &self.meta.instance_id,
                run_id,
                &run.session_id,
                "run.cancelled",
                "agentcpm-pd.run.event/1",
                &json!({"runId": run_id, "reason": "cancelled_while_queued"}),
                &now,
            )?;
            tx.execute(
                "UPDATE runs SET state = 'cancelled', cancel_requested = 1, revision = ?1, terminal = 1, outcome_code = 'CANCELLED', final_cursor = ?2, updated_at = ?3 WHERE run_id = ?4",
                params![rev, cursor, now, run_id],
            )
            .map_err(map_sql_err)?;
            tx.execute("DELETE FROM run_queue WHERE run_id = ?1", params![run_id])
                .map_err(map_sql_err)?;
            ("requested", RunState::Cancelled, rev, true, true)
        } else {
            let rev = next_rev_in_tx(&tx)?;
            let target_state = if run.state == RunState::Waiting {
                RunState::Cancelled
            } else {
                RunState::Cancelling
            };
            let is_term = target_state == RunState::Cancelled;
            let cursor = append_event_in_tx(
                &tx,
                &self.meta.instance_id,
                run_id,
                &run.session_id,
                if is_term {
                    "run.cancelled"
                } else {
                    "run.cancelling"
                },
                "agentcpm-pd.run.event/1",
                &json!({"runId": run_id}),
                &now,
            )?;
            tx.execute(
                "UPDATE runs SET state = ?1, cancel_requested = 1, revision = ?2, terminal = ?3, outcome_code = CASE WHEN ?3 = 1 THEN 'CANCELLED' ELSE outcome_code END, final_cursor = ?4, waiting_for_json = '[]', updated_at = ?5 WHERE run_id = ?6",
                params![
                    run_state_str(target_state),
                    rev,
                    if is_term { 1 } else { 0 },
                    cursor,
                    now,
                    run_id
                ],
            )
            .map_err(map_sql_err)?;
            if is_term {
                tx.execute("DELETE FROM run_queue WHERE run_id = ?1", params![run_id])
                    .map_err(map_sql_err)?;
                tx.execute(
                    "UPDATE interactions SET state = 'cancelled', resolution = 'cancelled', revision = ?1 WHERE run_id = ?2 AND state = 'pending'",
                    params![rev, run_id],
                )
                .map_err(map_sql_err)?;
                tx.execute(
                    "UPDATE sessions SET active_run_id = NULL WHERE session_id = ?1 AND active_run_id = ?2",
                    params![run.session_id, run_id],
                )
                .map_err(map_sql_err)?;
            }
            ("requested", target_state, rev, true, is_term)
        };

        let result = json!({
            "receipt": {
                "disposition": disposition,
                "revision": new_rev,
                "state": run_state_str(final_state),
            }
        });

        if let Some(key) = idempotency_key {
            let resource_val = json!({"kind": "run", "runId": run_id});
            tx.execute(
                "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'run.cancel', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&resource_val).unwrap(),
                    serde_json::to_string(&result).unwrap(),
                    now,
                    retained_until,
                ],
            )
            .map_err(map_sql_err)?;
        }

        tx.commit().map_err(map_sql_err)?;
        if notify {
            self.event_notify.notify_waiters();
            self.state_notify.notify_waiters();
        }
        Ok(CancelRunOutcome {
            receipt: result,
            should_notify_worker: notify,
            transitioned_immediately: immediate,
        })
    }

    pub fn lookup_request(&self, method_id: &str, key: &str) -> Result<Value, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let found: Option<Value> = conn
            .query_row(
                "SELECT request_state, reason, resource_json, rejection_code, input_digest, observed_at, retained_until, retry_disposition
                 FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
                params![key, method_id],
                |r| {
                    let res_json: Option<String> = r.get(2)?;
                    let resource = res_json
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                        .unwrap_or(Value::Null);
                    Ok(json!({
                        "requestState": r.get::<_, String>(0)?,
                        "reason": r.get::<_, String>(1)?,
                        "resource": resource,
                        "inputDigest": r.get::<_, String>(4)?,
                        "observedAt": r.get::<_, String>(5)?,
                        "retainedUntil": r.get::<_, Option<String>>(6)?,
                        "retryDisposition": r.get::<_, String>(7)?,
                        "rejectionCode": r.get::<_, Option<String>>(3)?,
                    }))
                },
            )
            .optional()
            .map_err(map_sql_err)?;

        if let Some(val) = found {
            return Ok(val);
        }

        let now = now_rfc3339();
        Ok(json!({
            "requestState": "result_unknown",
            "reason": "notObserved",
            "resource": Value::Null,
            "inputDigest": Value::Null,
            "observedAt": now,
            "retainedUntil": Value::Null,
            "retryDisposition": "replaySameKey",
            "rejectionCode": Value::Null,
        }))
    }

    pub fn create_interaction(
        &self,
        run_id: &str,
        kind: &str,
        prompt_text: &str,
        choices: Vec<String>,
        response_schema_id: Option<String>,
        response_schema: Value,
    ) -> Result<StoredInteraction, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;
        let run = fetch_run_in_tx(&tx, run_id)?.ok_or_else(|| not_found("run", run_id))?;

        let interaction_id = self.id_gen.next_id();
        let rev = next_rev_in_tx(&tx)?;
        let run_rev = next_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let prompt = vec![json!({"kind": "text", "text": prompt_text})];
        let prompt_json = serde_json::to_string(&prompt).unwrap();
        let choices_json = serde_json::to_string(&choices).unwrap();
        let schema_json = serde_json::to_string(&response_schema).unwrap();

        tx.execute(
            "INSERT INTO interactions (interaction_id, kind, run_id, project_id, session_id, revision, state, resolution, expires_at, prompt_json, choices_json, response_schema_id, response_schema_json, answer_json, lease_owner, policy_revision, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 'pending', NULL, ?7, ?8, ?9, ?10, NULL, NULL, NULL, ?11)",
            params![
                interaction_id,
                kind,
                run_id,
                run.project_id,
                run.session_id,
                rev,
                prompt_json,
                choices_json,
                response_schema_id,
                schema_json,
                now
            ],
        )
        .map_err(map_sql_err)?;

        let waiting_for = json!([interaction_id]);
        let cursor = append_event_in_tx(
            &tx,
            &self.meta.instance_id,
            run_id,
            &run.session_id,
            "interaction.requested",
            "agentcpm-pd.interaction.event/1",
            &json!({"interactionId": interaction_id, "kind": kind, "runId": run_id}),
            &now,
        )?;
        tx.execute(
            "UPDATE runs SET state = 'waiting', waiting_for_json = ?1, revision = ?2, final_cursor = ?3, updated_at = ?4 WHERE run_id = ?5",
            params![
                serde_json::to_string(&waiting_for).unwrap(),
                run_rev,
                cursor,
                now,
                run_id,
            ],
        )
        .map_err(map_sql_err)?;

        let stored = fetch_interaction_in_tx(&tx, &self.meta.instance_id, &interaction_id)?
            .expect("created interaction");
        tx.commit().map_err(map_sql_err)?;
        self.event_notify.notify_waiters();
        self.state_notify.notify_waiters();
        Ok(stored)
    }

    pub fn get_interaction(
        &self,
        interaction_id: &str,
    ) -> Result<StoredInteraction, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        fetch_interaction_in_tx(&conn, &self.meta.instance_id, interaction_id)?
            .ok_or_else(|| not_found("interaction", interaction_id))
    }

    pub fn list_interactions(
        &self,
        scope: Option<&Value>,
        kind_filter: Option<&str>,
        state_filter: Option<&str>,
        page: Option<&Value>,
    ) -> Result<Value, ContractError> {
        let (offset, limit) = parse_page(page)?;
        let conn = self.conn.lock().expect("sqlite mutex");
        let snap_rev = current_rev(&conn)?;
        let mut stmt = conn
            .prepare("SELECT interaction_id FROM interactions ORDER BY created_at ASC, interaction_id ASC")
            .map_err(map_sql_err)?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(map_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_err)?;
        drop(stmt);

        let mut items = Vec::new();
        for iid in ids {
            if let Some(ia) = fetch_interaction_in_tx(&conn, &self.meta.instance_id, &iid)? {
                let scope_match = match scope.and_then(|s| s.get("kind")).and_then(Value::as_str) {
                    Some("project") => {
                        scope
                            .and_then(|s| s.get("projectId"))
                            .and_then(Value::as_str)
                            == Some(ia.project_id.as_str())
                    }
                    Some("session") => {
                        scope
                            .and_then(|s| s.get("sessionId"))
                            .and_then(Value::as_str)
                            == Some(ia.session_id.as_str())
                    }
                    _ => true,
                };
                let kind_ok = match kind_filter {
                    Some("all") | None => true,
                    Some(k) => ia.kind == k,
                };
                let state_ok = match state_filter {
                    Some(s) => ia.state == s,
                    None => true,
                };
                if scope_match && kind_ok && state_ok {
                    items.push(ia.to_contract_json());
                }
            }
        }
        let (sliced, page_info) = paginate_vec(items, offset, limit, &snap_rev)?;
        Ok(json!({"items": sliced, "pageInfo": page_info}))
    }

    pub fn respond_interaction(
        &self,
        interaction_id: &str,
        expected_kind: Option<&str>,
        expected_revision: &str,
        answer_value: Value,
        remember_scope: Option<&str>,
    ) -> Result<InteractionResolveOutcome, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(map_sql_err)?;
        let existing = fetch_interaction_in_tx(&tx, &self.meta.instance_id, interaction_id)?
            .ok_or_else(|| not_found("interaction", interaction_id))?;

        if let Some(want) = expected_kind
            && existing.kind != want
        {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!(
                    "interaction `{interaction_id}` is `{}` not `{want}`",
                    existing.kind
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if existing.state != "pending" {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "interaction `{interaction_id}` is already `{}`",
                    existing.state
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        if existing.revision != expected_revision {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "interaction expectedRevision `{expected_revision}` does not match `{}`",
                    existing.revision
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        let new_rev = next_rev_in_tx(&tx)?;
        let policy_rev = remember_scope
            .filter(|s| *s != "none")
            .map(|_| next_rev_in_tx(&tx))
            .transpose()?;
        let now = now_rfc3339();
        let ans_json = serde_json::to_string(&answer_value).unwrap_or_else(|_| "null".into());

        tx.execute(
            "UPDATE interactions SET state = 'answered', resolution = 'answered', revision = ?1, answer_json = ?2, policy_revision = ?3 WHERE interaction_id = ?4",
            params![new_rev, ans_json, policy_rev, interaction_id],
        )
        .map_err(map_sql_err)?;

        let run = fetch_run_in_tx(&tx, &existing.run_id)?;
        let wake = run.as_ref().is_some_and(|r| !r.terminal);
        if wake {
            let run_rev = next_rev_in_tx(&tx)?;
            let cursor = append_event_in_tx(
                &tx,
                &self.meta.instance_id,
                &existing.run_id,
                &existing.session_id,
                "interaction.answered",
                "agentcpm-pd.interaction.event/1",
                &json!({
                    "interactionId": interaction_id,
                    "kind": existing.kind,
                    "answer": answer_value,
                }),
                &now,
            )?;
            tx.execute(
                "UPDATE runs SET state = 'running', waiting_for_json = '[]', revision = ?1, final_cursor = ?2, updated_at = ?3 WHERE run_id = ?4",
                params![run_rev, cursor, now, existing.run_id],
            )
            .map_err(map_sql_err)?;
        }

        let updated = fetch_interaction_in_tx(&tx, &self.meta.instance_id, interaction_id)?
            .expect("updated interaction");
        tx.commit().map_err(map_sql_err)?;
        self.event_notify.notify_waiters();
        self.state_notify.notify_waiters();
        Ok(InteractionResolveOutcome {
            interaction: updated,
            policy_revision: policy_rev,
            decision_or_answer: answer_value,
            should_wake_run: wake,
        })
    }

    /// Reads run events strictly after `after_cursor`, returning `Gap` if the cursor expired or is invalid.
    pub fn query_run_events_page(
        &self,
        run_id: &str,
        after_cursor: Option<&str>,
        limit: usize,
    ) -> Result<EventsQueryResult, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let run = fetch_run_in_tx(&conn, run_id)?.ok_or_else(|| not_found("run", run_id))?;
        let earliest_seq = get_earliest_cursor_seq(&conn)?;
        let earliest_cursor = format_cursor(earliest_seq);

        let after_seq = match after_cursor {
            None => 0,
            Some(c) => match parse_cursor(c) {
                Some(seq) if seq >= earliest_seq - 1 => seq,
                Some(_) => {
                    return Ok(EventsQueryResult::Gap(json!({
                        "frame": {
                            "kind": "gap",
                            "reason": "cursor_expired",
                            "resourceId": run_id,
                            "recoveryMethod": "run.snapshot",
                            "requestedCursor": c,
                            "earliestCursor": earliest_cursor,
                            "snapshotRevision": run.revision,
                            "recovery": "fetch_snapshot_then_resume",
                        }
                    })));
                }
                None => {
                    return Ok(EventsQueryResult::Gap(json!({
                        "frame": {
                            "kind": "gap",
                            "reason": "cursor_invalid",
                            "resourceId": run_id,
                            "recoveryMethod": "run.snapshot",
                            "requestedCursor": c,
                            "earliestCursor": earliest_cursor,
                            "snapshotRevision": run.revision,
                            "recovery": "restart_query",
                        }
                    })));
                }
            },
        };

        let events = fetch_run_events_after_conn(&conn, run_id, after_seq, limit)?;
        let high_watermark: Option<String> = conn
            .query_row(
                "SELECT cursor FROM events WHERE resource_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;

        let next_cursor = events.last().map(|e| e.cursor.clone());
        let event_objs: Vec<Value> = events
            .iter()
            .map(|e| {
                json!({
                    "resourceId": e.resource_id,
                    "cursor": e.cursor,
                    "type": e.event_type,
                    "schemaId": e.schema_id,
                    "payload": e.payload,
                    "occurredAt": e.occurred_at,
                    "version": "agentcpm-pd.event/1",
                    "instanceId": self.meta.instance_id,
                    "data": e.data,
                })
            })
            .collect();

        let caught_up = match (&next_cursor, &high_watermark) {
            (None, None) => true,
            (Some(nc), Some(hw)) => nc == hw,
            (None, Some(_)) => true,
            (Some(_), None) => true,
        };

        Ok(EventsQueryResult::Page(json!({
            "frame": {
                "events": event_objs,
                "nextCursor": next_cursor,
                "highWatermark": high_watermark,
                "caughtUp": caught_up,
                "terminal": run.terminal && caught_up,
            }
        })))
    }

    pub fn check_run_subscription_start(
        &self,
        run_id: &str,
        after_cursor: Option<&str>,
    ) -> Result<Result<i64, agentcpm_pd_protocol::contract::GapFrame>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let run = fetch_run_in_tx(&conn, run_id)?.ok_or_else(|| not_found("run", run_id))?;
        let earliest_seq = get_earliest_cursor_seq(&conn)?;
        let earliest_cursor = format_cursor(earliest_seq);
        match after_cursor {
            None => Ok(Ok(earliest_seq.saturating_sub(1))),
            Some(c) => match parse_cursor(c) {
                Some(seq) if seq >= earliest_seq - 1 => Ok(Ok(seq)),
                Some(_) => Ok(Err(agentcpm_pd_protocol::contract::GapFrame {
                    kind: "gap".to_owned(),
                    requested_cursor: Some(c.to_owned()),
                    earliest_cursor: Some(earliest_cursor),
                    snapshot_revision: run.revision,
                    recovery:
                        agentcpm_pd_protocol::contract::GapRecoveryPolicy::FetchSnapshotThenResume,
                })),
                None => Err(ContractError::new(
                    CODE_CURSOR_EXPIRED,
                    format!("invalid cursor `{c}`"),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                )),
            },
        }
    }

    pub fn fetch_run_events_after(
        &self,
        run_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<(Vec<StoredEvent>, bool), ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let run = fetch_run_in_tx(&conn, run_id)?.ok_or_else(|| not_found("run", run_id))?;
        let events = fetch_run_events_after_conn(&conn, run_id, after_seq, limit)?;
        Ok((events, run.terminal))
    }

    /// Builds inline `run.snapshot` (`state` + `history` + `interactions` sections <= 256 KiB each).
    pub fn build_run_snapshot(&self, run_id: &str) -> Result<Value, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let run = fetch_run_in_tx(&conn, run_id)?.ok_or_else(|| not_found("run", run_id))?;

        let high_cursor: Option<String> = conn
            .query_row(
                "SELECT cursor FROM events WHERE resource_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        let checkpoint_cursor = high_cursor.clone().unwrap_or_else(|| format_cursor(0));

        // P1 (wp4 review): each section's inline `data` must stay within the
        // contract's 256 KiB budget (events can be arbitrarily large). Fetch
        // one extra event so `truncated` is exact at the 200-event boundary,
        // then trim by serialized bytes from the tail; `nextCursor` names the
        // resume point for the remainder.
        let mut events = fetch_run_events_after_conn(&conn, run_id, 0, 201)?;
        let count_truncated = events.len() > 200;
        events.truncate(200);
        let mut next_cursor = events.last().map(|e| e.cursor.clone());
        let mut history_byte_truncated = false;
        let history_data = loop {
            let event_items: Vec<Value> = events
                .iter()
                .map(|e| {
                    json!({
                        "seq": e.seq,
                        "cursor": e.cursor,
                        "type": e.event_type,
                        "occurredAt": e.occurred_at,
                        "payload": e.payload,
                    })
                })
                .collect();
            let data = json!({
                "runId": run_id,
                "events": event_items,
                "truncated": count_truncated || history_byte_truncated,
                "nextCursor": next_cursor,
            });
            if section_fits_budget(&data) || events.is_empty() {
                break data;
            }
            events.pop();
            next_cursor = events.last().map(|e| e.cursor.clone());
            history_byte_truncated = true;
        };

        let mut ia_stmt = conn
            .prepare(
                "SELECT interaction_id FROM interactions WHERE run_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(map_sql_err)?;
        let ia_ids: Vec<String> = ia_stmt
            .query_map(params![run_id], |r| r.get(0))
            .map_err(map_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_err)?;
        drop(ia_stmt);

        let mut interactions = Vec::new();
        for iid in ia_ids {
            if let Some(ia) = fetch_interaction_in_tx(&conn, &self.meta.instance_id, &iid)? {
                interactions.push(ia.to_contract_json());
            }
        }
        let mut interactions_byte_truncated = false;
        let interactions_data = loop {
            let data = json!({
                "runId": run_id,
                "interactions": interactions,
                "truncated": interactions_byte_truncated,
            });
            if section_fits_budget(&data) || interactions.is_empty() {
                break data;
            }
            interactions.pop();
            interactions_byte_truncated = true;
        };

        // State section: shed bulky optional fields before degrading to a stub.
        let mut state_run = run.to_contract_json();
        let mut state_truncated = false;
        let state_data = loop {
            let data = json!({"run": state_run, "truncated": state_truncated});
            if section_fits_budget(&data) {
                break data;
            }
            if state_run
                .get("result")
                .is_some_and(|value| !value.is_null())
            {
                state_run["result"] = Value::Null;
                state_truncated = true;
                continue;
            }
            if state_run.get("input").is_some_and(|value| !value.is_null()) {
                state_run["input"] = Value::Null;
                state_truncated = true;
                continue;
            }
            if !state_truncated {
                state_truncated = true;
                continue;
            }
            // Pathological: even a stripped run object exceeds the budget.
            state_run = json!({
                "runId": run_id,
                "state": run_state_str(run.state),
                "note": "state section exceeded the 256 KiB budget; fetch the run via run.get",
            });
            break json!({"run": state_run, "truncated": true});
        };

        let sections = vec![
            build_snapshot_section("state", "agentcpm-pd.snapshot.state/1", state_data),
            build_snapshot_section("history", "agentcpm-pd.snapshot.history/1", history_data),
            build_snapshot_section(
                "interactions",
                "agentcpm-pd.snapshot.interactions/1",
                interactions_data,
            ),
        ];

        Ok(json!({
            "run": run.to_contract_json(),
            "snapshotCursor": high_cursor,
            "snapshot": {
                "checkpoint": {
                    "instanceId": self.meta.instance_id,
                    "resourceKind": "run",
                    "resourceId": run_id,
                    "cursor": checkpoint_cursor,
                    "revision": run.revision,
                },
                "sections": sections,
            }
        }))
    }

    pub fn active_run_counts(&self) -> Result<(usize, usize), ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let active: i64 = conn
            .query_row("SELECT COUNT(*) FROM runs WHERE terminal = 0", [], |r| {
                r.get(0)
            })
            .map_err(map_sql_err)?;
        let queued: i64 = conn
            .query_row("SELECT COUNT(*) FROM run_queue", [], |r| r.get(0))
            .map_err(map_sql_err)?;
        Ok((active.max(0) as usize, queued.max(0) as usize))
    }

    pub fn list_pending_runs_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<String>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let mut stmt = conn
            .prepare("SELECT run_id FROM run_queue WHERE session_id = ?1 ORDER BY position ASC")
            .map_err(map_sql_err)?;
        let out = stmt
            .query_map(params![session_id], |r| r.get(0))
            .map_err(map_sql_err)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(map_sql_err)?;
        Ok(out)
    }

    /// P1 (wp4 review): a worker must win exactly one atomic claim before it
    /// drives a run. Only a still-`queued` run can be claimed; the claim
    /// flips it to `running` so any competing worker (double spawn, queue
    /// pickup racing a submit, or a cancel that terminalized the run between
    /// the worker's read and its registration) observes zero updated rows and
    /// skips the run. The driver/owned paths re-transition `Running` right
    /// after, journaling the `run.started` event.
    pub fn claim_run_execution(&self, run_id: &str) -> Result<bool, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let updated = conn
            .execute(
                "UPDATE runs SET state = 'running', updated_at = ?2 WHERE run_id = ?1 AND state = 'queued'",
                params![run_id, now_rfc3339()],
            )
            .map_err(map_sql_err)?;
        Ok(updated == 1)
    }

    /// Helpers strictly for testing `ResponseOutcome` / unused-import-free verification.
    pub fn accepted_outcome() -> ResponseOutcome {
        ResponseOutcome::Accepted
    }
}

fn init_schema(conn: &Connection) -> Result<(), ContractError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS projects (
            project_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            root_json TEXT NOT NULL,
            registration_state TEXT NOT NULL,
            environment_id TEXT NOT NULL,
            revision TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            name TEXT NOT NULL,
            tags_json TEXT NOT NULL,
            revision TEXT NOT NULL,
            active_run_id TEXT,
            session_state TEXT NOT NULL,
            queue_paused INTEGER NOT NULL DEFAULT 0,
            pause_reason TEXT,
            queue_revision TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS runs (
            run_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            state TEXT NOT NULL,
            revision TEXT NOT NULL,
            cancel_requested INTEGER NOT NULL DEFAULT 0,
            waiting_for_json TEXT NOT NULL,
            effect_status TEXT NOT NULL,
            config_json TEXT NOT NULL,
            final_cursor TEXT,
            continued_from TEXT,
            outcome_code TEXT,
            result_json TEXT,
            input_json TEXT NOT NULL,
            terminal INTEGER NOT NULL DEFAULT 0,
            queue_paused INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS run_queue (
            session_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            run_id TEXT NOT NULL UNIQUE,
            disposition TEXT NOT NULL,
            PRIMARY KEY (session_id, position)
        );
        CREATE TABLE IF NOT EXISTS interactions (
            interaction_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            run_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            revision TEXT NOT NULL,
            state TEXT NOT NULL,
            resolution TEXT NOT NULL,
            expires_at TEXT,
            prompt_json TEXT NOT NULL,
            choices_json TEXT NOT NULL,
            response_schema_id TEXT,
            response_schema_json TEXT NOT NULL,
            answer_json TEXT,
            lease_owner TEXT,
            policy_revision TEXT,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            cursor TEXT NOT NULL UNIQUE,
            resource_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            schema_id TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            data_json TEXT NOT NULL,
            occurred_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_events_resource_seq ON events(resource_id, seq);
        CREATE TABLE IF NOT EXISTS idempotency_records (
            idempotency_key TEXT NOT NULL,
            method_id TEXT NOT NULL,
            input_digest TEXT NOT NULL,
            request_state TEXT NOT NULL,
            reason TEXT NOT NULL,
            resource_json TEXT,
            receipt_json TEXT,
            rejection_code TEXT,
            observed_at TEXT NOT NULL,
            retained_until TEXT,
            retry_disposition TEXT NOT NULL,
            PRIMARY KEY (idempotency_key, method_id)
        );
        ",
    )
    .map_err(map_sql_err)?;
    Ok(())
}

fn next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql_err)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(map_sql_err)?;
    Ok(format!("rev-{next:08}"))
}

fn current_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql_err)?
        .unwrap_or(1);
    Ok(format!("rev-{cur:08}"))
}

fn get_earliest_cursor_seq(conn: &Connection) -> Result<i64, ContractError> {
    conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'earliest_cursor_seq'",
        [],
        |r| r.get(0),
    )
    .optional()
    .map_err(map_sql_err)?
    .unwrap_or(1)
    .pipe(Ok)
}

trait PipeExt: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> PipeExt for T {}

pub fn format_cursor(seq: i64) -> String {
    format!("cur-{seq:012}")
}

pub fn parse_cursor(cursor: &str) -> Option<i64> {
    cursor.strip_prefix("cur-")?.parse::<i64>().ok()
}

#[allow(clippy::too_many_arguments)]
fn append_event_in_tx(
    conn: &Connection,
    _instance_id: &str,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    schema_id: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<String, ContractError> {
    let placeholder_cursor = format!("tmp-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    let data_str = payload_str.clone();
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            placeholder_cursor,
            resource_id,
            session_id,
            event_type,
            schema_id,
            payload_str,
            data_str,
            occurred_at,
        ],
    )
    .map_err(map_sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(map_sql_err)?;
    Ok(cursor)
}

fn record_rejection_in_tx(
    conn: &Connection,
    key: &str,
    method_id: &str,
    digest: &str,
    rejection_code: &str,
    observed_at: &str,
    retained_until: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6, 'correctRejectedInputWithNewKey')",
        params![key, method_id, digest, rejection_code, observed_at, retained_until],
    )
    .map_err(map_sql_err)?;
    Ok(())
}

fn fetch_session_json(conn: &Connection, session_id: &str) -> Result<Option<Value>, ContractError> {
    conn.query_row(
        "SELECT session_id, project_id, name, tags_json, revision, active_run_id, session_state
         FROM sessions WHERE session_id = ?1",
        params![session_id],
        |r| {
            let tags_json: String = r.get(3)?;
            Ok(json!({
                "sessionId": r.get::<_, String>(0)?,
                "projectId": r.get::<_, String>(1)?,
                "name": r.get::<_, String>(2)?,
                "tags": serde_json::from_str::<Value>(&tags_json).unwrap_or(json!([])),
                "revision": r.get::<_, String>(4)?,
                "activeRunId": r.get::<_, Option<String>>(5)?,
                "sessionState": r.get::<_, String>(6)?,
            }))
        },
    )
    .optional()
    .map_err(map_sql_err)
}

fn fetch_run_in_tx(conn: &Connection, run_id: &str) -> Result<Option<StoredRun>, ContractError> {
    let instance_id: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'instance_id'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql_err)?
        .unwrap_or_else(|| "local".to_owned());

    let row = conn
        .query_row(
            "SELECT run_id, session_id, project_id, state, revision, cancel_requested, waiting_for_json, effect_status, config_json, final_cursor, continued_from, outcome_code, result_json, input_json, terminal, queue_paused
             FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)? != 0,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, String>(13)?,
                    r.get::<_, i64>(14)? != 0,
                    r.get::<_, i64>(15)? != 0,
                ))
            },
        )
        .optional()
        .map_err(map_sql_err)?;

    Ok(row.map(
        |(
            run_id,
            session_id,
            project_id,
            state_str,
            revision,
            cancel_requested,
            waiting_for_json,
            effect_status,
            config_json,
            final_cursor,
            continued_from,
            outcome_code,
            result_json,
            input_json,
            terminal,
            queue_paused,
        )| {
            let cfg: Value = serde_json::from_str(&config_json).unwrap_or(json!({}));
            StoredRun {
                run_id,
                session_id,
                project_id,
                state: parse_run_state(&state_str),
                revision,
                cancel_requested,
                waiting_for: serde_json::from_str(&waiting_for_json).unwrap_or_default(),
                effect_status,
                config_revision: cfg["configRevision"]
                    .as_str()
                    .unwrap_or("init-1")
                    .to_owned(),
                effective_generation: cfg["effectiveGeneration"]
                    .as_str()
                    .unwrap_or("gen-1")
                    .to_owned(),
                final_cursor,
                continued_from,
                outcome_code,
                result: result_json.and_then(|s| serde_json::from_str(&s).ok()),
                input: serde_json::from_str(&input_json).unwrap_or(Value::Null),
                instance_id,
                terminal,
                queue_paused,
            }
        },
    ))
}

fn fetch_interaction_in_tx(
    conn: &Connection,
    instance_id: &str,
    interaction_id: &str,
) -> Result<Option<StoredInteraction>, ContractError> {
    let row = conn
        .query_row(
            "SELECT interaction_id, kind, run_id, project_id, session_id, revision, state, resolution, expires_at, prompt_json, choices_json, response_schema_id, response_schema_json, answer_json, lease_owner, policy_revision
             FROM interactions WHERE interaction_id = ?1",
            params![interaction_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, String>(12)?,
                    r.get::<_, Option<String>>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    r.get::<_, Option<String>>(15)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql_err)?;

    Ok(row.map(
        |(
            interaction_id,
            kind,
            run_id,
            project_id,
            session_id,
            revision,
            state,
            resolution,
            expires_at,
            prompt_json,
            choices_json,
            response_schema_id,
            response_schema_json,
            answer_json,
            lease_owner,
            policy_revision,
        )| StoredInteraction {
            interaction_id,
            kind,
            run_id,
            project_id,
            session_id,
            revision,
            state,
            resolution,
            expires_at,
            response_schema_id,
            response_schema: serde_json::from_str(&response_schema_json).unwrap_or(json!({})),
            prompt: serde_json::from_str(&prompt_json).unwrap_or_default(),
            choices: serde_json::from_str(&choices_json).unwrap_or_default(),
            lease_owner,
            answer: answer_json.and_then(|s| serde_json::from_str(&s).ok()),
            policy_revision,
            instance_id: instance_id.to_owned(),
        },
    ))
}

fn fetch_run_events_after_conn(
    conn: &Connection,
    run_id: &str,
    after_seq: i64,
    limit: usize,
) -> Result<Vec<StoredEvent>, ContractError> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at
             FROM events WHERE resource_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
        )
        .map_err(map_sql_err)?;
    let rows = stmt
        .query_map(params![run_id, after_seq, limit as i64], |r| {
            let payload_json: String = r.get(6)?;
            let data_json: String = r.get(7)?;
            let data: Map<String, Value> = serde_json::from_str(&data_json).unwrap_or_default();
            Ok(StoredEvent {
                seq: r.get(0)?,
                cursor: r.get(1)?,
                resource_id: r.get(2)?,
                session_id: r.get(3)?,
                event_type: r.get(4)?,
                schema_id: r.get(5)?,
                payload: serde_json::from_str(&payload_json).unwrap_or(Value::Null),
                data,
                occurred_at: r.get(8)?,
            })
        })
        .map_err(map_sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_err)?;
    Ok(rows)
}

fn build_queue_json(
    conn: &Connection,
    session_id: &str,
    offset: usize,
    limit: usize,
) -> Result<Value, ContractError> {
    let row: Option<(bool, Option<String>, String, Option<String>)> = conn
        .query_row(
            "SELECT queue_paused, pause_reason, queue_revision, active_run_id FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(map_sql_err)?;
    let Some((paused, pause_reason, qrev, active_run_id)) = row else {
        return Err(not_found("session", session_id));
    };

    let mut stmt = conn
        .prepare(
            "SELECT q.run_id, q.position, q.disposition, r.state, r.revision
             FROM run_queue q
             JOIN runs r ON r.run_id = q.run_id
             WHERE q.session_id = ?1
             ORDER BY q.position ASC",
        )
        .map_err(map_sql_err)?;
    let all_items: Vec<Value> = stmt
        .query_map(params![session_id], |r| {
            Ok(json!({
                "runId": r.get::<_, String>(0)?,
                "position": r.get::<_, i64>(1)?,
                "state": r.get::<_, String>(3)?,
                "revision": r.get::<_, String>(4)?,
                "queueDisposition": r.get::<_, String>(2)?,
            }))
        })
        .map_err(map_sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_err)?;

    let snap_rev = current_rev(conn)?;
    let (items, page_info) = paginate_vec(all_items, offset, limit, &snap_rev)?;

    Ok(json!({
        "queue": {
            "sessionId": session_id,
            "revision": qrev,
            "paused": paused,
            "pauseReason": pause_reason,
            "activeRunId": active_run_id,
            "items": items,
            "pageInfo": page_info,
        }
    }))
}

/// Contract budget for one inline `run.snapshot` section (`007 §7.3`: 每节
/// 上限 256 KiB). Sections over the budget are truncated by the caller.
const SNAPSHOT_SECTION_MAX_BYTES: usize = 256 * 1024;

fn section_fits_budget(data: &Value) -> bool {
    serde_json::to_vec(data)
        .map(|serialized| serialized.len() <= SNAPSHOT_SECTION_MAX_BYTES)
        .unwrap_or(false)
}

fn build_snapshot_section(name: &str, schema_id: &str, data: Value) -> Value {
    let serialized = serde_json::to_vec(&data).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&serialized);
    let sha256 = format!("{:x}", hasher.finalize());
    json!({
        "name": name,
        "schemaId": schema_id,
        "sha256": sha256,
        "data": data,
    })
}

fn parse_page(page: Option<&Value>) -> Result<(usize, usize), ContractError> {
    let obj = page.and_then(Value::as_object);
    let limit = obj
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let offset = match obj.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
        Some(c) => c.parse::<usize>().map_err(|_| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("invalid pagination cursor `{c}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?,
        None => 0,
    };
    Ok((offset, limit))
}

fn paginate_vec(
    items: Vec<Value>,
    offset: usize,
    limit: usize,
    snapshot_revision: &str,
) -> Result<(Vec<Value>, Value), ContractError> {
    if offset > items.len() {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "pagination cursor beyond end of collection",
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    let end = offset.saturating_add(limit).min(items.len());
    let complete = end >= items.len();
    let next_cursor = if complete {
        Value::Null
    } else {
        Value::String(end.to_string())
    };
    let page_info = json!({
        "snapshotRevision": snapshot_revision,
        "nextCursor": next_cursor,
        "complete": complete,
    });
    Ok((
        items.into_iter().skip(offset).take(limit).collect(),
        page_info,
    ))
}

pub fn run_state_str(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::Waiting => "waiting",
        RunState::Cancelling => "cancelling",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::Interrupted => "interrupted",
        RunState::ResultUnknown => "result_unknown",
    }
}

pub fn parse_run_state(s: &str) -> RunState {
    match s {
        "queued" => RunState::Queued,
        "running" => RunState::Running,
        "waiting" => RunState::Waiting,
        "cancelling" => RunState::Cancelling,
        "succeeded" => RunState::Succeeded,
        "failed" => RunState::Failed,
        "cancelled" => RunState::Cancelled,
        "interrupted" => RunState::Interrupted,
        "result_unknown" => RunState::ResultUnknown,
        _ => RunState::Failed,
    }
}

pub fn is_terminal_run_state(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded
            | RunState::Failed
            | RunState::Cancelled
            | RunState::Interrupted
            | RunState::ResultUnknown
    )
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn retention_expiry_rfc3339() -> String {
    (Utc::now() + chrono::Duration::days(7)).to_rfc3339()
}

fn not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn map_sql_err(err: rusqlite::Error) -> ContractError {
    storage_err(err.to_string())
}

fn storage_err(msg: String) -> ContractError {
    ContractError::new(
        agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
        format!("sqlite storage error: {msg}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

pub fn current_protocol_version() -> &'static str {
    OPERATION_VERSION
}
