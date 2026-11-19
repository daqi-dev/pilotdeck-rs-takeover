//! `configuration.*` — layered settings management, validation, and hot
//! reload (`cli.task.config`, S3; TS sources: gateway method `resolve_settings`,
//! `GET /api/config`, `GET /api/agents/runtime-config`).
//!
//! The v1 contract is a scoped key-value layer model: an `instance` layer
//! overlaid by `project` and `session` layers (`instance < project <
//! session`). Each layer has its own opaque stored revision (conditional
//! writes CAS against it); one instance-wide effective generation records
//! what the runtime has actually applied. Writing and taking effect are
//! separate versions: `configuration.update` stores and then attempts an
//! inline apply (a failed apply keeps the previous effective generation and
//! reports `reloadState: "failed"`); `configuration.reload` re-applies the
//! stored state as an idempotent async operation and is the recovery path
//! after a failed apply.
//!
//! Layering semantics (per contract acceptance):
//! * `resolve` folds the stack up to the requested scope and attributes
//!   every effective value to the layer that produced it
//!   (`sourceScope`/`sourceRevision`); `read_layer` returns one raw layer
//!   without mixing effective values.
//! * Sensitive keys (the TS baseline's `redactConfig` key pattern:
//!   `apiKey|authorization|cookie|token|secret|password|credential`,
//!   ASCII case-insensitive) are masked as `"<redacted>"` on every read
//!   path. Masking is not unsetting: the key stays present with
//!   `sensitive: true` and its stored value intact.
//! * `unset` removes only the target layer's own entry, re-exposing the
//!   lower layer's value (no cross-layer tombstones).
//! * Change-list entries that omit `value` (or carry the `"<redacted>"`
//!   mask back on a sensitive key) preserve the current value instead of
//!   clearing it — the contract's "秘密省略不清空" anchor, following the
//!   baseline's masked-secret round-trip (`preserveMaskedSecrets`).
//!
//! Validation is pluggable: other domains (models/extensions/policies,
//! later S3 tasks) register per-key validators through
//! [`register_key_validator`]; `update` routes each changed key through them
//! exactly once, and the apply step validates every stored entry the same
//! way, so nothing can take effect partially ("非法配置不部分应用").

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, ContractError,
    EffectStatus, RetrySpec,
};
use serde_json::{Map as JsonMap, Value, json};
use sha2::{Digest, Sha256};

#[path = "configuration_store.rs"]
mod configuration_store;

use self::configuration_store::{ConfigDb, LayerMap, OperationRecord, StoredEntry, domain_ddl};
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// The read-path mask for sensitive values (the baseline's gateway-wire
/// `redactConfig` replacement string).
pub const REDACTED: &str = "<redacted>";

