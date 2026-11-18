//! `policy.*` — scoped permission policies (S3 `cli.task.t005`, Phase A).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `policy.get` / `policy.update`
//! 条目:scope(instance|project|session)、mode(ask|deny|allowConfigured)与
//! rules(ruleId/effect/tool/resourcePattern)的形状一律照 schema。TS 基线
//! (cc7c4e6)是语义参考,不逐字复刻:
//! - `ui/server/services/permissionSettings.js` — 全局 `permissions.json` 的
//!   allowedTools/askTools/disallowedTools + skipPermissions,整体读写、无 CAS;
//! - `src/permission/protocol/types.ts` — 规则 = behavior(allow|deny|ask) +
//!   toolName + 可选 pattern,mode = default|plan|bypassPermissions;
//! - `src/cli/createLocalGateway.ts:1371`(`setPermissionMode`)与
//!   `src/gateway/client/GatewayInteractionCoordinator.ts:128`
//!   (`grantSessionPermission`) — 会话级 mode override 与会话 allow 授权,
//!   均不校验 sessionKey 是否已存在。
//! 新合同把这些归一为 scope 化的单一 Policy 对象(整体条件替换),继承链
//! instance ← project ← session:未定义的 scope 继承最近已定义祖先;全链未
//! 定义时返回默认策略(mode=ask、rules=[],revision=实例 initialization
//! _revision——保证首次 update 的 CAS 有确定性的 expected 值)。
//!
//! 会话权限的**即时授予**(`grant_session_permission` / interaction 的
//! remember-scope)是 run/interaction 域(substrate 拥有 `domains/run.rs`、
//! `interaction.rs` 与 `store.rs::respond_interaction`)的缝合点,本文件不改
//! 它们;本文件只提供策略的持久化与读取面(`V1Store::get_policy` /
//! `update_policy` + `PolicyScope`),供其后续接线「安全撤权实时重查」。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError, EffectStatus,
    RetrySpec,
};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;

/// Domain-registered DDL, executed by `V1Store::init_schema` after the core
/// S2 tables (S3 substrate rule: domains add tables through `reg.schema`
/// only; `store.rs`'s static batch stays untouched).
const POLICY_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS policies (
    scope_key TEXT PRIMARY KEY,
    scope_json TEXT NOT NULL,
    mode TEXT NOT NULL,
    rules_json TEXT NOT NULL,
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("policies", POLICY_TABLE_DDL);
    reg.handler("policy.get", |rt, input, _key, _rev| {
        Box::pin(policy_get(rt, input))
    });
    reg.handler("policy.update", |rt, input, _key, rev| {
        Box::pin(policy_update(rt, input, rev))
    });
}

async fn policy_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = PolicyScope::from_json(&input["scope"])?;
    let view = rt.store().get_policy(&scope)?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"policy": view.policy, "inheritedFrom": view.inherited_from}),
    ))
}

async fn policy_update(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = PolicyScope::from_json(&input["scope"])?;
    // The transport-level expectedRevision (envelope parameter) wins, the
    // input field is the fallback — same precedence as `queue.resume`.
    let expected = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default();
    let mode = input["mode"].as_str().unwrap_or_default();
    let rules = input.get("rules").cloned().unwrap_or(Value::Null);
    let policy = rt.store().update_policy(&scope, expected, mode, &rules)?;
    Ok((ResponseOutcome::Succeeded, json!({"policy": policy})))
}

// ---------------------------------------------------------------------------
// Scope model
// ---------------------------------------------------------------------------

