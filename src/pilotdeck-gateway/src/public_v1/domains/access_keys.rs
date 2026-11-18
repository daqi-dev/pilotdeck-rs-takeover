//! `access_key.*` — product access-credential management: create, list,
//! conditional enable/disable, and permanent revocation.
//!
//! Contract: `state/cli-v1-operations.json` (`access_key.list` / `.create` /
//! `.set_enabled` / `.revoke`). Semantic reference: the TS baseline's
//! `/api/settings/api-keys` routes (`ui/server/routes/settings.js`,
//! `ui/server/database/db.js`: `ck_`-prefixed hex keys, the full secret
//! shown only on creation, a sanitized list, and active-only validation at
//! authentication time).
//!
//! Storage and secret handling:
//!
//! * The secret is never persisted in plaintext. Each row stores
//!   `secret_hash = SHA-256(secret)` plus a display-only `fingerprint`
//!   derived from that hash; the plaintext secret exists only in the
//!   `access_key.create` response ("只首次输出secret") and never in error
//!   messages, list results, or logs.
//! * The `access_keys` table is registered through [`DomainRegistry::schema`]
//!   and created by `V1Store::init_schema` on the store's own connection.
//!   Rows are accessed through a second per-data-root SQLite connection
//!   opened by this file: `V1Store`'s connection is private to the
//!   `public_v1::store` module, so a domain file cannot execute SQL on it
//!   (substrate gap — the same second-connection pattern `bootstrap.rs`
//!   already uses for its read-only probes, extended here with
//!   `busy_timeout` and `BEGIN IMMEDIATE` write transactions so concurrent
//!   store transactions and access-key writes serialize at the SQLite
//!   level).
//!
//! Accepted deviations from the TS baseline (contract wins where it
//! speaks): soft `revoked` flag instead of row deletion, hash-based
//! authentication instead of plaintext lookup, caller-supplied `keyId`
//! instead of a database rowid, and a hash-derived fingerprint instead of a
//! 10-char plaintext prefix in listings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::now_rfc3339;

/// Contract error code for `access_key.set_enabled` on a revoked key
/// (declared in the operation's `errors` list; the protocol crate has no
/// shared constant for it yet).
const CODE_KEY_REVOKED: &str = "KEY_REVOKED";

const ACCESS_KEYS_DDL: &str = "
CREATE TABLE IF NOT EXISTS access_keys (
    key_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    secret_hash TEXT NOT NULL UNIQUE,
    fingerprint TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    revoked INTEGER NOT NULL DEFAULT 0,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_access_keys_created ON access_keys (created_at DESC, key_id ASC);
";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("access_keys", ACCESS_KEYS_DDL);
    reg.handler("access_key.list", |rt, input, _key, _rev| {
        Box::pin(access_key_list(rt, input))
    });
    reg.handler("access_key.create", |rt, input, _key, _rev| {
        Box::pin(access_key_create(rt, input))
    });
    reg.handler("access_key.set_enabled", |rt, input, _key, rev| {
        Box::pin(access_key_set_enabled(rt, input, rev))
    });
    reg.handler("access_key.revoke", |rt, input, _key, rev| {
        Box::pin(access_key_revoke(rt, input, rev))
    });
}

// ---------------------------------------------------------------------------
// Secret material handling
// ---------------------------------------------------------------------------