pub(super) fn register(reg: &mut DomainRegistry) {
    for (table, ddl) in domain_ddl() {
        reg.schema(table, ddl);
    }
    reg.handler("configuration.resolve", |rt, input, _key, _rev| {
        Box::pin(configuration_resolve(rt, input))
    });
    reg.handler("configuration.read_layer", |rt, input, _key, _rev| {
        Box::pin(configuration_read_layer(rt, input))
    });
    reg.handler("configuration.validate", |rt, input, _key, _rev| {
        Box::pin(configuration_validate(rt, input))
    });
    reg.handler("configuration.update", |rt, input, _key, rev| {
        Box::pin(configuration_update(rt, input, rev))
    });
    reg.handler("configuration.reload", |rt, input, key, _rev| {
        Box::pin(configuration_reload(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Scope model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScopeTarget {
    Instance,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl ScopeTarget {
    pub(crate) fn to_scope_json(&self) -> Value {
        match self {
            Self::Instance => json!({"kind": "instance"}),
            Self::Project { project_id } => {
                json!({"kind": "project", "projectId": project_id})
            }
            Self::Session {
                project_id,
                session_id,
            } => {
                json!({"kind": "session", "projectId": project_id, "sessionId": session_id})
            }
        }
    }

    /// The durable layer id. Project/session layers use their scope JSON
    /// (unambiguous; no separator-collision games with arbitrary ids).
    pub(crate) fn layer_id(&self) -> String {
        match self {
            Self::Instance => "instance".to_owned(),
            other => serde_json::to_string(&other.to_scope_json())
                .unwrap_or_else(|_| "instance".to_owned()),
        }
    }

    /// The overlay stack, bottom-up (`instance` first, target last).
    fn stack(&self) -> Vec<ScopeTarget> {
        match self {
            Self::Instance => vec![Self::Instance],
            Self::Project { project_id } => {
                vec![
                    Self::Instance,
                    Self::Project {
                        project_id: project_id.clone(),
                    },
                ]
            }
            Self::Session {
                project_id,
                session_id,
            } => vec![
                Self::Instance,
                Self::Project {
                    project_id: project_id.clone(),
                },
                Self::Session {
                    project_id: project_id.clone(),
                    session_id: session_id.clone(),
                },
            ],
        }
    }
}

/// Parses and validates the request scope. The scope's target must exist
/// (`INVALID_ARGUMENT` otherwise — the contract's error list for these
/// operations has no `NOT_FOUND`), and a session scope must name a session
/// that belongs to the named project.
fn parse_scope(rt: &PublicV1Runtime, input: &Value) -> Result<ScopeTarget, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(invalid_argument("input.scope is required"));
    };
    match scope.get("kind").and_then(Value::as_str) {
        Some("instance") => Ok(ScopeTarget::Instance),
        Some("project") => {
            let Some(project_id) = scope.get("projectId").and_then(Value::as_str) else {
                return Err(invalid_argument("scope.projectId is required"));
            };
            if rt.store().get_project(project_id).is_err() {
                return Err(invalid_argument(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(ScopeTarget::Project {
                project_id: project_id.to_owned(),
            })
        }
        Some("session") => {
            let (Some(project_id), Some(session_id)) = (
                scope.get("projectId").and_then(Value::as_str),
                scope.get("sessionId").and_then(Value::as_str),
            ) else {
                return Err(invalid_argument(
                    "scope.projectId and scope.sessionId are required",
                ));
            };
            let session = rt.store().get_session(session_id).map_err(|_| {
                invalid_argument(format!("scope session `{session_id}` does not exist"))
            })?;
            // `get_session` wraps the row: {"session": {...}, "snapshotCursor"}.
            if session["session"].get("projectId").and_then(Value::as_str) != Some(project_id) {
                return Err(invalid_argument(format!(
                    "session `{session_id}` does not belong to project `{project_id}`"
                )));
            }
            Ok(ScopeTarget::Session {
                project_id: project_id.to_owned(),
                session_id: session_id.to_owned(),
            })
        }
        other => Err(invalid_argument(format!(
            "scope.kind must be instance, project or session; got `{other:?}`"
        ))),
    }
}

fn invalid_argument(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// Sensitive keys (baseline `redactConfig` semantics)
// ---------------------------------------------------------------------------

/// `SECRET_KEY_PATTERN` from the TS baseline (`src/pilot/config/redact.ts`):
/// a case-insensitive substring match on the key name over a folded form
/// (ASCII lowercased, separator bytes dropped) — the providers.rs
/// `secret_looking_name` semantics (162fafc47), so `api_key`, `X-API-KEY`
/// and `apiKey` all fold to `apikey`. Dropping bytes can only create new
/// needle occurrences, never remove one, so previously-rejected keys stay
/// rejected.
pub(crate) fn sensitive_key(key: &str) -> bool {
    const NEEDLES: [&str; 7] = [
        "apikey",
        "authorization",
        "cookie",
        "token",
        "secret",
        "password",
        "credential",
    ];
    let folded: Vec<u8> = key
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| b.to_ascii_lowercase())
        .collect();
    NEEDLES
        .iter()
        .any(|needle| folded.windows(needle.len()).any(|w| w == needle.as_bytes()))
}

fn mask_value(key: &str, value: &Value) -> Value {
    if sensitive_key(key) {
        json!(REDACTED)
    } else {
        value.clone()
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// One field diagnostic in the contract's `Issue` shape (`{code, field?,
/// message}`) plus a blocking flag: blocking issues make `validate` answer
/// `valid: false` and make `update` reject the whole change list.
#[derive(Clone, Debug)]
pub struct VIssue {
    pub(crate) code: String,
    pub(crate) field: Option<String>,
    pub(crate) message: String,
    pub(crate) blocking: bool,
}

impl VIssue {
    fn new(code: &str, field: Option<String>, message: impl Into<String>, blocking: bool) -> Self {
        Self {
            code: code.to_owned(),
            field,
            message: message.into(),
            blocking,
        }
    }

    fn to_json(&self) -> Value {
        let mut obj = JsonMap::new();
        obj.insert("code".into(), json!(self.code));
        if let Some(field) = &self.field {
            obj.insert("field".into(), json!(field));
        }
        obj.insert("message".into(), json!(self.message));
        Value::Object(obj)
    }

    fn severity(&self) -> &'static str {
        if self.code == "UNKNOWN_FIELD" {
            "info"
        } else {
            "warning"
        }
    }
}

/// A per-key validator registered by the domain that owns that key's schema
/// (update precondition: "每个配置key按注册表进入其provider/model/policy
/// 领域的同一校验"). Returns blocking issues for values that key must not
/// take (e.g. dangling model references).
pub type KeyValidator = fn(key: &str, value: &Value) -> Vec<VIssue>;

fn key_validators() -> &'static Mutex<Vec<KeyValidator>> {
    static REGISTRY: OnceLock<Mutex<Vec<KeyValidator>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Registers a per-key validator. Intended for the model/extension/policy
/// domains (later S3 tasks); nothing registers one in this crate today, so
/// every key is "unknown" until an owning domain claims it (unknown fields
/// are preserved, not rejected — see `read_layer`'s acceptance).
pub fn register_key_validator(validator: KeyValidator) {
    key_validators()
        .lock()
        .expect("key validator registry")
        .push(validator);
}

fn run_key_validators(key: &str, value: &Value) -> Vec<VIssue> {
    let registry = key_validators().lock().expect("key validator registry");
    registry
        .iter()
        .flat_map(|v| v(key, value))
        .collect::<Vec<_>>()
}

/// One planned mutation of the target layer.
#[derive(Clone, Debug)]
enum Patch {
    Set { key: String, value: Value },
    Delete { key: String },
}

/// The planned result of one change list: the patches to apply (in order,
/// last write wins) plus every diagnostic the list produced (blocking and
/// non-blocking together; blocking means the patches must not be written).
struct ChangePlan {
    patches: Vec<Patch>,
    issues: Vec<VIssue>,
}

impl ChangePlan {
    fn has_blocking(&self) -> bool {
        self.issues.iter().any(|issue| issue.blocking)
    }
}

/// Validates one change list against the layer's current state and plans
/// the resulting patches.
fn plan_changes(
    conn: &rusqlite::Connection,
    layer: &str,
    changes: &[Value],
) -> Result<ChangePlan, ContractError> {
    let mut patches = Vec::new();
    let mut issues: Vec<VIssue> = Vec::new();
    for change in changes {
        let key = change
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if key.is_empty() {
            issues.push(VIssue::new(
                "EMPTY_KEY",
                None,
                "change key must be a non-empty string",
                true,
            ));
            continue;
        }
        match change.get("action").and_then(Value::as_str) {
            Some("set") => {
                let existing = configuration_store::read_entry(conn, layer, key)?;
                match change.get("value") {
                    None => {
                        // Omitted value: preserve the current value instead
                        // of clearing it ("秘密省略不清空").
                        match existing {
                            Some(_) => issues.push(VIssue::new(
                                "VALUE_OMITTED_PRESERVED",
                                Some(key.to_owned()),
                                "set without value keeps the current value",
                                false,
                            )),
                            None => issues.push(VIssue::new(
                                "VALUE_REQUIRED",
                                Some(key.to_owned()),
                                "set requires a value when the key is not present",
                                true,
                            )),
                        }
                    }
                    Some(value) => {
                        if sensitive_key(key) && value == &json!(REDACTED) {
                            // The read-path mask round-tripped back: keep the
                            // stored secret instead of overwriting it with the
                            // mask literal (the baseline's
                            // `preserveMaskedSecrets` rule).
                            match existing {
                                Some(_) => issues.push(VIssue::new(
                                    "MASKED_SECRET_PRESERVED",
                                    Some(key.to_owned()),
                                    "masked sensitive value keeps the current stored value",
                                    false,
                                )),
                                None => issues.push(VIssue::new(
                                    "MASKED_SECRET_UNRESOLVED",
                                    Some(key.to_owned()),
                                    "masked sensitive value has no current value to preserve",
                                    true,
                                )),
                            }
                        } else {
                            issues.extend(run_key_validators(key, value));
                            patches.push(Patch::Set {
                                key: key.to_owned(),
                                value: value.clone(),
                            });
                        }
                    }
                }
            }
            Some("unset") => match configuration_store::read_entry(conn, layer, key)? {
                Some(_) => patches.push(Patch::Delete {
                    key: key.to_owned(),
                }),
                None => issues.push(VIssue::new(
                    "UNSET_MISSING_KEY",
                    Some(key.to_owned()),
                    "unset of a key absent from this layer is a no-op",
                    false,
                )),
            },
            other => {
                issues.push(VIssue::new(
                    "INVALID_ACTION",
                    Some(key.to_owned()),
                    format!("change action must be set or unset; got `{other:?}`"),
                    true,
                ));
            }
        }
    }
    Ok(ChangePlan { patches, issues })
}

/// The layer state after applying the planned patches (used for the
/// candidate digest and for the pre-write check).
fn candidate_layer(current: &LayerMap, patches: &[Patch]) -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = current
        .iter()
        .map(|(k, e)| (k.clone(), e.value.clone()))
        .collect();
    for patch in patches {
        match patch {
            Patch::Set { key, value } => {
                out.insert(key.clone(), value.clone());
            }
            Patch::Delete { key } => {
                out.remove(key);
            }
        }
    }
    out
}

/// Digest of the candidate layer: canonical JSON (object keys sorted at
/// every level) over the *masked* values, prefixed with the layer id — the
/// baseline hashes the redacted form (`contentHash` over
/// `redactConfig(config)`), so a digest never encodes a secret and the same
/// candidate always digests identically.
fn candidate_digest(layer: &str, candidate: &BTreeMap<String, Value>) -> String {
    let mut masked = JsonMap::new();
    for (key, value) in candidate {
        masked.insert(key.clone(), mask_value(key, value));
    }
    let canonical = configuration_store::canonical_json(&Value::Object(masked));
    let mut hasher = Sha256::new();
    hasher.update(layer.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Entry-level validation over every stored layer entry — the apply gate.
/// Today's built-in rule is the key syntax (an externally corrupted row with
/// an empty key fails the apply); registered validators add per-key rules.
fn validate_stored_entries(conn: &rusqlite::Connection) -> Result<Vec<VIssue>, ContractError> {
    let mut all_issues = Vec::new();
    for layer in configuration_store::all_layers(conn)? {
        for (key, entry) in configuration_store::read_layer(conn, &layer)? {
            if key.is_empty() {
                all_issues.push(VIssue::new(
                    "EMPTY_KEY",
                    Some(layer.clone()),
                    "stored configuration row has an empty key",
                    true,
                ));
                continue;
            }
            for issue in run_key_validators(&key, &entry.value) {
                all_issues.push(issue);
            }
        }
    }
    Ok(all_issues)
}

/// Runs the apply: validates every stored entry and, on success, advances
/// the instance-wide effective generation. On failure the previous
/// generation stays effective and the failure (target revision + reason) is
/// recorded for `resolve` to report. An apply whose stored state is already
/// fully incorporated (and currently current) is a no-op that keeps the
/// existing generation — the baseline's reload reports `unchanged` instead
/// of republishing identical state.
fn attempt_apply(
    conn: &rusqlite::Connection,
    target_revision: &str,
) -> Result<(String, String, Vec<VIssue>), ContractError> {
    let issues = validate_stored_entries(conn)?;
    let blocking: Vec<&VIssue> = issues.iter().filter(|i| i.blocking).collect();
    if let Some(first) = blocking.first() {
        let reason = format!("{}: {}", first.code, first.message);
        configuration_store::apply_failure(conn, target_revision, &reason)?;
        let effective = configuration_store::effective_row(conn)?;
        return Ok((effective.generation, "failed".to_owned(), issues));
    }
    let effective = configuration_store::effective_row(conn)?;
    let layer_revisions = configuration_store::current_layer_revisions(
        conn,
        &configuration_store::all_layers(conn)?,
    )?;
    if effective.reload_state == "current" && effective.applied == layer_revisions {
        return Ok((effective.generation, "current".to_owned(), issues));
    }
    let generation = configuration_store::next_generation(conn)?;
    configuration_store::apply_success(conn, &generation, &layer_revisions)?;
    Ok((generation, "current".to_owned(), issues))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn configuration_resolve(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_scope(&rt, &input)?;
    let db = ConfigDb::open(rt.store().home())?;
    let mut effective: BTreeMap<String, (ScopeTarget, StoredEntry)> = BTreeMap::new();
    for layer_scope in scope.stack() {
        let layer_id = layer_scope.layer_id();
        for (key, entry) in db.read_layer(&layer_id)? {
            effective.insert(key, (layer_scope.clone(), entry));
        }
    }
    let filter: Option<HashSet<String>> = input.get("keys").and_then(Value::as_array).map(|keys| {
        keys.iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    });
    let mut values = Vec::new();
    for (key, (source_scope, entry)) in effective {
        if let Some(filter) = &filter
            && !filter.contains(&key)
        {
            continue;
        }
        values.push(json!({
            "key": key,
            "value": mask_value(&key, &entry.value),
            "sourceScope": source_scope.to_scope_json(),
            "sourceRevision": entry.write_revision,
            "sensitive": sensitive_key(&key),
        }));
    }
    let effective_state = db.effective()?;
    let stored_revision = db.layer_revision(&scope.layer_id())?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "values": values,
            "storedRevision": stored_revision,
            "effectiveGeneration": effective_state.generation,
            "reloadState": effective_state.reload_state,
        }),
    ))
}

async fn configuration_read_layer(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_scope(&rt, &input)?;
    let db = ConfigDb::open(rt.store().home())?;
    let layer_id = scope.layer_id();
    let layer = db.read_layer(&layer_id)?;
    let values: Vec<Value> = layer
        .iter()
        .map(|(key, entry)| {
            json!({
                "key": key,
                "value": mask_value(key, &entry.value),
            })
        })
        .collect();
    let revision = db.layer_revision(&layer_id)?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"values": values, "revision": revision}),
    ))
}

async fn configuration_validate(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_scope(&rt, &input)?;
    let mut db = ConfigDb::open(rt.store().home())?;
    let layer_id = scope.layer_id();
    let result = db.with_tx(|conn| {
        let expected = input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let current = configuration_store::layer_revision(conn, &layer_id)?;
        if expected != current {
            return Err(revision_conflict(expected, &current));
        }
        let layer = configuration_store::read_layer(conn, &layer_id)?;
        let changes = input
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let plan = plan_changes(conn, &layer_id, &changes)?;
        if plan.has_blocking() {
            return Ok((false, plan.issues, None));
        }
        let candidate = candidate_layer(&layer, &plan.patches);
        let digest = candidate_digest(&layer_id, &candidate);
        Ok((true, plan.issues, Some(digest)))
    })?;
    let (valid, issues, digest) = result;
    let digest = digest.unwrap_or_else(|| {
        // A rejected candidate still needs a digest: hash the error marker
        // plus the change list so repeated identical rejections agree.
        let canonical = configuration_store::canonical_json(&json!({
            "rejected": true,
            "changes": input.get("changes").cloned().unwrap_or(Value::Null),
        }));
        let mut hasher = Sha256::new();
        hasher.update(layer_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical.as_bytes());
        format!("{:x}", hasher.finalize())
    });
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "valid": valid,
            "issues": issues.iter().map(VIssue::to_json).collect::<Vec<_>>(),
            "candidateDigest": digest,
        }),
    ))
}

