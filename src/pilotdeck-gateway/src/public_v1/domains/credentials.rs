//! `credential.*` — protected credential management (`cli.task.t016`, S3).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `credential.list` / `.write`
//! / `.set_enabled` / `.remove` 条目。TS 基线（cc7c4e6）是语义参考：
//! `ui/server/routes/settings.js:119-203` + `ui/server/database/db.js:342-405`
//! —— 凭据按 `(name, type, value)` 存放、列表不回 value、toggle 启停、
//! 删除即移除。
//!
//! 安全设计（`c026`「凭据保护」）：
//! * **结果只携带指纹**：每个结果/列表项都是合同 `Credential`
//!   `{credentialId, kind, name, enabled, revision, fingerprint}` —— 没有任何
//!   secret 字段。指纹与 `access_keys.rs` 先例同构：`sha256:` + secret 哈希
//!   的前 16 个十六进制字符（对明文 secret 不可逆、稳定可比对）。
//! * **存储形态**：secret 以可恢复形态存放在 v1 store
//!   （`<dataRoot>/v1/state.sqlite`，位于 007 §8 规定的 0700 `v1` 目录之内
//!   ——与 `credentials/local.token` 同一保护边界）。可恢复是供应商凭据的
//!   功能前提（出网调用要用），而 workspace 禁新增依赖（无加密 crate），
//!   哈希-only 会废掉凭据的唯一用途（与 access_key 的「只比对」场景不同，
//!   差异已在任务报告登记）。CLI 侧 `--secret-file`/argv 通道已由 commands.rs
//!   的 secure-input-fields 规则封死，secret 只能经 `--input-json`（受保护
//!   文件或 stdin）进入。
//! * **secret 绝不进日志/事件/错误**：写入的错误消息只携带 credentialId，
//!   事件表本域零写入（合同的 events 列表为空）；单元测试对全部失败路径
//!   断言渲染后的错误不含 secret。
//! * `credential.remove` 的「仍有引用拒绝 RESOURCE_IN_USE」：任何
//!   `v1_providers.credential_ref` 指向该凭据即拒绝（同库事务内检查，与
//!   `provider.write` 在同一连接上串行）。
//! * 内部消费缝：`V1Store::resolve_credential_secret` 只对「存在且 enabled」
//!   的凭据返回 secret（「禁用后新调用不使用secret」「删除后旧ID无法读取
//!   秘密」），供后续 provider_test / 模型调用任务接线。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;

/// Contract error codes declared by the credential operations that the
/// protocol crate has no shared constant for yet.
const CODE_CREDENTIAL_TYPE_MISMATCH: &str = "CREDENTIAL_TYPE_MISMATCH";
const CODE_RESOURCE_IN_USE: &str = "RESOURCE_IN_USE";

const CREDENTIALS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS v1_credentials (
    credential_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    secret TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_credentials", CREDENTIALS_TABLE_DDL);
    reg.handler("credential.list", |rt, input, _key, _rev| {
        Box::pin(credential_list(rt, input))
    });
    reg.handler("credential.write", |rt, input, _key, rev| {
        Box::pin(credential_write(rt, input, rev))
    });
    reg.handler("credential.set_enabled", |rt, input, _key, rev| {
        Box::pin(credential_set_enabled(rt, input, rev))
    });
    reg.handler("credential.remove", |rt, input, _key, rev| {
        Box::pin(credential_remove(rt, input, rev))
    });
}

// ---------------------------------------------------------------------------
// Secret material handling
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Display-only fingerprint, derived from the stored secret's hash (never
/// the plaintext) — same scheme as `access_keys.rs::fingerprint_of`.
fn fingerprint_of(secret: &str) -> String {
    let hash = sha256_hex(secret.as_bytes());
    format!("sha256:{}", &hash[..hash.len().min(16)])
}

// ---------------------------------------------------------------------------
// Row model and helpers
// ---------------------------------------------------------------------------