/// Generates a fresh access-key secret: `ck_` + 64 lowercase hex chars
/// (256 bits), mirroring the TS baseline's `ck_` + 32 random bytes format.
/// Entropy comes from two getrandom-backed v4 UUIDs (the workspace's
/// CSPRNG source without a new dependency), mixed with per-process and
/// per-call state through SHA-256.
fn generate_secret() -> String {
    static CALL_SEQ: AtomicU64 = AtomicU64::new(0);
    let mut hasher = Sha256::new();
    hasher.update(Uuid::new_v4().to_string().as_bytes());
    hasher.update(Uuid::new_v4().to_string().as_bytes());
    hasher.update(now_rfc3339().as_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(CALL_SEQ.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    format!("ck_{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The display-only key fingerprint: a prefix of the stored secret hash
/// (never of the plaintext secret), stable across listings.
fn fingerprint_of(secret_hash: &str) -> String {
    format!("sha256:{}", &secret_hash[..secret_hash.len().min(16)])
}

// ---------------------------------------------------------------------------
// Domain SQLite access (second connection; see module docs)
// ---------------------------------------------------------------------------

/// Process-wide cache of per-data-root connections, so repeated operations
/// reuse one handle instead of reopening the database per request.
static DOMAIN_CONNECTIONS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>> =
    OnceLock::new();

fn domain_connection(home: &Path) -> Result<Arc<Mutex<Connection>>, ContractError> {
    let registry = DOMAIN_CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = registry.lock().expect("access_keys connection cache");
    if let Some(conn) = cache.get(home) {
        return Ok(conn.clone());
    }
    let db_path = home.join("v1").join("state.sqlite");
    if !db_path.exists() {
        // The serving gateway always opens the store (which creates the
        // database and runs the domain DDL) before dispatching, so a missing
        // file means this home was never initialized.
        return Err(storage_err(format!(
            "state database {} does not exist",
            db_path.display()
        )));
    }
    let conn = Connection::open(&db_path)
        .map_err(|e| storage_err(format!("open {}: {e}", db_path.display())))?;
    // Match the main store connection's pragmas: WAL is persistent in the
    // database file, while busy_timeout / foreign_keys / synchronous are
    // per-connection.
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;",
    )
    .map_err(|e| storage_err(format!("pragma {}: {e}", db_path.display())))?;
    let conn = Arc::new(Mutex::new(conn));
    cache.insert(home.to_path_buf(), conn.clone());
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Row model and helpers
// ---------------------------------------------------------------------------

struct StoredAccessKey {
    key_id: String,
    name: String,
    enabled: bool,
    revoked: bool,
    revision: String,
    fingerprint: String,
    created_at: String,
}

impl StoredAccessKey {
    /// The contract's `AccessKey` shape — no secret, no hash.
    fn to_contract_json(&self) -> Value {
        json!({
            "keyId": self.key_id,
            "name": self.name,
            "enabled": self.enabled,
            "revoked": self.revoked,
            "revision": self.revision,
            "fingerprint": self.fingerprint,
            "createdAt": self.created_at,
        })
    }
}

fn row_to_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredAccessKey> {
    Ok(StoredAccessKey {
        key_id: row.get(0)?,
        name: row.get(1)?,
        enabled: row.get::<_, i64>(2)? != 0,
        revoked: row.get::<_, i64>(3)? != 0,
        revision: row.get(4)?,
        fingerprint: row.get(5)?,
        created_at: row.get(6)?,
    })
}

const KEY_COLUMNS: &str = "key_id, name, enabled, revoked, revision, fingerprint, created_at";

fn load_key(conn: &Connection, key_id: &str) -> Result<Option<StoredAccessKey>, ContractError> {
    conn.query_row(
        &format!("SELECT {KEY_COLUMNS} FROM access_keys WHERE key_id = ?1"),
        params![key_id],
        row_to_key,
    )
    .optional()
    .map_err(map_sql_err)
}

fn load_all_keys(conn: &Connection) -> Result<Vec<StoredAccessKey>, ContractError> {
    // Newest first, matching the TS baseline's list ordering.
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {KEY_COLUMNS} FROM access_keys ORDER BY created_at DESC, key_id ASC"
        ))
        .map_err(map_sql_err)?;
    let keys = stmt
        .query_map([], row_to_key)
        .map_err(map_sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_err)?;
    Ok(keys)
}

/// Allocates the next opaque revision from the shared `meta.rev_counter`
/// (same protocol as `public_v1::store`'s private helper; duplicated here
/// because that helper is module-private).
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

/// Reads the current shared revision without incrementing it (the
/// `pageInfo.snapshotRevision` source).
fn read_current_rev(conn: &Connection) -> Result<String, ContractError> {
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

fn key_not_found(key_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("access key `{key_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn invalid_argument(message: impl Into<String>) -> ContractError {
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

fn map_sql_err(err: rusqlite::Error) -> ContractError {
    storage_err(err.to_string())
}

/// The conditional-write revision guard: `expected_revision` comes from the
/// envelope when present, otherwise from the (schema-required) input field.
fn expected_revision_of(input: &Value, envelope_revision: Option<&str>) -> String {
    envelope_revision
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Pagination (offset-cursor scheme, mirroring `public_v1::store`)
// ---------------------------------------------------------------------------

fn parse_page(page: Option<&Value>) -> Result<(usize, usize), ContractError> {
    let obj = page.and_then(Value::as_object);
    let limit = obj
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let offset = match obj.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| invalid_argument(format!("invalid pagination cursor `{cursor}`")))?,
        None => 0,
    };
    Ok((offset, limit))
}

fn paginate_keys(
    keys: Vec<StoredAccessKey>,
    offset: usize,
    limit: usize,
    snapshot_revision: &str,
) -> Result<(Vec<Value>, Value), ContractError> {
    if offset > keys.len() {
        return Err(invalid_argument(
            "pagination cursor beyond end of collection",
        ));
    }
    let end = offset.saturating_add(limit).min(keys.len());
    let complete = end >= keys.len();
    let next_cursor = if complete {
        Value::Null
    } else {
        Value::String(end.to_string())
    };
    let items = keys
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|key| key.to_contract_json())
        .collect();
    let page_info = json!({
        "snapshotRevision": snapshot_revision,
        "nextCursor": next_cursor,
        "complete": complete,
    });
    Ok((items, page_info))
}

// ---------------------------------------------------------------------------
// Operation handlers
// ---------------------------------------------------------------------------

async fn access_key_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let (offset, limit) = parse_page(input.get("page"))?;
    let conn = domain_connection(rt.store().home())?;
    let conn = conn.lock().expect("access_keys sqlite mutex");
    let snapshot_revision = read_current_rev(&conn)?;
    let keys = load_all_keys(&conn)?;
    let (items, page_info) = paginate_keys(keys, offset, limit, &snapshot_revision)?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"items": items, "pageInfo": page_info}),
    ))
}