async fn configuration_update(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_scope(&rt, &input)?;
    let layer_id = scope.layer_id();
    let mut db = ConfigDb::open(rt.store().home())?;
    let expected = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let changed_keys: Vec<String> = input
        .get("changes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("key").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let (stored_revision, effective_generation, reload_state, warnings) = db.with_tx(|conn| {
        // CAS against the layer's stored revision: a stale draft never
        // overwrites a newer configuration.
        let current = configuration_store::layer_revision(conn, &layer_id)?;
        if expected != current {
            return Err(revision_conflict(&expected, &current));
        }
        let _layer = configuration_store::read_layer(conn, &layer_id)?;
        let changes = input
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let plan = plan_changes(conn, &layer_id, &changes)?;
        // "非法配置整体不改": a blocking issue rejects the whole change list
        // before anything is written; the stored revision and the effective
        // generation stay untouched (returning the error aborts the
        // transaction before the first write).
        if plan.has_blocking() {
            let mut details = JsonMap::new();
            details.insert(
                "issues".into(),
                json!(plan.issues.iter().map(VIssue::to_json).collect::<Vec<_>>()),
            );
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "configuration changes failed validation; nothing was written",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .with_details(details));
        }
        let new_revision = configuration_store::next_revision(conn)?;
        for patch in plan.patches {
            match patch {
                Patch::Set { key, value } => {
                    configuration_store::write_entry(conn, &layer_id, &key, &value, &new_revision)?;
                }
                Patch::Delete { key } => {
                    configuration_store::delete_entry(conn, &layer_id, &key)?;
                }
            }
        }
        configuration_store::set_layer_revision(conn, &layer_id, &new_revision)?;
        // Writing and taking effect are separate versions: the layer is now
        // stored ahead, and the inline apply decides which generation the
        // runtime reports (a failure keeps the previous effective one).
        let (generation, reload_state, apply_issues) = attempt_apply(conn, &new_revision)?;
        let mut issues = plan.issues;
        issues.extend(apply_issues);
        let event_session = match &scope {
            ScopeTarget::Session { session_id, .. } => session_id.clone(),
            _ => String::new(),
        };
        let payload = json!({
            "scope": scope.to_scope_json(),
            "storedRevision": new_revision,
            "effectiveGeneration": generation,
            "reloadState": reload_state,
            "changedKeys": changed_keys,
        });
        configuration_store::append_config_event(
            conn,
            &format!("config:{layer_id}"),
            &event_session,
            &payload,
        )?;
        Ok((new_revision, generation, reload_state, issues))
    })?;
    rt.store().event_notifier().notify_waiters();
    rt.store().state_notifier().notify_waiters();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "storedRevision": stored_revision,
            "effectiveGeneration": effective_generation,
            "reloadState": reload_state,
            "issues": warnings.iter().map(VIssue::to_json).collect::<Vec<_>>(),
        }),
    ))
}