/// One policy scope, shaped exactly as the contract's `$defs.Scope` oneOf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PolicyScope {
    Instance,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl PolicyScope {
    /// Parses a validated contract `Scope` object. Dispatch already ran
    /// `validate_input`, so structural errors are defensive only; they answer
    /// `INVALID_ARGUMENT` instead of panicking.
    pub(crate) fn from_json(scope: &Value) -> Result<Self, ContractError> {
        let invalid = || {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                "policy scope must be one of {kind:instance}, {kind:project,projectId} or {kind:session,projectId,sessionId}",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        };
        let Some(kind) = scope.get("kind").and_then(Value::as_str) else {
            return Err(invalid());
        };
        match kind {
            "instance" => Ok(Self::Instance),
            "project" => {
                let Some(project_id) = scope.get("projectId").and_then(Value::as_str) else {
                    return Err(invalid());
                };
                Ok(Self::Project {
                    project_id: project_id.to_owned(),
                })
            }
            "session" => {
                let (Some(project_id), Some(session_id)) = (
                    scope.get("projectId").and_then(Value::as_str),
                    scope.get("sessionId").and_then(Value::as_str),
                ) else {
                    return Err(invalid());
                };
                Ok(Self::Session {
                    project_id: project_id.to_owned(),
                    session_id: session_id.to_owned(),
                })
            }
            _ => Err(invalid()),
        }
    }

    /// Canonical JSON for this scope. Built from the parsed fields (never from
    /// the raw input) so equal scopes always serialize identically — the
    /// serialized form is the `policies.scope_key` primary key.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Instance => json!({"kind": "instance"}),
            Self::Project { project_id } => {
                json!({"kind": "project", "projectId": project_id})
            }
            Self::Session {
                project_id,
                session_id,
            } => json!({"kind": "session", "projectId": project_id, "sessionId": session_id}),
        }
    }

    fn key(&self) -> String {
        serde_json::to_string(&self.to_json()).unwrap_or_else(|_| "?".to_owned())
    }

    /// The scopes this one inherits from when it defines no policy of its
    /// own, nearest first: session → project → instance.
    fn inheritance_chain(&self) -> Vec<Self> {
        match self {
            Self::Instance => Vec::new(),
            Self::Project { .. } => vec![Self::Instance],
            Self::Session { project_id, .. } => vec![
                Self::Project {
                    project_id: project_id.clone(),
                },
                Self::Instance,
            ],
        }
    }
}

/// The resolved answer of `policy.get`: the effective contract `Policy`
/// object plus the ancestor scopes its content was inherited from (empty
/// when the scope itself — or nothing — provided it).
pub(crate) struct PolicyView {
    pub policy: Value,
    pub inherited_from: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Storage (impl V1Store block, S3 substrate rule §1.4)
// ---------------------------------------------------------------------------

impl V1Store {
    /// `policy.get` data plane: resolves the effective policy for `scope`
    /// without side effects (contract postcondition: 无业务状态修改).
    pub(crate) fn get_policy(&self, scope: &PolicyScope) -> Result<PolicyView, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        self.resolve_policy(&conn, scope)
    }

