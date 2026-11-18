//! `integrations.detect` — the Taskmaster integration probe (t019 T2).
//!
//! TS 语义参考（cc7c4e6，`ui/server/routes/taskmaster.js:34-96` 的
//! `checkTaskMasterInstallation` 与 `:352-437` 的 `detect-all`）：查
//! `task-master` CLI 是否在 PATH（`which`），取 `--version` 输出；对每个
//! 已知项目查 `{projectPath}/.taskmaster` 目录与其 `tasks/tasks.json`
//! （`hasTaskmaster && hasEssentialFiles`）。v1 的受控实现把「which」改成
//! 对 PATH 的直接可执行文件搜索（不走 `shell: true`），版本探测用受控
//! 子进程 `task-master --version`（10 s 上限），项目枚举来自 v1 的
//! `projects` 表，项目路径经 `files.rs` 的授权根解析
//! （`resolve_project`）。
//!
//! 合同要求（`cli.case.contract.capability.integration`）：「真实检测可
//! 执行依赖与项目文件；未安装/未初始化/无权限分别可判断，不靠路由存在
//! 判可用」——本操作只读，无副作用。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, ContractError, EffectStatus, RetrySpec,
};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::resolve_project;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// The CLI the detection looks for (the TS baseline's `which task-master`).
const TASKMASTER_CLI: &str = "task-master";

/// The version probe bound (the TS route has none; a hung CLI must not hang
/// the detect).
const VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("integrations.detect", |rt, input, _key, _rev| {
        Box::pin(integrations_detect(rt, input))
    });
}

fn invalid_arg(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// `which TASKMASTER_CLI` without a shell: the first PATH entry carrying an
/// executable file of that name.
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(dir).join(name);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(candidate);
    }
    None
}

