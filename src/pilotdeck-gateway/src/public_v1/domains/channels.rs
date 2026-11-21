//! `channels.*` — the v1 channel-management domain (t019 T2, brief
//! `_tmp/20261118-s3/briefs/channels-schedules.md` §3).
//!
//! Scope of this implementation (the contract's own shape): v1 is a
//! **configuration + state-machine + probe** surface over the 19 baseline
//! adapter types. The channel runtimes themselves (inbound-to-session,
//! outbound delivery) live in the native-process host, not in this CLI
//! gateway, so the v1 semantics are:
//!
//! * **Storage**: every configured channel is a row in `v1_channels`
//!   (`<dataRoot>/v1/state.sqlite`) with its own opaque revision
//!   (`crev-…`) and, after the first transition, a running generation
//!   (`gen-…`). Nothing writes `pilotdeck.yaml` here — the v1 store *is* the
//!   configuration substrate (the TS baseline's `adapters.<key>` yaml layer
//!   has no scope/revision concept to map onto the contract).
//! * **Configuration schemas**: each adapter type carries a version-fixed
//!   field schema (`agentcpm-pd.channel.<type>.config/1`) derived from the
//!   TS baseline's `PilotAdaptersConfig` field sets. Unknown fields are
//!   rejected; secrets enter only as `credentialRef` references into the
//!   `credential.*` store (the contract's "凭据只接受credentialRef"), so a
//!   stored configuration never contains plaintext secret material and
//!   `channels.list` can return it as-is (脱敏 by construction).
//! * **`transition`** is the assembly gate: `enable`/`reload` re-validate
//!   the configuration and, when the configuration names a `probeEndpoint`,
//!   run the probe before reporting `ready` (合同: "渠道状态达到ready必须
//!   完成真实装配，不能只写enabled=true"). `disable` drains the old
//!   generation (`drainedGeneration`) and stops reporting online. Each
//!   successful transition advances the generation.
//! * **`probe`** is the controlled connectivity/authentication test: one
//!   `POST <probeEndpoint>` with `{"action":"probe"}` and the credential
//!   (when one is referenced) as a bearer token. Transport cause codes are
//!   classified into DNS / connection / timeout failures and the endpoint's
//!   application-level answer into authentication / signature failures, so
//!   the e2e protocol stubs can distinguish them (合同 acceptance). It
//!   sends no business message and registers no subscription.
//! * **`login`** (begin/poll/cancel) is a challenge state machine over
//!   `v1_channel_challenges` with the three baseline modes
//!   (feishu→`deviceCode`, weixin→`qr`, wecom→`externalBrowser`): explicit
//!   `challengeId`, irreversible terminal states, and credentials persisted
//!   through `v1_credentials` *before* the challenge reports `succeeded`
//!   (合同: "凭据持久化成功才返回succeeded"). The external exchange itself
//!   goes through the same `probeEndpoint` protocol stub
//!   (`action=begin`/`action=poll`), which is the e2e seam for the loopback
//!   mocks; the real external providers are out of this host's scope.
//!
//! v1 choices the contract does not pin (recorded in the task report):
//! scope visibility is exact (a channel row belongs to the scope that
//! created it); `channels.list`'s top-level `generation` is the domain's
//! snapshot counter; probe requires an explicit `probeEndpoint`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetryMode, RetrySpec,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, Uri, header};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::schedules::DDL_DOMAIN_OPERATIONS;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::runtime::model::http::{
    HyperModelTransport, ModelHttpTransport, ProxyInstallMode, ProxySettings,
};

// ---------------------------------------------------------------------------
// The adapter registry (19 baseline types)
// ---------------------------------------------------------------------------

/// The 15 `CHANNEL_LOADER_KEYS` (loader order) — the TS baseline's fixed
/// `Object.entries(CHANNEL_LOADERS)` iteration.
const LOADER_ADAPTER_KEYS: [&str; 15] = [
    "telegram",
    "discord",
    "slack",
    "matrix",
    "mattermost",
    "signal",
    "whatsapp",
    "bluebubbles",
    "dingtalk",
    "wecomCallback",
    "email",
    "sms",
    "homeassistant",
    "apiServer",
    "webhook",
];

/// The `BUNDLE_CHANNEL_KEYS` not already in the loader table, appended in
/// the bundle's own order — `supportedAdapterTypes` is exactly this list.
pub(crate) const BUNDLE_ONLY_ADAPTER_KEYS: [&str; 4] = ["weixin", "feishu", "qq", "wecom"];

/// Every adapter type this build knows (`channels.list`'s
/// `supportedAdapterTypes`): loader keys in order, then the bundle keys.
pub(crate) fn supported_adapter_types() -> Vec<String> {
    LOADER_ADAPTER_KEYS
        .iter()
        .chain(BUNDLE_ONLY_ADAPTER_KEYS.iter())
        .map(|key| (*key).to_owned())
        .collect()
}

/// The interactive login mode of the three baseline login-capable adapters
/// (`challengeType` of the contract).
fn login_challenge_type(adapter_type: &str) -> Option<&'static str> {
    match adapter_type {
        "feishu" => Some("deviceCode"),
        "weixin" => Some("qr"),
        "wecom" => Some("externalBrowser"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Per-adapter configuration schemas (version-fixed, /1)
// ---------------------------------------------------------------------------

/// One field of an adapter's configuration schema. The v1 schemas keep the
/// TS baseline's field names (`PilotAdaptersConfig`, types.ts:198-239);
/// secret-bearing fields become `credentialRef` references instead of
/// plaintext.
#[derive(Clone, Copy)]
enum FieldKind {
    Bool,
    Str,
    Int,
    StrEnum(&'static [&'static str]),
    StrList,
}

struct FieldSpec {
    name: &'static str,
    kind: FieldKind,
    required: bool,
}

const fn opt(name: &'static str, kind: FieldKind) -> FieldSpec {
    FieldSpec {
        name,
        kind,
        required: false,
    }
}

const fn req(name: &'static str, kind: FieldKind) -> FieldSpec {
    FieldSpec {
        name,
        kind,
        required: true,
    }
}

/// The shared `PilotPlatformAdapterConfig` field set (`enabled`, secret as
/// `credentialRef`, `webhookUrl`) plus the v1 probe seam `probeEndpoint`.
const PLATFORM_FIELDS: [FieldSpec; 4] = [
    req("enabled", FieldKind::Bool),
    opt("credentialRef", FieldKind::Str),
    opt("webhookUrl", FieldKind::Str),
    opt("probeEndpoint", FieldKind::Str),
];

const FEISHU_FIELDS: [FieldSpec; 8] = [
    req("enabled", FieldKind::Bool),
    opt("appId", FieldKind::Str),
    opt("credentialRef", FieldKind::Str),
    opt("defaultSessionLabel", FieldKind::Str),
    opt("connectionMode", FieldKind::StrEnum(&["stream", "webhook"])),
    opt("domainName", FieldKind::StrEnum(&["feishu", "lark"])),
    opt(
        "permissionMode",
        FieldKind::StrEnum(&["default", "bypassPermissions"]),
    ),
    opt("probeEndpoint", FieldKind::Str),
];

const WEIXIN_FIELDS: [FieldSpec; 3] = [
    req("enabled", FieldKind::Bool),
    opt("credentialRef", FieldKind::Str),
    opt("probeEndpoint", FieldKind::Str),
];

const QQ_FIELDS: [FieldSpec; 6] = [
    req("enabled", FieldKind::Bool),
    opt("credentialRef", FieldKind::Str),
    opt("allowGroups", FieldKind::StrList),
    opt("triggerPrefixes", FieldKind::StrList),
    opt("maxMessageLength", FieldKind::Int),
    opt("probeEndpoint", FieldKind::Str),
];

fn schema_fields(adapter_type: &str) -> &'static [FieldSpec] {
    match adapter_type {
        "feishu" => &FEISHU_FIELDS,
        "weixin" => &WEIXIN_FIELDS,
        "qq" => &QQ_FIELDS,
        // The 16 `PilotPlatformAdapterConfig` channels.
        _ => &PLATFORM_FIELDS,
    }
}

/// The version-fixed schema id one adapter type's configuration must
/// carry (`schemaId` of `validate`/`configure` inputs and `list` rows).
pub(crate) fn channel_schema_id(adapter_type: &str) -> String {
    format!("agentcpm-pd.channel.{adapter_type}.config/1")
}

/// One `channels.validate` diagnostic.
struct Diagnostic {
    code: &'static str,
    path: Option<String>,
    message: String,
    severity: &'static str,
}

impl Diagnostic {
    fn error(code: &'static str, path: Option<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            path,
            message: message.into(),
            severity: "error",
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "path": self.path,
            "message": self.message,
            "severity": self.severity,
        })
    }
}

