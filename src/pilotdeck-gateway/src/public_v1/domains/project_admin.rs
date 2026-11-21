//! `project.create` / `project.update` / `project.remove` (`t012`).
//!
//! `project.create` is the "create directory + register project" operation:
//! the parent address is the contract `Path { instanceId, rootId,
//! relativePath }`, resolved against the authorized-root registry owned by
//! `workspace_dirs`. The directory is created inside the authorized root
//! (lexical + canonical containment, non-empty existing targets answered
//! with `PATH_EXISTS`), the registration row is written in one transaction,
//! and a failed registration cleans up only directories this call created
//! and that are still empty. `project.remove` only deregisters — the disk
//! directory, sessions, runs and history survive (`仅移除项目登记`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError, EffectStatus,
    RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::background_tasks::has_active_tasks_for_sessions;
use crate::public_v1::domains::workspace_dirs::{
    CODE_ACTIVE_RESOURCES, CODE_CREATE_FAILED, CODE_PATH_EXISTS, CODE_PATH_NOT_ALLOWED,
    append_domain_event_in_tx, bump_rev_in_tx, decode_byte_path, domain_not_found,
    relative_components, sql_err,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("project.create", |rt, input, _key, _rev| {
        Box::pin(project_create(rt, input))
    });
    reg.handler("project.update", |rt, input, _key, _rev| {
        Box::pin(project_update(rt, input))
    });
    reg.handler("project.remove", |rt, input, _key, _rev| {
        Box::pin(project_remove(rt, input))
    });
}

// ---------------------------------------------------------------------------
// project.create
// ---------------------------------------------------------------------------