/// `task-master --version` under a timeout; the trimmed first line (the TS
/// route keeps the whole trimmed stdout — same shape here) or `None`.
async fn probe_version() -> Option<String> {
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        tokio::process::Command::new(TASKMASTER_CLI)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// One project's Taskmaster state: `{root}/.taskmaster` exists as a
/// directory and its `tasks/tasks.json` is readable (the TS
/// `hasTaskmaster && hasEssentialFiles` pair).
fn project_taskmaster_state(workspace: &std::path::Path) -> (bool, Option<String>) {
    let dir = workspace.join(".taskmaster");
    match std::fs::metadata(&dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return (false, Some("TASKMASTER_DIR_INVALID".to_owned())),
        Err(_) => return (false, Some("TASKMASTER_DIR_MISSING".to_owned())),
    }
    if dir.join("tasks").join("tasks.json").is_file() {
        (true, None)
    } else {
        (false, Some("TASKMASTER_TASKS_MISSING".to_owned()))
    }
}

/// `integrations.detect` — read-only. The runtime block reports the CLI
/// installation; `projects` reports per-project initialization. A
/// `projectId` narrows the probe to one project (must exist — the
/// contract's error list for this operation has no `NOT_FOUND`).
async fn integrations_detect(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let integration_type = input
        .get("integrationType")
        .and_then(Value::as_str)
        .unwrap_or("taskmaster");
    if integration_type != "taskmaster" {
        return Err(invalid_arg(format!(
            "unsupported integrationType `{integration_type}`"
        ))
        .into());
    }
    let requested_project = input
        .get("projectId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let store = rt.store();
    let project_ids: Vec<String> = match &requested_project {
        Some(project_id) => {
            if store.get_project(project_id).is_err() {
                return Err(invalid_arg(format!(
                    "project `{project_id}` is not registered"
                ))
                .into());
            }
            vec![project_id.clone()]
        }
        None => {
            let conn = store.conn.lock().expect("sqlite mutex");
            let mut stmt = conn
                .prepare("SELECT project_id FROM projects ORDER BY created_at ASC, project_id ASC")
                .map_err(|e| {
                    ContractError::new(
                        agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
                        format!("list projects: {e}"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                })?;
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| {
                    ContractError::new(
                        agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
                        format!("list projects: {e}"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    ContractError::new(
                        agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
                        format!("list projects: {e}"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                })?;
            drop(stmt);
            drop(conn);
            ids
        }
    };

    let mut projects = Vec::new();
    for project_id in &project_ids {
        let (initialized, reason_code) = match resolve_project(store, project_id) {
            Ok(workspace) => project_taskmaster_state(&workspace.workspace),
            Err(_) => (false, Some("PROJECT_ROOT_UNRESOLVED".to_owned())),
        };
        projects.push(json!({
            "projectId": project_id,
            "initialized": initialized,
            "reasonCode": reason_code,
        }));
    }

    let (installed, version) = match find_in_path(TASKMASTER_CLI) {
        Some(_) => (true, probe_version().await),
        None => (false, None),
    };
    let runtime = if installed {
        json!({
            "state": "ready",
            "available": true,
            "reasonCode": Value::Null,
            "generation": Value::Null,
        })
    } else {
        json!({
            "state": "not_installed",
            "available": false,
            "reasonCode": "TASKMASTER_NOT_INSTALLED",
            "generation": Value::Null,
        })
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "installed": installed,
            "version": version,
            "runtime": runtime,
            "projects": projects,
        }),
    ))
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
    use std::sync::Arc;

    struct Fixture {
        _tmp: TempDir,
        rt: Arc<PublicV1Runtime>,
    }

    async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let (file, _token) =
            crate::public_v1::bootstrap::load_local_instance_and_token(&home, None).unwrap();
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
        let store = V1Store::open_and_recover(&home, meta).unwrap();
        Fixture {
            _tmp: tmp,
            rt: PublicV1Runtime::new(store),
        }
    }

    async fn dispatch_err(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> ContractError {
        match rt.dispatch(method, input, None, None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn detect_reports_installation_and_no_projects() {
        let fx = fixture("integrations-empty").await;

        let (_, detected) = fx
            .rt
            .dispatch("integrations.detect", json!({}), None, None)
            .await
            .unwrap();
        // The CLI probe itself is environment-dependent (whether task-master
        // is on PATH); the shape is not.
        assert_eq!(
            detected["installed"],
            if find_in_path(TASKMASTER_CLI).is_some() {
                json!(true)
            } else {
                json!(false)
            }
        );
        if detected["installed"] == json!(true) {
            assert_eq!(detected["runtime"]["state"], "ready");
            assert_eq!(detected["runtime"]["available"], true);
            assert_eq!(detected["runtime"]["reasonCode"], Value::Null);
        } else {
            assert_eq!(detected["runtime"]["state"], "not_installed");
            assert_eq!(detected["runtime"]["available"], false);
            assert_eq!(detected["runtime"]["reasonCode"], "TASKMASTER_NOT_INSTALLED");
            assert_eq!(detected["version"], Value::Null);
        }
        assert_eq!(detected["runtime"]["generation"], Value::Null);
        assert_eq!(detected["projects"].as_array().map(Vec::len), Some(0));

        // An unregistered project rejects (no NOT_FOUND in this contract).
        let error = dispatch_err(
            &fx.rt,
            "integrations.detect",
            json!({"projectId": "proj-none"}),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
        // So does an unknown integration type (defense in depth past the
        // input schema's enum).
        let error = dispatch_err(
            &fx.rt,
            "integrations.detect",
            json!({"integrationType": "not-taskmaster"}),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn detect_reports_per_project_initialization() {
        let fx = fixture("integrations-projects").await;
        let store = fx.rt.store();
        let instance_id = store.meta().instance_id.clone();

        // One project with a resolved workspace and a .taskmaster folder;
        // one project without any authorized root (virtual).
        let workspace_root = fx._tmp.0.join("ws");
        std::fs::create_dir_all(workspace_root.join("proj-a").join(".taskmaster").join("tasks"))
            .unwrap();
        std::fs::write(
            workspace_root.join("proj-a").join(".taskmaster").join("tasks").join("tasks.json"),
            "{\"tasks\": []}",
        )
        .unwrap();
        // A .taskmaster directory without the tasks file.
        std::fs::create_dir_all(workspace_root.join("proj-b").join(".taskmaster")).unwrap();
        store
            .declare_authorized_root("root-detect", &workspace_root)
            .unwrap();
        for project_id in ["proj-a", "proj-b", "proj-virtual"] {
            fx.rt
                .dispatch(
                    "project.register",
                    json!({
                        "instanceId": instance_id,
                        "projectId": project_id,
                        "name": project_id,
                        "environmentId": "local",
                        "root": {"instanceId": instance_id, "rootId": "root-detect", "relativePath": project_id}
                    }),
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let (_, detected) = fx
            .rt
            .dispatch("integrations.detect", json!({}), None, None)
            .await
            .unwrap();
        let projects = detected["projects"].as_array().unwrap().clone();
        let by_id = |id: &str| {
            projects
                .iter()
                .find(|p| p["projectId"] == json!(id))
                .cloned()
                .unwrap_or_else(|| panic!("project {id} missing: {projects:?}"))
        };
        assert_eq!(by_id("proj-a")["initialized"], true);
        assert_eq!(by_id("proj-a")["reasonCode"], Value::Null);
        assert_eq!(by_id("proj-b")["initialized"], false);
        assert_eq!(by_id("proj-b")["reasonCode"], "TASKMASTER_TASKS_MISSING");
        assert_eq!(by_id("proj-virtual")["initialized"], false);
        assert_eq!(
            by_id("proj-virtual")["reasonCode"],
            "PROJECT_ROOT_UNRESOLVED"
        );

        // A single-project probe answers exactly that project.
        let (_, one) = fx
            .rt
            .dispatch(
                "integrations.detect",
                json!({"projectId": "proj-a"}),
                None,
                None,
            )
            .await
            .unwrap();
        let projects = one["projects"].as_array().unwrap().clone();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["projectId"], "proj-a");
        assert_eq!(projects[0]["initialized"], true);
    }
}