async fn access_key_create(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let Some(key_id) = input["keyId"].as_str() else {
        return Err(invalid_argument("`keyId` must be a string").into());
    };
    let Some(name) = input["name"].as_str() else {
        return Err(invalid_argument("`name` must be a string").into());
    };
    // The contract schema does not constrain the name's content; the TS
    // baseline rejects empty/whitespace-only names on create, so that
    // behavior is mirrored here.
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid_argument("key name is required and must not be blank").into());
    }

    let conn = domain_connection(rt.store().home())?;
    let mut conn = conn.lock().expect("access_keys sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_err)?;

    // nonReplayable create: a retry that lost the first response hits the
    // duplicate-id guard — no second key is generated and the secret is
    // never re-exposed.
    let exists: Option<String> = tx
        .query_row(
            "SELECT key_id FROM access_keys WHERE key_id = ?1",
            params![key_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql_err)?;
    if exists.is_some() {
        return Err(ContractError::new(
            CODE_ALREADY_EXISTS,
            format!("access key `{key_id}` already exists"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    let secret = generate_secret();
    let secret_hash = sha256_hex(secret.as_bytes());
    let fingerprint = fingerprint_of(&secret_hash);
    let revision = next_rev_in_tx(&tx)?;
    let now = now_rfc3339();
    tx.execute(
        "INSERT INTO access_keys
             (key_id, name, secret_hash, fingerprint, enabled, revoked, revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 1, 0, ?5, ?6, ?6)",
        params![key_id, name, secret_hash, fingerprint, revision, now],
    )
    .map_err(map_sql_err)?;
    tx.commit().map_err(map_sql_err)?;

    let key = StoredAccessKey {
        key_id: key_id.to_owned(),
        name: name.to_owned(),
        enabled: true,
        revoked: false,
        revision,
        fingerprint,
        created_at: now,
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({"key": key.to_contract_json(), "secret": secret}),
    ))
}

async fn access_key_set_enabled(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let Some(key_id) = input["keyId"].as_str() else {
        return Err(invalid_argument("`keyId` must be a string").into());
    };
    let Some(enabled) = input["enabled"].as_bool() else {
        return Err(invalid_argument("`enabled` must be a boolean").into());
    };
    let expected = expected_revision_of(&input, expected_revision.as_deref());

    let conn = domain_connection(rt.store().home())?;
    let mut conn = conn.lock().expect("access_keys sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_err)?;

    let Some(key) = load_key(&tx, key_id)? else {
        return Err(key_not_found(key_id).into());
    };
    // A revoked key is terminal: it can never be (re-)enabled, regardless
    // of the requested value or the revision presented.
    if key.revoked {
        return Err(ContractError::new(
            CODE_KEY_REVOKED,
            format!("access key `{key_id}` is revoked and cannot be enabled or disabled"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    if key.revision != expected {
        return Err(revision_conflict(&expected, &key.revision).into());
    }
    if key.enabled == enabled {
        // No state change requested; the confirmed current state is
        // returned without allocating a new revision.
        tx.commit().map_err(map_sql_err)?;
        return Ok((
            ResponseOutcome::Succeeded,
            json!({"key": key.to_contract_json()}),
        ));
    }

    let revision = next_rev_in_tx(&tx)?;
    let now = now_rfc3339();
    tx.execute(
        "UPDATE access_keys SET enabled = ?1, revision = ?2, updated_at = ?3 WHERE key_id = ?4",
        params![i64::from(enabled), revision, now, key_id],
    )
    .map_err(map_sql_err)?;
    tx.commit().map_err(map_sql_err)?;

    let mut updated = key;
    updated.enabled = enabled;
    updated.revision = revision;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"key": updated.to_contract_json()}),
    ))
}

async fn access_key_revoke(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let Some(key_id) = input["keyId"].as_str() else {
        return Err(invalid_argument("`keyId` must be a string").into());
    };
    let expected = expected_revision_of(&input, expected_revision.as_deref());

    let conn = domain_connection(rt.store().home())?;
    let mut conn = conn.lock().expect("access_keys sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_err)?;

    let Some(key) = load_key(&tx, key_id)? else {
        return Err(key_not_found(key_id).into());
    };
    if key.revision != expected {
        return Err(revision_conflict(&expected, &key.revision).into());
    }
    if key.revoked {
        // Revocation is permanent but re-revoking a key whose current state
        // the caller has already observed is a clear success ("重复撤销结果
        // 明确"): the confirmed current state is returned unchanged.
        tx.commit().map_err(map_sql_err)?;
        return Ok((
            ResponseOutcome::Succeeded,
            json!({"key": key.to_contract_json()}),
        ));
    }

    let revision = next_rev_in_tx(&tx)?;
    let now = now_rfc3339();
    tx.execute(
        "UPDATE access_keys SET revoked = 1, revision = ?1, updated_at = ?2 WHERE key_id = ?3",
        params![revision, now, key_id],
    )
    .map_err(map_sql_err)?;
    tx.commit().map_err(map_sql_err)?;

    let mut updated = key;
    updated.revoked = true;
    updated.revision = revision;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"key": updated.to_contract_json()}),
    ))
}

fn revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("access key expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// Authentication probe (wired by `PublicV1State::verify_token_str`)
// ---------------------------------------------------------------------------

/// Verifies a presented bearer secret against this instance's access keys.
///
/// This is the access-key half of the `/v1/ws` credential check, called by
/// `PublicV1State::verify_token_str` after its local.token constant-time
/// compare misses (HTTP-upgrade Bearer, `initialize` credential, and the
/// per-frame re-verification of the session credential all go through it).
/// Only a key that is enabled and not revoked authenticates; any storage
/// failure fails closed. The candidate is hashed before the indexed
/// lookup, so no plaintext secret comparison or logging occurs.
pub(crate) fn authenticate_secret(home: &Path, candidate: &str) -> bool {
    let Ok(conn) = domain_connection(home) else {
        return false;
    };
    let Ok(conn) = conn.lock() else {
        return false;
    };
    let candidate_hash = sha256_hex(candidate.as_bytes());
    let active: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM access_keys WHERE secret_hash = ?1 AND enabled = 1 AND revoked = 0",
            params![candidate_hash],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    active.is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;

    use super::*;

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
        (rt, file.instance_id)
    }

    fn fresh_home(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, String) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, instance_id) = open_runtime(&root_dir);
        (tmp, rt, instance_id)
    }

    fn application_error(err: DispatchError) -> ContractError {
        match err {
            DispatchError::Application(err) => err,
            DispatchError::Rpc { code, message, .. } => {
                panic!("expected an application error, got rpc {code}: {message}")
            }
        }
    }

    async fn create_key(
        rt: &Arc<PublicV1Runtime>,
        instance_id: &str,
        key_id: &str,
        name: &str,
    ) -> (Value, String) {
        let (outcome, result) = rt
            .dispatch(
                "access_key.create",
                json!({"instanceId": instance_id, "keyId": key_id, "name": name}),
                None,
                None,
            )
            .await
            .expect("create dispatches");
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("access_key.create", &result)
            .unwrap_or_else(|e| panic!("create result fails schema: {e:?}"));
        let secret = result["secret"].as_str().expect("secret").to_owned();
        (result, secret)
    }

    async fn list_keys(rt: &Arc<PublicV1Runtime>, instance_id: &str) -> Value {
        let (outcome, result) = rt
            .dispatch(
                "access_key.list",
                json!({"instanceId": instance_id}),
                None,
                None,
            )
            .await
            .expect("list dispatches");
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("access_key.list", &result)
            .unwrap_or_else(|e| panic!("list result fails schema: {e:?}"));
        result
    }

    fn row_count(home: &Path) -> i64 {
        let conn = domain_connection(home).unwrap();
        let conn = conn.lock().expect("access_keys sqlite mutex");
        conn.query_row("SELECT COUNT(*) FROM access_keys", [], |r| r.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn create_returns_secret_once_and_list_never_exposes_it() {
        let (_tmp, rt, iid) = fresh_home("ak-create");

        let (created, secret) = create_key(&rt, &iid, "k-primary", "  Primary key  ").await;
        let key = &created["key"];
        assert_eq!(key["keyId"], "k-primary");
        // The baseline trims the display name on create.
        assert_eq!(key["name"], "Primary key");
        assert_eq!(key["enabled"], true);
        assert_eq!(key["revoked"], false);
        assert!(
            key["revision"].as_str().is_some_and(|r| !r.is_empty()),
            "revision must be non-empty: {key}"
        );
        assert!(
            key["fingerprint"]
                .as_str()
                .is_some_and(|f| f.starts_with("sha256:")),
            "fingerprint must be hash-derived: {key}"
        );
        assert!(
            key["createdAt"].as_str().is_some_and(|c| c.contains('T')),
            "createdAt must be a timestamp: {key}"
        );
        // `ck_` + 64 lowercase hex, mirroring the baseline key format.
        // The messages deliberately do not embed the secret value.
        assert_eq!(secret.len(), 3 + 64, "secret must be ck_ + 64 hex chars");
        assert!(
            secret.starts_with("ck_"),
            "secret must carry the ck_ prefix"
        );
        assert!(
            secret[3..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "secret must be lowercase hex"
        );

        // The stored row must not contain the plaintext secret, only its
        // hash form (checked through the domain connection).
        {
            let conn = domain_connection(rt.store().home()).unwrap();
            let conn = conn.lock().expect("access_keys sqlite mutex");
            let hash: String = conn
                .query_row(
                    "SELECT secret_hash FROM access_keys WHERE key_id = 'k-primary'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(hash, sha256_hex(secret.as_bytes()));
            assert_ne!(hash, secret);
        }

        let listed = list_keys(&rt, &iid).await;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        let item = &listed["items"][0];
        assert_eq!(item["keyId"], "k-primary");
        let item_text = item.to_string();
        assert!(
            !item_text.contains(&secret),
            "list must never expose the secret: {item_text}"
        );
        assert!(
            !item_text.contains("secret") && !item_text.contains("hash"),
            "list must not carry secret/hash fields: {item_text}"
        );
        assert!(
            item["fingerprint"] != secret,
            "fingerprint must not equal the secret"
        );
    }

    #[tokio::test]
    async fn duplicate_create_is_rejected_without_reexposing_the_secret() {
        let (_tmp, rt, iid) = fresh_home("ak-dup");

        let (_, secret) = create_key(&rt, &iid, "k-dup", "first").await;
        let err = application_error(
            rt.dispatch(
                "access_key.create",
                json!({"instanceId": iid, "keyId": "k-dup", "name": "second"}),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "ALREADY_EXISTS");
        assert_eq!(err.effect, EffectStatus::NotApplied);
        let err_text = serde_json::to_string(&err).unwrap();
        assert!(
            !err_text.contains(&secret),
            "duplicate-create error must not expose the secret: {err_text}"
        );

        // "丢响应同ID不生成第二key": still exactly one row.
        assert_eq!(row_count(rt.store().home()), 1);
        let listed = list_keys(&rt, &iid).await;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn blank_names_are_rejected_like_the_baseline() {
        let (_tmp, rt, iid) = fresh_home("ak-blank");
        for name in ["", "   ", "\t"] {
            let err = application_error(
                rt.dispatch(
                    "access_key.create",
                    json!({"instanceId": iid, "keyId": "k-blank", "name": name}),
                    None,
                    None,
                )
                .await
                .unwrap_err(),
            );
            assert_eq!(err.code, "INVALID_ARGUMENT", "name {name:?}");
        }
        assert_eq!(row_count(rt.store().home()), 0);
    }

    #[tokio::test]
    async fn set_enabled_flips_state_and_requires_the_current_revision() {
        let (_tmp, rt, iid) = fresh_home("ak-toggle");

        let (created, _secret) = create_key(&rt, &iid, "k-toggle", "toggle me").await;
        let rev1 = created["key"]["revision"].as_str().unwrap().to_owned();

        // Stale revision first: no write happened yet beyond creation, but
        // the guard must reject a wrong revision regardless.
        let err = application_error(
            rt.dispatch(
                "access_key.set_enabled",
                json!({
                    "instanceId": iid, "keyId": "k-toggle",
                    "expectedRevision": "rev-00000000", "enabled": false
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "REVISION_CONFLICT");
        assert_eq!(err.effect, EffectStatus::NotApplied);

        // Disable with the current revision.
        let (outcome, result) = rt
            .dispatch(
                "access_key.set_enabled",
                json!({
                    "instanceId": iid, "keyId": "k-toggle",
                    "expectedRevision": rev1, "enabled": false
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("access_key.set_enabled", &result)
            .unwrap_or_else(|e| panic!("set_enabled result fails schema: {e:?}"));
        let key = &result["key"];
        assert_eq!(key["enabled"], false);
        assert_eq!(key["revoked"], false, "disabled and revoked stay separate");
        let rev2 = key["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev1, rev2, "a real state change bumps the revision");

        // The same-value write is a clean no-op: success, no revision bump.
        let (_, same) = rt
            .dispatch(
                "access_key.set_enabled",
                json!({
                    "instanceId": iid, "keyId": "k-toggle",
                    "expectedRevision": rev2, "enabled": false
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(same["key"]["revision"], rev2.as_str());

        // Stale revision after the real write.
        let err = application_error(
            rt.dispatch(
                "access_key.set_enabled",
                json!({
                    "instanceId": iid, "keyId": "k-toggle",
                    "expectedRevision": rev1, "enabled": true
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "REVISION_CONFLICT");

        // Unknown key.
        let err = application_error(
            rt.dispatch(
                "access_key.set_enabled",
                json!({
                    "instanceId": iid, "keyId": "k-missing",
                    "expectedRevision": rev2, "enabled": true
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "NOT_FOUND");

        // The list still shows the disabled key with revoked=false.
        let listed = list_keys(&rt, &iid).await;
        assert_eq!(listed["items"][0]["enabled"], false);
        assert_eq!(listed["items"][0]["revoked"], false);
    }

    #[tokio::test]
    async fn revoked_keys_cannot_be_enabled() {
        let (_tmp, rt, iid) = fresh_home("ak-rev");

        let (created, _secret) = create_key(&rt, &iid, "k-rev", "doomed").await;
        let rev1 = created["key"]["revision"].as_str().unwrap().to_owned();
        let (_, revoked) = rt
            .dispatch(
                "access_key.revoke",
                json!({
                    "instanceId": iid, "keyId": "k-rev", "expectedRevision": rev1
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let rev2 = revoked["key"]["revision"].as_str().unwrap().to_owned();

        // Any set_enabled on the revoked key is KEY_REVOKED, even with the
        // current revision and even to disable.
        for enabled in [true, false] {
            let err = application_error(
                rt.dispatch(
                    "access_key.set_enabled",
                    json!({
                        "instanceId": iid, "keyId": "k-rev",
                        "expectedRevision": rev2, "enabled": enabled
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err(),
            );
            assert_eq!(err.code, "KEY_REVOKED", "enabled={enabled}");
            assert_eq!(err.effect, EffectStatus::NotApplied);
        }
    }

    #[tokio::test]
    async fn revoke_is_permanent_and_repeat_revoke_is_explicit() {
        let (_tmp, rt, iid) = fresh_home("ak-rer");

        let (created, _secret) = create_key(&rt, &iid, "k-rer", "once").await;
        let rev1 = created["key"]["revision"].as_str().unwrap().to_owned();

        let (outcome, result) = rt
            .dispatch(
                "access_key.revoke",
                json!({"instanceId": iid, "keyId": "k-rer", "expectedRevision": rev1}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("access_key.revoke", &result)
            .unwrap_or_else(|e| panic!("revoke result fails schema: {e:?}"));
        assert_eq!(result["key"]["revoked"], true);
        // Revocation does not rewrite `enabled`: the terminal state is
        // carried by `revoked` alone.
        assert_eq!(result["key"]["enabled"], true);
        let rev2 = result["key"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev1, rev2);

        // Re-revoke with the observed state: clear success, no change.
        let (_, again) = rt
            .dispatch(
                "access_key.revoke",
                json!({"instanceId": iid, "keyId": "k-rer", "expectedRevision": rev2}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(again["key"]["revoked"], true);
        assert_eq!(again["key"]["revision"], rev2.as_str());

        // Re-revoke with a stale revision is a revision conflict.
        let err = application_error(
            rt.dispatch(
                "access_key.revoke",
                json!({"instanceId": iid, "keyId": "k-rer", "expectedRevision": rev1}),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "REVISION_CONFLICT");

        // Unknown key.
        let err = application_error(
            rt.dispatch(
                "access_key.revoke",
                json!({"instanceId": iid, "keyId": "k-missing", "expectedRevision": rev2}),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "NOT_FOUND");

        // The revoked key still appears in the list with its flags intact.
        let listed = list_keys(&rt, &iid).await;
        assert_eq!(listed["items"][0]["revoked"], true);
        assert_eq!(listed["items"][0]["enabled"], true);
    }

    #[tokio::test]
    async fn list_paginates_with_offset_cursors_and_rejects_bad_ones() {
        let (_tmp, rt, iid) = fresh_home("ak-page");

        for n in 0..3 {
            create_key(&rt, &iid, &format!("k-{n}"), &format!("key {n}")).await;
        }

        let first = rt
            .dispatch(
                "access_key.list",
                json!({"instanceId": iid, "page": {"limit": 2}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(first["items"].as_array().map(Vec::len), Some(2));
        assert_eq!(first["pageInfo"]["complete"], false);
        let cursor = first["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        assert!(
            first["pageInfo"]["snapshotRevision"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
        );

        let second = rt
            .dispatch(
                "access_key.list",
                json!({"instanceId": iid, "page": {"limit": 2, "cursor": cursor}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(second["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(second["pageInfo"]["complete"], true);
        assert_eq!(second["pageInfo"]["nextCursor"], Value::Null);

        for bad_cursor in ["not-a-number", "99"] {
            let err = application_error(
                rt.dispatch(
                    "access_key.list",
                    json!({"instanceId": iid, "page": {"cursor": bad_cursor}}),
                    None,
                    None,
                )
                .await
                .unwrap_err(),
            );
            assert_eq!(err.code, "INVALID_ARGUMENT", "cursor {bad_cursor:?}");
        }
    }

    #[tokio::test]
    async fn instance_mismatch_is_refused_before_any_side_effect() {
        let (_tmp, rt, iid) = fresh_home("ak-mismatch");
        assert_ne!(iid, "local-00000000-0000-7000-8000-00000000dead");

        let err = application_error(
            rt.dispatch(
                "access_key.create",
                json!({
                    "instanceId": "local-00000000-0000-7000-8000-00000000dead",
                    "keyId": "k-mismatch",
                    "name": "never created"
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "INSTANCE_MISMATCH");
        assert_eq!(err.effect, EffectStatus::NotApplied);

        // Zero side effects on this end: nothing was written.
        assert_eq!(row_count(rt.store().home()), 0);
        let listed = list_keys(&rt, &iid).await;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
        assert_eq!(listed["pageInfo"]["complete"], true);
    }

    #[tokio::test]
    async fn keys_are_isolated_across_instances() {
        let (_tmp_a, rt_a, iid_a) = fresh_home("ak-iso-a");
        let (_tmp_b, rt_b, iid_b) = fresh_home("ak-iso-b");
        assert_ne!(iid_a, iid_b);

        let (_, secret_a) = create_key(&rt_a, &iid_a, "k-shared-name", "instance A").await;
        let (_, _secret_b) = create_key(&rt_b, &iid_b, "k-other", "instance B").await;

        // Instance B's list does not contain A's key.
        let listed_b = list_keys(&rt_b, &iid_b).await;
        let ids_b: Vec<&str> = listed_b["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["keyId"].as_str())
            .collect();
        assert_eq!(ids_b, vec!["k-other"]);

        // A's key cannot be mutated through instance B.
        let listed_a = list_keys(&rt_a, &iid_a).await;
        let rev_a = listed_a["items"][0]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let attempts = [
            (
                "access_key.set_enabled",
                json!({
                    "instanceId": iid_b, "keyId": "k-shared-name",
                    "expectedRevision": rev_a, "enabled": false
                }),
            ),
            (
                "access_key.revoke",
                json!({"instanceId": iid_b, "keyId": "k-shared-name", "expectedRevision": rev_a}),
            ),
        ];
        for (method, input) in attempts {
            let err =
                application_error(rt_b.dispatch(method, input, None, None).await.unwrap_err());
            assert_eq!(err.code, "NOT_FOUND", "{method} must not cross instances");
        }

        // The stored secret authenticates only against its own instance.
        assert!(authenticate_secret(rt_a.store().home(), &secret_a));
        assert!(!authenticate_secret(rt_b.store().home(), &secret_a));
    }

    #[tokio::test]
    async fn authenticate_secret_accepts_only_active_keys() {
        let (_tmp, rt, iid) = fresh_home("ak-auth");

        // No keys yet: nothing authenticates.
        assert!(!authenticate_secret(rt.store().home(), "ck_whatever"));

        let (_, secret) = create_key(&rt, &iid, "k-auth", "auth probe").await;
        assert!(authenticate_secret(rt.store().home(), &secret));
        assert!(!authenticate_secret(rt.store().home(), "ck_not_the_secret"));

        let listed = list_keys(&rt, &iid).await;
        let rev = listed["items"][0]["revision"].as_str().unwrap().to_owned();

        // Disabled keys stop authenticating.
        rt.dispatch(
            "access_key.set_enabled",
            json!({"instanceId": iid, "keyId": "k-auth", "expectedRevision": rev, "enabled": false}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(!authenticate_secret(rt.store().home(), &secret));

        // Re-enabled again.
        let listed = list_keys(&rt, &iid).await;
        let rev = listed["items"][0]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "access_key.set_enabled",
            json!({"instanceId": iid, "keyId": "k-auth", "expectedRevision": rev, "enabled": true}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(authenticate_secret(rt.store().home(), &secret));

        // Revocation is permanent.
        let listed = list_keys(&rt, &iid).await;
        let rev = listed["items"][0]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "access_key.revoke",
            json!({"instanceId": iid, "keyId": "k-auth", "expectedRevision": rev}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            !authenticate_secret(rt.store().home(), &secret),
            "a revoked key must stop authenticating immediately"
        );

        // A secret for an uninitialized home fails closed without panicking.
        let foreign = TempDir::new("ak-auth-foreign");
        assert!(!authenticate_secret(&foreign.0, &secret));
    }

    #[tokio::test]
    async fn no_error_path_ever_leaks_the_secret() {
        let (_tmp, rt, iid) = fresh_home("ak-leak");

        let (created, secret) = create_key(&rt, &iid, "k-leak", "leak probe").await;
        let rev = created["key"]["revision"].as_str().unwrap().to_owned();

        // Every failing dispatch this domain can produce, collected and
        // scanned for the secret.
        let failures: Vec<DispatchError> = vec![
            rt.dispatch(
                "access_key.create",
                json!({"instanceId": iid, "keyId": "k-leak", "name": "again"}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "access_key.create",
                json!({"instanceId": iid, "keyId": "k-other", "name": ""}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "access_key.set_enabled",
                json!({"instanceId": iid, "keyId": "k-missing", "expectedRevision": rev, "enabled": true}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "access_key.set_enabled",
                json!({"instanceId": iid, "keyId": "k-leak", "expectedRevision": "stale", "enabled": true}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "access_key.revoke",
                json!({"instanceId": iid, "keyId": "k-missing", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "access_key.revoke",
                json!({"instanceId": iid, "keyId": "k-leak", "expectedRevision": "stale"}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "access_key.list",
                json!({"instanceId": iid, "page": {"cursor": "bogus"}}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "access_key.create",
                json!({"instanceId": "another", "keyId": "k-leak2", "name": "mismatch"}),
                None,
                None,
            )
            .await
            .unwrap_err(),
        ];
        assert_eq!(failures.len(), 8);
        for err in failures {
            let rendered = match err {
                DispatchError::Application(err) => serde_json::to_string(&err).unwrap(),
                DispatchError::Rpc { message, data, .. } => {
                    format!("{message}|{}", data.unwrap_or(Value::Null))
                }
            };
            assert!(
                !rendered.contains(&secret),
                "an error path leaked the secret: {rendered}"
            );
        }
    }

    #[test]
    fn fingerprint_is_derived_from_the_hash_not_the_secret() {
        let secret = "ck_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let hash = sha256_hex(secret.as_bytes());
        let fingerprint = fingerprint_of(&hash);
        assert!(fingerprint.starts_with("sha256:"));
        assert_eq!(fingerprint.len(), "sha256:".len() + 16);
        assert!(!fingerprint.contains(secret));
        assert!(!secret.contains(&fingerprint));
        // Stable across calls.
        assert_eq!(fingerprint, fingerprint_of(&hash));
    }

    #[test]
    fn generated_secrets_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let secret = generate_secret();
            assert!(secret.starts_with("ck_"));
            assert_eq!(secret.len(), 67);
            assert!(secret[3..].chars().all(|c| c.is_ascii_hexdigit()));
            assert!(seen.insert(secret), "generated a duplicate secret");
        }
    }

    #[test]
    fn paginate_keys_handles_empty_and_exact_pages() {
        let page_info_of = |keys: Vec<StoredAccessKey>, offset: usize, limit: usize| {
            let snapshot = "rev-00000001";
            let (items, page_info) =
                paginate_keys(keys, offset, limit, snapshot).expect("paginate");
            (items.len(), page_info)
        };
        let key = || StoredAccessKey {
            key_id: "k".to_owned(),
            name: "n".to_owned(),
            enabled: true,
            revoked: false,
            revision: "rev-00000001".to_owned(),
            fingerprint: "sha256:0123456789abcdef".to_owned(),
            created_at: "2026-11-18T00:00:00Z".to_owned(),
        };

        let (count, info) = page_info_of(vec![], 0, 100);
        assert_eq!(count, 0);
        assert_eq!(info["complete"], json!(true));
        assert_eq!(info["nextCursor"], Value::Null);

        let (count, info) = page_info_of(vec![key()], 0, 1);
        assert_eq!(count, 1);
        assert_eq!(info["complete"], json!(true));

        let (count, info) = page_info_of(vec![key(), key()], 0, 1);
        assert_eq!(count, 0);
        assert_eq!(info["complete"], json!(false));
        assert_eq!(info["nextCursor"], json!("1"));

        assert!(paginate_keys(vec![key()], 5, 10, "rev-1").is_err());
    }

    /// The registered DDL executes idempotently (the store opens existing
    /// databases too) — mirrors the substrate's own DDL-hook test.
    #[test]
    fn registered_ddl_is_executable_and_idempotent() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .expect("meta table");
        for _ in 0..2 {
            conn.execute_batch(ACCESS_KEYS_DDL)
                .expect("access_keys ddl executes");
        }
        conn.execute(
            "INSERT INTO access_keys
                 (key_id, name, secret_hash, fingerprint, enabled, revoked, revision, created_at, updated_at)
             VALUES ('k', 'n', 'h', 'f', 1, 0, 'rev-00000001', 't', 't')",
            [],
        )
        .expect("insert into access_keys");
        let hash: String = conn
            .query_row(
                "SELECT secret_hash FROM access_keys WHERE key_id = 'k'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash, "h");
    }

    #[test]
    fn domain_connection_caches_per_home_and_fails_closed_on_missing_db() {
        let foreign = TempDir::new("ak-conn-foreign");
        let err = domain_connection(&foreign.0).expect_err("uninitialized home must fail");
        assert_eq!(err.code, "UNAVAILABLE");

        // Fail-closed probe for the Phase B seam on the same missing home.
        assert!(!authenticate_secret(&foreign.0, "ck_anything"));
    }

    // -----------------------------------------------------------------------
    // Phase B: the authorization seam (`PublicV1State::verify_token_str`)
    // -----------------------------------------------------------------------

    /// The seam itself: local.token stays valid before, during, and after
    /// access-key changes; an access key authenticates only while it is
    /// enabled and not revoked.
    #[tokio::test]
    async fn verify_token_str_seam_accepts_local_token_and_active_keys_only() {
        let (tmp, rt, iid) = fresh_home("ak-seam");
        let root_dir = tmp.0.join("pd-home");
        let (_file, token) = load_local_instance_and_token(&root_dir, None).unwrap();
        let state = crate::public_v1::PublicV1State::new(rt.clone(), token.clone());

        assert!(state.verify_token_str(&token));
        assert!(!state.verify_token_str("ck_not_a_stored_key"));
        assert!(!state.verify_token_str(""));

        let (_, secret) = create_key(&rt, &iid, "k-seam", "seam probe").await;
        assert!(
            state.verify_token_str(&secret),
            "an active access key must authenticate"
        );
        assert!(state.verify_token_str(&token));

        let listed = list_keys(&rt, &iid).await;
        let rev = listed["items"][0]["revision"].as_str().unwrap().to_owned();

        // Disabled keys stop authenticating; local.token is unaffected.
        rt.dispatch(
            "access_key.set_enabled",
            json!({"instanceId": iid, "keyId": "k-seam", "expectedRevision": rev, "enabled": false}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(!state.verify_token_str(&secret));
        assert!(state.verify_token_str(&token));

        // Re-enabled again.
        let listed = list_keys(&rt, &iid).await;
        let rev = listed["items"][0]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "access_key.set_enabled",
            json!({"instanceId": iid, "keyId": "k-seam", "expectedRevision": rev, "enabled": true}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(state.verify_token_str(&secret));

        // Revocation is permanent at the seam.
        let listed = list_keys(&rt, &iid).await;
        let rev = listed["items"][0]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "access_key.revoke",
            json!({"instanceId": iid, "keyId": "k-seam", "expectedRevision": rev}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            !state.verify_token_str(&secret),
            "a revoked key must stop authenticating at the seam"
        );
        assert!(state.verify_token_str(&token));
    }

    /// The full real link (`cli.task.keys` Phase B acceptance): create a key
    /// over `/v1/ws`, authenticate a real connection with it, revoke it,
    /// and watch the open stream get cut at its next admission boundary
    /// while fresh connections are refused at the HTTP upgrade. The keyed
    /// connection omits the `initialize` credential on purpose so the
    /// re-verification exercises the captured HTTP Bearer (Plan A).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn access_key_lives_the_real_wire_end_to_end() {
        use std::time::{Duration, Instant};

        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        type ClientWs = tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >;

        async fn ws_connect(
            url: &str,
            credential: &str,
        ) -> Result<ClientWs, tokio_tungstenite::tungstenite::Error> {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut request = url.into_client_request()?;
            request.headers_mut().insert(
                "authorization",
                tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
                    "Bearer {credential}"
                ))
                .expect("bearer header"),
            );
            tokio_tungstenite::connect_async(request)
                .await
                .map(|(ws, _)| ws)
        }

        async fn ws_send(ws: &mut ClientWs, frame: &Value) {
            ws.send(Message::Text(frame.to_string().into()))
                .await
                .expect("send frame");
        }

        async fn ws_wait_response(ws: &mut ClientWs, id: &str) -> Value {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match ws.next().await.expect("stream open").expect("ws frame") {
                        Message::Text(txt) => {
                            let v: Value = serde_json::from_str(txt.as_str()).unwrap_or_else(|e| {
                                panic!("non-JSON frame `{}`: {e}", txt.as_str())
                            });
                            if v.get("id").and_then(Value::as_str) == Some(id) {
                                return v;
                            }
                        }
                        Message::Close(frame) => {
                            panic!("connection closed while waiting for {id}: {frame:?}")
                        }
                        other => panic!("unexpected frame waiting for {id}: {other:?}"),
                    }
                }
            })
            .await
            .expect("response timeout")
        }

        async fn ws_initialize(ws: &mut ClientWs, credential: Option<&str>) -> Value {
            let mut params = json!({
                "abi_version": 1,
                "api_version": agentcpm_pd_protocol::contract::OPERATION_VERSION,
                "features": ["subscriptions"],
                "client": {"name": "ak-wire-test", "version": "0.0.0"},
            });
            if let Some(cred) = credential {
                params["credential"] = json!(cred);
            }
            ws_send(
                ws,
                &json!({"jsonrpc": "2.0", "id": "init", "method": "initialize", "params": params}),
            )
            .await;
            let resp = ws_wait_response(ws, "init").await;
            assert_eq!(resp["result"]["supported"], true, "{resp}");
            resp
        }

        let tmp = TempDir::new("ak-wire");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (_file, token) = load_local_instance_and_token(&root_dir, None).unwrap();

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(crate::public_v1::bootstrap::serve_v1_gateway(
            root_dir.clone(),
            None,
            crate::public_v1::bootstrap::default_loopback_addr(),
            async move {
                let _ = stop_rx.await;
            },
        ));

        let ready_path = root_dir.join("run").join("ready.json");
        let deadline = Instant::now() + Duration::from_secs(30);
        let ws_url = loop {
            if let Ok(raw) = std::fs::read_to_string(&ready_path)
                && let Ok(v) = serde_json::from_str::<Value>(&raw)
                && let Some(url) = v["ws_url"].as_str()
            {
                break url.to_owned();
            }
            assert!(
                Instant::now() < deadline,
                "gateway did not become ready: {}",
                ready_path.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        // The owner (local.token) creates the key over the wire.
        let mut owner = ws_connect(&ws_url, &token).await.expect("owner ws connect");
        ws_initialize(&mut owner, Some(&token)).await;
        ws_send(
            &mut owner,
            &json!({
                "jsonrpc": "2.0", "id": "r1", "method": "access_key.create",
                "params": {"input": {"keyId": "k-live", "name": "wire key"}}
            }),
        )
        .await;
        let created = ws_wait_response(&mut owner, "r1").await;
        assert_eq!(created["result"]["outcome"], "succeeded", "{created}");
        let secret = created["result"]["result"]["secret"]
            .as_str()
            .expect("secret in create response")
            .to_owned();
        let revision = created["result"]["result"]["key"]["revision"]
            .as_str()
            .expect("revision")
            .to_owned();

        // The access key authenticates a real connection end to end.
        let mut keyed = ws_connect(&ws_url, &secret)
            .await
            .expect("access-key ws connect");
        ws_initialize(&mut keyed, None).await;
        ws_send(
            &mut keyed,
            &json!({
                "jsonrpc": "2.0", "id": "r2", "method": "access_key.list",
                "params": {"input": {}}
            }),
        )
        .await;
        let listed = ws_wait_response(&mut keyed, "r2").await;
        assert_eq!(listed["result"]["outcome"], "succeeded", "{listed}");
        assert_eq!(
            listed["result"]["result"]["items"].as_array().map(Vec::len),
            Some(1)
        );

        // Revoke through the owner connection.
        ws_send(
            &mut owner,
            &json!({
                "jsonrpc": "2.0", "id": "r3", "method": "access_key.revoke",
                "params": {"input": {"keyId": "k-live", "expectedRevision": revision}}
            }),
        )
        .await;
        let revoked = ws_wait_response(&mut owner, "r3").await;
        assert_eq!(revoked["result"]["outcome"], "succeeded", "{revoked}");

        // The still-open keyed connection is cut at its next admission
        // boundary: the frame answers UNAUTHENTICATED, then the stream
        // closes (旧观察流不能继续读取).
        ws_send(
            &mut keyed,
            &json!({
                "jsonrpc": "2.0", "id": "r4", "method": "access_key.list",
                "params": {"input": {}}
            }),
        )
        .await;
        let rejection = ws_wait_response(&mut keyed, "r4").await;
        assert_eq!(
            rejection["error"]["data"]["code"], "UNAUTHENTICATED",
            "{rejection}"
        );
        let closed = tokio::time::timeout(Duration::from_secs(30), keyed.next())
            .await
            .expect("close arrived");
        assert!(
            matches!(closed, Some(Ok(Message::Close(_))) | None),
            "expected the stream to close, got {closed:?}"
        );

        // A fresh connection with the revoked secret is refused at the
        // HTTP upgrade (撤销后新访问拒绝).
        let refused = ws_connect(&ws_url, &secret).await;
        match refused.err() {
            Some(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), 401);
            }
            other => panic!("expected HTTP 401 at the upgrade, got {other:?}"),
        }

        // local.token keeps working throughout.
        ws_send(
            &mut owner,
            &json!({
                "jsonrpc": "2.0", "id": "r5", "method": "access_key.list",
                "params": {"input": {}}
            }),
        )
        .await;
        let after = ws_wait_response(&mut owner, "r5").await;
        assert_eq!(after["result"]["outcome"], "succeeded", "{after}");
        assert_eq!(
            after["result"]["result"]["items"].as_array().map(Vec::len),
            Some(1)
        );

        let _ = stop_tx.send(());
        server.await.expect("server task").expect("serve ok");
    }
}