struct StoredCredential {
    credential_id: String,
    kind: String,
    name: String,
    enabled: bool,
    revision: String,
    fingerprint: String,
}

impl StoredCredential {
    /// The contract `Credential` shape — metadata and fingerprint only, no
    /// secret (「无明文secret」).
    fn to_contract_json(&self) -> Value {
        json!({
            "credentialId": self.credential_id,
            "kind": self.kind,
            "name": self.name,
            "enabled": self.enabled,
            "revision": self.revision,
            "fingerprint": self.fingerprint,
        })
    }
}

const CREDENTIAL_COLUMNS: &str = "credential_id, kind, name, enabled, revision, fingerprint";

fn row_to_credential(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCredential> {
    Ok(StoredCredential {
        credential_id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        enabled: row.get::<_, i64>(3)? != 0,
        revision: row.get(4)?,
        fingerprint: row.get(5)?,
    })
}

fn load_credential(
    conn: &Connection,
    credential_id: &str,
) -> Result<Option<StoredCredential>, ContractError> {
    conn.query_row(
        &format!("SELECT {CREDENTIAL_COLUMNS} FROM v1_credentials WHERE credential_id = ?1"),
        params![credential_id],
        row_to_credential,
    )
    .optional()
    .map_err(credentials_sql_err)
}

fn credentials_sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn credentials_invalid_argument(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn credential_not_found(credential_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("credential `{credential_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn credentials_revision_conflict(expected: &str, current: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("credential expectedRevision `{expected}` does not match `{current}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn credentials_next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(credentials_sql_err)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(credentials_sql_err)?;
    Ok(format!("rev-{next:08}"))
}

fn credentials_read_current_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(credentials_sql_err)?
        .unwrap_or(1);
    Ok(format!("rev-{cur:08}"))
}

// ---------------------------------------------------------------------------
// Operation handlers
// ---------------------------------------------------------------------------

async fn credential_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let kind_filter = input.get("kind").and_then(Value::as_str);
    let page = input.get("page").and_then(Value::as_object);
    let limit = page
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let offset = match page.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            credentials_invalid_argument(format!("invalid pagination cursor `{cursor}`"))
        })?,
        None => 0,
    };

    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let snapshot_revision = credentials_read_current_rev(&conn)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM v1_credentials
             ORDER BY created_at DESC, credential_id ASC"
        ))
        .map_err(credentials_sql_err)?;
    let mut credentials = stmt
        .query_map([], row_to_credential)
        .map_err(credentials_sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(credentials_sql_err)?;
    drop(stmt);
    drop(conn);
    // 「类型过滤准确」: exact kind match.
    if let Some(kind) = kind_filter {
        credentials.retain(|c| c.kind == kind);
    }

    let total = credentials.len();
    if offset > total {
        return Err(
            credentials_invalid_argument("pagination cursor beyond end of collection").into(),
        );
    }
    let end = offset.saturating_add(limit).min(total);
    let complete = end >= total;
    let items: Vec<Value> = credentials
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|c| c.to_contract_json())
        .collect();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": snapshot_revision,
                "nextCursor": if complete { Value::Null } else { json!(end.to_string()) },
                "complete": complete,
            },
        }),
    ))
}

async fn credential_write(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let credential_id = input
        .get("credentialId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if credential_id.is_empty() {
        return Err(credentials_invalid_argument("credentialId is required").into());
    }
    let kind = input
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if kind.is_empty() {
        return Err(credentials_invalid_argument(
            "credential kind is required and must not be blank",
        )
        .into());
    }
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if name.is_empty() {
        return Err(credentials_invalid_argument(
            "credential name is required and must not be blank",
        )
        .into());
    }
    let secret = input
        .get("secret")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if secret.trim().is_empty() {
        return Err(credentials_invalid_argument(
            "credential secret is required and must not be blank",
        )
        .into());
    }
    let enabled = input
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let expected = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str));

    let store = rt.store();
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(credentials_sql_err)?;

    let existing = load_credential(&tx, &credential_id)?;
    match (expected, &existing) {
        // Create: omitted revision is only usable when absent (same rule as
        // model.write / provider.write —「创建省revision仅不存在可用」).
        (None, Some(current)) => {
            return Err(credentials_revision_conflict("<omitted>", &current.revision).into());
        }
        (Some(_), None) => {
            return Err(
                credentials_revision_conflict(expected.unwrap_or_default(), "<absent>").into(),
            );
        }
        (Some(expected), Some(current)) if current.revision != expected => {
            return Err(credentials_revision_conflict(expected, &current.revision).into());
        }
        _ => {}
    }

    // 「更新类型不匹配拒绝保留旧值」: replacing a credential with a
    // different kind is refused; the old value (and kind) stay intact.
    if let Some(current) = &existing
        && current.kind != kind
    {
        return Err(ContractError::new(
            CODE_CREDENTIAL_TYPE_MISMATCH,
            format!(
                "credential `{credential_id}` has kind `{}`; it cannot be replaced with kind `{kind}`",
                current.kind
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    // The secret is stored verbatim (no trimming — silently rewriting a
    // secret risks corrupting it; only a blank check runs). The baseline
    // trims; this deviation is registered in the task report.
    let fingerprint = fingerprint_of(secret);
    let new_rev = credentials_next_rev_in_tx(&tx)?;
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO v1_credentials
             (credential_id, kind, name, secret, fingerprint, enabled, revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(credential_id) DO UPDATE SET
            kind = excluded.kind,
            name = excluded.name,
            secret = excluded.secret,
            fingerprint = excluded.fingerprint,
            enabled = excluded.enabled,
            revision = excluded.revision,
            updated_at = excluded.updated_at",
        params![
            credential_id,
            kind,
            name,
            secret,
            fingerprint,
            i64::from(enabled),
            new_rev,
            now,
        ],
    )
    .map_err(credentials_sql_err)?;
    tx.commit().map_err(credentials_sql_err)?;

    Ok((
        ResponseOutcome::Succeeded,
        json!({"credential": {
            "credentialId": credential_id,
            "kind": kind,
            "name": name,
            "enabled": enabled,
            "revision": new_rev,
            "fingerprint": fingerprint,
        }}),
    ))
}

async fn credential_set_enabled(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let credential_id = input
        .get("credentialId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let enabled = input
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| credentials_invalid_argument("`enabled` must be a boolean"))?;
    let expected = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();

    let store = rt.store();
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(credentials_sql_err)?;

    let Some(current) = load_credential(&tx, &credential_id)? else {
        return Err(credential_not_found(&credential_id).into());
    };
    if current.revision != expected {
        return Err(credentials_revision_conflict(&expected, &current.revision).into());
    }
    if current.enabled == enabled {
        // No state change: confirmed current state, no revision bump (the
        // access_keys precedent for same-value conditional writes).
        tx.commit().map_err(credentials_sql_err)?;
        return Ok((
            ResponseOutcome::Succeeded,
            json!({"credential": current.to_contract_json()}),
        ));
    }

    let new_rev = credentials_next_rev_in_tx(&tx)?;
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "UPDATE v1_credentials SET enabled = ?1, revision = ?2, updated_at = ?3
         WHERE credential_id = ?4",
        params![i64::from(enabled), new_rev, now, credential_id],
    )
    .map_err(credentials_sql_err)?;
    tx.commit().map_err(credentials_sql_err)?;

    let mut updated = current;
    updated.enabled = enabled;
    updated.revision = new_rev;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"credential": updated.to_contract_json()}),
    ))
}

async fn credential_remove(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let credential_id = input
        .get("credentialId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expected = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();

    let store = rt.store();
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(credentials_sql_err)?;

    let Some(current) = load_credential(&tx, &credential_id)? else {
        return Err(credential_not_found(&credential_id).into());
    };
    if current.revision != expected {
        return Err(credentials_revision_conflict(&expected, &current.revision).into());
    }

    // 「仍有引用拒绝RESOURCE_IN_USE」: any provider referencing the
    // credential blocks removal (same-connection transaction, so it is
    // serialized against concurrent provider.write).
    let referencing: Option<String> = tx
        .query_row(
            "SELECT provider_id FROM v1_providers WHERE credential_ref = ?1 LIMIT 1",
            params![credential_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(credentials_sql_err)?;
    if let Some(provider_id) = referencing {
        return Err(ContractError::new(
            CODE_RESOURCE_IN_USE,
            format!("credential `{credential_id}` is referenced by provider `{provider_id}`"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    tx.execute(
        "DELETE FROM v1_credentials WHERE credential_id = ?1",
        params![credential_id],
    )
    .map_err(credentials_sql_err)?;
    let new_rev = credentials_next_rev_in_tx(&tx)?;
    tx.commit().map_err(credentials_sql_err)?;

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "removedCredentialId": credential_id,
            "revision": new_rev,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Internal consumption seam (usage sites: provider_test / model calls, later
// S3 tasks; not a public operation)
// ---------------------------------------------------------------------------

impl V1Store {
    /// Returns the stored secret for an **existing and enabled** credential,
    /// or `None` for a missing, disabled, or deleted credential
    /// (「禁用后新调用不使用secret」「删除后旧ID无法读取秘密」). Failures
    /// answer `None` rather than erroring so callers treat them as
    /// "no usable credential" — the same fail-closed shape
    /// `access_keys.rs::authenticate_secret` uses.
    ///
    /// Unused in the S3 build outside tests: the consumers are the later
    /// provider-test / model-invocation tasks, and no shared-path caller may
    /// be wired before those seams are approved (the `access_keys.rs`
    /// precedent for suppressing the warning deliberately).
    #[allow(dead_code)]
    pub(crate) fn resolve_credential_secret(
        &self,
        credential_id: &str,
    ) -> Result<Option<String>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let secret: Option<String> = conn
            .query_row(
                "SELECT secret FROM v1_credentials WHERE credential_id = ?1 AND enabled = 1",
                params![credential_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(credentials_sql_err)?;
        Ok(secret)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use agentcpm_pd_protocol::registry::OperationRegistry;

    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::InstanceMeta;
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

    fn application_error(err: DispatchError) -> ContractError {
        match err {
            DispatchError::Application(err) => err,
            DispatchError::Rpc { code, message, .. } => {
                panic!("expected an application error, got rpc {code}: {message}")
            }
        }
    }

    async fn write_credential(
        rt: &Arc<PublicV1Runtime>,
        iid: &str,
        credential_id: &str,
        kind: &str,
        name: &str,
        secret: &str,
    ) -> Value {
        let (outcome, result) = rt
            .dispatch(
                "credential.write",
                json!({
                    "instanceId": iid,
                    "credentialId": credential_id,
                    "kind": kind,
                    "name": name,
                    "secret": secret,
                }),
                None,
                None,
            )
            .await
            .expect("credential.write dispatches");
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("credential.write", &result)
            .unwrap_or_else(|e| panic!("credential.write result fails schema: {e:?}"));
        result
    }

    async fn list_credentials(rt: &Arc<PublicV1Runtime>, iid: &str, kind: Option<&str>) -> Value {
        let input = match kind {
            Some(kind) => json!({"instanceId": iid, "kind": kind}),
            None => json!({"instanceId": iid}),
        };
        let (outcome, result) = rt
            .dispatch("credential.list", input, None, None)
            .await
            .expect("credential.list dispatches");
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        OperationRegistry::global()
            .validate_result("credential.list", &result)
            .unwrap_or_else(|e| panic!("credential.list result fails schema: {e:?}"));
        result
    }

    #[tokio::test]
    async fn write_then_list_never_exposes_the_secret() {
        let (_tmp, rt, iid) = fresh_home("cred-round");
        let secret = "sk-live-abcdef0123456789";
        let created = write_credential(&rt, &iid, "cred-1", "api_key", "Primary", secret).await;
        let credential = &created["credential"];
        assert_eq!(credential["credentialId"], "cred-1");
        assert_eq!(credential["kind"], "api_key");
        assert_eq!(credential["name"], "Primary");
        assert_eq!(credential["enabled"], true);
        assert!(
            credential["fingerprint"]
                .as_str()
                .is_some_and(|f| f.starts_with("sha256:")),
            "fingerprint must be hash-derived: {credential}"
        );
        assert_eq!(credential["fingerprint"], fingerprint_of(secret));

        let listed = list_credentials(&rt, &iid, None).await;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        let text = serde_json::to_string(&listed).unwrap();
        assert!(
            !text.contains(secret),
            "the list must never expose the secret: {text}"
        );
        assert!(
            !text.contains("secret"),
            "list items must not even carry a secret-named field: {text}"
        );

        // The fingerprint is stable and comparable across listings.
        let again = list_credentials(&rt, &iid, Some("api_key")).await;
        assert_eq!(again["items"][0]["fingerprint"], credential["fingerprint"]);
        // 「类型过滤准确」.
        let other_kind = list_credentials(&rt, &iid, Some("github_token")).await;
        assert_eq!(other_kind["items"].as_array().map(Vec::len), Some(0));
        assert_eq!(other_kind["pageInfo"]["complete"], true);
    }

    #[tokio::test]
    async fn write_is_conditional_and_kind_changes_are_refused() {
        let (_tmp, rt, iid) = fresh_home("cred-cas");
        let created = write_credential(&rt, &iid, "cred-c", "api_key", "c", "secret-one").await;
        let rev1 = created["credential"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        // Omitted revision on an existing credential conflicts.
        let err = application_error(
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-c",
                    "kind": "api_key",
                    "name": "c",
                    "secret": "secret-two",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "REVISION_CONFLICT");

        // Stale revision conflicts.
        let err = application_error(
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-c",
                    "expectedRevision": "rev-00000001",
                    "kind": "api_key",
                    "name": "c",
                    "secret": "secret-two",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "REVISION_CONFLICT");

        // 「更新类型不匹配拒绝保留旧值」.
        let err = application_error(
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-c",
                    "expectedRevision": rev1,
                    "kind": "github_token",
                    "name": "c",
                    "secret": "secret-two",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "CREDENTIAL_TYPE_MISMATCH");
        let err_text = serde_json::to_string(&err).unwrap();
        assert!(
            !err_text.contains("secret-one") && !err_text.contains("secret-two"),
            "the mismatch error must not leak either secret: {err_text}"
        );

        // Replace with the current revision and matching kind.
        let replaced = rt
            .dispatch(
                "credential.write",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-c",
                    "expectedRevision": rev1,
                    "kind": "api_key",
                    "name": "c rotated",
                    "secret": "secret-two",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(replaced["credential"]["name"], "c rotated");
        assert_eq!(
            replaced["credential"]["fingerprint"],
            fingerprint_of("secret-two")
        );
        let rev2 = replaced["credential"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(rev1, rev2);

        // Blank inputs are rejected.
        for (kind, name, secret) in [
            ("", "n", "s"),
            ("api_key", "  ", "s"),
            ("api_key", "n", "   "),
        ] {
            let err = application_error(
                rt.dispatch(
                    "credential.write",
                    json!({
                        "instanceId": iid,
                        "credentialId": "cred-x",
                        "kind": kind,
                        "name": name,
                        "secret": secret,
                    }),
                    None,
                    None,
                )
                .await
                .unwrap_err(),
            );
            assert_eq!(err.code, "INVALID_ARGUMENT", "kind={kind:?} name={name:?}");
        }
    }

    #[tokio::test]
    async fn set_enabled_is_conditional_and_leaves_others_untouched() {
        let (_tmp, rt, iid) = fresh_home("cred-toggle");
        write_credential(&rt, &iid, "cred-a", "api_key", "a", "secret-a").await;
        let created_b = write_credential(&rt, &iid, "cred-b", "api_key", "b", "secret-b").await;
        let rev_b = created_b["credential"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        // Stale revision.
        let err = application_error(
            rt.dispatch(
                "credential.set_enabled",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-b",
                    "expectedRevision": "rev-00000001",
                    "enabled": false,
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "REVISION_CONFLICT");

        // Missing credential.
        let err = application_error(
            rt.dispatch(
                "credential.set_enabled",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-missing",
                    "expectedRevision": rev_b,
                    "enabled": false,
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "NOT_FOUND");

        let disabled = rt
            .dispatch(
                "credential.set_enabled",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-b",
                    "expectedRevision": rev_b,
                    "enabled": false,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        OperationRegistry::global()
            .validate_result("credential.set_enabled", &disabled)
            .unwrap();
        assert_eq!(disabled["credential"]["enabled"], false);
        let rev_b2 = disabled["credential"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(rev_b, rev_b2);

        // Same-value write is a clean no-op.
        let noop = rt
            .dispatch(
                "credential.set_enabled",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-b",
                    "expectedRevision": rev_b2,
                    "enabled": false,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(noop["credential"]["revision"], rev_b2.as_str());

        // 「不改其他凭据及引用」: cred-a keeps its state and fingerprint.
        let listed = list_credentials(&rt, &iid, None).await;
        let by_id: std::collections::BTreeMap<&str, &Value> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["credentialId"].as_str().unwrap(), c))
            .collect();
        assert_eq!(by_id["cred-a"]["enabled"], true);
        assert_eq!(by_id["cred-a"]["fingerprint"], fingerprint_of("secret-a"));
        assert_eq!(by_id["cred-b"]["enabled"], false);

        // 「禁用后新调用不使用secret」: the internal seam stops resolving.
        assert_eq!(
            rt.store().resolve_credential_secret("cred-b").unwrap(),
            None,
            "a disabled credential must not yield its secret"
        );
        assert_eq!(
            rt.store().resolve_credential_secret("cred-a").unwrap(),
            Some("secret-a".to_owned())
        );
        assert_eq!(
            rt.store()
                .resolve_credential_secret("cred-missing")
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn remove_refuses_referenced_credentials_then_deletes() {
        let (_tmp, rt, iid) = fresh_home("cred-remove");
        let secret = "secret-removable";
        let created = write_credential(&rt, &iid, "cred-r", "api_key", "r", secret).await;
        let rev1 = created["credential"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        // Reference it from a provider.
        rt.dispatch(
            "provider.write",
            json!({
                "instanceId": iid,
                "providerId": "prov-r",
                "name": "r",
                "protocol": "openai",
                "endpoint": "https://api.example.com",
                "credentialRef": "cred-r",
            }),
            None,
            None,
        )
        .await
        .unwrap();

        // 「仍有引用拒绝RESOURCE_IN_USE」.
        let err = application_error(
            rt.dispatch(
                "credential.remove",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-r",
                    "expectedRevision": rev1,
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "RESOURCE_IN_USE");
        assert_eq!(err.effect, EffectStatus::NotApplied);
        // The credential is still there and still resolvable.
        assert_eq!(
            rt.store().resolve_credential_secret("cred-r").unwrap(),
            Some(secret.to_owned())
        );

        // Stale revision / unknown id.
        let err = application_error(
            rt.dispatch(
                "credential.remove",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-r",
                    "expectedRevision": "rev-00000001",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "REVISION_CONFLICT");
        let err = application_error(
            rt.dispatch(
                "credential.remove",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-missing",
                    "expectedRevision": rev1,
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(err.code, "NOT_FOUND");

        // Detach the reference, then remove.
        let prov = rt
            .dispatch(
                "provider.get",
                json!({"instanceId": iid, "providerId": "prov-r"}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        rt.dispatch(
            "provider.write",
            json!({
                "instanceId": iid,
                "providerId": "prov-r",
                "expectedRevision": prov["provider"]["revision"],
                "name": "r",
                "protocol": "openai",
                "endpoint": "https://api.example.com",
                "credentialRef": null,
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let removed = rt
            .dispatch(
                "credential.remove",
                json!({
                    "instanceId": iid,
                    "credentialId": "cred-r",
                    "expectedRevision": rev1,
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        OperationRegistry::global()
            .validate_result("credential.remove", &removed)
            .unwrap();
        assert_eq!(removed["removedCredentialId"], "cred-r");
        assert!(
            removed["revision"].as_str().is_some_and(|r| !r.is_empty()),
            "the post-removal store revision is required: {removed}"
        );

        // 「删除后旧ID无法读取秘密」.
        assert_eq!(
            rt.store().resolve_credential_secret("cred-r").unwrap(),
            None
        );
        let listed = list_credentials(&rt, &iid, None).await;
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn no_error_path_ever_leaks_the_secret() {
        let (_tmp, rt, iid) = fresh_home("cred-leak");
        let secret = "sk-live-leakprobe-000";
        let created = write_credential(&rt, &iid, "cred-l", "api_key", "l", secret).await;
        let rev = created["credential"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        let failures = vec![
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid, "credentialId": "cred-l",
                    "kind": "api_key", "name": "l", "secret": "sk-live-other",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid, "credentialId": "cred-l",
                    "kind": "github_token", "name": "l", "secret": "sk-live-other",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid, "credentialId": "cred-l",
                    "expectedRevision": "stale", "kind": "api_key",
                    "name": "l", "secret": "sk-live-other",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": iid, "credentialId": "cred-x",
                    "kind": "api_key", "name": "   ", "secret": "sk-live-other",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.set_enabled",
                json!({
                    "instanceId": iid, "credentialId": "cred-missing",
                    "expectedRevision": rev, "enabled": true,
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.set_enabled",
                json!({
                    "instanceId": iid, "credentialId": "cred-l",
                    "expectedRevision": "stale", "enabled": true,
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.remove",
                json!({
                    "instanceId": iid, "credentialId": "cred-missing",
                    "expectedRevision": rev,
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.remove",
                json!({
                    "instanceId": iid, "credentialId": "cred-l",
                    "expectedRevision": "stale",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.list",
                json!({"instanceId": iid, "page": {"cursor": "bogus"}}),
                None,
                None,
            )
            .await
            .unwrap_err(),
            rt.dispatch(
                "credential.write",
                json!({
                    "instanceId": "another", "credentialId": "cred-mismatch",
                    "kind": "api_key", "name": "m", "secret": "sk-live-other",
                }),
                None,
                None,
            )
            .await
            .unwrap_err(),
        ];
        // 10 enumerated failure paths: write without revision, write with a
        // foreign kind, write with a stale revision, write with a blank
        // name, set_enabled/remove on a missing credential,
        // set_enabled/remove with a stale revision, list with a bogus
        // cursor, write with a foreign instanceId.
        assert_eq!(failures.len(), 10);
        for err in failures {
            let rendered = match err {
                DispatchError::Application(err) => serde_json::to_string(&err).unwrap(),
                DispatchError::Rpc { message, data, .. } => {
                    format!("{message}|{}", data.unwrap_or(Value::Null))
                }
            };
            assert!(
                !rendered.contains(secret),
                "an error path leaked the secret: {rendered}"
            );
        }

        // The durable event log stays secret-free too (this domain writes no
        // events by contract; assert nothing leaked from anywhere).
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let event_text: Option<String> = conn
            .query_row(
                "SELECT payload_json FROM events WHERE payload_json LIKE '%' || ?1 || '%'",
                params![secret],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        drop(conn);
        assert!(
            event_text.is_none(),
            "the secret must never appear in the event log"
        );
    }
}