async fn project_create(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = tokio::task::spawn_blocking(move || create_project(&store, &input))
        .await
        .map_err(|e| {
            DispatchError::Application(ContractError::new(
                agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
                format!("project.create worker failed: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            ))
        })??;
    Ok((ResponseOutcome::Succeeded, result))
}

/// The `Path` input of the `project.*` family: a plain relative string
/// (not the directories family's `BytePath`).
struct ParentPath {
    root_id: String,
    relative: PathBuf,
}

fn invalid_argument(msg: String) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        msg,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn path_not_allowed(msg: String) -> ContractError {
    ContractError::new(
        CODE_PATH_NOT_ALLOWED,
        msg,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn decode_parent_path(store: &V1Store, parent: &Value) -> Result<ParentPath, ContractError> {
    let instance_id = store.meta().instance_id.clone();
    let parent_instance = parent
        .get("instanceId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if parent_instance != instance_id {
        return Err(ContractError::instance_mismatch(
            &instance_id,
            parent_instance,
        ));
    }
    let root_id = parent
        .get("rootId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_argument("parent.rootId is required".to_owned()))?
        .to_owned();
    // `Path.relativePath` is a plain string; route it through the same
    // decoder so the shape stays uniform, then re-map escape errors onto
    // this family's `PATH_NOT_ALLOWED` code.
    let raw = parent.get("relativePath").cloned().unwrap_or(Value::Null);
    let byte_path = json!({"encoding": "utf8", "value": raw.as_str().unwrap_or_default()});
    let relative = decode_byte_path(&byte_path).map_err(|mut e| {
        e.code = CODE_PATH_NOT_ALLOWED.to_owned();
        e
    })?;
    let _components = relative_components(&relative).map_err(|mut e| {
        e.code = CODE_PATH_NOT_ALLOWED.to_owned();
        e
    })?;
    let _ = store.resolve_authorized_root(&root_id)?;
    Ok(ParentPath { root_id, relative })
}

fn create_project_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn map_create_insert_err(err: rusqlite::Error) -> ContractError {
    match &err {
        rusqlite::Error::SqliteFailure(sqlite_err, _)
            if sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            ContractError::new(
                CODE_CREATE_FAILED,
                format!("project registration collided with an existing record: {err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        }
        _ => sql_err(err),
    }
}

fn create_project(store: &V1Store, input: &Value) -> Result<Value, ContractError> {
    let project_id = input["projectId"].as_str().unwrap_or_default();
    let name = input["name"].as_str().unwrap_or_default();
    let directory_name = input["directoryName"].as_str().unwrap_or_default();
    if directory_name.is_empty()
        || directory_name == "."
        || directory_name == ".."
        || directory_name.contains('/')
        || directory_name.contains('\\')
    {
        return Err(invalid_argument(format!(
            "directoryName `{directory_name}` must be a single non-special path component"
        )));
    }

    let parent = decode_parent_path(store, &input["parent"])?;

    // Serialize concurrent `project.create` calls so FS creation, emptiness
    // verification, DB registration, and failure cleanup never interleave (#17).
    let _create_guard = create_project_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // Pre-flight the registration key before touching the filesystem.
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        let exists: Option<String> = conn
            .query_row(
                "SELECT project_id FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if exists.is_some() {
            return Err(ContractError::new(
                CODE_CREATE_FAILED,
                format!("project `{project_id}` is already registered"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }

    // Walk every component from canonicalized authorized root down to the
    // final target BEFORE any descendant stat/read_dir, checking symlinks at
    // every pre-existing AND newly-created level (#2 P0).
    let (created, canonical_target) = create_dir_chain(store, &parent, directory_name)?;

    // Registration is one Immediate transaction; re-verify the target is still
    // empty right before inserting so external concurrent writes fail cleanly.
    let register = (|| -> Result<String, ContractError> {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let is_still_empty = canonical_target.is_dir()
            && std::fs::read_dir(&canonical_target)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
        if !is_still_empty {
            return Err(ContractError::new(
                CODE_PATH_EXISTS,
                format!(
                    "directory `{}` became non-empty before registration completed",
                    canonical_target.display()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let rev = bump_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        let relative = join_relative(&parent.relative, directory_name);
        let root_json = json!({
            "instanceId": store.meta().instance_id,
            "rootId": parent.root_id,
            "relativePath": relative,
        });
        tx.execute(
            "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
             VALUES (?1, ?2, ?3, 'registered', 'local', ?4, ?5)",
            params![
                project_id,
                name,
                serde_json::to_string(&root_json).unwrap_or_else(|_| "{}".into()),
                rev,
                now
            ],
        )
        .map_err(map_create_insert_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(rev)
    })();
    match register {
        Ok(rev) => {
            store.state_notifier().notify_waiters();
            Ok(json!({
                "project": {
                    "projectId": project_id,
                    "name": name,
                    "root": {
                        "instanceId": store.meta().instance_id,
                        "rootId": parent.root_id,
                        "relativePath": join_relative(&parent.relative, directory_name),
                    },
                    "revision": rev,
                    "registrationState": "registered",
                    "environmentId": "local",
                }
            }))
        }
        Err(error) => {
            cleanup_created_dirs_safe(&created, store);
            Err(error)
        }
    }
}

/// Creates the missing directory chain from the authorized root down to the
/// project directory (parent path components + `directory_name`), returning
/// `(created_dirs_deepest_last, canonical_target)`. Every pre-existing AND
/// newly-created level is verified by `symlink_metadata` + `canonicalize` +
/// `strip_prefix(&canonical_root)` so symlinks escaping the root (or dangling)
/// never get probed or traversed.
fn create_dir_chain(
    store: &V1Store,
    parent: &ParentPath,
    directory_name: &str,
) -> Result<(Vec<PathBuf>, PathBuf), ContractError> {
    let root = store.resolve_authorized_root(&parent.root_id)?;
    let canonical_root = std::fs::canonicalize(&root).map_err(|e| {
        ContractError::new(
            CODE_CREATE_FAILED,
            format!("cannot resolve authorized root `{}`: {e}", root.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;

    let mut components = relative_components(&parent.relative).map_err(|mut e| {
        e.code = CODE_PATH_NOT_ALLOWED.to_owned();
        e
    })?;
    components.push(std::ffi::OsString::from(directory_name));

    let mut created: Vec<PathBuf> = Vec::new();
    let mut next = canonical_root.clone();
    let last_idx = components.len().saturating_sub(1);
    for (idx, component) in components.iter().enumerate() {
        let is_final = idx == last_idx;
        next.push(component);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    let verified = std::fs::canonicalize(&next).map_err(|_| {
                        cleanup_created_dirs_safe(&created, store);
                        path_not_allowed(format!(
                            "path component `{}` is a dangling or unresolvable symlink",
                            next.display()
                        ))
                    })?;
                    if verified.strip_prefix(&canonical_root).is_err() {
                        cleanup_created_dirs_safe(&created, store);
                        return Err(path_not_allowed(format!(
                            "path component `{}` escapes the authorized root",
                            next.display()
                        )));
                    }
                    if is_final {
                        cleanup_created_dirs_safe(&created, store);
                        return Err(ContractError::new(
                            CODE_PATH_EXISTS,
                            format!(
                                "target `{}` is a symlink; refusing symlink project directory",
                                next.display()
                            ),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    if !verified.is_dir() {
                        cleanup_created_dirs_safe(&created, store);
                        return Err(ContractError::new(
                            CODE_PATH_EXISTS,
                            format!("`{}` exists and is not a directory", next.display()),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    next = verified;
                } else {
                    let verified = std::fs::canonicalize(&next).map_err(|e| {
                        cleanup_created_dirs_safe(&created, store);
                        ContractError::new(
                            CODE_CREATE_FAILED,
                            format!("cannot resolve `{}`: {e}", next.display()),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        )
                    })?;
                    if verified.strip_prefix(&canonical_root).is_err() {
                        cleanup_created_dirs_safe(&created, store);
                        return Err(path_not_allowed(format!(
                            "path component `{}` escapes the authorized root",
                            next.display()
                        )));
                    }
                    if !verified.is_dir() {
                        cleanup_created_dirs_safe(&created, store);
                        return Err(ContractError::new(
                            CODE_PATH_EXISTS,
                            format!("`{}` exists and is not a directory", next.display()),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    if is_final {
                        let is_empty = std::fs::read_dir(&verified)
                            .map(|mut d| d.next().is_none())
                            .unwrap_or(false);
                        if !is_empty {
                            cleanup_created_dirs_safe(&created, store);
                            return Err(ContractError::new(
                                CODE_PATH_EXISTS,
                                format!(
                                    "directory `{}` already exists and is not empty; refusing to overwrite",
                                    next.display()
                                ),
                                RetrySpec::never(),
                                EffectStatus::NotApplied,
                            ));
                        }
                    }
                    next = verified;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Err(create_err) = std::fs::create_dir(&next) {
                    cleanup_created_dirs_safe(&created, store);
                    return Err(ContractError::new(
                        CODE_CREATE_FAILED,
                        format!("failed to create `{}`: {create_err}", next.display()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                created.push(next.clone());

                // Post-create confinement & symlink check before descending.
                let post_meta = std::fs::symlink_metadata(&next).map_err(|e| {
                    cleanup_created_dirs_safe(&created, store);
                    ContractError::new(
                        CODE_CREATE_FAILED,
                        format!("cannot stat created `{}`: {e}", next.display()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                })?;
                if post_meta.file_type().is_symlink() {
                    cleanup_created_dirs_safe(&created, store);
                    return Err(path_not_allowed(format!(
                        "created path `{}` unexpectedly resolved to a symlink",
                        next.display()
                    )));
                }
                let verified = std::fs::canonicalize(&next).map_err(|_| {
                    cleanup_created_dirs_safe(&created, store);
                    path_not_allowed(format!(
                        "created path `{}` cannot be canonicalized within authorized root",
                        next.display()
                    ))
                })?;
                if verified.strip_prefix(&canonical_root).is_err() || !verified.is_dir() {
                    cleanup_created_dirs_safe(&created, store);
                    return Err(path_not_allowed(format!(
                        "created path `{}` escapes the authorized root",
                        next.display()
                    )));
                }
                next = verified;
            }
            Err(e) => {
                cleanup_created_dirs_safe(&created, store);
                return Err(ContractError::new(
                    CODE_CREATE_FAILED,
                    format!("cannot stat `{}`: {e}", next.display()),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
    }
    Ok((created, next))
}

fn registered_project_paths(store: &V1Store) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let raw_roots: Vec<String> = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let Ok(mut stmt) = conn.prepare("SELECT root_json FROM projects") else {
            return out;
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            return out;
        };
        rows.filter_map(Result::ok).collect()
    };
    for raw in raw_roots {
        let Ok(root_val) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(root_id) = root_val.get("rootId").and_then(Value::as_str) else {
            continue;
        };
        let rel = root_val
            .get("relativePath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Ok(auth_root) = store.resolve_authorized_root(root_id) else {
            continue;
        };
        let canonical_root = std::fs::canonicalize(&auth_root).unwrap_or(auth_root);
        let mut path = canonical_root;
        if let Ok(comps) = relative_components(Path::new(rel)) {
            for c in comps {
                path.push(c);
            }
        }
        out.insert(std::fs::canonicalize(&path).unwrap_or(path));
    }
    out
}

/// Removes the directories this call created, deepest first, but only those
/// that are still empty and not referenced (or an ancestor of) any committed
/// project row.
fn cleanup_created_dirs_safe(created: &[PathBuf], store: &V1Store) {
    let protected = registered_project_paths(store);
    for path in created.iter().rev() {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if protected
            .iter()
            .any(|p| p == &canonical || p.starts_with(&canonical))
        {
            continue;
        }
        let is_empty = std::fs::read_dir(path)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if is_empty {
            let _ = std::fs::remove_dir(path);
        }
    }
}

fn join_relative(parent: &Path, directory_name: &str) -> String {
    let parent_str = parent.to_string_lossy().trim_matches('/').to_owned();
    if parent_str.is_empty() {
        directory_name.to_owned()
    } else {
        format!("{parent_str}/{directory_name}")
    }
}

// ---------------------------------------------------------------------------
// project.update
// ---------------------------------------------------------------------------

async fn run_blocking<T, F>(task: F) -> Result<T, ContractError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ContractError> + Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("project admin worker failed: {e}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?
}

async fn project_update(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = run_blocking(move || store.update_project_name(&input)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, ContractError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |r| r.get(0),
    )
    .map_err(sql_err)
}

fn has_active_project_resources(
    tx: &Connection,
    project_id: &str,
    current_epoch: &str,
) -> Result<bool, ContractError> {
    let run_active: i64 = tx
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM runs r JOIN sessions s ON s.session_id = r.session_id
                WHERE s.project_id = ?1 AND r.terminal = 0)
             OR EXISTS(
                SELECT 1 FROM run_queue q JOIN sessions s ON s.session_id = q.session_id
                WHERE s.project_id = ?1)",
            params![project_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if run_active != 0 {
        return Ok(true);
    }

    if table_exists(tx, "operations")? {
        let op_active: i64 = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM operations
                    WHERE terminal = 0
                      AND (project_id = ?1 OR session_id IN (SELECT session_id FROM sessions WHERE project_id = ?1)))",
                params![project_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if op_active != 0 {
            return Ok(true);
        }
    }

    if table_exists(tx, "git_operations")? {
        let git_active: i64 = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM git_operations
                    WHERE project_id = ?1 AND state IN ('queued', 'running'))",
                params![project_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if git_active != 0 {
            return Ok(true);
        }
    }

    if table_exists(tx, "async_operations")? && table_exists(tx, "uploads")? {
        let upload_active: i64 = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM async_operations ao
                    JOIN uploads u ON u.upload_id = ao.resource_id
                    WHERE u.project_id = ?1 AND ao.state IN ('queued', 'running'))",
                params![project_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if upload_active != 0 {
            return Ok(true);
        }
    }

    if table_exists(tx, "terminals")? {
        let term_active: i64 = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM terminals
                    WHERE project_id = ?1 AND server_epoch = ?2 AND state NOT IN ('exited', 'lost'))",
                params![project_id, current_epoch],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if term_active != 0 {
            return Ok(true);
        }
    }

    if table_exists(tx, "v1_schedules")? {
        let sched_active: i64 = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM v1_schedules
                    WHERE project_id = ?1 AND enabled != 0)",
                params![project_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if sched_active != 0 {
            return Ok(true);
        }
        if table_exists(tx, "v1_schedule_executions")? {
            let exec_active: i64 = tx
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM v1_schedule_executions e
                        JOIN v1_schedules s ON s.schedule_id = e.schedule_id
                        WHERE s.project_id = ?1 AND e.state IN ('pending', 'running'))",
                    params![project_id],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            if exec_active != 0 {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

impl V1Store {
    /// `project.update`: conditional (CAS) rename of the display name. The
    /// disk root is never moved ("不移动磁盘根").
    pub(crate) fn update_project_name(&self, input: &Value) -> Result<Value, ContractError> {
        let project_id = input["projectId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let name = input["name"].as_str().unwrap_or_default();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let current: Option<(String, String, String, String, String)> = tx
            .query_row(
                "SELECT name, root_json, registration_state, environment_id, revision
                 FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((old_name, root_json, reg_state, env_id, revision)) = current else {
            return Err(domain_not_found("project", project_id));
        };
        if revision != expected {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!("project expectedRevision `{expected}` does not match `{revision}`"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let rev = bump_rev_in_tx(&tx)?;
        let now = now_rfc3339();
        tx.execute(
            "UPDATE projects SET name = ?1, revision = ?2 WHERE project_id = ?3",
            params![name, rev, project_id],
        )
        .map_err(sql_err)?;
        append_domain_event_in_tx(
            &tx,
            &self.meta().instance_id,
            project_id,
            "",
            "project.changed",
            &json!({"projectId": project_id, "name": name, "previousName": old_name, "revision": rev}),
            &now,
        )?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();
        let root: Value = serde_json::from_str(&root_json).unwrap_or(Value::Null);
        Ok(json!({
            "project": {
                "projectId": project_id,
                "name": name,
                "root": root,
                "revision": rev,
                "registrationState": reg_state,
                "environmentId": env_id,
            }
        }))
    }

    /// `project.remove`: deregistration + closing open sessions. Refuses with
    /// `ACTIVE_RESOURCES` while any active/queued run, operation, live terminal,
    /// enabled schedule, or active background task exists under the project; the
    /// disk directory, session tombstones, runs, and history survive.
    pub(crate) fn remove_project(&self, input: &Value) -> Result<Value, ContractError> {
        let project_id = input["projectId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let revision: Option<String> = tx
            .query_row(
                "SELECT revision FROM projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(revision) = revision else {
            return Err(domain_not_found("project", project_id));
        };
        if revision != expected {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!("project expectedRevision `{expected}` does not match `{revision}`"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        let session_ids: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT session_id FROM sessions WHERE project_id = ?1")
                .map_err(sql_err)?;
            stmt.query_map(params![project_id], |r| r.get::<_, String>(0))
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?
        };

        let db_busy = has_active_project_resources(&tx, project_id, &self.meta().generation)?;
        let bg_busy = has_active_tasks_for_sessions(self.home(), &session_ids);
        if db_busy || bg_busy {
            return Err(ContractError::new(
                CODE_ACTIVE_RESOURCES,
                format!(
                    "project `{project_id}` still has active resources (runs, operations, terminals, schedules, or background tasks); \
                     stop or wait for them before removing the registration"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "DELETE FROM projects WHERE project_id = ?1",
            params![project_id],
        )
        .map_err(sql_err)?;
        // Close open sessions belonging to the removed project so run.submit
        // immediately refuses new work while preserving session history/search.
        tx.execute(
            "UPDATE sessions SET session_state = 'closed', revision = ?1
             WHERE project_id = ?2 AND session_state = 'open'",
            params![rev, project_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.state_notifier().notify_waiters();
        Ok(json!({
            "removedProjectId": project_id,
            "revision": rev,
        }))
    }
}

// ---------------------------------------------------------------------------
// project.remove
// ---------------------------------------------------------------------------

async fn project_remove(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let result = run_blocking(move || store.remove_project(&input)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

// ---------------------------------------------------------------------------
// Tests (contract anchors: 非空目录不覆盖；登记失败仅清理本次创建且仍空的目录；
// 磁盘目录hash不变；活跃资源拒绝；并发CAS一方成功)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    fn open_runtime(root_dir: &std::path::Path) -> (std::sync::Arc<PublicV1Runtime>, String) {
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

    fn contract_code(error: &DispatchError) -> String {
        match error {
            DispatchError::Application(e) => e.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    fn parent(iid: &str, relative: &str) -> Value {
        json!({"instanceId": iid, "rootId": "default", "relativePath": relative})
    }

    #[tokio::test]
    async fn project_create_makes_the_directory_and_registers() {
        let tmp = TempDir::new("proj-create");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);

        let (_, created) = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-1",
                    "parent": parent(&iid, ""),
                    "directoryName": "workdir",
                    "name": "First",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("project.create", &created)
            .unwrap();
        assert_eq!(created["project"]["registrationState"], "registered");
        assert_eq!(created["project"]["root"]["relativePath"], "workdir");
        assert!(
            root_dir.join("workspaces").join("workdir").is_dir(),
            "the project directory is created inside the authorized root"
        );

        // A duplicate projectId fails before any filesystem effect.
        let error = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-1",
                    "parent": parent(&iid, ""),
                    "directoryName": "other",
                    "name": "Second",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "CREATE_FAILED");
        assert!(!root_dir.join("workspaces").join("other").exists());

        // A non-empty existing directory is never overwritten.
        let occupied = root_dir.join("workspaces").join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("keep.txt"), b"data").unwrap();
        let error = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-2",
                    "parent": parent(&iid, ""),
                    "directoryName": "occupied",
                    "name": "Occupied",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "PATH_EXISTS");
        assert!(occupied.join("keep.txt").exists());

        // Parent escapes are refused with this family's code.
        let error = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-3",
                    "parent": parent(&iid, "../escape"),
                    "directoryName": "x",
                    "name": "Esc",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "PATH_NOT_ALLOWED");

        // The parent's instanceId must match the envelope's.
        let error = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-4",
                    "parent": json!({
                        "instanceId": "someone-else",
                        "rootId": "default",
                        "relativePath": "",
                    }),
                    "directoryName": "x",
                    "name": "Mismatch",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "INSTANCE_MISMATCH");
    }

    #[tokio::test]
    async fn project_update_is_cas_on_the_revision_and_moves_no_disk_root() {
        let tmp = TempDir::new("proj-update");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_, created) = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-1",
                    "parent": parent(&iid, ""),
                    "directoryName": "workdir",
                    "name": "Old name",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let rev1 = created["project"]["revision"].as_str().unwrap().to_owned();

        let (_, updated) = rt
            .dispatch(
                "project.update",
                json!({
                    "instanceId": iid,
                    "projectId": "p-1",
                    "expectedRevision": rev1,
                    "name": "New name",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("project.update", &updated)
            .unwrap();
        assert_eq!(updated["project"]["name"], "New name");
        assert_ne!(updated["project"]["revision"], rev1);

        // A concurrent writer holding the previous revision loses the CAS.
        let error = rt
            .dispatch(
                "project.update",
                json!({
                    "instanceId": iid,
                    "projectId": "p-1",
                    "expectedRevision": rev1,
                    "name": "Loser",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "REVISION_CONFLICT");

        let error = rt
            .dispatch(
                "project.update",
                json!({
                    "instanceId": iid,
                    "projectId": "ghost",
                    "expectedRevision": rev1,
                    "name": "x",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");

        // The registration survives restart-reopen and the disk root is
        // untouched by the rename.
        let (_, fetched) = rt
            .dispatch(
                "project.get",
                json!({"instanceId": iid, "projectId": "p-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(fetched["project"]["name"], "New name");
        assert_eq!(fetched["project"]["root"]["relativePath"], "workdir");
        assert!(root_dir.join("workspaces").join("workdir").is_dir());
    }

    #[tokio::test]
    async fn project_remove_deregisters_only_and_refuses_active_resources() {
        let tmp = TempDir::new("proj-remove");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (_, created) = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-1",
                    "parent": parent(&iid, ""),
                    "directoryName": "workdir",
                    "name": "Doomed",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let rev = created["project"]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "s-1", "projectId": "p-1"}),
            None,
            None,
        )
        .await
        .unwrap();

        // An active run under the project refuses the removal.
        let (_, submitted) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": "s-1",
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=2500]"}],
                }),
                Some("run-key-remove-1"),
                None,
            )
            .await
            .unwrap();
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        let error = rt
            .dispatch(
                "project.remove",
                json!({"instanceId": iid, "projectId": "p-1", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "ACTIVE_RESOURCES");

        let (_, done) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 15000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(done["run"]["state"], "succeeded");

        // With the run terminal, removal deregisters only: the directory,
        // the session row and the run tombstone survive.
        let (_, removed) = rt
            .dispatch(
                "project.remove",
                json!({"instanceId": iid, "projectId": "p-1", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("project.remove", &removed)
            .unwrap();
        assert_eq!(removed["removedProjectId"], "p-1");
        assert!(removed["revision"].as_str().is_some());
        let error = rt
            .dispatch(
                "project.get",
                json!({"instanceId": iid, "projectId": "p-1"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
        assert!(root_dir.join("workspaces").join("workdir").is_dir());
        let (_, session) = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": "s-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(session["session"]["sessionState"], "closed");
        let (_, run) = rt
            .dispatch(
                "run.get",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(run["run"]["state"], "succeeded");

        // Post-removal session cannot submit new runs or reopen.
        let submit_after = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": "s-1",
                    "input": [{"kind": "text", "text": "after-remove"}],
                }),
                Some("run-after-project-remove"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&submit_after), "SESSION_CLOSED");

        let reopen_rev = session["session"]["revision"].as_str().unwrap().to_owned();
        let reopen_after = rt
            .dispatch(
                "session.reopen",
                json!({"instanceId": iid, "sessionId": "s-1", "expectedRevision": reopen_rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&reopen_after), "NOT_FOUND");

        // Removing again (or with a stale revision) is NOT_FOUND.
        let error = rt
            .dispatch(
                "project.remove",
                json!({"instanceId": iid, "projectId": "p-1", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_create_blocks_symlink_parent_and_dangling_target_before_probing() {
        let tmp = TempDir::new("proj-sym-p0");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);

        let workspaces = root_dir.join("workspaces");
        let outside = tmp.0.join("outside");
        let outside_nonempty = outside.join("sub");
        std::fs::create_dir_all(&outside_nonempty).unwrap();
        std::fs::write(outside_nonempty.join("secret.txt"), b"sensitive").unwrap();

        // Symlink parent pointing outside authorized root (`workspaces/esc -> ../outside`),
        // where `../outside/sub` is a non-empty directory. Must report PATH_NOT_ALLOWED,
        // NOT PATH_EXISTS (no descendant probing outside authorized root).
        std::os::unix::fs::symlink(&outside, workspaces.join("esc")).unwrap();
        let err = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-sym-1",
                    "parent": parent(&iid, "esc"),
                    "directoryName": "sub",
                    "name": "Escape",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), CODE_PATH_NOT_ALLOWED);

        // Dangling symlink parent must also return PATH_NOT_ALLOWED.
        std::os::unix::fs::symlink(
            tmp.0.join("does-not-exist-anywhere"),
            workspaces.join("dangling"),
        )
        .unwrap();
        let err = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-sym-2",
                    "parent": parent(&iid, "dangling"),
                    "directoryName": "leaf",
                    "name": "Dangling",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), CODE_PATH_NOT_ALLOWED);

        // Final target as an escaping symlink must also return PATH_NOT_ALLOWED.
        let err = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-sym-3",
                    "parent": parent(&iid, ""),
                    "directoryName": "esc",
                    "name": "EscapeLeaf",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), CODE_PATH_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn project_create_concurrent_race_never_cleans_committed_directory() {
        let tmp = TempDir::new("proj-race");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);

        // Two concurrent `project.create` calls racing on the same `projectId`
        // under the same parent chain: exactly one succeeds and the loser
        // receives CREATE_FAILED without deleting the winner's directory.
        let fut_a = rt.dispatch(
            "project.create",
            json!({
                "instanceId": iid,
                "projectId": "p-race",
                "parent": parent(&iid, "team/nested"),
                "directoryName": "app",
                "name": "WinnerA",
            }),
            None,
            None,
        );
        let fut_b = rt.dispatch(
            "project.create",
            json!({
                "instanceId": iid,
                "projectId": "p-race",
                "parent": parent(&iid, "team/nested"),
                "directoryName": "app-b",
                "name": "WinnerB",
            }),
            None,
            None,
        );
        let (res_a, res_b) = tokio::join!(fut_a, fut_b);
        let (winner_dir, err) = match (res_a, res_b) {
            (Ok((_, v)), Err(e)) | (Err(e), Ok((_, v))) => (
                v["project"]["root"]["relativePath"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                e,
            ),
            other => panic!("expected exactly one Ok and one Err, got {other:?}"),
        };
        assert_eq!(contract_code(&err), CODE_CREATE_FAILED);
        assert!(
            root_dir.join("workspaces").join(&winner_dir).is_dir(),
            "winner directory `{winner_dir}` must survive loser cleanup"
        );
    }

    #[tokio::test]
    async fn project_remove_blocks_on_active_operations_and_schedules() {
        let tmp = TempDir::new("proj-active-res");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);

        let (_, created) = rt
            .dispatch(
                "project.create",
                json!({
                    "instanceId": iid,
                    "projectId": "p-act",
                    "parent": parent(&iid, ""),
                    "directoryName": "act",
                    "name": "Active",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let rev = created["project"]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "session.create",
            json!({"instanceId": iid, "sessionId": "s-act", "projectId": "p-act"}),
            None,
            None,
        )
        .await
        .unwrap();

        // 1) Active operation under project blocks removal.
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO operations (operation_id, operation_type, scope_kind, project_id, session_id, state, revision, terminal, created_at, updated_at)
                 VALUES ('op-act-1', 'git.push', 'project', 'p-act', NULL, 'running', 'rev-op', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "project.remove",
                json!({"instanceId": iid, "projectId": "p-act", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), CODE_ACTIVE_RESOURCES);

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE operations SET state = 'succeeded', terminal = 1 WHERE operation_id = 'op-act-1'",
                [],
            )
            .unwrap();
            // 2) Enabled schedule under project blocks removal.
            conn.execute(
                "INSERT INTO v1_schedules (schedule_id, task_id, project_id, session_id, kind, expression, timezone, enabled, message, channel_id, revision, created_at, updated_at)
                 VALUES ('sched-act-1', 'task-1', 'p-act', 's-act', 'cron', '* * * * *', 'UTC', 1, 'ping', NULL, 'srev-1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "project.remove",
                json!({"instanceId": iid, "projectId": "p-act", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), CODE_ACTIVE_RESOURCES);

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE v1_schedules SET enabled = 0 WHERE schedule_id = 'sched-act-1'",
                [],
            )
            .unwrap();
        }
        let (_, removed) = rt
            .dispatch(
                "project.remove",
                json!({"instanceId": iid, "projectId": "p-act", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(removed["removedProjectId"], "p-act");
    }
}
