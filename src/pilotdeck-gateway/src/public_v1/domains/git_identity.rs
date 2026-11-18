//! `git_identity.*` — effective Git identity per project / product default
//! (S3 `cli.task.t014`).
//!
//! Storage is a plain table (name/email are not secrets — the TS baseline
//! stores them in SQLite `users.git_name`/`git_email`,
//! `ui/server/database/db.js:232-247`; secrets belong to the credential
//! domain). Precedence when committing: `project` > `productDefault` >
//! `system` (read-only `git config`) > unset; commits inject the effective
//! identity through `-c user.name=… -c user.email=…` and never write any
//! git config file.
//!
//! Registered contract divergence (bug-level difference in the TS baseline,
//! `ui/server/routes/user.js:80-81`): the TS `POST /api/git-config` also ran
//! `git config --global user.name/user.email`, mutating the user's global
//! git configuration as a side effect of a product-scoped write. The
//! contract forbids this ("不写系统global配置"), and so does this port.
//! Likewise the TS read path auto-backfilled the database from the system
//! config (`user.js:39-48`); this port keeps `source=system` explainable
//! and never backfills.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_NOT_FOUND, CODE_REVISION_CONFLICT, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::git::CODE_INVALID_IDENTITY;
use crate::public_v1::domains::git::EffectiveIdentity;
use crate::public_v1::domains::git::RepoCtx;
use crate::public_v1::domains::git::git_run_str;
use crate::public_v1::domains::git::invalid_argument;
use crate::public_v1::domains::git::map_sql_err;
use crate::public_v1::domains::git::next_domain_rev;
use crate::public_v1::domains::git::resolve_repo;
use crate::public_v1::runtime::DispatchError;
use crate::public_v1::runtime::PublicV1Runtime;
use crate::public_v1::store::now_rfc3339;

/// The `identity_key` of the product-default row.
const DEFAULT_KEY: &str = "__product_default__";

// ---------------------------------------------------------------------------
// Registration.
// ---------------------------------------------------------------------------

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "git_identity",
        "CREATE TABLE IF NOT EXISTS git_identity (
            identity_key TEXT PRIMARY KEY,
            scope TEXT NOT NULL,
            name TEXT NOT NULL,
            email TEXT NOT NULL,
            revision TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    );

    reg.handler("git_identity.get", |rt, input, _key, _rev| {
        Box::pin(identity_get(rt, input))
    });
    reg.handler("git_identity.set", |rt, input, _key, _rev| {
        Box::pin(identity_set(rt, input))
    });
}

// ---------------------------------------------------------------------------
// Row helpers (the shared `V1Store` connection, same as git.rs).
// ---------------------------------------------------------------------------

struct IdentityRow {
    name: String,
    email: String,
    revision: String,
}

