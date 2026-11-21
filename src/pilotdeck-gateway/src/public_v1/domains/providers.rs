//! `provider.*` — model-provider configuration (`cli.task.t016`, S3).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `provider.list` / `.get` /
//! `.write` 条目。TS 基线（cc7c4e6）是语义参考：
//! * `src/model/protocol/canonical.ts:7` — `ModelProtocol` =
//!   `"anthropic" | "openai" | "openai-responses" | "google"`（v1 供应商
//!   `protocol` 的支持集，大小写不敏感、按小写归一存储，基线
//!   `ui/server/routes/config.js` 的 POST /api/config/models 同样先
//!   `toLowerCase()`）；其它协议 → `PROTOCOL_UNSUPPORTED`。
//! * `ui/server/routes/config.js` 的 GET /api/config/provider 会回显
//!   `apiKey` 明文——v1 合同优先（「只输出凭据引用」）：供应商记录只存
//!   `credentialRef`，本域任何结果都不携带 secret。
//! * 基线把 endpoint 规范化为去尾斜杠的小写协议 URL；v1 沿用 trim +
//!   去尾斜杠，且拒绝非 http/https、带 userinfo（`user:pass@host`，含
//!   key 形用户名）、带 fragment 与带 secret 形态 query 参数的 endpoint
//!   （合同验收「endpoint秘密被拒或脱敏」——选择「被拒」，写入即拒绝，
//!   存储中因此永远不会出现带秘密的 endpoint）。
//!
//! 旧 Rust 线的 `runtime/model/provider_registry.rs` 是进程内调用租约注册
//! 表（生成/退役/排水），与持久化供应商配置不是一个面；v1 供应商配置按
//! `configuration.*` 域先例落在 v1 store（`AGENTCPM_PD_HOME`，007 §8），
//! 差异登记见任务报告。
//!
//! 「保存不自动出网」：`provider.write` 是纯存储操作，本域不发起任何网络
//! I/O（连接测试属 `provider_test.*` / `model-test` 任务）。
//!
//! 凭据引用完整性：`credentialRef` 省略时保留现有引用；显式 `null` 清除；
//! 指向 id 时要求凭据存在且 `kind` 为 `api_key`（模型供应商凭据在基线
//! `ProviderConfig.apiKey` 中唯一对应形态），否则 `CREDENTIAL_TYPE_MISMATCH`
//! ——见任务报告的「合同未明确」清单。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use url::Url;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::configuration::{self, VIssue};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;

/// Contract error codes declared by `provider.write` that the protocol crate
/// has no shared constant for yet.
const CODE_PROTOCOL_UNSUPPORTED: &str = "PROTOCOL_UNSUPPORTED";
const CODE_CREDENTIAL_TYPE_MISMATCH: &str = "CREDENTIAL_TYPE_MISMATCH";

/// The model-protocol allowlist (TS baseline `ModelProtocol`).
const SUPPORTED_PROTOCOLS: [&str; 4] = ["anthropic", "openai", "openai-responses", "google"];

/// The credential kind attachable to a model provider. The contract defines
/// no kind vocabulary; `api_key` mirrors the baseline `ProviderConfig.apiKey`
/// field (the only credential shape a model provider consumes). Flagged in
/// the task report for integrator confirmation.
const PROVIDER_CREDENTIAL_KIND: &str = "api_key";