/// Validates a candidate configuration against the adapter's `/1` schema.
/// `credential_exists` resolves `credentialRef` references (the same
/// storage check `validate`'s acceptance names: "校验endpoint、路由映射及
/// credentialRef"). Pure: no writes, no network.
fn validate_configuration(
    adapter_type: &str,
    configuration: &Value,
    credential_exists: impl Fn(&str) -> Result<bool, ContractError>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(object) = configuration.as_object() else {
        out.push(Diagnostic::error(
            "TYPE_MISMATCH",
            None,
            "configuration must be a JSON object",
        ));
        return out;
    };
    let fields = schema_fields(adapter_type);
    for spec in fields {
        match object.get(spec.name) {
            None => {
                if spec.required {
                    out.push(Diagnostic::error(
                        "MISSING_FIELD",
                        Some(format!("$.{}", spec.name)),
                        format!("`{}` is required", spec.name),
                    ));
                }
            }
            Some(value) => match spec.kind {
                FieldKind::Bool => {
                    if !value.is_boolean() {
                        out.push(Diagnostic::error(
                            "TYPE_MISMATCH",
                            Some(format!("$.{}", spec.name)),
                            format!("`{}` must be a boolean", spec.name),
                        ));
                    }
                }
                FieldKind::Str => {
                    if !value.is_string() {
                        out.push(Diagnostic::error(
                            "TYPE_MISMATCH",
                            Some(format!("$.{}", spec.name)),
                            format!("`{}` must be a string", spec.name),
                        ));
                    } else if spec.name == "credentialRef" {
                        // The reference must resolve; a storage failure is
                        // reported as its own diagnostic instead of a silent
                        // "not found".
                        match credential_exists(value.as_str().unwrap_or_default()) {
                            Ok(true) => {}
                            Ok(false) => out.push(Diagnostic::error(
                                "CREDENTIAL_NOT_FOUND",
                                Some("$.credentialRef".to_owned()),
                                "credentialRef does not resolve to a stored credential",
                            )),
                            Err(_) => out.push(Diagnostic::error(
                                "CREDENTIAL_LOOKUP_FAILED",
                                Some("$.credentialRef".to_owned()),
                                "the credential store could not be read",
                            )),
                        }
                    }
                }
                FieldKind::Int => {
                    if !value.is_i64() {
                        out.push(Diagnostic::error(
                            "TYPE_MISMATCH",
                            Some(format!("$.{}", spec.name)),
                            format!("`{}` must be an integer", spec.name),
                        ));
                    }
                }
                FieldKind::StrEnum(values) => {
                    let Some(text) = value.as_str() else {
                        out.push(Diagnostic::error(
                            "TYPE_MISMATCH",
                            Some(format!("$.{}", spec.name)),
                            format!("`{}` must be a string", spec.name),
                        ));
                        continue;
                    };
                    if !values.contains(&text) {
                        out.push(Diagnostic::error(
                            "INVALID_VALUE",
                            Some(format!("$.{}", spec.name)),
                            format!("`{}` must be one of {}", spec.name, values.join(", ")),
                        ));
                    }
                }
                FieldKind::StrList => {
                    let ok = value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string));
                    if !ok {
                        out.push(Diagnostic::error(
                            "TYPE_MISMATCH",
                            Some(format!("$.{}", spec.name)),
                            format!("`{}` must be an array of strings", spec.name),
                        ));
                    }
                }
            },
        }
    }
    for key in object.keys() {
        if !fields.iter().any(|spec| spec.name == key.as_str()) {
            // The contract's hard line on candidate configurations: unknown
            // fields are rejected (no `method`/`shell` escape hatch).
            out.push(Diagnostic::error(
                "UNKNOWN_FIELD",
                Some(format!("$.{key}")),
                format!("unknown field `{key}` for this adapter schema"),
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The contract's `scope` input (global | project | session)
// ---------------------------------------------------------------------------

/// The `scope` object shared by the channels and notifications-preferences
/// operations: `{"kind": "global"|"project"|"session", "id": …}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum V1Scope {
    Global,
    Project(String),
    Session(String),
}

impl V1Scope {
    /// The durable storage key parts.
    pub(crate) fn storage_parts(&self) -> (&'static str, String) {
        match self {
            Self::Global => ("global", String::new()),
            Self::Project(id) => ("project", id.clone()),
            Self::Session(id) => ("session", id.clone()),
        }
    }

    fn matches_row(&self, kind: &str, id: &str) -> bool {
        let (want_kind, want_id) = self.storage_parts();
        kind == want_kind && id == want_id
    }
}

/// Parses and validates the request scope. The input schema already pins
/// `kind`'s enum; this enforces the id contract ("project/session时必填；
/// global必须为null") and that the target exists.
pub(crate) fn parse_v1_scope(
    rt: &PublicV1Runtime,
    input: &Value,
) -> Result<V1Scope, ContractError> {
    let Some(scope) = input.get("scope") else {
        return Err(invalid_arg("input.scope is required"));
    };
    let id = match scope.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(_) => return Err(invalid_arg("scope.id must be a non-empty string or null")),
    };
    match scope.get("kind").and_then(Value::as_str) {
        Some("global") => {
            if id.is_some() {
                return Err(invalid_arg("scope.id must be null when kind is `global`"));
            }
            Ok(V1Scope::Global)
        }
        Some("project") => {
            let Some(project_id) = id else {
                return Err(invalid_arg("scope.id is required when kind is `project`"));
            };
            if rt.store().get_project(&project_id).is_err() {
                return Err(invalid_arg(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(V1Scope::Project(project_id))
        }
        Some("session") => {
            let Some(session_id) = id else {
                return Err(invalid_arg("scope.id is required when kind is `session`"));
            };
            if rt.store().get_session(&session_id).is_err() {
                return Err(invalid_arg(format!(
                    "scope session `{session_id}` does not exist"
                )));
            }
            Ok(V1Scope::Session(session_id))
        }
        other => Err(invalid_arg(format!(
            "scope.kind must be global, project or session; got `{other:?}`"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

const DDL_CHANNELS: &str = "CREATE TABLE IF NOT EXISTS v1_channels (
    channel_id TEXT PRIMARY KEY,
    adapter_type TEXT NOT NULL,
    schema_id TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL DEFAULT '',
    configuration_json TEXT NOT NULL,
    revision TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'disabled',
    reason_code TEXT,
    generation TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

const DDL_CHALLENGES: &str = "CREATE TABLE IF NOT EXISTS v1_channel_challenges (
    challenge_id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    challenge_type TEXT NOT NULL,
    state TEXT NOT NULL,
    user_action_url TEXT,
    qr_payload TEXT,
    user_code TEXT,
    expires_at TEXT NOT NULL,
    credential_ref TEXT,
    error_code TEXT,
    idempotency_key TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

const DDL_CHANNELS_META: &str = "CREATE TABLE IF NOT EXISTS v1_channels_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

const DDL_CHANNEL_TRANSITIONS: &str = "CREATE TABLE IF NOT EXISTS v1_channel_transitions (
    channel_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);";

const DDL_CHANNEL_IDEMPOTENCY: &str = "CREATE TABLE IF NOT EXISTS v1_channel_idempotency (
    method_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    response_json TEXT,
    rejection_code TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (method_id, idempotency_key)
);";

const DDL_CHALLENGES_IDEM_INDEX: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_v1_channel_challenges_idem
    ON v1_channel_challenges(idempotency_key) WHERE idempotency_key IS NOT NULL;";

const DDL_DOMAIN_OPS_IDEM_INDEX: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_v1_domain_operations_idem
    ON v1_domain_operations(method_id, idempotency_key) WHERE idempotency_key IS NOT NULL;";

/// The credentials table (owned by the `credentials` domain, DDL registered
/// there): `poll_login`'s succeeded path inserts the exchanged secret here.
const DDL_CREDENTIALS: &str = "CREATE TABLE IF NOT EXISTS v1_credentials (
    credential_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    secret TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_channels", DDL_CHANNELS);
    reg.schema("v1_channel_challenges", DDL_CHALLENGES);
    reg.schema("v1_channels_meta", DDL_CHANNELS_META);
    reg.schema("v1_channel_transitions", DDL_CHANNEL_TRANSITIONS);
    reg.schema("v1_channel_idempotency", DDL_CHANNEL_IDEMPOTENCY);
    reg.schema("idx_v1_channel_challenges_idem", DDL_CHALLENGES_IDEM_INDEX);
    reg.schema("idx_v1_domain_operations_idem", DDL_DOMAIN_OPS_IDEM_INDEX);
    reg.handler("channels.list", |rt, input, _key, _rev| {
        Box::pin(channels_list(rt, input))
    });
    reg.handler("channels.validate", |rt, input, _key, _rev| {
        Box::pin(channels_validate(rt, input))
    });
    reg.handler("channels.configure", |rt, input, _key, _rev| {
        Box::pin(channels_configure(rt, input))
    });
    reg.handler("channels.probe", |rt, input, key, _rev| {
        Box::pin(channels_probe(rt, input, key))
    });
    reg.handler("channels.transition", |rt, input, key, _rev| {
        Box::pin(channels_transition(rt, input, key))
    });
    reg.handler("channels.begin_login", |rt, input, key, _rev| {
        Box::pin(channels_begin_login(rt, input, key))
    });
    reg.handler("channels.poll_login", |rt, input, key, _rev| {
        Box::pin(channels_poll_login(rt, input, key))
    });
    reg.handler("channels.cancel_login", |rt, input, key, _rev| {
        Box::pin(channels_cancel_login(rt, input, key))
    });
}

#[derive(Clone)]
struct ChannelRow {
    channel_id: String,
    adapter_type: String,
    schema_id: String,
    scope_kind: String,
    scope_id: String,
    configuration: Value,
    revision: String,
    enabled: bool,
    state: String,
    reason_code: Option<String>,
    generation: Option<String>,
}

#[derive(Clone)]
struct ChallengeRow {
    challenge_id: String,
    channel_id: String,
    challenge_type: String,
    state: String,
    user_action_url: Option<String>,
    qr_payload: Option<String>,
    user_code: Option<String>,
    expires_at: String,
    credential_ref: Option<String>,
    error_code: Option<String>,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn sql_err(context: &str, error: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("{context}: {error}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
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

fn revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

/// The contract's `GENERATION_CONFLICT` (no protocol constant exists).
fn generation_conflict(expected: &str) -> ContractError {
    ContractError::new(
        "GENERATION_CONFLICT",
        format!("expectedGeneration `{expected}` does not match the current generation"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn upstream_error(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "UPSTREAM_ERROR",
        message,
        RetrySpec::backoff_read(Some(1000)),
        EffectStatus::NotApplied,
    )
}

/// This domain's SQLite handle: a second connection to the shared
/// `<home>/v1/state.sqlite` (WAL, short transactions).
pub(crate) struct ChannelDb {
    conn: Mutex<Connection>,
    notifier: Option<Arc<Notify>>,
}

impl ChannelDb {
    fn open_with_notifier(
        home: &std::path::Path,
        notifier: Option<Arc<Notify>>,
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
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| sql_err("pragma v1 state db", e))?;
        for ddl in [
            DDL_CHANNELS,
            DDL_CHALLENGES,
            DDL_CHANNELS_META,
            DDL_CHANNEL_TRANSITIONS,
            DDL_CHANNEL_IDEMPOTENCY,
            DDL_DOMAIN_OPERATIONS,
            DDL_CREDENTIALS,
            DDL_CHALLENGES_IDEM_INDEX,
            DDL_DOMAIN_OPS_IDEM_INDEX,
        ] {
            conn.execute_batch(ddl)
                .map_err(|e| sql_err("channels ddl", e))?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            notifier,
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

    fn counter_in_tx(tx: &Transaction<'_>, key: &str) -> Result<i64, ContractError> {
        tx.query_row(
            "SELECT CAST(value AS INTEGER) FROM v1_channels_meta WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| sql_err("read channels counter", e))
        .map(|opt| opt.unwrap_or(0))
    }

    fn next_counter_in_tx(tx: &Transaction<'_>, key: &str) -> Result<i64, ContractError> {
        let current = Self::counter_in_tx(tx, key)?;
        let next = current + 1;
        tx.execute(
            "INSERT INTO v1_channels_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, next.to_string()],
        )
        .map_err(|e| sql_err("write channels counter", e))?;
        Ok(next)
    }

    fn next_shared_meta_revision_in_tx(tx: &Transaction<'_>) -> Result<String, ContractError> {
        let current: i64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_err("read credential revision", e))?
            .unwrap_or(1);
        let next = current + 1;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![next.to_string()],
        )
        .map_err(|e| sql_err("write credential revision", e))?;
        Ok(format!("rev-{next:08}"))
    }

    fn counter(&self, key: &str) -> Result<i64, ContractError> {
        self.with_conn("read channels counter", |conn| {
            Ok(conn
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM v1_channels_meta WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0))
        })
    }

    fn current_snapshot_generation(&self) -> Result<String, ContractError> {
        Ok(format!("cgen-{:08}", self.counter("snapshot_counter")?))
    }

    // ---- channels --------------------------------------------------------

    fn channel(&self, channel_id: &str) -> Result<Option<ChannelRow>, ContractError> {
        self.with_conn("read channel", |conn| {
            Self::channel_on_conn(conn, channel_id)
        })
    }

    fn channel_on_conn(
        conn: &Connection,
        channel_id: &str,
    ) -> Result<Option<ChannelRow>, rusqlite::Error> {
        conn.query_row(
            "SELECT channel_id, adapter_type, schema_id, scope_kind, scope_id,
                    configuration_json, revision, enabled, state, reason_code, generation
             FROM v1_channels WHERE channel_id = ?1",
            params![channel_id],
            row_to_channel,
        )
        .optional()
    }

    fn channels_for_scope(&self, scope: &V1Scope) -> Result<Vec<ChannelRow>, ContractError> {
        let (kind, id) = scope.storage_parts();
        self.with_conn("list channels", |conn| {
            let mut stmt = conn.prepare(
                "SELECT channel_id, adapter_type, schema_id, scope_kind, scope_id,
                        configuration_json, revision, enabled, state, reason_code, generation
                 FROM v1_channels WHERE scope_kind = ?1 AND scope_id = ?2
                 ORDER BY created_at ASC, channel_id ASC",
            )?;
            let rows = stmt
                .query_map(params![kind, id], row_to_channel)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Atomic single-transaction `channels.configure` CAS: checks
    /// `expectedRevision`, scope, and `adapterType`, increments counters, and
    /// writes the row in one `BEGIN IMMEDIATE` transaction.
    #[allow(clippy::too_many_arguments)]
    fn configure_channel_cas(
        &self,
        channel_id: &str,
        adapter_type: &str,
        schema_id: &str,
        scope_kind: &str,
        scope_id: &str,
        configuration: &Value,
        expected_revision: &str,
    ) -> Result<(String, Option<String>), ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin configure cas tx", e))?;
        let existing = Self::channel_on_conn(&tx, channel_id)
            .map_err(|e| sql_err("read channel in configure tx", e))?;
        match (expected_revision, &existing) {
            ("absent", Some(_)) => {
                return Err(ContractError::new(
                    CODE_REVISION_CONFLICT,
                    format!(
                        "channel `{channel_id}` already exists; pass its revision to update it"
                    ),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                ));
            }
            ("absent", None) => {}
            (_, Some(row)) if expected_revision == row.revision => {
                if row.scope_kind != scope_kind || row.scope_id != scope_id {
                    return Err(invalid_arg(format!(
                        "channel `{channel_id}` belongs to another scope"
                    )));
                }
                if row.adapter_type != adapter_type {
                    return Err(invalid_arg(format!(
                        "channel `{channel_id}` is a `{}` adapter; re-typing requires a new channel",
                        row.adapter_type
                    )));
                }
            }
            (revision, row) => {
                let current = row
                    .as_ref()
                    .map(|row| row.revision.clone())
                    .unwrap_or_else(|| "absent".to_owned());
                return Err(revision_conflict(revision, &current));
            }
        }
        let enabled = configuration
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let revision = format!("crev-{:08}", Self::next_counter_in_tx(&tx, "rev_counter")?);
        let _snapshot = Self::next_counter_in_tx(&tx, "snapshot_counter")?;
        let (state, reason_code, generation) = match &existing {
            Some(row) if !enabled => ("disabled".to_owned(), None, row.generation.clone()),
            Some(row) => (
                row.state.clone(),
                row.reason_code.clone(),
                row.generation.clone(),
            ),
            None => ("disabled".to_owned(), None, None),
        };
        let config_json = serde_json::to_string(configuration).unwrap_or_else(|_| "{}".into());
        let now = now_rfc3339();
        match &existing {
            None => {
                tx.execute(
                    "INSERT INTO v1_channels (channel_id, adapter_type, schema_id, scope_kind,
                        scope_id, configuration_json, revision, enabled, state, reason_code,
                        generation, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
                    params![
                        channel_id,
                        adapter_type,
                        schema_id,
                        scope_kind,
                        scope_id,
                        config_json,
                        revision,
                        if enabled { 1 } else { 0 },
                        state,
                        reason_code,
                        generation,
                        now,
                    ],
                )
                .map_err(|e| sql_err("insert channel in configure tx", e))?;
            }
            Some(prior) => {
                let changed = tx
                    .execute(
                        "UPDATE v1_channels
                         SET schema_id = ?2, configuration_json = ?3, revision = ?4,
                             enabled = ?5, state = ?6, reason_code = ?7, generation = ?8,
                             updated_at = ?9
                         WHERE channel_id = ?1 AND revision = ?10",
                        params![
                            channel_id,
                            schema_id,
                            config_json,
                            revision,
                            if enabled { 1 } else { 0 },
                            state,
                            reason_code,
                            generation,
                            now,
                            prior.revision,
                        ],
                    )
                    .map_err(|e| sql_err("update channel in configure tx", e))?;
                if changed != 1 {
                    return Err(revision_conflict(expected_revision, &prior.revision));
                }
            }
        }
        tx.commit()
            .map_err(|e| sql_err("commit configure cas tx", e))?;
        Ok((revision, generation))
    }

    /// Transitions channel runtime to `connecting` only if `generation IS ?expected_gen`.
    fn mark_channel_connecting_cas(
        &self,
        channel_id: &str,
        expected_gen: Option<&str>,
    ) -> Result<bool, ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin connecting cas tx", e))?;
        let changed = tx
            .execute(
                "UPDATE v1_channels
                 SET state = 'connecting', reason_code = 'ASSEMBLING', updated_at = ?2
                 WHERE channel_id = ?1 AND generation IS ?3",
                params![channel_id, now_rfc3339(), expected_gen],
            )
            .map_err(|e| sql_err("mark channel connecting", e))?;
        tx.commit()
            .map_err(|e| sql_err("commit connecting cas tx", e))?;
        Ok(changed == 1)
    }

    fn release_transition(&self, channel_id: &str, operation_id: &str) {
        let _ = self.with_conn("release channel transition", |conn| {
            conn.execute(
                "DELETE FROM v1_channel_transitions
                 WHERE channel_id = ?1 AND operation_id = ?2",
                params![channel_id, operation_id],
            )?;
            Ok(())
        });
    }

    /// Single-transaction CAS completion of `channels.transition`:
    /// checks `generation IS ?expected_gen`, prevents disabled or modified
    /// channels from being forced `ready`, bumps `gen_counter` + `snapshot_counter`,
    /// updates `v1_channels` conditionally, and releases `v1_channel_transitions`.
    #[allow(clippy::too_many_arguments)]
    fn finish_transition_cas(
        &self,
        channel_id: &str,
        expected_gen: Option<&str>,
        expected_config_revision: &str,
        action: &str,
        probed_state: &str,
        probed_reason: Option<&str>,
        operation_id: &str,
    ) -> Result<(String, Option<String>, String), ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin finish transition cas tx", e))?;
        let current = Self::channel_on_conn(&tx, channel_id)
            .map_err(|e| sql_err("read channel in finish transition tx", e))?;
        let Some(current) = current else {
            tx.execute(
                "DELETE FROM v1_channel_transitions WHERE channel_id = ?1 AND operation_id = ?2",
                params![channel_id, operation_id],
            )
            .map_err(|e| sql_err("release transition", e))?;
            tx.commit()
                .map_err(|e| sql_err("commit missing transition release", e))?;
            return Err(not_found("channel", channel_id));
        };
        if current.generation.as_deref() != expected_gen {
            tx.execute(
                "DELETE FROM v1_channel_transitions WHERE channel_id = ?1 AND operation_id = ?2",
                params![channel_id, operation_id],
            )
            .map_err(|e| sql_err("release transition", e))?;
            tx.commit()
                .map_err(|e| sql_err("commit conflict transition release", e))?;
            return Err(generation_conflict(expected_gen.unwrap_or("absent")));
        }

        let (final_state, final_reason): (String, Option<String>) = if action == "disable" {
            ("disabled".to_owned(), None)
        } else if !current.enabled {
            ("disabled".to_owned(), Some("CHANNEL_DISABLED".to_owned()))
        } else if current.revision != expected_config_revision {
            (
                "failed".to_owned(),
                Some("CONFIGURATION_CHANGED".to_owned()),
            )
        } else {
            (
                probed_state.to_owned(),
                probed_reason.map(ToOwned::to_owned),
            )
        };

        let new_generation = format!("gen-{:08}", Self::next_counter_in_tx(&tx, "gen_counter")?);
        let _snapshot = Self::next_counter_in_tx(&tx, "snapshot_counter")?;
        let changed = tx
            .execute(
                "UPDATE v1_channels
                 SET state = ?2, reason_code = ?3, generation = ?4, updated_at = ?5
                 WHERE channel_id = ?1 AND generation IS ?6",
                params![
                    channel_id,
                    final_state,
                    final_reason,
                    new_generation,
                    now_rfc3339(),
                    expected_gen,
                ],
            )
            .map_err(|e| sql_err("update channel runtime cas", e))?;
        if changed != 1 {
            return Err(generation_conflict(expected_gen.unwrap_or("absent")));
        }
        tx.execute(
            "DELETE FROM v1_channel_transitions WHERE channel_id = ?1 AND operation_id = ?2",
            params![channel_id, operation_id],
        )
        .map_err(|e| sql_err("release transition", e))?;
        tx.commit()
            .map_err(|e| sql_err("commit finish transition cas tx", e))?;
        Ok((final_state, final_reason, new_generation))
    }

    // ---- challenges ------------------------------------------------------

    fn challenge(&self, challenge_id: &str) -> Result<Option<ChallengeRow>, ContractError> {
        self.with_conn("read challenge", |conn| {
            conn.query_row(
                "SELECT challenge_id, channel_id, challenge_type, state, user_action_url,
                        qr_payload, user_code, expires_at, credential_ref, error_code
                 FROM v1_channel_challenges WHERE challenge_id = ?1",
                params![challenge_id],
                row_to_challenge,
            )
            .optional()
        })
    }

    /// Inserts a login challenge and pins its idempotency digest atomically
    /// inside ONE `BEGIN IMMEDIATE` transaction. If another caller raced to
    /// insert the same `idempotency_key` with the same `input_digest`, returns
    /// the existing challenge instead of failing on `UNIQUE`.
    fn insert_challenge_cas(
        &self,
        row: &ChallengeRow,
        key: Option<&str>,
        input_digest: &str,
    ) -> Result<ChallengeRow, ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin insert challenge tx", e))?;
        if let Some(k) = key {
            let existing: Option<(String, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, rejection_code FROM v1_channel_idempotency
                     WHERE method_id = 'channels.begin_login' AND idempotency_key = ?1",
                    params![k],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| sql_err("lookup begin_login idempotency", e))?;
            if let Some((stored_digest, rejection_code)) = existing {
                if stored_digest != input_digest {
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        "idempotencyKey already used for channels.begin_login with different input",
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if let Some(code) = rejection_code {
                    return Err(ContractError::new(
                        code,
                        "channels.begin_login request previously rejected definitively",
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if let Some(ch) = tx
                    .query_row(
                        "SELECT challenge_id, channel_id, challenge_type, state, user_action_url,
                                qr_payload, user_code, expires_at, credential_ref, error_code
                         FROM v1_channel_challenges WHERE idempotency_key = ?1",
                        params![k],
                        row_to_challenge,
                    )
                    .optional()
                    .map_err(|e| sql_err("read existing challenge", e))?
                {
                    return Ok(ch);
                }
            }
            let now = now_rfc3339();
            tx.execute(
                "INSERT OR IGNORE INTO v1_channel_idempotency
                    (method_id, idempotency_key, input_digest, response_json, rejection_code, created_at)
                 VALUES ('channels.begin_login', ?1, ?2, ?3, NULL, ?4)",
                params![
                    k,
                    input_digest,
                    serde_json::to_string(&begin_login_json(row, DEFAULT_POLL_AFTER_MS))
                        .unwrap_or_else(|_| "{}".into()),
                    now,
                ],
            )
            .map_err(|e| sql_err("record begin_login idempotency", e))?;
        }
        tx.execute(
            "INSERT INTO v1_channel_challenges (challenge_id, channel_id, challenge_type,
                state, user_action_url, qr_payload, user_code, expires_at, credential_ref,
                error_code, idempotency_key, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![
                row.challenge_id,
                row.channel_id,
                row.challenge_type,
                row.state,
                row.user_action_url,
                row.qr_payload,
                row.user_code,
                row.expires_at,
                row.credential_ref,
                row.error_code,
                key,
                now_rfc3339(),
            ],
        )
        .map_err(|e| sql_err("insert challenge", e))?;
        tx.commit()
            .map_err(|e| sql_err("commit insert challenge tx", e))?;
        Ok(row.clone())
    }

    /// Terminalizes a pending challenge; an already-terminal row (a racing
    /// cancel/poll won) is left untouched. Returns whether this call won.
    fn finish_challenge(
        &self,
        challenge_id: &str,
        state: &str,
        credential_ref: Option<&str>,
        error_code: Option<&str>,
    ) -> Result<bool, ContractError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin finish challenge tx", e))?;
        let changed = tx
            .execute(
                "UPDATE v1_channel_challenges
                 SET state = ?2, credential_ref = ?3, error_code = ?4, updated_at = ?5
                 WHERE challenge_id = ?1 AND state = 'pending'",
                params![
                    challenge_id,
                    state,
                    credential_ref,
                    error_code,
                    now_rfc3339()
                ],
            )
            .map_err(|e| sql_err("finish challenge", e))?;
        tx.commit()
            .map_err(|e| sql_err("commit finish challenge tx", e))?;
        Ok(changed == 1)
    }

    /// Atomically verifies `state == 'pending'` and non-expired, marks the
    /// challenge `'succeeded'`, AND persists the credential in `v1_credentials`
    /// all inside ONE `BEGIN IMMEDIATE` transaction. If `cancel_login` or
    /// expiry won first, returns `Ok(false)` without touching `v1_credentials`.
    fn complete_login_challenge(
        &self,
        challenge_id: &str,
        credential_id: &str,
        kind: &str,
        name: &str,
        secret: &str,
    ) -> Result<bool, ContractError> {
        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let fingerprint = format!("sha256:{}", &hash[..hash.len().min(16)]);

        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin complete login challenge tx", e))?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT state, expires_at FROM v1_channel_challenges WHERE challenge_id = ?1",
                params![challenge_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| sql_err("read challenge in complete tx", e))?;
        let Some((state, expires_at)) = row else {
            return Ok(false);
        };
        if state != "pending" {
            return Ok(false);
        }
        let now = now_rfc3339();
        if now >= expires_at {
            tx.execute(
                "UPDATE v1_channel_challenges
                 SET state = 'expired', updated_at = ?2
                 WHERE challenge_id = ?1 AND state = 'pending'",
                params![challenge_id, now],
            )
            .map_err(|e| sql_err("expire challenge in complete tx", e))?;
            tx.commit()
                .map_err(|e| sql_err("commit expired challenge tx", e))?;
            return Ok(false);
        }

        let changed = tx
            .execute(
                "UPDATE v1_channel_challenges
                 SET state = 'succeeded', credential_ref = ?2, error_code = NULL, updated_at = ?3
                 WHERE challenge_id = ?1 AND state = 'pending'",
                params![challenge_id, credential_id, now],
            )
            .map_err(|e| sql_err("win challenge in complete tx", e))?;
        if changed != 1 {
            return Ok(false);
        }

        let revision = Self::next_shared_meta_revision_in_tx(&tx)?;
        tx.execute(
            "INSERT INTO v1_credentials (credential_id, kind, name, secret, fingerprint,
                enabled, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)",
            params![
                credential_id,
                kind,
                name,
                secret,
                fingerprint,
                revision,
                now,
            ],
        )
        .map_err(|e| sql_err("insert channel credential", e))?;

        tx.commit()
            .map_err(|e| sql_err("commit complete login challenge tx", e))?;
        Ok(true)
    }

    // ---- sync idempotency (begin_login / poll_login / cancel_login) -------

    fn lookup_sync_idempotency(
        &self,
        method_id: &str,
        key: &str,
        input_digest: &str,
    ) -> Result<Option<Result<Value, ContractError>>, ContractError> {
        let row: Option<(String, Option<String>, Option<String>)> =
            self.with_conn("lookup sync idempotency", |conn| {
                conn.query_row(
                    "SELECT input_digest, response_json, rejection_code
                     FROM v1_channel_idempotency
                     WHERE method_id = ?1 AND idempotency_key = ?2",
                    params![method_id, key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
            })?;
        let Some((stored_digest, response_json, rejection_code)) = row else {
            return Ok(None);
        };
        if stored_digest != input_digest {
            return Err(ContractError::new(
                CODE_IDEMPOTENCY_CONFLICT,
                format!("idempotencyKey already used for {method_id} with different input"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if let Some(code) = rejection_code {
            return Ok(Some(Err(ContractError::new(
                code,
                format!("{method_id} request previously rejected definitively"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))));
        }
        if let Some(raw) = response_json {
            let val: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
            return Ok(Some(Ok(val)));
        }
        Ok(None)
    }

    fn record_sync_idempotency(
        &self,
        method_id: &str,
        key: &str,
        input_digest: &str,
        outcome: Result<&Value, &ContractError>,
    ) -> Result<(), ContractError> {
        let (response_json, rejection_code) = match outcome {
            Ok(val) => (
                Some(serde_json::to_string(val).unwrap_or_else(|_| "{}".into())),
                None,
            ),
            Err(err) if err.retry.mode == RetryMode::Never => (None, Some(err.code.clone())),
            Err(_) => return Ok(()),
        };
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sql_err("begin record sync idempotency tx", e))?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT input_digest FROM v1_channel_idempotency
                 WHERE method_id = ?1 AND idempotency_key = ?2",
                params![method_id, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_err("check existing sync idempotency", e))?;
        if let Some(stored) = existing {
            if stored != input_digest {
                return Err(ContractError::new(
                    CODE_IDEMPOTENCY_CONFLICT,
                    format!("idempotencyKey already used for {method_id} with different input"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            return Ok(());
        }
        tx.execute(
            "INSERT INTO v1_channel_idempotency
                (method_id, idempotency_key, input_digest, response_json, rejection_code, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                method_id,
                key,
                input_digest,
                response_json,
                rejection_code,
                now_rfc3339(),
            ],
        )
        .map_err(|e| sql_err("insert sync idempotency", e))?;
        tx.commit()
            .map_err(|e| sql_err("commit sync idempotency tx", e))?;
        Ok(())
    }

    /// The credential store's read seam (`V1Store::resolve_credential_secret`
    /// semantics: only an existing, enabled credential resolves).
    fn credential_secret(&self, credential_ref: &str) -> Result<Option<String>, ContractError> {
        self.with_conn("resolve channel credential", |conn| {
            conn.query_row(
                "SELECT secret FROM v1_credentials WHERE credential_id = ?1 AND enabled = 1",
                params![credential_ref],
                |r| r.get(0),
            )
            .optional()
        })
    }

    fn credential_exists(&self, credential_ref: &str) -> Result<bool, ContractError> {
        self.with_conn("check channel credential", |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM v1_credentials WHERE credential_id = ?1",
                    params![credential_ref],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
    }

    // ---- domain events (the shared events table) --------------------------

    /// The same placeholder→`cur-<seq>` pattern the core store uses, through
    /// this domain's connection.
    fn append_event(
        &self,
        resource_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), ContractError> {
        self.with_conn("append channel event", |conn| {
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
        })?;
        if let Some(notifier) = &self.notifier {
            notifier.notify_waiters();
        }
        Ok(())
    }
}

fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelRow> {
    let configuration_json: String = row.get(5)?;
    Ok(ChannelRow {
        channel_id: row.get(0)?,
        adapter_type: row.get(1)?,
        schema_id: row.get(2)?,
        scope_kind: row.get(3)?,
        scope_id: row.get(4)?,
        configuration: serde_json::from_str(&configuration_json).unwrap_or(Value::Null),
        revision: row.get(6)?,
        enabled: row.get::<_, i64>(7)? != 0,
        state: row.get(8)?,
        reason_code: row.get(9)?,
        generation: row.get(10)?,
    })
}

fn row_to_challenge(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChallengeRow> {
    Ok(ChallengeRow {
        challenge_id: row.get(0)?,
        channel_id: row.get(1)?,
        challenge_type: row.get(2)?,
        state: row.get(3)?,
        user_action_url: row.get(4)?,
        qr_payload: row.get(5)?,
        user_code: row.get(6)?,
        expires_at: row.get(7)?,
        credential_ref: row.get(8)?,
        error_code: row.get(9)?,
    })
}

// ---------------------------------------------------------------------------
// Async-operation records (same table and pattern as the schedules domain)
// ---------------------------------------------------------------------------

struct AcceptedOperation {
    operation_id: String,
    receipt: Value,
}

enum OperationLookup {
    Fresh,
    Replay(Value),
    Conflict,
    Rejected(String),
}

fn lookup_operation_in_tx(
    tx: &Transaction<'_>,
    method_id: &str,
    digest: &str,
    idempotency_key: Option<&str>,
) -> Result<OperationLookup, ContractError> {
    let Some(key) = idempotency_key else {
        return Ok(OperationLookup::Fresh);
    };
    let existing = tx
        .query_row(
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
        .map_err(|e| sql_err("lookup channels operation", e))?;
    match existing {
        None => Ok(OperationLookup::Fresh),
        Some((stored_digest, receipt_json, rejection)) => {
            if stored_digest != digest {
                return Ok(OperationLookup::Conflict);
            }
            if let Some(code) = rejection {
                return Ok(OperationLookup::Rejected(code));
            }
            let mut receipt: Value =
                serde_json::from_str(&receipt_json).unwrap_or_else(|_| json!({}));
            if let Some(obj) = receipt.as_object_mut() {
                obj.insert("deduplicated".into(), json!(true));
            }
            Ok(OperationLookup::Replay(receipt))
        }
    }
}

fn record_rejection_in_tx(
    tx: &Transaction<'_>,
    method_id: &str,
    digest: &str,
    idempotency_key: Option<&str>,
    code: &str,
) -> Result<(), ContractError> {
    let Some(key) = idempotency_key else {
        return Ok(());
    };
    let now = now_rfc3339();
    tx.execute(
        "INSERT OR REPLACE INTO v1_domain_operations
            (operation_id, method_id, idempotency_key, input_digest, state, receipt_json,
             completion_json, rejection_code, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'rejected', '{}', NULL, ?5, ?6, ?6)",
        params![
            format!("rejected-{method_id}-{key}"),
            method_id,
            key,
            digest,
            code,
            now
        ],
    )
    .map_err(|e| sql_err("record channels operation rejection", e))?;
    Ok(())
}

/// Atomic single-transaction accept flow of `channels.probe` and
/// `channels.transition`: idempotency lookup, pre-accept validation (against
/// the live `tx`), counter bump, and operation insert all run in ONE
/// `BEGIN IMMEDIATE` transaction.
fn accept_operation_cas<T>(
    db: &ChannelDb,
    method_id: &str,
    input: &Value,
    idempotency_key: Option<&str>,
    new_operation_id: impl FnOnce() -> String,
    validate: impl FnOnce(&Transaction<'_>, &str) -> Result<T, ContractError>,
) -> Result<(AcceptedOperation, Option<T>), ContractError> {
    let digest = compute_input_digest(method_id, input);
    let mut conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| sql_err("begin channels accept tx", e))?;
    match lookup_operation_in_tx(&tx, method_id, &digest, idempotency_key)? {
        OperationLookup::Replay(receipt) => {
            return Ok((
                AcceptedOperation {
                    operation_id: receipt["operationId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    receipt,
                },
                None,
            ));
        }
        OperationLookup::Conflict => {
            return Err(ContractError::new(
                CODE_IDEMPOTENCY_CONFLICT,
                format!("idempotencyKey already used for {method_id} with different input"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        OperationLookup::Rejected(code) => {
            return Err(ContractError::new(
                code,
                format!("{method_id} request previously rejected definitively"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        OperationLookup::Fresh => {}
    }

    let operation_id = new_operation_id();
    let validated = match validate(&tx, &operation_id) {
        Ok(val) => val,
        Err(error) => {
            if error.retry.mode == RetryMode::Never {
                record_rejection_in_tx(&tx, method_id, &digest, idempotency_key, &error.code)?;
                tx.commit()
                    .map_err(|e| sql_err("commit channels rejection tx", e))?;
            }
            return Err(error);
        }
    };

    let rev = ChannelDb::counter_in_tx(&tx, "rev_counter")?;
    let receipt = json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": format!("crev-{rev:08}"),
        "deduplicated": false,
        "state": "queued",
    });
    let now = now_rfc3339();
    tx.execute(
        "INSERT INTO v1_domain_operations
            (operation_id, method_id, idempotency_key, input_digest, state, receipt_json,
             completion_json, rejection_code, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'queued', ?5, NULL, NULL, ?6, ?6)",
        params![
            operation_id,
            method_id,
            idempotency_key,
            digest,
            serde_json::to_string(&receipt).unwrap_or_else(|_| "{}".into()),
            now,
        ],
    )
    .map_err(|e| sql_err("insert channels operation", e))?;
    tx.commit()
        .map_err(|e| sql_err("commit channels accept tx", e))?;
    Ok((
        AcceptedOperation {
            operation_id,
            receipt,
        },
        Some(validated),
    ))
}

fn mark_operation_running(db: &ChannelDb, operation_id: &str) -> Result<(), ContractError> {
    db.with_conn("mark channels operation running", |conn| {
        conn.execute(
            "UPDATE v1_domain_operations SET state = 'running', updated_at = ?2
             WHERE operation_id = ?1",
            params![operation_id, now_rfc3339()],
        )?;
        Ok(())
    })?;
    db.append_event(
        operation_id,
        "operation.started",
        &json!({"operationId": operation_id}),
    )
}

fn finish_operation(
    db: &ChannelDb,
    operation_id: &str,
    state: &str,
    completion: &Value,
) -> Result<(), ContractError> {
    db.with_conn("finish channels operation", |conn| {
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
        event,
        &json!({"operationId": operation_id, "result": completion}),
    )
}

// ---------------------------------------------------------------------------
// The per-dataRoot domain singleton
// ---------------------------------------------------------------------------

type SyncIdemLockMap = Mutex<HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>>;

pub(crate) struct ChannelsDomain {
    db: ChannelDb,
    /// Per-(method, key) async singleflight locks for sync idempotent login calls.
    sync_idem_locks: SyncIdemLockMap,
}

impl ChannelsDomain {
    fn sync_idem_lock(&self, method_id: &str, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .sync_idem_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.entry((method_id.to_owned(), key.to_owned()))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

struct TransitionGuard {
    domain: Arc<ChannelsDomain>,
    channel_id: String,
    operation_id: String,
    active: bool,
}

impl TransitionGuard {
    fn new(domain: Arc<ChannelsDomain>, channel_id: String, operation_id: String) -> Self {
        Self {
            domain,
            channel_id,
            operation_id,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TransitionGuard {
    fn drop(&mut self) {
        if self.active {
            self.domain
                .db
                .release_transition(&self.channel_id, &self.operation_id);
        }
    }
}

fn domains_registry() -> &'static Mutex<HashMap<PathBuf, Weak<ChannelsDomain>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<ChannelsDomain>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn domain_for(rt: &Arc<PublicV1Runtime>) -> Result<Arc<ChannelsDomain>, ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let domain = Arc::new(ChannelsDomain {
        db: ChannelDb::open_with_notifier(&home, Some(rt.store().event_notifier()))?,
        sync_idem_locks: Mutex::new(HashMap::new()),
    });
    registry.insert(home, Arc::downgrade(&domain));
    Ok(domain)
}

/// Test hygiene: forget the domain singleton of one dataRoot.
#[cfg(test)]
pub(crate) fn shutdown_for_tests(home: &std::path::Path) {
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.remove(home);
}

// ---------------------------------------------------------------------------
// The probe protocol (one HTTP exchange, classified)
// ---------------------------------------------------------------------------

/// The TS baseline feishu test's exchange budget (gateway.js:199).
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// One probe / login-stub HTTP exchange against a channel's
/// `probeEndpoint`. Transport failures carry the undici-style cause code.
async fn post_endpoint_json(
    endpoint: &str,
    body: &Value,
    bearer: Option<&str>,
) -> Result<(hyper::StatusCode, Option<Value>), String> {
    let uri: Uri = endpoint
        .parse()
        .map_err(|_| format!("probeEndpoint `{endpoint}` is not a valid URI"))?;
    if uri.scheme_str() != Some("http") && uri.scheme_str() != Some("https") {
        return Err(format!(
            "probeEndpoint `{endpoint}` must use the http or https scheme"
        ));
    }
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(secret) = bearer {
        // The bearer value is transport-only: it never reaches an error
        // message, an event payload, or a completion result.
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {secret}"));
    }
    let request = builder
        .body(Full::new(Bytes::from(payload)))
        .map_err(|e| format!("build probe request: {e}"))?;
    // Explicit no-proxy settings (reload semantics with an empty URL): the
    // probe must not be diverted through an ambient HTTP_PROXY.
    let transport = HyperModelTransport::with_proxy_settings(Some(&ProxySettings {
        url: String::new(),
        no_proxy: None,
        mode: ProxyInstallMode::Reload,
    }));
    // The whole exchange — connect and response headers — is bounded at the
    // TS baseline feishu test's 10 s budget (gateway.js:199).
    let response = tokio::time::timeout(EXCHANGE_TIMEOUT, transport.send(request))
        .await
        .map_err(|_| "probe transport failure: timed out (UND_ERR_HEADERS_TIMEOUT)".to_owned())?
        .map_err(|failure| {
            format!(
                "probe transport failure: {} ({})",
                failure.message,
                failure.cause_code.unwrap_or("UNKNOWN")
            )
        })?;
    let status = response.status();
    let bytes = Limited::new(response.into_body(), 65_536)
        .collect()
        .await
        .map_err(|_| "probe response body failed".to_owned())?
        .to_bytes();
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    Ok((status, parsed))
}

/// The probe classification of one exchange (合同 acceptance: DNS / 连接 /
/// 签名 / 认证 failures must be distinguishable).
struct ProbeOutcome {
    reachable: bool,
    authenticated: bool,
    diagnostics: Vec<(String, String)>,
}

async fn run_probe(endpoint: &str, secret: Option<&str>) -> ProbeOutcome {
    match post_endpoint_json(endpoint, &json!({"action": "probe"}), secret).await {
        Err(failure) => {
            let code = if failure.contains("ENOTFOUND") {
                "DNS_FAILURE"
            } else if failure.contains("UND_ERR_HEADERS_TIMEOUT") {
                "TIMEOUT"
            } else if failure.contains("is not a valid URI") || failure.contains("scheme") {
                "ENDPOINT_INVALID"
            } else {
                "CONN_FAILURE"
            };
            ProbeOutcome {
                reachable: false,
                authenticated: false,
                diagnostics: vec![(code.to_owned(), failure)],
            }
        }
        Ok((status, body)) => {
            let mut diagnostics = Vec::new();
            let authenticated = if secret.is_none() {
                diagnostics.push((
                    "NO_CREDENTIAL".to_owned(),
                    "the channel has no credentialRef to authenticate with".to_owned(),
                ));
                false
            } else if !status.is_success() {
                if status == hyper::StatusCode::UNAUTHORIZED
                    || status == hyper::StatusCode::FORBIDDEN
                {
                    diagnostics.push((
                        "AUTHENTICATION_FAILED".to_owned(),
                        format!("the endpoint rejected the credential (HTTP {status})"),
                    ));
                } else {
                    diagnostics.push((
                        "PROTOCOL_ERROR".to_owned(),
                        format!("the endpoint answered HTTP {status}"),
                    ));
                }
                false
            } else if let Some(code) = body
                .as_ref()
                .and_then(|b| b.get("code"))
                .and_then(Value::as_str)
            {
                match code {
                    "UNAUTHENTICATED" => {
                        diagnostics.push((
                            "AUTHENTICATION_FAILED".to_owned(),
                            "the endpoint rejected the credential".to_owned(),
                        ));
                        false
                    }
                    "BAD_SIGNATURE" | "INVALID_SIGNATURE" => {
                        diagnostics.push((
                            "SIGNATURE_INVALID".to_owned(),
                            "the endpoint rejected the request signature".to_owned(),
                        ));
                        false
                    }
                    other => {
                        diagnostics.push((
                            "PROTOCOL_ERROR".to_owned(),
                            format!("the endpoint reported code `{other}`"),
                        ));
                        false
                    }
                }
            } else {
                true
            };
            ProbeOutcome {
                reachable: true,
                authenticated,
                diagnostics,
            }
        }
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

/// `channels.list` — the per-scope view: configured rows plus (by default)
/// one synthetic `unconfigured` row per adapter type with no instance in
/// the scope. `supportedAdapterTypes` is the 19-type registry.
async fn channels_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let scope = parse_v1_scope(&rt, &input)?;
    let include_unconfigured = input
        .get("includeUnconfigured")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let rows = domain.db.channels_for_scope(&scope)?;
    let snapshot_generation = domain.db.current_snapshot_generation()?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(channel_json(row));
    }
    if include_unconfigured {
        let configured: Vec<String> = rows.iter().map(|row| row.adapter_type.clone()).collect();
        for adapter_type in supported_adapter_types() {
            if !configured.contains(&adapter_type) {
                items.push(unconfigured_channel_json(&adapter_type));
            }
        }
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "channels": items,
            "supportedAdapterTypes": supported_adapter_types(),
            "generation": snapshot_generation,
        }),
    ))
}

/// `channels.list`'s `runtime` view of one row: `available` is exactly the
/// ready state (「禁用…不伪报在线」), `reasonCode` names why not.
fn runtime_json(row: &ChannelRow) -> Value {
    let available = row.enabled && row.state == "ready";
    let reason_code = if available {
        None
    } else if !row.enabled {
        Some("CHANNEL_DISABLED".to_owned())
    } else {
        Some(match row.state.as_str() {
            "connecting" => "ASSEMBLING".to_owned(),
            "failed" => row
                .reason_code
                .clone()
                .unwrap_or_else(|| "ASSEMBLY_FAILED".to_owned()),
            _ => "NOT_ASSEMBLED".to_owned(),
        })
    };
    json!({
        "state": row.state,
        "available": available,
        "reasonCode": reason_code,
        "generation": row.generation,
    })
}

fn channel_json(row: &ChannelRow) -> Value {
    json!({
        "channelId": row.channel_id,
        "adapterType": row.adapter_type,
        "schemaId": row.schema_id,
        "revision": row.revision,
        "enabled": row.enabled,
        "runtime": runtime_json(row),
        // The stored configuration carries no plaintext secret (references
        // only), so it is already the redacted form.
        "configuration": row.configuration,
    })
}

fn unconfigured_channel_json(adapter_type: &str) -> Value {
    json!({
        "channelId": adapter_type,
        "adapterType": adapter_type,
        "schemaId": channel_schema_id(adapter_type),
        "revision": "absent",
        "enabled": false,
        "runtime": {
            "state": "unconfigured",
            "available": false,
            "reasonCode": "NOT_CONFIGURED",
            "generation": Value::Null,
        },
        "configuration": {},
    })
}

/// `channels.validate` — the pure configuration check. Adapter/schema
/// mismatches and field problems answer as diagnostics (`valid: false`),
/// not as contract errors: the operation succeeded, the candidate did not.
async fn channels_validate(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let _scope = parse_v1_scope(&rt, &input)?;
    let adapter_type = str_field(&input, "adapterType")?;
    let schema_id = str_field(&input, "schemaId")?;
    let configuration = input
        .get("configuration")
        .cloned()
        .ok_or_else(|| invalid_arg("`configuration` is required"))?;

    let mut diagnostics = Vec::new();
    if !supported_adapter_types().contains(&adapter_type) {
        diagnostics.push(Diagnostic::error(
            "ADAPTER_UNKNOWN",
            Some("$.adapterType".to_owned()),
            format!("adapterType `{adapter_type}` is not a known adapter type"),
        ));
    } else if schema_id != channel_schema_id(&adapter_type) {
        diagnostics.push(Diagnostic::error(
            "SCHEMA_MISMATCH",
            Some("$.schemaId".to_owned()),
            format!(
                "schemaId must be `{}` for adapterType `{adapter_type}`",
                channel_schema_id(&adapter_type)
            ),
        ));
    } else {
        diagnostics.extend(validate_configuration(
            &adapter_type,
            &configuration,
            |id| domain.db.credential_exists(id),
        ));
    }
    let valid = diagnostics.is_empty();
    let diagnostics: Vec<Value> = diagnostics.iter().map(Diagnostic::to_json).collect();
    Ok((
        ResponseOutcome::Succeeded,
        json!({"valid": valid, "diagnostics": diagnostics}),
    ))
}

/// `channels.configure` — the atomic conditional write. An invalid
/// candidate never overwrites the stored row (「非法候选不覆盖旧配置」),
/// and saving never touches the runtime state beyond reflecting a
/// disabled configuration (「保存和在线状态独立」).
async fn channels_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let scope = parse_v1_scope(&rt, &input)?;
    let channel_id = str_field(&input, "channelId")?;
    let adapter_type = str_field(&input, "adapterType")?;
    let schema_id = str_field(&input, "schemaId")?;
    let expected_revision = str_field(&input, "expectedRevision")?;
    let configuration = input
        .get("configuration")
        .cloned()
        .ok_or_else(|| invalid_arg("`configuration` is required"))?;

    if !supported_adapter_types().contains(&adapter_type) {
        return Err(invalid_arg(format!(
            "adapterType `{adapter_type}` is not a known adapter type"
        ))
        .into());
    }
    if schema_id != channel_schema_id(&adapter_type) {
        return Err(invalid_arg(format!(
            "schemaId must be `{}` for adapterType `{adapter_type}`",
            channel_schema_id(&adapter_type)
        ))
        .into());
    }
    let diagnostics = validate_configuration(&adapter_type, &configuration, |id| {
        domain.db.credential_exists(id)
    });
    if let Some(first) = diagnostics.first() {
        // The full diagnostic list rides the message; no configuration
        // value is included.
        return Err(invalid_arg(format!(
            "configuration rejected: {} {} ({})",
            first.code,
            first.path.as_deref().unwrap_or("$"),
            first.message
        ))
        .into());
    }

    let (scope_kind, scope_id) = scope.storage_parts();
    let (revision, generation) = domain.db.configure_channel_cas(
        &channel_id,
        &adapter_type,
        &schema_id,
        scope_kind,
        &scope_id,
        &configuration,
        &expected_revision,
    )?;
    let _ = domain.db.append_event(
        &channel_id,
        "channel.configurationChanged",
        &json!({"channelId": channel_id, "revision": revision}),
    );
    Ok((
        ResponseOutcome::Succeeded,
        json!({"revision": revision, "applied": true, "generation": generation}),
    ))
}

/// `channels.probe` — async. One classified exchange against the channel's
/// `probeEndpoint`; the stored channel row is untouched (a probe is a
/// test, not an assembly).
async fn channels_probe(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let scope = parse_v1_scope(&rt, &input)?;
    let channel_id = str_field(&input, "channelId")?;
    let expected_revision = str_field(&input, "expectedRevision")?;

    let (operation, validated) = accept_operation_cas(
        &domain.db,
        "channels.probe",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        |tx, _op_id| {
            let row = ChannelDb::channel_on_conn(tx, &channel_id)
                .map_err(|e| sql_err("read channel in probe tx", e))?;
            let Some(row) = row else {
                return Err(not_found("channel", &channel_id));
            };
            if !scope.matches_row(&row.scope_kind, &row.scope_id) {
                return Err(not_found("channel in scope", &channel_id));
            }
            if expected_revision != row.revision {
                return Err(revision_conflict(&expected_revision, &row.revision));
            }
            if probe_endpoint_of(&row).is_none() {
                return Err(invalid_arg(format!(
                    "channel `{channel_id}` has no probeEndpoint configured"
                )));
            }
            Ok(row)
        },
    )?;
    let Some(row) = validated else {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    };
    let runner = domain.clone();
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        let _ = mark_operation_running(&runner.db, &operation_id);
        let endpoint = probe_endpoint_of(&row).unwrap_or_default();
        let secret =
            credential_ref_of(&row).and_then(|id| runner.db.credential_secret(&id).ok().flatten());
        let outcome = run_probe(&endpoint, secret.as_deref()).await;
        let diagnostics: Vec<Value> = outcome
            .diagnostics
            .iter()
            .map(|(code, message)| json!({"code": code, "message": message}))
            .collect();
        let completion = json!({
            "channelId": row.channel_id,
            "adapterType": row.adapter_type,
            "reachable": outcome.reachable,
            "authenticated": outcome.authenticated,
            "diagnostics": diagnostics,
        });
        let _ = finish_operation(&runner.db, &operation_id, "succeeded", &completion);
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

fn probe_endpoint_of(row: &ChannelRow) -> Option<String> {
    row.configuration
        .get("probeEndpoint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn credential_ref_of(row: &ChannelRow) -> Option<String> {
    row.configuration
        .get("credentialRef")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// `channels.transition` — async. The assembly gate: `enable`/`reload`
/// re-validate the stored configuration (requiring `enabled == true`) and run
/// the probe when the configuration names one (only a real assembly reports
/// `ready` — 「渠道状态达到ready必须完成真实装配」); `disable` drains the old generation.
async fn channels_transition(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let scope = parse_v1_scope(&rt, &input)?;
    let channel_id = str_field(&input, "channelId")?;
    let action = str_field(&input, "action")?;
    let expected_generation = str_field(&input, "expectedGeneration")?;

    let (operation, validated) = accept_operation_cas(
        &domain.db,
        "channels.transition",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        |tx, op_id| {
            let row = ChannelDb::channel_on_conn(tx, &channel_id)
                .map_err(|e| sql_err("read channel in transition tx", e))?;
            let Some(row) = row else {
                return Err(not_found("channel", &channel_id));
            };
            if !scope.matches_row(&row.scope_kind, &row.scope_id) {
                return Err(not_found("channel in scope", &channel_id));
            }
            let current = row
                .generation
                .clone()
                .unwrap_or_else(|| "absent".to_owned());
            if expected_generation != current {
                return Err(generation_conflict(&expected_generation));
            }
            if !matches!(action.as_str(), "enable" | "disable" | "reload") {
                return Err(invalid_arg(format!("unknown action `{action}`")));
            }
            if action != "disable" && !row.enabled {
                return Err(invalid_arg(format!(
                    "channel `{channel_id}` is disabled in configuration; enable it before transition"
                )));
            }
            let inflight: Option<String> = tx
                .query_row(
                    "SELECT t.operation_id
                     FROM v1_channel_transitions t
                     JOIN v1_domain_operations o ON o.operation_id = t.operation_id
                     WHERE t.channel_id = ?1 AND o.state IN ('queued', 'running')",
                    params![channel_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| sql_err("check inflight channel transition", e))?;
            if inflight.is_some() {
                return Err(generation_conflict(&expected_generation));
            }
            tx.execute(
                "INSERT OR REPLACE INTO v1_channel_transitions (channel_id, operation_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params![channel_id, op_id, now_rfc3339()],
            )
            .map_err(|e| sql_err("claim channel transition", e))?;
            Ok(row)
        },
    )?;
    let Some(row) = validated else {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    };
    let runner = domain.clone();
    let operation_id = operation.operation_id.clone();
    let drained_generation = row.generation.clone();
    tokio::spawn(async move {
        let mut guard =
            TransitionGuard::new(runner.clone(), row.channel_id.clone(), operation_id.clone());
        let _ = mark_operation_running(&runner.db, &operation_id);
        if action != "disable" {
            // The assembling intermediate state: the old generation keeps
            // running until the new one takes over (the drain window).
            match runner
                .db
                .mark_channel_connecting_cas(&row.channel_id, row.generation.as_deref())
            {
                Ok(true) => {}
                Ok(false) => {
                    runner.db.release_transition(&row.channel_id, &operation_id);
                    guard.disarm();
                    let _ = finish_operation(
                        &runner.db,
                        &operation_id,
                        "failed",
                        &json!({
                            "code": "GENERATION_CONFLICT",
                            "message": "channel generation changed before transition assembly started",
                            "channelId": row.channel_id,
                        }),
                    );
                    return;
                }
                Err(err) => {
                    runner.db.release_transition(&row.channel_id, &operation_id);
                    guard.disarm();
                    let _ = finish_operation(
                        &runner.db,
                        &operation_id,
                        "failed",
                        &json!({
                            "code": err.code,
                            "message": err.message,
                            "channelId": row.channel_id,
                        }),
                    );
                    return;
                }
            }
        }
        let (probed_state, probed_reason) = if action == "disable" {
            ("disabled".to_owned(), None)
        } else {
            // The assembly check: the stored configuration must still
            // validate, and — when it names a probeEndpoint — the endpoint
            // must answer and accept the credential.
            let diagnostics = validate_configuration(&row.adapter_type, &row.configuration, |id| {
                runner.db.credential_exists(id)
            });
            if diagnostics.is_empty() {
                if let Some(endpoint) = probe_endpoint_of(&row) {
                    let secret = credential_ref_of(&row)
                        .and_then(|id| runner.db.credential_secret(&id).ok().flatten());
                    let outcome = run_probe(&endpoint, secret.as_deref()).await;
                    if outcome.reachable && outcome.authenticated {
                        ("ready".to_owned(), None)
                    } else {
                        let reason = outcome
                            .diagnostics
                            .first()
                            .map(|(code, _)| code.clone())
                            .unwrap_or_else(|| "ASSEMBLY_FAILED".to_owned());
                        ("failed".to_owned(), Some(reason))
                    }
                } else {
                    // No probe seam configured: the assembly surface is the
                    // enabled configuration itself (v1 CLI host, report-registered).
                    ("ready".to_owned(), None)
                }
            } else {
                (
                    "failed".to_owned(),
                    Some("CONFIGURATION_INVALID".to_owned()),
                )
            }
        };
        let cas_result = runner.db.finish_transition_cas(
            &row.channel_id,
            row.generation.as_deref(),
            &row.revision,
            &action,
            &probed_state,
            probed_reason.as_deref(),
            &operation_id,
        );
        guard.disarm();
        match cas_result {
            Ok((final_state, _final_reason, generation)) => {
                let completion = json!({
                    "channelId": row.channel_id,
                    "state": final_state,
                    "generation": generation,
                    "drainedGeneration": drained_generation,
                });
                let _ = finish_operation(&runner.db, &operation_id, "succeeded", &completion);
            }
            Err(err) => {
                let _ = finish_operation(
                    &runner.db,
                    &operation_id,
                    "failed",
                    &json!({
                        "code": err.code,
                        "message": err.message,
                        "channelId": row.channel_id,
                    }),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// `channels.begin_login` — sync. Creates one addressable challenge; the
/// same idempotency key replays the original challenge (singleflight + atomic CAS).
async fn channels_begin_login(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let _idem_lock = key
        .as_deref()
        .map(|k| domain.sync_idem_lock("channels.begin_login", k));
    let _idem_guard = if let Some(lock) = &_idem_lock {
        Some(lock.lock().await)
    } else {
        None
    };
    let digest = compute_input_digest("channels.begin_login", &input);
    if let Some(k) = key.as_deref()
        && let Some(cached) =
            domain
                .db
                .lookup_sync_idempotency("channels.begin_login", k, &digest)?
    {
        return match cached {
            Ok(val) => Ok((ResponseOutcome::Succeeded, val)),
            Err(err) => Err(err.into()),
        };
    }

    let result: Result<Value, ContractError> = async {
        let scope = parse_v1_scope(&rt, &input)?;
        let channel_id = str_field(&input, "channelId")?;

        let Some(row) = domain.db.channel(&channel_id)? else {
            return Err(not_found("channel", &channel_id));
        };
        if !scope.matches_row(&row.scope_kind, &row.scope_id) {
            return Err(not_found("channel in scope", &channel_id));
        }
        let Some(challenge_type) = login_challenge_type(&row.adapter_type) else {
            return Err(invalid_arg(format!(
                "adapter type `{}` does not support interactive login",
                row.adapter_type
            )));
        };

        let challenge_id = rt.store().next_id();
        let expires_at =
            (chrono::Utc::now() + chrono::Duration::seconds(LOGIN_TTL_SECONDS)).to_rfc3339();
        let mut challenge = ChallengeRow {
            challenge_id: challenge_id.clone(),
            channel_id: channel_id.clone(),
            challenge_type: challenge_type.to_owned(),
            state: "pending".to_owned(),
            user_action_url: None,
            qr_payload: None,
            user_code: None,
            expires_at,
            credential_ref: None,
            error_code: None,
        };
        // The external exchange goes through the same probeEndpoint protocol
        // stub (`action=begin`); every presentation field the stub returns
        // replaces the local default.
        if let Some(endpoint) = probe_endpoint_of(&row) {
            let body = json!({
                "action": "begin",
                "channelId": channel_id,
                "challengeId": challenge_id,
                "challengeType": challenge_type,
            });
            match post_endpoint_json(&endpoint, &body, None).await {
                Ok((status, Some(payload))) if status.is_success() => {
                    if let Some(url) = payload.get("userActionUrl").and_then(Value::as_str) {
                        challenge.user_action_url = Some(url.to_owned());
                    }
                    if let Some(qr) = payload.get("qrPayload").and_then(Value::as_str) {
                        challenge.qr_payload = Some(qr.to_owned());
                    }
                    if let Some(code) = payload.get("userCode").and_then(Value::as_str) {
                        challenge.user_code = Some(code.to_owned());
                    }
                    if let Some(expires) = payload.get("expiresAt").and_then(Value::as_str) {
                        challenge.expires_at = expires.to_owned();
                    }
                }
                Ok((status, _)) => {
                    return Err(upstream_error(format!(
                        "login endpoint answered HTTP {status}"
                    )));
                }
                Err(failure) => {
                    return Err(upstream_error(failure));
                }
            }
        }
        let stored = domain
            .db
            .insert_challenge_cas(&challenge, key.as_deref(), &digest)?;
        if stored.challenge_id == challenge.challenge_id {
            let _ = domain.db.append_event(
                &challenge_id,
                "channel.loginPending",
                &json!({
                    "channelId": channel_id,
                    "challengeId": challenge_id,
                    "challengeType": challenge_type,
                }),
            );
        }
        Ok(begin_login_json(&stored, DEFAULT_POLL_AFTER_MS))
    }
    .await;

    match result {
        Ok(val) => Ok((ResponseOutcome::Succeeded, val)),
        Err(err) => {
            if let Some(k) = key.as_deref() {
                let _ = domain.db.record_sync_idempotency(
                    "channels.begin_login",
                    k,
                    &digest,
                    Err(&err),
                );
            }
            Err(err.into())
        }
    }
}

/// The baseline login windows: the feishu device-code flow's ~5-minute
/// verification window (verification_uri TTL), applied to every mode as
/// the default expiry.
const LOGIN_TTL_SECONDS: i64 = 300;
const DEFAULT_POLL_AFTER_MS: i64 = 1000;

fn begin_login_json(challenge: &ChallengeRow, poll_after_ms: i64) -> Value {
    json!({
        "challengeId": challenge.challenge_id,
        "challengeType": challenge.challenge_type,
        "userActionUrl": challenge.user_action_url,
        "qrPayload": challenge.qr_payload,
        "userCode": challenge.user_code,
        "expiresAt": challenge.expires_at,
        "pollAfterMs": poll_after_ms,
    })
}

fn poll_login_json(challenge: &ChallengeRow, poll_after_ms: i64) -> Value {
    json!({
        "challengeId": challenge.challenge_id,
        "state": challenge.state,
        "pollAfterMs": poll_after_ms,
        "credentialRef": challenge.credential_ref,
        "errorCode": challenge.error_code,
    })
}

/// `channels.poll_login` — sync. Advances one challenge; terminal states
/// are immutable and replaying them never re-registers (重复轮询不重复
/// 注册). A `succeeded` answer requires the challenge to still be `pending`
/// and non-expired and persists the credential inside the SAME atomic tx.
async fn channels_poll_login(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let _idem_lock = key
        .as_deref()
        .map(|k| domain.sync_idem_lock("channels.poll_login", k));
    let _idem_guard = if let Some(lock) = &_idem_lock {
        Some(lock.lock().await)
    } else {
        None
    };
    let digest = compute_input_digest("channels.poll_login", &input);
    if let Some(k) = key.as_deref()
        && let Some(cached) =
            domain
                .db
                .lookup_sync_idempotency("channels.poll_login", k, &digest)?
    {
        return match cached {
            Ok(val) => Ok((ResponseOutcome::Succeeded, val)),
            Err(err) => Err(err.into()),
        };
    }

    let result: Result<Value, ContractError> = async {
        let scope = parse_v1_scope(&rt, &input)?;
        let channel_id = str_field(&input, "channelId")?;
        let challenge_id = str_field(&input, "challengeId")?;

        let Some(challenge) = domain.db.challenge(&challenge_id)? else {
            return Err(not_found("challenge", &challenge_id));
        };
        if challenge.channel_id != channel_id {
            return Err(not_found("challenge for channel", &challenge_id));
        }
        let Some(row) = domain.db.channel(&channel_id)? else {
            return Err(not_found("channel", &channel_id));
        };
        if !scope.matches_row(&row.scope_kind, &row.scope_id) {
            return Err(not_found("channel in scope", &channel_id));
        }

        let mut won_succeeded = false;
        let mut won_failed = false;

        if challenge.state == "pending" {
            if chrono::Utc::now().to_rfc3339() >= challenge.expires_at {
                let _ = domain
                    .db
                    .finish_challenge(&challenge_id, "expired", None, None)?;
            } else if let Some(endpoint) = probe_endpoint_of(&row) {
                let body = json!({
                    "action": "poll",
                    "channelId": channel_id,
                    "challengeId": challenge_id,
                });
                match post_endpoint_json(&endpoint, &body, None).await {
                    Ok((status, Some(payload))) if status.is_success() => {
                        match payload.get("state").and_then(Value::as_str) {
                            Some("succeeded") => {
                                let credential = payload
                                    .get("credential")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                if credential.is_empty() {
                                    won_failed = domain.db.finish_challenge(
                                        &challenge_id,
                                        "failed",
                                        None,
                                        Some("CREDENTIAL_MISSING"),
                                    )?;
                                } else {
                                    // Atomically CAS challenge `pending -> succeeded`
                                    // AND persist the credential in ONE tx so a late
                                    // callback after `cancel_login` cannot write credentials.
                                    let credential_id =
                                        format!("channel-credential-{challenge_id}");
                                    match domain.db.complete_login_challenge(
                                        &challenge_id,
                                        &credential_id,
                                        &format!("channel.{}", row.adapter_type),
                                        &format!("channel:{channel_id}"),
                                        credential,
                                    ) {
                                        Ok(won) => {
                                            won_succeeded = won;
                                        }
                                        Err(error) => {
                                            let _ = domain.db.finish_challenge(
                                                &challenge_id,
                                                "failed",
                                                None,
                                                Some("CREDENTIAL_WRITE_FAILED"),
                                            );
                                            return Err(error);
                                        }
                                    }
                                }
                            }
                            Some("expired") => {
                                let _ = domain.db.finish_challenge(
                                    &challenge_id,
                                    "expired",
                                    None,
                                    None,
                                )?;
                            }
                            Some("failed") => {
                                let code = payload
                                    .get("errorCode")
                                    .and_then(Value::as_str)
                                    .unwrap_or("LOGIN_FAILED");
                                won_failed = domain.db.finish_challenge(
                                    &challenge_id,
                                    "failed",
                                    None,
                                    Some(code),
                                )?;
                            }
                            // pending (or absent): nothing to advance.
                            _ => {}
                        }
                    }
                    Ok((status, _)) => {
                        return Err(upstream_error(format!(
                            "login endpoint answered HTTP {status}"
                        )));
                    }
                    Err(failure) => {
                        return Err(upstream_error(failure));
                    }
                }
            }
        }

        let challenge = domain.db.challenge(&challenge_id)?.unwrap_or(challenge);
        if won_succeeded {
            let _ = domain.db.append_event(
                &challenge_id,
                "channel.loginSucceeded",
                &json!({"channelId": channel_id, "challengeId": challenge_id}),
            );
        } else if won_failed {
            let _ = domain.db.append_event(
                &challenge_id,
                "channel.loginFailed",
                &json!({"channelId": channel_id, "challengeId": challenge_id}),
            );
        }
        Ok(poll_login_json(&challenge, DEFAULT_POLL_AFTER_MS))
    }
    .await;

    if let Some(k) = key.as_deref() {
        let _ =
            domain
                .db
                .record_sync_idempotency("channels.poll_login", k, &digest, result.as_ref());
    }
    match result {
        Ok(val) => Ok((ResponseOutcome::Succeeded, val)),
        Err(err) => Err(err.into()),
    }
}

/// `channels.cancel_login` — sync. Cancels a pending challenge (a late
/// succeeded callback can no longer install credentials — the terminal
/// state is immutable); an already-terminal challenge reports itself with
/// `cancelled: false`.
async fn channels_cancel_login(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let _idem_lock = key
        .as_deref()
        .map(|k| domain.sync_idem_lock("channels.cancel_login", k));
    let _idem_guard = if let Some(lock) = &_idem_lock {
        Some(lock.lock().await)
    } else {
        None
    };
    let digest = compute_input_digest("channels.cancel_login", &input);
    if let Some(k) = key.as_deref()
        && let Some(cached) =
            domain
                .db
                .lookup_sync_idempotency("channels.cancel_login", k, &digest)?
    {
        return match cached {
            Ok(val) => Ok((ResponseOutcome::Succeeded, val)),
            Err(err) => Err(err.into()),
        };
    }

    let result: Result<Value, ContractError> = (|| {
        let scope = parse_v1_scope(&rt, &input)?;
        let channel_id = str_field(&input, "channelId")?;
        let challenge_id = str_field(&input, "challengeId")?;

        let Some(mut challenge) = domain.db.challenge(&challenge_id)? else {
            return Err(not_found("challenge", &challenge_id));
        };
        if challenge.channel_id != channel_id {
            return Err(not_found("challenge for channel", &challenge_id));
        }
        if let Some(row) = domain.db.channel(&channel_id)?
            && !scope.matches_row(&row.scope_kind, &row.scope_id)
        {
            return Err(not_found("channel in scope", &channel_id));
        }
        let cancelled = if challenge.state == "pending" {
            let won = domain
                .db
                .finish_challenge(&challenge_id, "cancelled", None, None)?;
            if won {
                challenge.state = "cancelled".to_owned();
                true
            } else {
                challenge = domain.db.challenge(&challenge_id)?.unwrap_or(challenge);
                false
            }
        } else {
            false
        };
        if cancelled {
            let _ = domain.db.append_event(
                &challenge_id,
                "channel.loginCancelled",
                &json!({"channelId": channel_id, "challengeId": challenge_id}),
            );
        }
        Ok(json!({
            "challengeId": challenge_id,
            "state": challenge.state,
            "cancelled": cancelled,
        }))
    })();

    if let Some(k) = key.as_deref() {
        let _ =
            domain
                .db
                .record_sync_idempotency("channels.cancel_login", k, &digest, result.as_ref());
    }
    match result {
        Ok(val) => Ok((ResponseOutcome::Succeeded, val)),
        Err(err) => Err(err.into()),
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
    use std::time::{Duration, Instant};

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

    async fn setup_project(fx: &Fixture) {
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": fx.instance_id,
                    "projectId": "proj-1",
                    "name": "Channels",
                    "environmentId": "local",
                    "root": {"instanceId": fx.instance_id, "rootId": "root-1", "relativePath": "w"}
                }),
                None,
                None,
            )
            .await
            .unwrap();
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> ContractError {
        dispatch_err_keyed(rt, method, input, "key-err").await
    }

    /// Like [`dispatch_err`] but passes an explicit idempotency key (the
    /// `retry=idempotentSubmit` operations reject a missing key at the
    /// dispatch gate before reaching the domain).
    async fn dispatch_err_keyed(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: &str,
    ) -> ContractError {
        match rt.dispatch(method, input, Some(key), None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    /// A minimal HTTP/1.1 protocol stub on loopback: every connection
    /// consumes the next scripted `(status, json_body)` answer; once the
    /// script is exhausted it answers `200 {"ok":true}`. Requests are read
    /// fully (headers + Content-Length body) before answering.
    async fn spawn_stub(responses: Vec<(u16, String)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // The script answers head-first (theVec::pop tail would invert it).
        let mut responses = responses;
        responses.reverse();
        let script = std::sync::Arc::new(tokio::sync::Mutex::new(responses));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let script = script.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = [0u8; 8192];
                    let mut seen = Vec::new();
                    let mut content_length = 0usize;
                    // Read until the end of the request (headers, then body).
                    loop {
                        let n = match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        seen.extend_from_slice(&buffer[..n]);
                        let text = String::from_utf8_lossy(&seen);
                        if let Some(header_end) = text.find("\r\n\r\n") {
                            let headers = &text[..header_end];
                            if let Some(value) = headers.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            }) {
                                content_length = value;
                            }
                            if seen.len() >= header_end + 4 + content_length {
                                break;
                            }
                        }
                        if seen.len() > 1 << 20 {
                            return;
                        }
                    }
                    let (status, body) = script
                        .lock()
                        .await
                        .pop()
                        .unwrap_or((200, "{\"ok\":true}".to_owned()));
                    let answer = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn configure_input(adapter_type: &str, configuration: Value) -> Value {
        json!({
            "scope": {"kind": "global"},
            "channelId": format!("ch-{adapter_type}"),
            "adapterType": adapter_type,
            "schemaId": channel_schema_id(adapter_type),
            "configuration": configuration,
            "expectedRevision": "absent",
        })
    }

    /// Polls `operations.get` until the operation is terminal and returns
    /// its `result` (the completion). The contract's `Operation` object has
    /// no `terminal` property — terminality is the `state` enum
    /// (`$defs.Operation.state`, `additionalProperties: false`), exactly
    /// like `tools.rs`'s wait helper reads it.
    async fn wait_operation(fx: &Fixture, operation_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, got) = fx
                .rt
                .dispatch(
                    "operations.get",
                    json!({
                        "instanceId": fx.instance_id,
                        "operationId": operation_id,
                    }),
                    None,
                    None,
                )
                .await
                .unwrap();
            let operation = got["operation"].clone();
            let state = operation["state"].as_str().unwrap_or_default().to_owned();
            if matches!(
                state.as_str(),
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return operation["result"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "operation `{operation_id}` never became terminal (state `{state}`)"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn validate_reports_field_and_credential_diagnostics() {
        let fx = fixture("ch-validate").await;
        setup_project(&fx).await;

        // A clean feishu candidate validates.
        let (_, valid) = fx
            .rt
            .dispatch(
                "channels.validate",
                json!({
                    "scope": {"kind": "global"},
                    "adapterType": "feishu",
                    "schemaId": channel_schema_id("feishu"),
                    "configuration": {"enabled": true, "appId": "cli-a"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(valid["valid"], true);
        assert_eq!(valid["diagnostics"].as_array().map(Vec::len), Some(0));

        // Unknown field, missing `enabled`, bad enum, dangling
        // credentialRef — all in one candidate, all reported.
        let (_, invalid) = fx
            .rt
            .dispatch(
                "channels.validate",
                json!({
                    "scope": {"kind": "global"},
                    "adapterType": "feishu",
                    "schemaId": channel_schema_id("feishu"),
                    "configuration": {
                        "appId": 7,
                        "method": "/bin/sh",
                        "connectionMode": "carrier-pigeon",
                        "credentialRef": "cred-none",
                    },
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(invalid["valid"], false);
        let codes: Vec<String> = invalid["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["code"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(codes.contains(&"MISSING_FIELD".to_owned()), "{codes:?}");
        assert!(codes.contains(&"UNKNOWN_FIELD".to_owned()), "{codes:?}");
        assert!(codes.contains(&"TYPE_MISMATCH".to_owned()), "{codes:?}");
        assert!(codes.contains(&"INVALID_VALUE".to_owned()), "{codes:?}");
        assert!(
            codes.contains(&"CREDENTIAL_NOT_FOUND".to_owned()),
            "{codes:?}"
        );
        // The schema-mismatch diagnostic (same shape as the TS save
        // validation reporting).
        let (_, mismatch) = fx
            .rt
            .dispatch(
                "channels.validate",
                json!({
                    "scope": {"kind": "global"},
                    "adapterType": "feishu",
                    "schemaId": "agentcpm-pd.channel.feishu.config/9",
                    "configuration": {"enabled": true},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(mismatch["valid"], false);
        assert_eq!(mismatch["diagnostics"][0]["code"], "SCHEMA_MISMATCH");
    }

    #[tokio::test]
    async fn configure_list_round_trip_and_revision_gate() {
        let fx = fixture("ch-configure").await;
        setup_project(&fx).await;

        // `configure` resolves `credentialRef` against the credential store
        // (「凭据只接受credentialRef」, a dangling reference refuses the
        // write), so the referenced credential is seeded first.
        fx.rt
            .dispatch(
                "credential.write",
                json!({
                    "instanceId": fx.instance_id,
                    "credentialId": "cred-1",
                    "kind": "channel.feishu",
                    "name": "feishu app credential",
                    "secret": "secret-1",
                }),
                None,
                None,
            )
            .await
            .unwrap();

        let (_, created) = fx
            .rt
            .dispatch(
                "channels.configure",
                configure_input(
                    "feishu",
                    json!({
                        "enabled": true,
                        "appId": "cli-a",
                        "credentialRef": "cred-1",
                        "connectionMode": "stream",
                    }),
                ),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(created["applied"], true);
        let revision = created["revision"].as_str().unwrap().to_owned();
        assert!(revision.starts_with("crev-"));
        // Saving is not running: no generation until a transition.
        assert_eq!(created["generation"], Value::Null);

        // A dangling credentialRef refuses the write (the candidate never
        // overwrites the stored row).
        let error = dispatch_err(
            &fx.rt,
            "channels.configure",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-feishu",
                "adapterType": "feishu",
                "schemaId": channel_schema_id("feishu"),
                "configuration": {"enabled": true, "credentialRef": "cred-none"},
                "expectedRevision": revision,
            }),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        // Revision gate: absent-on-existing and stale revisions reject.
        let error = dispatch_err(
            &fx.rt,
            "channels.configure",
            configure_input("feishu", json!({"enabled": false})),
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");
        let mut stale = configure_input("feishu", json!({"enabled": false}));
        stale["expectedRevision"] = json!("crev-99999999");
        let error = dispatch_err(&fx.rt, "channels.configure", stale).await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        // The list view carries the configured row plus the 18 unconfigured
        // types, and the row's secrets stay references.
        let (_, listed) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let channels = listed["channels"].as_array().unwrap();
        assert_eq!(
            listed["supportedAdapterTypes"].as_array().map(Vec::len),
            Some(19)
        );
        assert_eq!(channels.len(), 19);
        let row = channels
            .iter()
            .find(|c| c["channelId"] == json!("ch-feishu"))
            .unwrap();
        assert_eq!(row["adapterType"], "feishu");
        assert_eq!(row["revision"], revision);
        assert_eq!(row["enabled"], true);
        assert_eq!(row["runtime"]["state"], "disabled");
        assert_eq!(row["runtime"]["available"], false);
        assert_eq!(row["runtime"]["reasonCode"], "NOT_ASSEMBLED");
        assert_eq!(row["configuration"]["credentialRef"], "cred-1");
        assert_eq!(row["configuration"]["appId"], "cli-a");
        let unconfigured = channels
            .iter()
            .find(|c| c["channelId"] == json!("telegram"))
            .unwrap();
        assert_eq!(unconfigured["runtime"]["state"], "unconfigured");
        assert_eq!(unconfigured["runtime"]["reasonCode"], "NOT_CONFIGURED");

        // Excluding unconfigured rows leaves exactly the configured one.
        let (_, only) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": {"kind": "global"}, "includeUnconfigured": false}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(only["channels"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn scope_gates_and_visibility() {
        let fx = fixture("ch-scope").await;
        setup_project(&fx).await;

        // Unknown scope targets reject before anything else.
        let error = dispatch_err(
            &fx.rt,
            "channels.list",
            json!({"scope": {"kind": "project", "id": "proj-none"}}),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        let error = dispatch_err(
            &fx.rt,
            "channels.list",
            json!({"scope": {"kind": "global", "id": "proj-1"}}),
        )
        .await;
        assert_eq!(
            error.code, "INVALID_ARGUMENT",
            "global must carry a null id"
        );

        // A channel saved under the project scope is invisible from the
        // global scope and vice versa (exact-scope visibility).
        let project_scope = json!({"kind": "project", "id": "proj-1"});
        fx.rt
            .dispatch(
                "channels.configure",
                {
                    let mut input = configure_input("telegram", json!({"enabled": true}));
                    input["scope"] = project_scope.clone();
                    input
                },
                None,
                None,
            )
            .await
            .unwrap();
        let (_, global) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": {"kind": "global"}, "includeUnconfigured": false}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(global["channels"].as_array().map(Vec::len), Some(0));
        let (_, project) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": project_scope, "includeUnconfigured": false}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(project["channels"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn probe_classifies_transport_and_authentication_outcomes() {
        let fx = fixture("ch-probe").await;
        setup_project(&fx).await;
        // The authentication-outcome classifications below need a credential
        // wired in: without a `credentialRef` the probe classifies
        // NO_CREDENTIAL before any authentication attempt (the e2e twin
        // pins the same wiring).
        fx.rt
            .dispatch(
                "credential.write",
                json!({
                    "instanceId": fx.instance_id,
                    "credentialId": "cred-probe",
                    "kind": "channel.telegram",
                    "name": "probe credential",
                    "secret": "probe-secret",
                }),
                None,
                None,
            )
            .await
            .unwrap();

        // A closed port classifies as a connection failure. Bind then drop a
        // listener to get a guaranteed-closed port.
        let closed_port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            format!("http://{addr}")
        };
        let (_, created) = fx
            .rt
            .dispatch(
                "channels.configure",
                configure_input(
                    "telegram",
                    json!({
                        "enabled": true,
                        "probeEndpoint": closed_port,
                    }),
                ),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["revision"].as_str().unwrap().to_owned();
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.probe",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-telegram",
                    "expectedRevision": revision,
                }),
                Some("key-probe-closed"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], "operation");
        let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
        let completion = wait_operation(&fx, &operation_id).await;
        assert_eq!(completion["reachable"], false);
        assert_eq!(completion["authenticated"], false);
        assert_eq!(
            completion["diagnostics"][0]["code"], "CONN_FAILURE",
            "{completion}"
        );

        // The same key replays the same operation.
        let (_, replay) = fx
            .rt
            .dispatch(
                "channels.probe",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-telegram",
                    "expectedRevision": revision,
                }),
                Some("key-probe-closed"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"], operation_id);

        // An unauthenticated 200 classifies as reachable but not
        // authenticated; a 401 as an authentication failure. Both need the
        // credential referenced (see the seed above).
        let stub = spawn_stub(vec![
            (200, "{\"code\":\"UNAUTHENTICATED\"}".to_owned()),
            (401, "{\"error\":\"invalid_token\"}".to_owned()),
        ])
        .await;
        let (_, updated) = fx
            .rt
            .dispatch(
                "channels.configure",
                {
                    let mut input = configure_input(
                        "telegram",
                        json!({
                            "enabled": true,
                            "probeEndpoint": stub,
                            "credentialRef": "cred-probe",
                        }),
                    );
                    input["expectedRevision"] = json!(revision);
                    input
                },
                None,
                None,
            )
            .await
            .unwrap();
        let revision = updated["revision"].as_str().unwrap().to_owned();
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.probe",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-telegram",
                    "expectedRevision": revision,
                }),
                Some("key-probe-unauth"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["reachable"], true);
        assert_eq!(completion["authenticated"], false);
        assert_eq!(
            completion["diagnostics"][0]["code"],
            "AUTHENTICATION_FAILED"
        );
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.probe",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-telegram",
                    "expectedRevision": revision,
                }),
                Some("key-probe-401"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["reachable"], true);
        assert_eq!(completion["authenticated"], false);
        assert_eq!(
            completion["diagnostics"][0]["code"],
            "AUTHENTICATION_FAILED"
        );

        // Revision gate and the unknown-channel refusal.
        let error = dispatch_err_keyed(
            &fx.rt,
            "channels.probe",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-telegram",
                "expectedRevision": "crev-00000001",
            }),
            "key-probe-stale",
        )
        .await;
        assert_eq!(error.code, "REVISION_CONFLICT");
        let error = fx
            .rt
            .dispatch(
                "channels.probe",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-none",
                    "expectedRevision": revision,
                }),
                Some("key-probe-none"),
                None,
            )
            .await;
        match error {
            Err(DispatchError::Application(error)) => assert_eq!(error.code, "NOT_FOUND"),
            other => panic!("expected NOT_FOUND, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transition_advances_generation_and_states() {
        let fx = fixture("ch-transition").await;
        setup_project(&fx).await;

        let (_, created) = fx
            .rt
            .dispatch(
                "channels.configure",
                configure_input("webhook", json!({"enabled": true})),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["revision"].as_str().unwrap().to_owned();

        // The first enable runs against `expectedGeneration: "absent"`.
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.transition",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-webhook",
                    "action": "enable",
                    "expectedGeneration": "absent",
                }),
                Some("key-tr-enable"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["state"], "ready");
        let generation = completion["generation"].as_str().unwrap().to_owned();
        assert!(generation.starts_with("gen-"));
        assert_eq!(completion["drainedGeneration"], Value::Null);

        // The list view reports the assembled channel online.
        let (_, listed) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": {"kind": "global"}, "includeUnconfigured": false}),
                None,
                None,
            )
            .await
            .unwrap();
        let row = &listed["channels"][0];
        assert_eq!(row["runtime"]["state"], "ready");
        assert_eq!(row["runtime"]["available"], true);
        assert_eq!(row["runtime"]["reasonCode"], Value::Null);
        assert_eq!(row["runtime"]["generation"], generation);
        assert_eq!(
            row["revision"], revision,
            "transition does not rewrite the configuration"
        );

        // A stale expected generation rejects; a reload drains the old one.
        let error = dispatch_err_keyed(
            &fx.rt,
            "channels.transition",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-webhook",
                "action": "reload",
                "expectedGeneration": "absent",
            }),
            "key-tr-stale",
        )
        .await;
        assert_eq!(error.code, "GENERATION_CONFLICT");
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.transition",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-webhook",
                    "action": "reload",
                    "expectedGeneration": generation,
                }),
                Some("key-tr-reload"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        let next = completion["generation"].as_str().unwrap().to_owned();
        assert_ne!(next, generation);
        assert_eq!(completion["drainedGeneration"], generation);

        // Disable stops reporting online.
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.transition",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-webhook",
                    "action": "disable",
                    "expectedGeneration": next,
                }),
                Some("key-tr-disable"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["state"], "disabled");
        let (_, listed) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": {"kind": "global"}, "includeUnconfigured": false}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(listed["channels"][0]["runtime"]["available"], false);
    }

    #[tokio::test]
    async fn transition_enable_requires_probe_success_for_ready() {
        let fx = fixture("ch-transition-probe").await;
        setup_project(&fx).await;
        // The signature classification needs the credential referenced (a
        // missing credentialRef classifies NO_CREDENTIAL first), so seed it.
        fx.rt
            .dispatch(
                "credential.write",
                json!({
                    "instanceId": fx.instance_id,
                    "credentialId": "cred-tr-probe",
                    "kind": "channel.feishu",
                    "name": "transition probe credential",
                    "secret": "probe-secret",
                }),
                None,
                None,
            )
            .await
            .unwrap();

        // The stub answers 200 with a BAD_SIGNATURE code: enable must land
        // in `failed` with the probe reason queryable, never `ready` with
        // enabled=true. (An HTTP-level 403 classifies AUTHENTICATION_FAILED;
        // the signature code is an application-level answer.)
        let stub = spawn_stub(vec![(200, "{\"code\":\"BAD_SIGNATURE\"}".to_owned())]).await;
        fx.rt
            .dispatch(
                "channels.configure",
                configure_input(
                    "feishu",
                    json!({
                        "enabled": true,
                        "appId": "cli-a",
                        "probeEndpoint": stub,
                        "credentialRef": "cred-tr-probe",
                    }),
                ),
                None,
                None,
            )
            .await
            .unwrap();
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.transition",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-feishu",
                    "action": "enable",
                    "expectedGeneration": "absent",
                }),
                Some("key-tr-probe"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["state"], "failed");
        let (_, listed) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": {"kind": "global"}, "includeUnconfigured": false}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(listed["channels"][0]["runtime"]["state"], "failed");
        assert_eq!(
            listed["channels"][0]["runtime"]["reasonCode"],
            "SIGNATURE_INVALID"
        );
        assert_eq!(listed["channels"][0]["runtime"]["available"], false);
    }

    #[tokio::test]
    async fn login_challenge_lifecycle() {
        let fx = fixture("ch-login").await;
        setup_project(&fx).await;

        // The stub script: begin returns the presentation fields; the first
        // poll is pending, the second carries the exchanged credential.
        let stub = spawn_stub(vec![
            (
                200,
                "{\"userActionUrl\":\"https://example.test/device\",\"userCode\":\"AB-12\"}"
                    .to_owned(),
            ),
            (200, "{\"state\":\"pending\"}".to_owned()),
            (
                200,
                "{\"state\":\"succeeded\",\"credential\":\"exchanged-secret\"}".to_owned(),
            ),
        ])
        .await;
        fx.rt
            .dispatch(
                "channels.configure",
                configure_input("weixin", json!({"enabled": true, "probeEndpoint": stub})),
                None,
                None,
            )
            .await
            .unwrap();

        // An adapter without a login mode rejects the challenge outright
        // (the channel itself must exist, or the answer is NOT_FOUND).
        fx.rt
            .dispatch(
                "channels.configure",
                configure_input("telegram", json!({"enabled": true})),
                None,
                None,
            )
            .await
            .unwrap();
        let error = dispatch_err_keyed(
            &fx.rt,
            "channels.begin_login",
            json!({"scope": {"kind": "global"}, "channelId": "ch-telegram"}),
            "key-login-unsupported",
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");

        let (_, begun) = fx
            .rt
            .dispatch(
                "channels.begin_login",
                json!({"scope": {"kind": "global"}, "channelId": "ch-weixin"}),
                Some("key-login-begin"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(begun["challengeType"], "qr");
        assert_eq!(begun["userActionUrl"], "https://example.test/device");
        assert_eq!(begun["userCode"], "AB-12");
        assert!(begun["expiresAt"].as_str().is_some());
        assert_eq!(begun["pollAfterMs"], 1000);
        let challenge_id = begun["challengeId"].as_str().unwrap().to_owned();

        // The same idempotency key replays the original challenge.
        let (_, replay) = fx
            .rt
            .dispatch(
                "channels.begin_login",
                json!({"scope": {"kind": "global"}, "channelId": "ch-weixin"}),
                Some("key-login-begin"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["challengeId"], challenge_id);

        let poll_input = json!({
            "scope": {"kind": "global"},
            "channelId": "ch-weixin",
            "challengeId": challenge_id,
        });
        let (_, pending) = fx
            .rt
            .dispatch(
                "channels.poll_login",
                poll_input.clone(),
                Some("key-login-poll-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(pending["state"], "pending");
        assert_eq!(pending["credentialRef"], Value::Null);

        // The succeeded answer persists the credential first; repeated
        // polls replay it without re-registering.
        let (_, succeeded) = fx
            .rt
            .dispatch(
                "channels.poll_login",
                poll_input.clone(),
                Some("key-login-poll-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(succeeded["state"], "succeeded");
        let credential_ref = succeeded["credentialRef"].as_str().unwrap().to_owned();
        assert!(!credential_ref.is_empty());
        let (_, again) = fx
            .rt
            .dispatch(
                "channels.poll_login",
                poll_input.clone(),
                Some("key-login-poll-3"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(again["state"], "succeeded");
        assert_eq!(again["credentialRef"], credential_ref);

        // The credential is visible through the credential store and
        // never carries its secret back.
        let (_, creds) = fx
            .rt
            .dispatch("credential.list", json!({}), None, None)
            .await
            .unwrap();
        let stored = creds["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["credentialId"] == json!(credential_ref))
            .cloned()
            .unwrap();
        assert_eq!(stored["kind"], "channel.weixin");
        assert!(stored.get("secret").is_none());

        // Cancelling a terminal challenge reports `cancelled: false`
        // without changing anything.
        let (_, cancelled) = fx
            .rt
            .dispatch(
                "channels.cancel_login",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "challengeId": challenge_id,
                }),
                Some("key-login-cancel-terminal"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancelled["cancelled"], false);
        assert_eq!(cancelled["state"], "succeeded");

        // Unknown challenge / channel pairs are NOT_FOUND.
        let error = dispatch_err_keyed(
            &fx.rt,
            "channels.poll_login",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-weixin",
                "challengeId": "chall-none",
            }),
            "key-login-unknown",
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn cancel_makes_pending_irreversible_and_expiry_applies() {
        let fx = fixture("ch-login-cancel").await;
        setup_project(&fx).await;

        fx.rt
            .dispatch(
                "channels.configure",
                configure_input("wecom", json!({"enabled": true})),
                None,
                None,
            )
            .await
            .unwrap();
        let (_, begun) = fx
            .rt
            .dispatch(
                "channels.begin_login",
                json!({"scope": {"kind": "global"}, "channelId": "ch-wecom"}),
                Some("key-login-cancel-begin"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(begun["challengeType"], "externalBrowser");
        let challenge_id = begun["challengeId"].as_str().unwrap().to_owned();
        let poll_input = json!({
            "scope": {"kind": "global"},
            "channelId": "ch-wecom",
            "challengeId": challenge_id,
        });

        // Cancel wins on a pending challenge; a later poll cannot revive
        // it (a late succeeded callback installs nothing).
        let (_, cancelled) = fx
            .rt
            .dispatch(
                "channels.cancel_login",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-wecom",
                    "challengeId": challenge_id,
                }),
                Some("key-login-cancel-cancel"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(cancelled["state"], "cancelled");
        let (_, polled) = fx
            .rt
            .dispatch(
                "channels.poll_login",
                poll_input.clone(),
                Some("key-login-cancel-poll"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(polled["state"], "cancelled");
        assert_eq!(polled["credentialRef"], Value::Null);

        // Expiry: a challenge past its window terminalizes as expired on
        // the next poll, irreversibly.
        let (_, begun) = fx
            .rt
            .dispatch(
                "channels.begin_login",
                json!({"scope": {"kind": "global"}, "channelId": "ch-wecom"}),
                Some("key-login-expire-begin"),
                None,
            )
            .await
            .unwrap();
        let challenge_id = begun["challengeId"].as_str().unwrap().to_owned();
        let domain = domain_for(&fx.rt).unwrap();
        domain
            .db
            .with_conn("age challenge", |conn| {
                conn.execute(
                    "UPDATE v1_channel_challenges SET expires_at = '2000-01-01T00:00:00+00:00'
                     WHERE challenge_id = ?1",
                    params![challenge_id],
                )?;
                Ok(())
            })
            .unwrap();
        let (_, polled) = fx
            .rt
            .dispatch(
                "channels.poll_login",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-wecom",
                    "challengeId": challenge_id,
                }),
                Some("key-login-expire-poll"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(polled["state"], "expired");
    }

    #[tokio::test]
    async fn probe_results_validate_against_the_contract_schema() {
        let fx = fixture("ch-schema").await;
        setup_project(&fx).await;
        let stub = spawn_stub(Vec::new()).await;
        let (_, created) = fx
            .rt
            .dispatch(
                "channels.configure",
                configure_input(
                    "email",
                    json!({
                        "enabled": true,
                        "probeEndpoint": stub,
                    }),
                ),
                None,
                None,
            )
            .await
            .unwrap();
        let (_, receipt) = fx
            .rt
            .dispatch(
                "channels.probe",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-email",
                    "expectedRevision": created["revision"],
                }),
                Some("key-schema"),
                None,
            )
            .await
            .unwrap();
        // The receipt is the shared public receipt shape.
        agentcpm_pd_protocol::registry::OperationRegistry::global()
            .validate_result("channels.probe", &receipt)
            .expect("probe receipt validates");
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        // No credentialRef on the channel: reachable, but not authenticated
        // (the diagnostic names why).
        assert_eq!(completion["reachable"], true);
        assert_eq!(completion["authenticated"], false);
        assert_eq!(completion["diagnostics"][0]["code"], "NO_CREDENTIAL");
    }

    async fn spawn_gated_stub(
        arrived_tx: tokio::sync::oneshot::Sender<()>,
        release_rx: tokio::sync::oneshot::Receiver<()>,
        gated_action: &'static str,
        gated_response: (u16, String),
        immediate_responses: Vec<(u16, String)>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let arrived = std::sync::Arc::new(tokio::sync::Mutex::new(Some(arrived_tx)));
        let release = std::sync::Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let mut immediate = immediate_responses;
        immediate.reverse();
        let immediate = std::sync::Arc::new(tokio::sync::Mutex::new(immediate));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let arrived = arrived.clone();
                let release = release.clone();
                let immediate = immediate.clone();
                let gated_response = gated_response.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = [0u8; 8192];
                    let mut seen = Vec::new();
                    let mut header_end_idx = None;
                    let mut content_length = 0usize;
                    loop {
                        let n = match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        seen.extend_from_slice(&buffer[..n]);
                        let text = String::from_utf8_lossy(&seen);
                        if header_end_idx.is_none()
                            && let Some(header_end) = text.find("\r\n\r\n")
                        {
                            header_end_idx = Some(header_end + 4);
                            let headers = &text[..header_end];
                            if let Some(value) = headers.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            }) {
                                content_length = value;
                            }
                        }
                        if let Some(end_idx) = header_end_idx
                            && seen.len() >= end_idx + content_length
                        {
                            break;
                        }
                    }
                    let Some(header_end_idx) = header_end_idx else {
                        return;
                    };
                    let req_body = serde_json::from_slice::<Value>(&seen[header_end_idx..]).ok();
                    let is_gated = req_body
                        .as_ref()
                        .and_then(|v| v.get("action").and_then(Value::as_str))
                        == Some(gated_action);
                    let (status, body) = if is_gated {
                        if let Some(tx) = arrived.lock().await.take() {
                            let _ = tx.send(());
                        }
                        if let Some(rx) = release.lock().await.take() {
                            let _ = rx.await;
                        }
                        gated_response
                    } else {
                        immediate
                            .lock()
                            .await
                            .pop()
                            .unwrap_or((200, "{\"ok\":true}".to_owned()))
                    };
                    let answer = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn concurrent_transition_cas_and_disable_during_probe_prevent_ready() {
        let fx = fixture("ch-cas-disable").await;
        setup_project(&fx).await;

        // 1. Disabled configuration cannot transition to enable.
        let (_, disabled_cfg) = fx
            .rt
            .dispatch(
                "channels.configure",
                configure_input("telegram", json!({"enabled": false})),
                None,
                None,
            )
            .await
            .unwrap();
        let err = dispatch_err_keyed(
            &fx.rt,
            "channels.transition",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-telegram",
                "action": "enable",
                "expectedGeneration": "absent",
            }),
            "key-tr-disabled-upfront",
        )
        .await;
        assert_eq!(err.code, "INVALID_ARGUMENT");

        // 2. While probe is parked inside channels.transition(enable):
        //    - A concurrent transition(enable) gets GENERATION_CONFLICT.
        //    - Configuring enabled=false mid-probe prevents landing `ready`.
        let _ = disabled_cfg;
        fx.rt
            .dispatch(
                "credential.write",
                json!({
                    "instanceId": fx.instance_id,
                    "credentialId": "cred-cas-race",
                    "kind": "channel.feishu",
                    "name": "feishu cas credential",
                    "secret": "cas-secret",
                }),
                None,
                None,
            )
            .await
            .unwrap();

        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let stub = spawn_gated_stub(
            arrived_tx,
            release_rx,
            "probe",
            (200, "{\"ok\":true}".to_owned()),
            Vec::new(),
        )
        .await;

        let (_, cfg) = fx
            .rt
            .dispatch(
                "channels.configure",
                configure_input(
                    "feishu",
                    json!({
                        "enabled": true,
                        "appId": "cli-cas",
                        "probeEndpoint": stub,
                        "credentialRef": "cred-cas-race",
                    }),
                ),
                None,
                None,
            )
            .await
            .unwrap();
        let rev1 = cfg["revision"].as_str().unwrap().to_owned();

        let (_, receipt1) = fx
            .rt
            .dispatch(
                "channels.transition",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-feishu",
                    "action": "enable",
                    "expectedGeneration": "absent",
                }),
                Some("key-cas-1"),
                None,
            )
            .await
            .unwrap();

        // Park until the background task reaches run_probe().await.
        arrived_rx.await.unwrap();

        // A concurrent transition with the same expectedGeneration is rejected cleanly.
        let concurrent_err = dispatch_err_keyed(
            &fx.rt,
            "channels.transition",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-feishu",
                "action": "enable",
                "expectedGeneration": "absent",
            }),
            "key-cas-2",
        )
        .await;
        assert_eq!(concurrent_err.code, "GENERATION_CONFLICT");

        // Disable the channel in configuration while the probe is still in flight.
        fx.rt
            .dispatch(
                "channels.configure",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-feishu",
                    "adapterType": "feishu",
                    "schemaId": "agentcpm-pd.channel.feishu.config/1",
                    "expectedRevision": rev1,
                    "configuration": {
                        "enabled": false,
                        "appId": "cli-cas",
                        "probeEndpoint": stub,
                        "credentialRef": "cred-cas-race",
                    },
                }),
                None,
                None,
            )
            .await
            .unwrap();

        let _ = release_tx.send(());
        let completion = wait_operation(&fx, receipt1["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["state"], "disabled");

        let (_, listed) = fx
            .rt
            .dispatch(
                "channels.list",
                json!({"scope": {"kind": "global"}, "includeUnconfigured": false}),
                None,
                None,
            )
            .await
            .unwrap();
        let row = listed["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["channelId"] == "ch-feishu")
            .unwrap();
        assert_eq!(row["enabled"], false);
        assert_eq!(row["runtime"]["state"], "disabled");
        assert_eq!(row["runtime"]["available"], false);
        assert_eq!(row["runtime"]["reasonCode"], "CHANNEL_DISABLED");
    }

    #[tokio::test]
    async fn poll_login_cancel_race_and_login_idempotency() {
        let fx = fixture("ch-login-race").await;
        setup_project(&fx).await;

        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let stub = spawn_gated_stub(
            arrived_tx,
            release_rx,
            "poll",
            (
                200,
                "{\"state\":\"succeeded\",\"credential\":\"late-secret\"}".to_owned(),
            ),
            vec![(200, "{\"userCode\":\"WX-RACE\"}".to_owned())],
        )
        .await;

        fx.rt
            .dispatch(
                "channels.configure",
                configure_input("weixin", json!({"enabled": true, "probeEndpoint": stub})),
                None,
                None,
            )
            .await
            .unwrap();

        let (_, begun) = fx
            .rt
            .dispatch(
                "channels.begin_login",
                json!({"scope": {"kind": "global"}, "channelId": "ch-weixin"}),
                Some("key-race-begin"),
                None,
            )
            .await
            .unwrap();
        let challenge_id = begun["challengeId"].as_str().unwrap().to_owned();

        // Start poll_login concurrently and pause while upstream poll HTTP is parked.
        let rt_poll = fx.rt.clone();
        let ch_id = challenge_id.clone();
        let poll_handle = tokio::spawn(async move {
            rt_poll
                .dispatch(
                    "channels.poll_login",
                    json!({
                        "scope": {"kind": "global"},
                        "channelId": "ch-weixin",
                        "challengeId": ch_id,
                    }),
                    Some("key-race-poll"),
                    None,
                )
                .await
        });

        arrived_rx.await.unwrap();

        // Cancel wins while poll is in flight.
        let (_, cancelled) = fx
            .rt
            .dispatch(
                "channels.cancel_login",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "challengeId": challenge_id,
                }),
                Some("key-race-cancel"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancelled["state"], "cancelled");
        assert_eq!(cancelled["cancelled"], true);

        // Now let the late `succeeded` callback arrive at poll_login.
        let _ = release_tx.send(());
        let (_, polled) = poll_handle.await.unwrap().unwrap();
        assert_eq!(polled["state"], "cancelled");
        assert_eq!(polled["credentialRef"], Value::Null);

        // Verify NO credential was persisted in v1_credentials.
        let domain = domain_for(&fx.rt).unwrap();
        let cred_count: i64 = domain
            .db
            .with_conn("count race credentials", |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM v1_credentials WHERE credential_id = ?1",
                    params![format!("channel-credential-{challenge_id}")],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(cred_count, 0);

        // Idempotency replay & conflict enforcement on poll_login and cancel_login.
        let (_, poll_replay) = fx
            .rt
            .dispatch(
                "channels.poll_login",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "challengeId": challenge_id,
                }),
                Some("key-race-poll"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(poll_replay["state"], "cancelled");

        let poll_conflict = dispatch_err_keyed(
            &fx.rt,
            "channels.poll_login",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-weixin",
                "challengeId": "other-challenge",
            }),
            "key-race-poll",
        )
        .await;
        assert_eq!(poll_conflict.code, "IDEMPOTENCY_CONFLICT");

        let (_, cancel_replay) = fx
            .rt
            .dispatch(
                "channels.cancel_login",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "challengeId": challenge_id,
                }),
                Some("key-race-cancel"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancel_replay["cancelled"], true);

        let cancel_conflict = dispatch_err_keyed(
            &fx.rt,
            "channels.cancel_login",
            json!({
                "scope": {"kind": "global"},
                "channelId": "ch-weixin",
                "challengeId": "other-challenge",
            }),
            "key-race-cancel",
        )
        .await;
        assert_eq!(cancel_conflict.code, "IDEMPOTENCY_CONFLICT");
    }

    #[tokio::test]
    async fn weixin_login_configure_credential_ref_probe_and_enable() {
        let fx = fixture("ch-weixin-credref").await;
        setup_project(&fx).await;

        let stub = spawn_stub(vec![
            (200, "{\"qrPayload\":\"qr://wx\"}".to_owned()),
            (
                200,
                "{\"state\":\"succeeded\",\"credential\":\"wx-token-secret\"}".to_owned(),
            ),
            (200, "{\"ok\":true}".to_owned()),
            (200, "{\"ok\":true}".to_owned()),
        ])
        .await;

        let (_, configured) = fx
            .rt
            .dispatch(
                "channels.configure",
                configure_input("weixin", json!({"enabled": true, "probeEndpoint": stub})),
                None,
                None,
            )
            .await
            .unwrap();
        let rev1 = configured["revision"].as_str().unwrap().to_owned();

        let (_, begun) = fx
            .rt
            .dispatch(
                "channels.begin_login",
                json!({"scope": {"kind": "global"}, "channelId": "ch-weixin"}),
                Some("key-wx-begin"),
                None,
            )
            .await
            .unwrap();
        let challenge_id = begun["challengeId"].as_str().unwrap().to_owned();

        let (_, polled) = fx
            .rt
            .dispatch(
                "channels.poll_login",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "challengeId": challenge_id,
                }),
                Some("key-wx-poll"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(polled["state"], "succeeded");
        let cred_ref = polled["credentialRef"].as_str().unwrap().to_owned();

        let (_, rebound) = fx
            .rt
            .dispatch(
                "channels.configure",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "adapterType": "weixin",
                    "schemaId": "agentcpm-pd.channel.weixin.config/1",
                    "expectedRevision": rev1,
                    "configuration": {
                        "enabled": true,
                        "probeEndpoint": stub,
                        "credentialRef": cred_ref,
                    },
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let rev2 = rebound["revision"].as_str().unwrap().to_owned();

        let (_, probe_rcpt) = fx
            .rt
            .dispatch(
                "channels.probe",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "expectedRevision": rev2,
                }),
                Some("key-wx-probe"),
                None,
            )
            .await
            .unwrap();
        let probe_done = wait_operation(&fx, probe_rcpt["operationId"].as_str().unwrap()).await;
        assert_eq!(probe_done["reachable"], true);
        assert_eq!(probe_done["authenticated"], true);

        let (_, tr_rcpt) = fx
            .rt
            .dispatch(
                "channels.transition",
                json!({
                    "scope": {"kind": "global"},
                    "channelId": "ch-weixin",
                    "action": "enable",
                    "expectedGeneration": "absent",
                }),
                Some("key-wx-enable"),
                None,
            )
            .await
            .unwrap();
        let tr_done = wait_operation(&fx, tr_rcpt["operationId"].as_str().unwrap()).await;
        assert_eq!(tr_done["state"], "ready");
    }
}