fn fetch_identity_row(conn: &Connection, key: &str) -> Result<Option<IdentityRow>, ContractError> {
    conn.query_row(
        "SELECT name, email, revision FROM git_identity WHERE identity_key = ?1",
        params![key],
        |r| {
            Ok(IdentityRow {
                name: r.get(0)?,
                email: r.get(1)?,
                revision: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(map_sql_err)
}

fn with_store_row(
    rt: &Arc<PublicV1Runtime>,
    key: &str,
) -> Result<Option<IdentityRow>, DispatchError> {
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    fetch_identity_row(&conn, key).map_err(DispatchError::Application)
}

/// Reads what git itself would use for `user.name` / `user.email` — read
/// only, in the repository when one is resolvable (repo-local > global >
/// system per git's own resolution), otherwise the global/system config
/// (the TS baseline's `getSystemGitConfig` read, `utils/gitConfig.js`).
async fn read_system_identity(
    rt: &Arc<PublicV1Runtime>,
    project_id: Option<&str>,
) -> Option<(String, String)> {
    let cwd = match project_id {
        Some(pid) => match resolve_repo(rt, pid).await {
            Ok(repo) => repo.repo_root,
            Err(_) => std::env::temp_dir(),
        },
        None => std::env::temp_dir(),
    };
    let name = git_run_str(&cwd, &["config", "--get", "user.name"])
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned())
        .filter(|s| !s.is_empty());
    let email = git_run_str(&cwd, &["config", "--get", "user.email"])
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned())
        .filter(|s| !s.is_empty());
    match (name, email) {
        (Some(name), Some(email)) => Some((name, email)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// git_identity.get
// ---------------------------------------------------------------------------

async fn identity_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input.get("projectId").and_then(Value::as_str);

    let mut result = Map::new();
    if let Some(pid) = project_id {
        result.insert("projectId".into(), json!(pid));
    }
    if let Some(pid) = project_id
        && let Some(row) = with_store_row(&rt, pid)?
    {
        result.insert("name".into(), json!(row.name));
        result.insert("email".into(), json!(row.email));
        result.insert("source".into(), json!("project"));
        result.insert("revision".into(), json!(row.revision));
        return Ok((ResponseOutcome::Succeeded, Value::Object(result)));
    }
    if let Some(row) = with_store_row(&rt, DEFAULT_KEY)? {
        result.insert("name".into(), json!(row.name));
        result.insert("email".into(), json!(row.email));
        result.insert("source".into(), json!("productDefault"));
        result.insert("revision".into(), json!(row.revision));
        return Ok((ResponseOutcome::Succeeded, Value::Object(result)));
    }
    if let Some((name, email)) = read_system_identity(&rt, project_id).await {
        result.insert("name".into(), json!(name));
        result.insert("email".into(), json!(email));
        result.insert("source".into(), json!("system"));
        // System config has no product-side revision; the empty string marks
        // "unset from the store's perspective" (and is what a subsequent
        // `git_identity.set` expects as expectedRevision).
        result.insert("revision".into(), json!(""));
        return Ok((ResponseOutcome::Succeeded, Value::Object(result)));
    }
    // Unset and empty values stay distinguishable through `source=unset`.
    result.insert("name".into(), json!(""));
    result.insert("email".into(), json!(""));
    result.insert("source".into(), json!("unset"));
    result.insert("revision".into(), json!(""));
    Ok((ResponseOutcome::Succeeded, Value::Object(result)))
}

// ---------------------------------------------------------------------------
// git_identity.set
// ---------------------------------------------------------------------------

fn validate_identity(name: &str, email: &str) -> Result<(), ContractError> {
    for (field, value) in [("name", name), ("email", email)] {
        if value.trim().is_empty() {
            return Err(identity_invalid(format!("{field} must not be empty")));
        }
        if value.len() > 256 {
            return Err(identity_invalid(format!("{field} exceeds 256 characters")));
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(identity_invalid(format!(
                "{field} contains control characters"
            )));
        }
    }
    // Email shape — the TS baseline's check (`user.js:73`).
    let parts: Vec<&str> = email.split('@').collect();
    if parts.len() != 2 {
        return Err(identity_invalid("invalid email format"));
    }
    let (local, domain) = (parts[0], parts[1]);
    if local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || domain.split('.').any(|p| p.is_empty())
        || email.chars().any(|c| c.is_whitespace())
    {
        return Err(identity_invalid("invalid email format"));
    }
    Ok(())
}

fn identity_invalid(message: String) -> ContractError {
    ContractError::new(
        CODE_INVALID_IDENTITY,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict() -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        "expectedRevision does not match the current identity revision",
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

async fn identity_set(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = input
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let clear = input.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let project_id = input.get("projectId").and_then(Value::as_str);
    let identity_key = match scope {
        "project" => {
            let Some(pid) = project_id else {
                return Err(invalid_argument("scope=project requires projectId").into());
            };
            pid.to_owned()
        }
        "productDefault" => DEFAULT_KEY.to_owned(),
        other => return Err(invalid_argument(format!("unknown identity scope `{other}`")).into()),
    };

    if scope == "project" {
        // The target project must exist (projects table, same database).
        let exists = rt
            .store()
            .conn
            .lock()
            .expect("sqlite mutex")
            .query_row(
                "SELECT project_id FROM projects WHERE project_id = ?1",
                params![identity_key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sql_err)
            .map_err(DispatchError::Application)?;
        if exists.is_none() {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("project `{identity_key}` not found"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
    }

    let current = with_store_row(&rt, &identity_key)?;
    let current_revision = current
        .as_ref()
        .map(|row| row.revision.clone())
        .unwrap_or_default();
    if current_revision != expected_revision {
        return Err(revision_conflict().into());
    }

    if clear {
        if current.is_none() {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("no git identity set for scope `{scope}` to clear"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        rt.store()
            .conn
            .lock()
            .expect("sqlite mutex")
            .execute(
                "DELETE FROM git_identity WHERE identity_key = ?1",
                params![identity_key],
            )
            .map_err(map_sql_err)
            .map_err(DispatchError::Application)?;
        // Cleared state reports the unset revision (""), matching what a
        // subsequent `git_identity.get` (and the next `set`'s
        // expectedRevision) observes.
        return Ok((
            ResponseOutcome::Succeeded,
            json!({"revision": "", "scope": scope}),
        ));
    }

    if input.get("name").is_none() || input.get("email").is_none() {
        return Err(invalid_argument("name and email are required unless clear=true").into());
    }
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let email = input
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    validate_identity(name, email).map_err(DispatchError::Application)?;

    let revision = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let rev = next_domain_rev(&conn).map_err(DispatchError::Application)?;
        conn.execute(
            "INSERT INTO git_identity (identity_key, scope, name, email, revision, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(identity_key) DO UPDATE SET
                scope = excluded.scope,
                name = excluded.name,
                email = excluded.email,
                revision = excluded.revision,
                updated_at = excluded.updated_at",
            params![identity_key, scope, name, email, rev, now_rfc3339()],
        )
        .map_err(map_sql_err)
        .map_err(DispatchError::Application)?;
        rev
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({"revision": revision, "scope": scope, "name": name, "email": email}),
    ))
}

// ---------------------------------------------------------------------------
// Effective identity for `git.commit`.
// ---------------------------------------------------------------------------

/// project > productDefault > system (read-only) — the value `git.commit`
/// injects through `-c user.name/-c user.email`. Returns `None` when every
/// source is unset (the caller answers `IDENTITY_REQUIRED`).
pub(super) async fn effective_identity_for_commit(
    rt: &Arc<PublicV1Runtime>,
    repo: &RepoCtx,
) -> Option<EffectiveIdentity> {
    let conn = rt.store().conn.lock().ok()?;
    let identity = fetch_identity_row(&conn, &repo.project_id)
        .ok()
        .flatten()
        .or_else(|| fetch_identity_row(&conn, DEFAULT_KEY).ok().flatten());
    drop(conn);
    if let Some(row) = identity {
        return Some(EffectiveIdentity {
            name: row.name,
            email: row.email,
        });
    }
    // System source, read inside the repository (repo-local > global >
    // system per git's own resolution).
    let name = git_run_str(&repo.repo_root, &["config", "--get", "user.name"])
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned())
        .filter(|s| !s.is_empty())?;
    let email = git_run_str(&repo.repo_root, &["config", "--get", "user.email"])
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned())
        .filter(|s| !s.is_empty())?;
    Some(EffectiveIdentity { name, email })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_validation_rejects_bad_emails_and_control_chars() {
        assert!(validate_identity("User", "user@example.com").is_ok());
        assert!(validate_identity("中文用户", "zh@example.com").is_ok());
        assert_eq!(
            validate_identity("User", "not-an-email").unwrap_err().code,
            CODE_INVALID_IDENTITY
        );
        assert_eq!(
            validate_identity("User", "a@b").unwrap_err().code,
            CODE_INVALID_IDENTITY
        );
        assert_eq!(
            validate_identity("User", "a b@example.com")
                .unwrap_err()
                .code,
            CODE_INVALID_IDENTITY
        );
        assert_eq!(
            validate_identity("Bad\u{1}Name", "a@example.com")
                .unwrap_err()
                .code,
            CODE_INVALID_IDENTITY
        );
        assert_eq!(
            validate_identity("", "a@example.com").unwrap_err().code,
            CODE_INVALID_IDENTITY
        );
    }
}