    /// `policy.update` data plane: conditional replace of `scope`'s policy.
    /// `expected_revision` must equal the scope's currently-effective
    /// revision (the same value `policy.get` reports — the scope's own, the
    /// nearest defined ancestor's, or the initialization revision when the
    /// whole chain is undefined); a mismatch answers `REVISION_CONFLICT`
    /// with nothing applied. A successful update bumps the store revision,
    /// appends a durable `policy.changed` event, and returns the new
    /// contract `Policy` object.
    pub(crate) fn update_policy(
        &self,
        scope: &PolicyScope,
        expected_revision: &str,
        mode: &str,
        rules: &Value,
    ) -> Result<Value, ContractError> {
        validate_policy_update(mode, rules)?;

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(policy_sql_err)?;

        let current = self.resolve_policy(&tx, scope)?;
        let current_rev = current.policy["revision"].as_str().unwrap_or_default();
        if expected_revision != current_rev {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "policy expectedRevision `{expected_revision}` does not match `{current_rev}` for scope {}",
                    scope.key()
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        let new_rev = policy_next_rev_in_tx(&tx)?;
        let now = Utc::now().to_rfc3339();
        let scope_json = serde_json::to_string(&scope.to_json()).unwrap_or_default();
        let rules_json = serde_json::to_string(rules).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT INTO policies (scope_key, scope_json, mode, rules_json, revision, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(scope_key) DO UPDATE SET
                scope_json = excluded.scope_json,
                mode = excluded.mode,
                rules_json = excluded.rules_json,
                revision = excluded.revision,
                updated_at = excluded.updated_at",
            params![scope.key(), scope_json, mode, rules_json, new_rev, now],
        )
        .map_err(policy_sql_err)?;

        // Durable `policy.changed` event (contract events list). The event
        // resource is the scope key so subscription consumers can filter by
        // it the way they filter runs.
        let session_column = match scope {
            PolicyScope::Session { session_id, .. } => session_id.clone(),
            _ => String::new(),
        };
        append_policy_event_in_tx(
            &tx,
            &scope.key(),
            &session_column,
            "policy.changed",
            &json!({
                "scope": scope.to_json(),
                "revision": new_rev,
                "mode": mode,
                "rules": rules,
            }),
            &now,
        )?;

        tx.commit().map_err(policy_sql_err)?;
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();

        Ok(json!({
            "scope": scope.to_json(),
            "revision": new_rev,
            "mode": mode,
            "rules": rules,
            "effectiveGeneration": self.meta().generation,
        }))
    }

    /// Shared resolution: walk the scope's own record first, then its
    /// inheritance chain (nearest first), and report the first defined
    /// policy with the *requested* scope label (contract acceptance:
    /// 实际有效策略可解释). With nothing defined anywhere the default
    /// policy is `ask` with an empty rule set; its revision is the
    /// instance's initialization revision, a deterministic per-instance
    /// value so the first `policy.update` can CAS against it.
    fn resolve_policy(
        &self,
        conn: &Connection,
        scope: &PolicyScope,
    ) -> Result<PolicyView, ContractError> {
        let mut chain = vec![scope.clone()];
        chain.extend(scope.inheritance_chain());
        for candidate in &chain {
            if let Some((mode, rules, revision)) = fetch_policy_row(conn, &candidate.key())? {
                let inherited_from = if candidate == scope {
                    Vec::new()
                } else {
                    vec![candidate.to_json()]
                };
                return Ok(PolicyView {
                    policy: json!({
                        "scope": scope.to_json(),
                        "revision": revision,
                        "mode": mode,
                        "rules": rules,
                        "effectiveGeneration": self.meta().generation,
                    }),
                    inherited_from,
                });
            }
        }
        Ok(PolicyView {
            policy: json!({
                "scope": scope.to_json(),
                "revision": self.meta().initialization_revision,
                "mode": "ask",
                "rules": [],
                "effectiveGeneration": self.meta().generation,
            }),
            inherited_from: Vec::new(),
        })
    }
}

/// Reads one stored policy row: `(mode, rules, revision)`.
fn fetch_policy_row(
    conn: &Connection,
    scope_key: &str,
) -> Result<Option<(String, Value, String)>, ContractError> {
    conn.query_row(
        "SELECT mode, rules_json, revision FROM policies WHERE scope_key = ?1",
        params![scope_key],
        |r| {
            let mode: String = r.get(0)?;
            let rules_json: String = r.get(1)?;
            let revision: String = r.get(2)?;
            Ok((mode, rules_json, revision))
        },
    )
    .optional()
    .map_err(policy_sql_err)
    .map(|row| {
        row.map(|(mode, rules_json, revision)| {
            let rules: Value = serde_json::from_str(&rules_json).unwrap_or(json!([]));
            (mode, rules, revision)
        })
    })
}

/// Semantical validation beyond the JSON schema: the contract's acceptance
/// 「无效规则整体拒绝」— a rule set with duplicate or empty `ruleId`s, or a
/// non-array `rules` value, is rejected before any state is touched.
fn validate_policy_update(mode: &str, rules: &Value) -> Result<(), ContractError> {
    let invalid = |message: String| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    if !matches!(mode, "ask" | "deny" | "allowConfigured") {
        return Err(invalid(format!(
            "unsupported policy mode `{mode}`; expected one of ask|deny|allowConfigured"
        )));
    }
    let Some(rule_array) = rules.as_array() else {
        return Err(invalid(
            "policy rules must be an array of rule objects".to_owned(),
        ));
    };
    let mut seen = std::collections::BTreeSet::new();
    for rule in rule_array {
        let rule_id = rule
            .get("ruleId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if rule_id.is_empty() {
            return Err(invalid("policy rule is missing ruleId".to_owned()));
        }
        if !seen.insert(rule_id.to_owned()) {
            return Err(invalid(format!(
                "duplicate policy ruleId `{rule_id}`; rules are rejected as a whole"
            )));
        }
    }
    Ok(())
}

fn policy_sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

/// Same revision counter `store.rs::next_rev_in_tx` advances (private there,
/// so policy owns an identical copy over the same `meta.rev_counter` row —
/// both run inside the store's single serialized connection).
fn policy_next_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(policy_sql_err)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(policy_sql_err)?;
    Ok(format!("rev-{next:08}"))
}