const PROVIDERS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS v1_providers (
    provider_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    protocol TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    credential_ref TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

fn validate_provider_config_key(
    conn: &Connection,
    key: &str,
    value: &Value,
) -> Result<Vec<VIssue>, ContractError> {
    let mut issues = Vec::new();
    let is_provider_obj = key == "providers" || key.starts_with("providers.");
    if (key == "provider.endpoint" || key == "provider.baseUrl")
        && let Some(raw) = value.as_str()
    {
        if let Err(err) = normalize_endpoint(raw) {
            issues.push(VIssue::new(
                "INVALID_ENDPOINT",
                Some(key.to_owned()),
                err.message,
                true,
            ));
        }
    } else if is_provider_obj {
        if key.ends_with(".endpoint") || key.ends_with(".baseUrl") {
            let Some(raw) = value.as_str() else {
                issues.push(VIssue::new(
                    "INVALID_ENDPOINT",
                    Some(key.to_owned()),
                    "provider endpoint must be a string",
                    true,
                ));
                return Ok(issues);
            };
            if let Err(err) = normalize_endpoint(raw) {
                issues.push(VIssue::new(
                    "INVALID_ENDPOINT",
                    Some(key.to_owned()),
                    err.message,
                    true,
                ));
            }
        } else if let Some(obj) = value.as_object() {
            if let Some(ep) = obj.get("endpoint").or_else(|| obj.get("baseUrl")) {
                match ep.as_str() {
                    Some(raw) => {
                        if let Err(err) = normalize_endpoint(raw) {
                            issues.push(VIssue::new(
                                "INVALID_ENDPOINT",
                                Some(format!("{key}.endpoint")),
                                err.message,
                                true,
                            ));
                        }
                    }
                    None => issues.push(VIssue::new(
                        "INVALID_ENDPOINT",
                        Some(format!("{key}.endpoint")),
                        "provider endpoint must be a string",
                        true,
                    )),
                }
            }
            if let Some(proto) = obj.get("protocol")
                && proto
                    .as_str()
                    .map(|p| SUPPORTED_PROTOCOLS.contains(&p.trim().to_ascii_lowercase().as_str()))
                    != Some(true)
            {
                issues.push(VIssue::new(
                    CODE_PROTOCOL_UNSUPPORTED,
                    Some(format!("{key}.protocol")),
                    "unsupported provider protocol",
                    true,
                ));
            }
            if let Some(cref) = obj.get("credentialRef")
                && !cref.is_null()
            {
                let Some(id) = cref.as_str().filter(|s| !s.is_empty()) else {
                    issues.push(VIssue::new(
                        CODE_CREDENTIAL_TYPE_MISMATCH,
                        Some(format!("{key}.credentialRef")),
                        "provider credentialRef must be a non-empty string or null",
                        true,
                    ));
                    return Ok(issues);
                };
                let kind: Option<String> = conn
                    .query_row(
                        "SELECT kind FROM v1_credentials WHERE credential_id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(providers_sql_err)?;
                if kind.as_deref() != Some(PROVIDER_CREDENTIAL_KIND) {
                    issues.push(VIssue::new(
                        CODE_CREDENTIAL_TYPE_MISMATCH,
                        Some(format!("{key}.credentialRef")),
                        format!("credentialRef `{id}` does not resolve to an api_key credential"),
                        true,
                    ));
                }
            }
        }
    }
    Ok(issues)
}

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_providers", PROVIDERS_TABLE_DDL);
    configuration::register_named_scoped_validator("providers", validate_provider_config_key);
    reg.handler("provider.list", |rt, input, _key, _rev| {
        Box::pin(provider_list(rt, input))
    });
    reg.handler("provider.get", |rt, input, _key, _rev| {
        Box::pin(provider_get(rt, input))
    });
    reg.handler("provider.write", |rt, input, _key, rev| {
        Box::pin(provider_write(rt, input, rev))
    });
}

// ---------------------------------------------------------------------------
// Row model and shared helpers
// ---------------------------------------------------------------------------

struct StoredProvider {
    provider_id: String,
    name: String,
    protocol: String,
    endpoint: String,
    credential_ref: Option<String>,
    enabled: bool,
    revision: String,
}

impl StoredProvider {
    /// The contract `Provider` shape — `credentialRef` only, never a secret
    /// (「只输出凭据引用」); the row itself stores no secret material.
    fn to_contract_json(&self) -> Value {
        json!({
            "providerId": self.provider_id,
            "name": self.name,
            "protocol": self.protocol,
            "endpoint": self.endpoint,
            "credentialRef": self.credential_ref,
            "enabled": self.enabled,
            "revision": self.revision,
        })
    }
}

const PROVIDER_COLUMNS: &str =
    "provider_id, name, protocol, endpoint, credential_ref, enabled, revision";

fn row_to_provider(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredProvider> {
    Ok(StoredProvider {
        provider_id: row.get(0)?,
        name: row.get(1)?,
        protocol: row.get(2)?,
        endpoint: row.get(3)?,
        credential_ref: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        revision: row.get(6)?,
    })
}

fn load_provider(
    conn: &Connection,
    provider_id: &str,
) -> Result<Option<StoredProvider>, ContractError> {
    conn.query_row(
        &format!("SELECT {PROVIDER_COLUMNS} FROM v1_providers WHERE provider_id = ?1"),
        params![provider_id],
        row_to_provider,
    )
    .optional()
    .map_err(providers_sql_err)
}

fn providers_sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn providers_invalid_argument(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn providers_revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("provider expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn providers_next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(providers_sql_err)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(providers_sql_err)?;
    Ok(format!("rev-{next:08}"))
}

fn providers_read_current_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(providers_sql_err)?
        .unwrap_or(1);
    Ok(format!("rev-{cur:08}"))
}

/// The baseline's `redactConfig` secret needles (ASCII case-insensitive
/// substring; local copy of `configuration.rs::sensitive_key`'s list so this
/// domain does not import another task's file).
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode_str(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Checks decoded query parameter names (including Google's `?key=`, `auth`,
/// `sig`, `passwd`, and any secret/token/credential spelling).
fn secret_looking_name(name: &str) -> bool {
    const SUBSTR_NEEDLES: [&str; 11] = [
        "apikey",
        "authorization",
        "cookie",
        "token",
        "secret",
        "password",
        "credential",
        "passwd",
        "accesskey",
        "privatekey",
        "signature",
    ];
    let decoded = percent_decode_str(name);
    let folded: Vec<u8> = decoded
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| b.to_ascii_lowercase())
        .collect();
    if matches!(
        folded.as_slice(),
        b"key" | b"auth" | b"sig" | b"bearer" | b"jwt" | b"pwd" | b"sid"
    ) {
        return true;
    }
    // Check separator-split segments as well (e.g. `google_key`, `x-key`).
    if decoded
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|seg| {
            matches!(
                seg.to_ascii_lowercase().as_str(),
                "key" | "auth" | "sig" | "pwd"
            )
        })
    {
        return true;
    }
    SUBSTR_NEEDLES
        .iter()
        .any(|needle| folded.windows(needle.len()).any(|w| w == needle.as_bytes()))
}

fn looks_like_secret_value(val: &str) -> bool {
    let decoded = percent_decode_str(val);
    let trimmed = decoded.trim();
    (trimmed.starts_with("sk-") && trimmed.len() >= 10)
        || (trimmed.starts_with("AIza") && trimmed.len() >= 12)
        || (trimmed.starts_with("ghp_") && trimmed.len() >= 12)
        || (trimmed.starts_with("gho_") && trimmed.len() >= 12)
        || (trimmed.starts_with("github_pat_") && trimmed.len() >= 16)
        || (trimmed.starts_with("xoxb-") && trimmed.len() >= 12)
        || trimmed.to_ascii_lowercase().starts_with("bearer ")
}

/// Endpoint normalization + validation using `url::Url::parse`:
/// * Never echoes the raw URL in error messages (prevents leaking userinfo,
///   fragments, or query credentials on rejection).
/// * Rejects non-http/https schemes, empty/missing host, userinfo, fragments,
///   and secret-looking query parameter names or token values (handling `&`/`;`
///   and percent-encoding such as `api%5Fkey` / `?key=AIza...`).
pub(crate) fn normalize_endpoint(raw: &str) -> Result<String, ContractError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(providers_invalid_argument("provider endpoint is required"));
    }
    let stripped = trimmed.trim_end_matches('/');
    if stripped.contains('#') {
        return Err(providers_invalid_argument(
            "provider endpoint must not carry a URL fragment",
        ));
    }
    let parsed = Url::parse(stripped).map_err(|_| {
        providers_invalid_argument("provider endpoint must be a valid http/https URL")
    })?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(ContractError::new(
            CODE_PROTOCOL_UNSUPPORTED,
            format!("provider endpoint scheme `{scheme}` is not http/https"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let after_scheme = stripped
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("");
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || after_scheme[..authority_end].contains('@')
    {
        return Err(providers_invalid_argument(
            "provider endpoint must not embed a user:password authority; \
             pass credentials through credentialRef instead",
        ));
    }
    if parsed.host_str().is_none_or(|h| h.is_empty()) {
        return Err(providers_invalid_argument("provider endpoint has no host"));
    }

    for (k, v) in parsed.query_pairs() {
        if secret_looking_name(&k) || looks_like_secret_value(&v) || looks_like_secret_value(&k) {
            return Err(providers_invalid_argument(
                "provider endpoint query must not contain secret-looking parameters; \
                 pass credentials through credentialRef instead",
            ));
        }
    }
    if let Some(raw_query) = parsed.query() {
        for segment in raw_query.split(['&', ';']) {
            if segment.is_empty() {
                continue;
            }
            let (raw_k, raw_v) = segment.split_once('=').unwrap_or((segment, ""));
            if secret_looking_name(raw_k)
                || looks_like_secret_value(raw_v)
                || looks_like_secret_value(raw_k)
            {
                return Err(providers_invalid_argument(
                    "provider endpoint query must not contain secret-looking parameters; \
                     pass credentials through credentialRef instead",
                ));
            }
        }
    }
    Ok(stripped.to_owned())
}

fn providers_mutex_err() -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        "sqlite store mutex poisoned",
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

async fn run_blocking<F>(rt: &Arc<PublicV1Runtime>, f: F) -> Result<Value, DispatchError>
where
    F: FnOnce(&V1Store) -> Result<Value, ContractError> + Send + 'static,
{
    let store = Arc::clone(rt.store());
    let out = tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|e| {
            DispatchError::from(ContractError::new(
                CODE_UNAVAILABLE,
                format!("blocking provider task failed: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            ))
        })?;
    out.map_err(DispatchError::from)
}

// ---------------------------------------------------------------------------
// provider.list / provider.get
// ---------------------------------------------------------------------------

async fn provider_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let body = run_blocking(&rt, move |store| {
        let page = input.get("page").and_then(Value::as_object);
        let limit = page
            .and_then(|p| p.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;
        let offset = match page.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
            Some(cursor) => cursor.parse::<usize>().map_err(|_| {
                providers_invalid_argument(format!("invalid pagination cursor `{cursor}`"))
            })?,
            None => 0,
        };

        let conn = store.conn.lock().map_err(|_| providers_mutex_err())?;
        let snapshot_revision = providers_read_current_rev(&conn)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {PROVIDER_COLUMNS} FROM v1_providers ORDER BY provider_id ASC"
            ))
            .map_err(providers_sql_err)?;
        let providers = stmt
            .query_map([], row_to_provider)
            .map_err(providers_sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(providers_sql_err)?;
        drop(stmt);
        drop(conn);

        let total = providers.len();
        if offset > total {
            return Err(providers_invalid_argument(
                "pagination cursor beyond end of collection",
            ));
        }
        let end = offset.saturating_add(limit).min(total);
        let complete = end >= total;
        let items: Vec<Value> = providers
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|p| p.to_contract_json())
            .collect();
        Ok(json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": snapshot_revision,
                "nextCursor": if complete { Value::Null } else { json!(end.to_string()) },
                "complete": complete,
            },
        }))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, body))
}

