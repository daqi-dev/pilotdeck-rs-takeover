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

use std::path::PathBuf;
use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
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

fn decode_parent_path(
    store: &V1Store,
    parent: &Value,
) -> Result<(ParentPath, PathBuf), ContractError> {
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
    let components = relative_components(&relative).map_err(|mut e| {
        e.code = CODE_PATH_NOT_ALLOWED.to_owned();
        e
    })?;
    let root = store.resolve_authorized_root(&root_id)?;
    let mut target = root.clone();
    for component in &components {
        target.push(component);
    }
    Ok((ParentPath { root_id, relative }, target))
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

    let (parent, parent_target) = decode_parent_path(store, &input["parent"])?;
    let target = parent_target.join(directory_name);

    // Pre-flight the registration key so a collision fails before any
    // filesystem side effect.
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

    // Existing target: an empty directory is adopted, anything else
    // (non-empty directory or a file) is a recognizable conflict.
    match std::fs::symlink_metadata(&target) {
        Ok(meta) => {
            let is_empty_dir = meta.is_dir()
                && std::fs::read_dir(&target)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false);
            if !is_empty_dir {
                return Err(ContractError::new(
                    CODE_PATH_EXISTS,
                    format!(
                        "directory `{}` already exists and is not empty; refusing to overwrite",
                        target.display()
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ContractError::new(
                CODE_CREATE_FAILED,
                format!("cannot stat `{}`: {e}", target.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }

    // Create the missing directory chain level by level, verifying each
    // existing level stays inside the authorized root (symlink escapes are
    // refused) and remembering what this call created for failure cleanup.
    let created = create_dir_chain(store, &parent, directory_name)?;

    // Registration is one transaction; on failure only the directories this
    // call created (and that are still empty) are removed.
    let register = (|| -> Result<String, ContractError> {
        let mut conn = store.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
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
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(rev)
    })();
    match register {
        Ok(rev) => Ok(json!({
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
        })),
        Err(error) => {
            cleanup_created_dirs(&created);
            Err(error)
        }
    }
}

/// Creates the missing directory chain from the authorized root down to the
/// project directory (parent path components + `directory_name`), returning
/// the directories this call created, deepest last. Every pre-existing level
/// is canonicalized and confined to the authorized root; a file blocking
/// the chain is a recognizable conflict.
fn create_dir_chain(
    store: &V1Store,
    parent: &ParentPath,
    directory_name: &str,
) -> Result<Vec<PathBuf>, ContractError> {
    let root = store.resolve_authorized_root(&parent.root_id)?;
    let canonical_root = std::fs::canonicalize(&root).map_err(|e| {
        ContractError::new(
            CODE_CREATE_FAILED,
            format!("cannot resolve authorized root `{}`: {e}", root.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;

    let mut components = relative_components(&parent.relative)?;
    components.push(std::ffi::OsString::from(directory_name));

    let mut created: Vec<PathBuf> = Vec::new();
    let mut next = canonical_root.clone();
    for component in &components {
        next.push(component);
        match std::fs::symlink_metadata(&next) {
            Ok(_) => {
                let verified = std::fs::canonicalize(&next).map_err(|e| {
                    cleanup_created_dirs(&created);
                    ContractError::new(
                        CODE_CREATE_FAILED,
                        format!("cannot resolve `{}`: {e}", next.display()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                })?;
                if verified.strip_prefix(&canonical_root).is_err() {
                    cleanup_created_dirs(&created);
                    return Err(path_not_allowed(format!(
                        "path component `{}` escapes the authorized root",
                        next.display()
                    )));
                }
                if !verified.is_dir() {
                    cleanup_created_dirs(&created);
                    return Err(ContractError::new(
                        CODE_PATH_EXISTS,
                        format!("`{}` exists and is not a directory", next.display()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Err(e) = std::fs::create_dir(&next) {
                    cleanup_created_dirs(&created);
                    return Err(ContractError::new(
                        CODE_CREATE_FAILED,
                        format!("failed to create `{}`: {e}", next.display()),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                created.push(next.clone());
            }
            Err(e) => {
                cleanup_created_dirs(&created);
                return Err(ContractError::new(
                    CODE_CREATE_FAILED,
                    format!("cannot stat `{}`: {e}", next.display()),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
    }
    Ok(created)
}

/// Removes the directories this call created, deepest first, but only those
/// that are still empty (`登记失败仅清理本次创建且仍空的目录`).
fn cleanup_created_dirs(created: &[PathBuf]) {
    for path in created.iter().rev() {
        let is_empty = std::fs::read_dir(path)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if is_empty {
            let _ = std::fs::remove_dir(path);
        }
    }
}

fn join_relative(parent: &PathBuf, directory_name: &str) -> String {
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

async fn project_update(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().update_project_name(&input)?,
    ))
}

impl V1Store {
    /// `project.update`: conditional (CAS) rename of the display name. The
    /// disk root is never moved ("不移动磁盘根").
    pub(crate) fn update_project_name(&self, input: &Value) -> Result<Value, ContractError> {
        let project_id = input["projectId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let name = input["name"].as_str().unwrap_or_default();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
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

    /// `project.remove`: deregistration only. Refuses with
    /// `ACTIVE_RESOURCES` while any session of the project still has an
    /// active (non-terminal) or queued run; the directory, sessions, runs
    /// and history survive.
    pub(crate) fn remove_project(&self, input: &Value) -> Result<Value, ContractError> {
        let project_id = input["projectId"].as_str().unwrap_or_default();
        let expected = input["expectedRevision"].as_str().unwrap_or_default();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
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
        let active: i64 = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM runs r JOIN sessions s ON s.session_id = r.session_id
                    WHERE s.project_id = ?1 AND r.terminal = 0)
                 OR EXISTS(
                    SELECT 1 FROM run_queue q JOIN sessions s ON s.session_id = q.session_id
                    WHERE s.project_id = ?1)",
                params![project_id, project_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if active != 0 {
            return Err(ContractError::new(
                CODE_ACTIVE_RESOURCES,
                format!(
                    "project `{project_id}` still has sessions with active or queued runs; \
                     cancel or wait for them before removing the registration"
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
        tx.commit().map_err(sql_err)?;
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
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().remove_project(&input)?,
    ))
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
        assert_eq!(session["session"]["sessionState"], "open");
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
}