/// Appends one durable event into the shared `events` table, mirroring
/// `store.rs::append_event_in_tx`'s two-step cursor assignment (private
/// there, so policy owns an identical copy).
fn append_policy_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<(), ContractError> {
    let placeholder_cursor = format!("tmp-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.policy.event/1', ?5, ?6, ?7)",
        params![
            placeholder_cursor,
            resource_id,
            session_id,
            event_type,
            payload_str,
            payload_str.clone(),
            occurred_at,
        ],
    )
    .map_err(policy_sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = crate::public_v1::store::format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(policy_sql_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests: contract-shaped assertions against the real store + dispatch path
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_pd_protocol::registry::OperationRegistry;

    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;

    fn open_runtime(root_dir: &std::path::Path) -> (Arc<PublicV1Runtime>, String, String) {
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
        (
            rt,
            file.instance_id.clone(),
            file.initialization_revision.clone(),
        )
    }

    fn fresh_home(tag: &str) -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        (tmp, root_dir)
    }

    fn instance_scope() -> Value {
        json!({"kind": "instance"})
    }

    fn project_scope(project_id: &str) -> Value {
        json!({"kind": "project", "projectId": project_id})
    }

    fn session_scope(project_id: &str, session_id: &str) -> Value {
        json!({"kind": "session", "projectId": project_id, "sessionId": session_id})
    }

    fn sample_rules() -> Value {
        json!([
            {"ruleId": "r-bash-1", "effect": "allow", "tool": "bash", "resourcePattern": "npm *"},
            {"ruleId": "r-web", "effect": "deny", "tool": "web_fetch", "resourcePattern": "*.internal"},
        ])
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

    #[tokio::test]
    async fn policy_get_on_undefined_chain_returns_default_ask_policy() {
        let (_tmp, root) = fresh_home("policy-default");
        let (rt, iid, init_rev) = open_runtime(&root);

        for scope in [
            instance_scope(),
            project_scope("proj-any"),
            session_scope("proj-any", "sess-any"),
        ] {
            let (_, result) = rt
                .dispatch(
                    "policy.get",
                    json!({"instanceId": iid, "scope": scope.clone()}),
                    None,
                    None,
                )
                .await
                .unwrap();
            OperationRegistry::global()
                .validate_result("policy.get", &result)
                .unwrap();
            assert_eq!(result["policy"]["scope"], scope);
            assert_eq!(result["policy"]["mode"], "ask");
            assert_eq!(result["policy"]["rules"], json!([]));
            assert_eq!(result["policy"]["revision"], init_rev);
            assert_eq!(result["inheritedFrom"], json!([]));
        }
    }

    #[tokio::test]
    async fn policy_update_first_write_uses_initialization_revision_then_cas_rejects_stale() {
        let (_tmp, root) = fresh_home("policy-cas");
        let (rt, iid, init_rev) = open_runtime(&root);

        // First write: CAS against the default's revision.
        let (_, updated) = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": instance_scope(),
                    "expectedRevision": init_rev,
                    "mode": "deny",
                    "rules": sample_rules(),
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("policy.update", &updated)
            .unwrap();
        assert_eq!(updated["policy"]["mode"], "deny");
        let rev1 = updated["policy"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev1, init_rev);

        // get reflects the stored policy.
        let (_, got) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": instance_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["policy"]["mode"], "deny");
        assert_eq!(got["policy"]["rules"], sample_rules());
        assert_eq!(got["policy"]["revision"], rev1);
        assert_eq!(got["inheritedFrom"], json!([]));

        // Stale revision (the pre-write default) is rejected with nothing applied.
        let err = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": instance_scope(),
                    "expectedRevision": init_rev,
                    "mode": "ask",
                    "rules": [],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "REVISION_CONFLICT");
        let (_, still) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": instance_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(still["policy"]["mode"], "deny");

        // The policy.changed event is durable.
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'policy.changed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(count, 1, "exactly one policy.changed event per update");
    }

    #[tokio::test]
    async fn undefined_scopes_inherit_nearest_defined_ancestor() {
        let (_tmp, root) = fresh_home("policy-inherit");
        let (rt, iid, init_rev) = open_runtime(&root);

        // instance-level policy (first write via the default revision).
        let (_, inst) = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": instance_scope(),
                    "expectedRevision": init_rev,
                    "mode": "allowConfigured",
                    "rules": sample_rules(),
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let inst_rev = inst["policy"]["revision"].as_str().unwrap().to_owned();

        // project scope inherits it, labeled with the requested scope.
        let (_, p) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": project_scope("proj-1")}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("policy.get", &p)
            .unwrap();
        assert_eq!(p["policy"]["scope"], project_scope("proj-1"));
        assert_eq!(p["policy"]["mode"], "allowConfigured");
        assert_eq!(p["policy"]["rules"], sample_rules());
        assert_eq!(p["policy"]["revision"], inst_rev);
        assert_eq!(
            p["inheritedFrom"],
            json!([{"kind": "instance"}]),
            "the contributing ancestor is named"
        );

        // session scope inherits through the project level too.
        let (_, s) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": session_scope("proj-1", "sess-1")}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(s["policy"]["mode"], "allowConfigured");
        assert_eq!(
            s["inheritedFrom"],
            json!([{"kind": "instance"}]),
            "the nearest defined ancestor wins, not a merged chain"
        );

        // A project-level definition shadows the instance one for its own
        // subtree; the session scope then names the project as contributor.
        let (_, pj) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": project_scope("proj-1")}),
                None,
                None,
            )
            .await
            .unwrap();
        let pj_rev = pj["policy"]["revision"].as_str().unwrap().to_owned();
        let (_, pj_written) = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": project_scope("proj-1"),
                    "expectedRevision": pj_rev,
                    "mode": "ask",
                    "rules": [],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let pj_new_rev = pj_written["policy"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(pj_new_rev, pj_rev);

        let (_, s2) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": session_scope("proj-1", "sess-1")}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(s2["policy"]["mode"], "ask");
        assert_eq!(s2["policy"]["rules"], json!([]));
        assert_eq!(s2["policy"]["revision"], pj_new_rev);
        assert_eq!(
            s2["inheritedFrom"],
            json!([{"kind": "project", "projectId": "proj-1"}])
        );

        // An ancestor change invalidates a stale CAS on the inheriting scope.
        let err = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": session_scope("proj-1", "sess-1"),
                    "expectedRevision": inst_rev,
                    "mode": "deny",
                    "rules": [],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            contract_code_of(&err),
            "REVISION_CONFLICT",
            "stale inherited revision must not overwrite"
        );
    }

    #[tokio::test]
    async fn invalid_rules_are_rejected_as_a_whole() {
        let (_tmp, root) = fresh_home("policy-invalid");
        let (rt, iid, init_rev) = open_runtime(&root);

        // Duplicate ruleIds: rejected before any write.
        let err = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": instance_scope(),
                    "expectedRevision": init_rev,
                    "mode": "ask",
                    "rules": [
                        {"ruleId": "dup", "effect": "allow", "tool": "bash", "resourcePattern": "x"},
                        {"ruleId": "dup", "effect": "deny", "tool": "bash", "resourcePattern": "y"},
                    ],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");

        // Out-of-enum mode: rejected by the input schema gate in dispatch.
        let err = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": instance_scope(),
                    "expectedRevision": init_rev,
                    "mode": "bypassPermissions",
                    "rules": [],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");

        // Malformed scope: also the schema gate.
        let err = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "workspace"},
                    "expectedRevision": init_rev,
                    "mode": "ask",
                    "rules": [],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");

        // Nothing was written by any rejected attempt.
        let (_, got) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": instance_scope()}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["policy"]["mode"], "ask");
        assert_eq!(got["policy"]["revision"], init_rev);
    }

    #[tokio::test]
    async fn policy_operations_are_registered_and_schema_validated() {
        // The domain file registered both handlers: dispatch no longer
        // answers -32004 for policy.*, and results pass the embedded schema.
        let (_tmp, root) = fresh_home("policy-registered");
        let (rt, iid, init_rev) = open_runtime(&root);

        let (_, got) = rt
            .dispatch(
                "policy.get",
                json!({"instanceId": iid, "scope": session_scope("p:1", "s:2")}),
                None,
                None,
            )
            .await
            .unwrap();
        // Scope keys must not collide for tricky ids (colon-containing).
        assert_eq!(got["policy"]["scope"], session_scope("p:1", "s:2"));

        let (_, written) = rt
            .dispatch(
                "policy.update",
                json!({
                    "instanceId": iid,
                    "scope": session_scope("p:1", "s:2"),
                    "expectedRevision": init_rev,
                    "mode": "ask",
                    "rules": [{"ruleId": "r1", "effect": "ask", "tool": "bash", "resourcePattern": "*"}],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("policy.update", &written)
            .unwrap();
    }
}