async fn provider_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let body = run_blocking(&rt, move |store| {
        let provider_id = input
            .get("providerId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let conn = store.conn.lock().map_err(|_| providers_mutex_err())?;
        let found = load_provider(&conn, provider_id)?;
        drop(conn);
        // 「错误ID不回落默认」: exact lookup, no default provider fallback.
        let Some(provider) = found else {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("provider `{provider_id}` not found"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        Ok(json!({"provider": provider.to_contract_json()}))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, body))
}

// ---------------------------------------------------------------------------
// provider.write
// ---------------------------------------------------------------------------

async fn provider_write(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let body = run_blocking(&rt, move |store| {
        let provider_id = input
            .get("providerId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if provider_id.is_empty() {
            return Err(providers_invalid_argument("providerId is required"));
        }
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if name.is_empty() {
            return Err(providers_invalid_argument(
                "provider name is required and must not be blank",
            ));
        }
        // 「不支持协议拒绝」: case-insensitive match against the baseline
        // ModelProtocol set, stored normalized (lowercase).
        let raw_protocol = input
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let protocol = if SUPPORTED_PROTOCOLS.contains(&raw_protocol.as_str()) {
            raw_protocol
        } else {
            return Err(ContractError::new(
                CODE_PROTOCOL_UNSUPPORTED,
                format!(
                    "provider protocol `{raw_protocol}` is not supported; expected one of {}",
                    SUPPORTED_PROTOCOLS.join("|")
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        let endpoint = normalize_endpoint(
            input
                .get("endpoint")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )?;
        let enabled = input
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        // 「省略凭据保留/null显式清除」: tri-state — absent preserves, explicit
        // null clears, an id must resolve to an `api_key` credential.
        let credential_input = input.get("credentialRef");
        let expected = expected_revision
            .as_deref()
            .or_else(|| input.get("expectedRevision").and_then(Value::as_str));

        let mut conn = store.conn.lock().map_err(|_| providers_mutex_err())?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(providers_sql_err)?;

        let existing = load_provider(&tx, &provider_id)?;
        match (expected, &existing) {
            (None, Some(current)) => {
                return Err(providers_revision_conflict("<omitted>", &current.revision));
            }
            (Some(_), None) => {
                return Err(providers_revision_conflict(
                    expected.unwrap_or_default(),
                    "<absent>",
                ));
            }
            (Some(expected), Some(current)) if current.revision != expected => {
                return Err(providers_revision_conflict(expected, &current.revision));
            }
            _ => {}
        }

        let credential_ref = match credential_input {
            None => existing.as_ref().and_then(|p| p.credential_ref.clone()),
            Some(Value::Null) => None,
            Some(value) => {
                let id = value.as_str().unwrap_or_default();
                if id.is_empty() {
                    return Err(providers_invalid_argument(
                        "provider credentialRef must be a non-empty credential id or null",
                    ));
                }
                let kind: Option<String> = tx
                    .query_row(
                        "SELECT kind FROM v1_credentials WHERE credential_id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(providers_sql_err)?;
                match kind.as_deref() {
                    Some(PROVIDER_CREDENTIAL_KIND) => Some(id.to_owned()),
                    Some(other) => {
                        return Err(ContractError::new(
                            CODE_CREDENTIAL_TYPE_MISMATCH,
                            format!(
                                "credential `{id}` has kind `{other}`; providers reference `{PROVIDER_CREDENTIAL_KIND}` credentials"
                            ),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    None => {
                        return Err(ContractError::new(
                            CODE_CREDENTIAL_TYPE_MISMATCH,
                            format!("credential `{id}` does not exist"),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                }
            }
        };

        let new_rev = providers_next_rev_in_tx(&tx)?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO v1_providers
                 (provider_id, name, protocol, endpoint, credential_ref, enabled, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(provider_id) DO UPDATE SET
                name = excluded.name,
                protocol = excluded.protocol,
                endpoint = excluded.endpoint,
                credential_ref = excluded.credential_ref,
                enabled = excluded.enabled,
                revision = excluded.revision,
                updated_at = excluded.updated_at",
            params![
                provider_id,
                name,
                protocol,
                endpoint,
                credential_ref,
                i64::from(enabled),
                new_rev,
                now,
            ],
        )
        .map_err(providers_sql_err)?;

        // Durable `provider.changed` event (contract events list). The payload
        // carries the reference form only — no secret material exists in this
        // table at all.
        append_provider_event_in_tx(
            &tx,
            &provider_id,
            "provider.changed",
            &json!({
                "providerId": provider_id,
                "revision": new_rev,
                "enabled": enabled,
                "protocol": protocol,
                "credentialRef": credential_ref,
            }),
            &now,
        )?;
        tx.commit().map_err(providers_sql_err)?;
        drop(conn);
        store.event_notifier().notify_waiters();
        store.state_notifier().notify_waiters();

        Ok(json!({"provider": {
            "providerId": provider_id,
            "name": name,
            "protocol": protocol,
            "endpoint": endpoint,
            "credentialRef": credential_ref,
            "enabled": enabled,
            "revision": new_rev,
        }}))
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, body))
}

fn append_provider_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<(), ContractError> {
    let placeholder_cursor = format!("tmp-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, '', ?3, 'agentcpm-pd.provider.event/1', ?4, ?5, ?6)",
        params![
            placeholder_cursor,
            resource_id,
            event_type,
            payload_str,
            payload_str.clone(),
            occurred_at,
        ],
    )
    .map_err(providers_sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = crate::public_v1::store::format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(providers_sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use agentcpm_pd_protocol::registry::OperationRegistry;

    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;

    use super::*;

    fn open_runtime(root_dir: &std::path::Path) -> (Arc<PublicV1Runtime>, String) {
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
        (rt, file.instance_id.clone())
    }

    fn fresh_home(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, String) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, instance_id) = open_runtime(&root_dir);
        (tmp, rt, instance_id)
    }

    fn contract_code_of(err: &DispatchError) -> String {
        match err {
            DispatchError::Application(e) => e.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    async fn write_provider(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        provider_id: &str,
        protocol: &str,
        endpoint: &str,
    ) -> Value {
        let (outcome, result) = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": provider_id,
                    "name": format!("{provider_id} display"),
                    "protocol": protocol,
                    "endpoint": endpoint,
                }),
                None,
                None,
            )
            .await
            .expect("provider.write dispatches");
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("provider.write", &result)
            .unwrap_or_else(|e| panic!("provider.write result fails schema: {e:?}"));
        result
    }

    #[tokio::test]
    async fn provider_write_get_list_round_trip() {
        let (_tmp, rt, iid) = fresh_home("prov-round");
        let created = write_provider(
            &rt,
            &iid,
            "prov-openai",
            "OpenAI",
            "https://api.openai.com/v1/",
        )
        .await;
        let provider = &created["provider"];
        assert_eq!(provider["providerId"], "prov-openai");
        assert_eq!(provider["protocol"], "openai");
        // Baseline normalization: trailing slash stripped.
        assert_eq!(provider["endpoint"], "https://api.openai.com/v1");
        assert_eq!(provider["credentialRef"], Value::Null);
        assert_eq!(provider["enabled"], true);
        assert!(provider["revision"].as_str().is_some_and(|r| !r.is_empty()));

        let (_, got) = rt
            .dispatch(
                "provider.get",
                json!({"instanceId": iid, "providerId": "prov-openai"}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("provider.get", &got)
            .unwrap();
        assert_eq!(got["provider"]["revision"], provider["revision"]);

        let (_, listed) = rt
            .dispatch("provider.list", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("provider.list", &listed)
            .unwrap();
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        assert!(
            listed["pageInfo"]["snapshotRevision"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
        );
        assert_eq!(listed["pageInfo"]["complete"], true);

        // provider.changed is durable.
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'provider.changed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn unsupported_protocol_and_bad_endpoints_are_rejected() {
        let (_tmp, rt, iid) = fresh_home("prov-invalid");
        for protocol in ["ftp", "ollama", ""] {
            let err = rt
                .dispatch(
                    "provider.write",
                    json!({
                        "instanceId": iid,
                        "providerId": "prov-x",
                        "name": "x",
                        "protocol": protocol,
                        "endpoint": "https://api.example.com",
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(
                contract_code_of(&err),
                "PROTOCOL_UNSUPPORTED",
                "protocol `{protocol}`"
            );
        }

        // All four baseline protocols are accepted (case-insensitively).
        for protocol in ["Anthropic", "openai", "openai-responses", "google"] {
            write_provider(
                &rt,
                &iid,
                &format!("prov-{protocol}"),
                protocol,
                "https://api.example.com",
            )
            .await;
        }

        // Bad endpoints: non-URL, non-http scheme, missing host,
        // secret-looking query parameters, embedded userinfo (both the
        // user:pass form and a key-shaped username), and fragments.
        for endpoint in [
            "not-a-url",
            "ftp://files.example.com",
            "https://",
            "https://api.example.com/v1?api_key=sk-live-1234",
            "https://api.example.com/v1?Authorization=Bearer%20x",
            "https://alice:sk-secret@api.example.com/v1",
            "https://sk-live-1234@api.example.com/v1",
            "https://api.example.com/v1#access_token=sk-1234",
            "https://api.example.com/v1#",
        ] {
            let err = rt
                .dispatch(
                    "provider.write",
                    json!({
                        "instanceId": iid,
                        "providerId": "prov-e",
                        "name": "e",
                        "protocol": "openai",
                        "endpoint": endpoint,
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            let code = contract_code_of(&err);
            assert!(
                code == "INVALID_ARGUMENT" || code == "PROTOCOL_UNSUPPORTED",
                "endpoint `{endpoint}`: {code}"
            );
        }
        // Nothing was written by the rejected attempts.
        let (_, listed) = rt
            .dispatch("provider.list", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(4));
    }

    #[tokio::test]
    async fn provider_write_cas_semantics() {
        let (_tmp, rt, iid) = fresh_home("prov-cas");
        let created =
            write_provider(&rt, &iid, "prov-c", "openai", "https://api.example.com").await;
        let rev1 = created["provider"]["revision"].as_str().unwrap().to_owned();

        // Create-only omission on an existing provider conflicts.
        let err = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-c",
                    "name": "c",
                    "protocol": "openai",
                    "endpoint": "https://api.example.com/v2",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // Stale revision conflicts.
        let err = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-c",
                    "expectedRevision": "rev-00000001",
                    "name": "c",
                    "protocol": "openai",
                    "endpoint": "https://api.example.com/v2",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // CAS target must exist: a revision for an absent provider conflicts.
        let err = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-missing",
                    "expectedRevision": rev1,
                    "name": "m",
                    "protocol": "openai",
                    "endpoint": "https://api.example.com",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");

        // Update with the current revision.
        let updated = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-c",
                    "expectedRevision": rev1,
                    "name": "c v2",
                    "protocol": "anthropic",
                    "endpoint": "https://api.anthropic.com",
                    "enabled": false,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(updated["provider"]["protocol"], "anthropic");
        assert_eq!(updated["provider"]["enabled"], false);
        assert_ne!(updated["provider"]["revision"], rev1.as_str());

        // Unknown id: no default fallback.
        let err = rt
            .dispatch(
                "provider.get",
                json!({"instanceId": iid, "providerId": "prov-missing"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "NOT_FOUND");
    }

    #[tokio::test]
    async fn credential_ref_is_omitted_preserved_null_cleared_and_type_checked() {
        let (_tmp, rt, iid) = fresh_home("prov-cred");
        // Seed one `api_key` and one `github_token` credential through the
        // credential domain.
        for (cid, kind) in [("cred-ok", "api_key"), ("cred-git", "github_token")] {
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid,
                    "credentialId": cid,
                    "kind": kind,
                    "name": format!("{cid} name"),
                    "secret": format!("secret-of-{cid}"),
                }),
                None,
                None,
            )
            .await
            .expect("seed credential");
        }

        let created =
            write_provider(&rt, &iid, "prov-k", "openai", "https://api.example.com").await;
        let rev1 = created["provider"]["revision"].as_str().unwrap().to_owned();

        // Attach the api_key credential.
        let attached = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-k",
                    "expectedRevision": rev1,
                    "name": "k",
                    "protocol": "openai",
                    "endpoint": "https://api.example.com",
                    "credentialRef": "cred-ok",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(attached["provider"]["credentialRef"], "cred-ok");
        let rev2 = attached["provider"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        // Omitted credentialRef preserves the current reference.
        let preserved = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-k",
                    "expectedRevision": rev2,
                    "name": "k renamed",
                    "protocol": "openai",
                    "endpoint": "https://api.example.com",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(preserved["provider"]["credentialRef"], "cred-ok");
        assert_eq!(preserved["provider"]["name"], "k renamed");
        let rev3 = preserved["provider"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        // Wrong-kind and unknown credentials are refused with the reference
        // intact (the only credential-shaped error in the contract list).
        for bad in ["cred-git", "cred-nope"] {
            let err = rt
                .dispatch(
                    "provider.write",
                    json!({
                        "instanceId": iid,
                        "providerId": "prov-k",
                        "expectedRevision": rev3,
                        "name": "k",
                        "protocol": "openai",
                        "endpoint": "https://api.example.com",
                        "credentialRef": bad,
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(
                contract_code_of(&err),
                "CREDENTIAL_TYPE_MISMATCH",
                "ref `{bad}`"
            );
        }

        // Explicit null clears.
        let cleared = rt
            .dispatch(
                "provider.write",
                json!({
                    "instanceId": iid,
                    "providerId": "prov-k",
                    "expectedRevision": rev3,
                    "name": "k",
                    "protocol": "openai",
                    "endpoint": "https://api.example.com",
                    "credentialRef": null,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(cleared["provider"]["credentialRef"], Value::Null);

        // No provider result ever carries secret material: the row has no
        // secret column at all, and the seed secret never appears.
        let (_, listed) = rt
            .dispatch("provider.list", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        let text = serde_json::to_string(&listed).unwrap();
        assert!(
            !text.contains("secret-of-"),
            "provider output must stay secret-free: {text}"
        );
    }

    #[tokio::test]
    async fn endpoint_rejects_obfuscated_query_secrets_and_never_echoes_secret_in_errors() {
        let (_tmp, rt, iid) = fresh_home("prov-endpoint-secrets");
        let leaked_tokens = [
            "sk-live-leak-111",
            "AIzaSyLeakedGoogleKey",
            "sk-semicolon-leak",
            "sk-frag-leak",
        ];
        let bad_urls = [
            "https://alice:sk-live-leak-111@api.example.com/v1",
            "https://api.example.com/v1#access_token=sk-frag-leak",
            "https://generativelanguage.googleapis.com/v1beta?key=AIzaSyLeakedGoogleKey",
            "https://api.example.com/v1?api%5Fkey=sk-live-leak-111",
            "https://api.example.com/v1?ok=1;token=sk-semicolon-leak",
            "https://api.example.com/v1?custom=sk-live-leak-111",
        ];
        for url in bad_urls {
            let err = rt
                .dispatch(
                    "provider.write",
                    json!({
                        "instanceId": iid,
                        "providerId": "prov-leak-check",
                        "name": "leak",
                        "protocol": "openai",
                        "endpoint": url,
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT", "url `{url}`");
            let rendered = match err {
                DispatchError::Application(e) => serde_json::to_string(&e).unwrap(),
                DispatchError::Rpc { message, data, .. } => {
                    format!("{message}|{}", data.unwrap_or(Value::Null))
                }
            };
            for token in leaked_tokens {
                assert!(
                    !rendered.contains(token),
                    "error message echoed secret `{token}` for `{url}`: {rendered}"
                );
            }
        }
    }
}