async fn configuration_reload(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_scope(&rt, &input)?;
    let layer_id = scope.layer_id();
    let digest = compute_input_digest("configuration.reload", &input);
    let mut db = ConfigDb::open(rt.store().home())?;
    let tx = db.begin()?;

    // Idempotency first (the `run.submit` pattern): a replay of the same
    // key + semantic input returns the original receipt; the same key with
    // different input is a content conflict.
    if let Some(key) = idempotency_key.as_deref()
        && let Some(record) = configuration_store::read_idempotency(tx.conn(), key)?
    {
        if record.input_digest != digest {
            return Err(DispatchError::from(ContractError::new(
                CODE_IDEMPOTENCY_CONFLICT,
                format!(
                    "idempotencyKey `{key}` already used for configuration.reload with different input"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
        if record.request_state == "accepted"
            && let Some(raw) = record.receipt_json
            && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
            && let Some(obj) = receipt.as_object_mut()
        {
            obj.insert("deduplicated".into(), json!(true));
            return Ok((ResponseOutcome::Accepted, receipt));
        }
        return Err(DispatchError::from(ContractError::new(
            record
                .rejection_code
                .unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
            "request previously rejected definitively".to_owned(),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }

    // CAS against the target layer's stored revision; the rejection is
    // durably recorded before the error is answered so replays of the same
    // key get the same definitive answer.
    let expected = input
        .get("expectedStoredRevision")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let current = configuration_store::layer_revision(tx.conn(), &layer_id)?;
    if expected != current {
        let now = crate::public_v1::store::now_rfc3339();
        if let Some(key) = idempotency_key.as_deref() {
            configuration_store::record_rejection(
                tx.conn(),
                key,
                &digest,
                CODE_REVISION_CONFLICT,
                &now,
            )?;
        }
        tx.commit()?;
        return Err(DispatchError::from(revision_conflict(expected, &current)));
    }

    let operation_id = rt.store().next_id();
    let accepted_revision = configuration_store::next_revision(tx.conn())?;
    let (state, result, error, progress) = {
        let (generation, reload_state, issues) = attempt_apply(tx.conn(), &current)?;
        if reload_state == "failed" {
            let first = issues.iter().find(|i| i.blocking);
            let (code, message) = first
                .map(|i| (i.code.clone(), i.message.clone()))
                .unwrap_or_else(|| {
                    (
                        "CONFIG_APPLY_FAILED".to_owned(),
                        "configuration apply failed".to_owned(),
                    )
                });
            let progress = vec![
                json!({"key": "targetStoredRevision", "value": current}),
                json!({"key": "effectiveGeneration", "value": generation}),
            ];
            (
                "failed".to_owned(),
                None,
                Some(json!({"code": code, "message": message})),
                progress,
            )
        } else {
            let diagnostics: Vec<Value> = issues
                .iter()
                .map(|i| {
                    json!({
                        "code": i.code,
                        "severity": i.severity(),
                        "message": i.message,
                    })
                })
                .collect();
            (
                "succeeded".to_owned(),
                Some(json!({
                    "storedRevision": current,
                    "effectiveGeneration": generation,
                    "moduleDiagnostics": diagnostics,
                })),
                None,
                Vec::new(),
            )
        }
    };
    let record = OperationRecord {
        operation_id: operation_id.clone(),
        operation_type: "configuration.reload".to_owned(),
        scope_json: scope.to_scope_json(),
        state: state.clone(),
        revision: accepted_revision.clone(),
        cancel_requested: false,
        effect_status: "known".to_owned(),
        progress,
        result,
        error,
    };
    configuration_store::write_operation(tx.conn(), &record)?;
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id.clone(),
        "durable": true,
        "acceptedRevision": accepted_revision,
        "deduplicated": false,
        "state": state,
    });
    if let Some(key) = idempotency_key.as_deref() {
        let now = crate::public_v1::store::now_rfc3339();
        let resource = json!({"kind": "operation", "operationId": operation_id});
        configuration_store::record_acceptance(tx.conn(), key, &digest, &resource, &receipt, &now)?;
    }
    tx.commit()?;
    Ok((ResponseOutcome::Accepted, receipt))
}

fn revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("configuration expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

#[cfg(test)]
#[path = "configuration_tests.rs"]
mod configuration_tests;
