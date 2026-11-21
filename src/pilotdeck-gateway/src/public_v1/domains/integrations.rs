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
use agentcpm_pd_protocol::errors::{CODE_INVALID_ARGUMENT, ContractError, EffectStatus, RetrySpec};
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
#[derive(Debug, Clone, PartialEq, Eq)]
enum FindOutcome {
    Found(std::path::PathBuf),
    PermissionDenied,
    NotFound,
}

fn find_in_path_env(name: &str, path_env: Option<&std::ffi::OsStr>) -> FindOutcome {
    let Some(path) = path_env else {
        return FindOutcome::NotFound;
    };
    let mut saw_permission_denied = false;
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        let metadata = match std::fs::metadata(&candidate) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                saw_permission_denied = true;
                continue;
            }
            Err(_) => continue,
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
        return FindOutcome::Found(candidate);
    }
    if saw_permission_denied {
        FindOutcome::PermissionDenied
    } else {
        FindOutcome::NotFound
    }
}

#[cfg(test)]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    match find_in_path_env(name, std::env::var_os("PATH").as_deref()) {
        FindOutcome::Found(path) => Some(path),
        FindOutcome::PermissionDenied | FindOutcome::NotFound => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeOutcome {
    Ready(Option<String>),
    PermissionDenied,
    Timeout,
    Failed,
}

const MAX_VERSION_BYTES: u64 = 64 * 1024;
const PROBE_TERM_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

async fn read_bounded_stdout(pipe: Option<tokio::process::ChildStdout>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let Some(pipe) = pipe else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.take(MAX_VERSION_BYTES).read_to_end(&mut buf).await;
    buf
}

#[cfg(unix)]
fn child_group(child: &tokio::process::Child) -> Option<rustix::process::Pid> {
    child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
}

#[cfg(not(unix))]
fn child_group(_child: &tokio::process::Child) -> Option<rustix::process::Pid> {
    None
}

/// `task-master --version` in an owned process group with `kill_on_drop` and
/// bounded SIGTERM/SIGKILL group teardown on timeout or cancellation.
async fn probe_version_with(
    binary: &std::path::Path,
    probe_timeout: std::time::Duration,
) -> ProbeOutcome {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return ProbeOutcome::PermissionDenied;
        }
        Err(_) => return ProbeOutcome::Failed,
    };
    let group = child_group(&child);
    let mut guard = crate::process_group::GroupGuard(group);
    let stdout_task = tokio::spawn(read_bounded_stdout(child.stdout.take()));

    match tokio::time::timeout(probe_timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let drained = tokio::time::timeout(PROBE_TERM_GRACE, stdout_task).await;
            guard.disarm();
            if !status.success() {
                return ProbeOutcome::Failed;
            }
            let Ok(Ok(bytes)) = drained else {
                return ProbeOutcome::Failed;
            };
            let text = String::from_utf8_lossy(&bytes).trim().to_owned();
            ProbeOutcome::Ready((!text.is_empty()).then_some(text))
        }
        Ok(Err(e)) => {
            guard.signal(rustix::process::Signal::KILL);
            let _ = child.wait().await;
            guard.disarm();
            stdout_task.abort();
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                ProbeOutcome::PermissionDenied
            } else {
                ProbeOutcome::Failed
            }
        }
        Err(_elapsed) => {
            guard.signal(rustix::process::Signal::TERM);
            let _ = crate::process_group::wait_for_exit_unreaped(group, PROBE_TERM_GRACE).await;
            guard.signal(rustix::process::Signal::KILL);
            if group.is_none() {
                let _ = child.kill().await;
            }
            let _ = child.wait().await;
            guard.disarm();
            let _ = tokio::time::timeout(std::time::Duration::from_millis(200), stdout_task).await;
            ProbeOutcome::Timeout
        }
    }
}

/// One project's Taskmaster state: `{root}/.taskmaster` exists strictly inside
/// the workspace (no symlink escape) and `tasks/tasks.json` is openable and
/// readable, distinguishing missing vs invalid vs permission denied.
#[cfg(unix)]
fn project_taskmaster_state(workspace: &std::path::Path) -> (bool, Option<String>) {
    let dir = workspace.join(".taskmaster");
    let tm_meta = match std::fs::symlink_metadata(&dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (false, Some("TASKMASTER_DIR_MISSING".to_owned()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()));
        }
        Err(_) => return (false, Some("TASKMASTER_DIR_INVALID".to_owned())),
    };
    if tm_meta.file_type().is_symlink() || !tm_meta.is_dir() {
        return (false, Some("TASKMASTER_DIR_INVALID".to_owned()));
    }
    let tm_dir = match crate::project_files::open_directory_beneath(workspace, &dir) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (false, Some("TASKMASTER_DIR_MISSING".to_owned()));
        }
        Err(e) if crate::project_files::nofollow_loop(&e) => {
            return (false, Some("TASKMASTER_DIR_INVALID".to_owned()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()));
        }
        Err(_) => return (false, Some("TASKMASTER_DIR_INVALID".to_owned())),
    };
    let tasks_rel = std::path::Path::new("tasks").join("tasks.json");
    match crate::project_files::open_file_at(&tm_dir, &tasks_rel) {
        Ok(mut file) => match file.metadata() {
            Ok(meta) if !meta.is_file() => (false, Some("TASKMASTER_TASKS_MISSING".to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()))
            }
            Err(_) => (false, Some("TASKMASTER_TASKS_MISSING".to_owned())),
            Ok(_) => {
                let mut probe = [0u8; 1];
                match std::io::Read::read(&mut file, &mut probe) {
                    Ok(_) => (true, None),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()))
                    }
                    Err(_) => (false, Some("TASKMASTER_TASKS_MISSING".to_owned())),
                }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (false, Some("TASKMASTER_TASKS_MISSING".to_owned()))
        }
        Err(e) if crate::project_files::nofollow_loop(&e) => {
            (false, Some("TASKMASTER_TASKS_INVALID".to_owned()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()))
        }
        Err(_) => (false, Some("TASKMASTER_TASKS_MISSING".to_owned())),
    }
}

#[cfg(not(unix))]
fn project_taskmaster_state(workspace: &std::path::Path) -> (bool, Option<String>) {
    let dir = workspace.join(".taskmaster");
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if !m.file_type().is_symlink() && m.is_dir() => {}
        Ok(_) => return (false, Some("TASKMASTER_DIR_INVALID".to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (false, Some("TASKMASTER_DIR_MISSING".to_owned()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()));
        }
        Err(_) => return (false, Some("TASKMASTER_DIR_INVALID".to_owned())),
    }
    let tasks = dir.join("tasks").join("tasks.json");
    match std::fs::symlink_metadata(&tasks) {
        Ok(m) if !m.file_type().is_symlink() && m.is_file() => match std::fs::File::open(&tasks) {
            Ok(mut f) => {
                let mut probe = [0u8; 1];
                match std::io::Read::read(&mut f, &mut probe) {
                    Ok(_) => (true, None),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()))
                    }
                    Err(_) => (false, Some("TASKMASTER_TASKS_MISSING".to_owned())),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()))
            }
            Err(_) => (false, Some("TASKMASTER_TASKS_MISSING".to_owned())),
        },
        Ok(_) => (false, Some("TASKMASTER_TASKS_INVALID".to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (false, Some("TASKMASTER_TASKS_MISSING".to_owned()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            (false, Some("TASKMASTER_PERMISSION_DENIED".to_owned()))
        }
        Err(_) => (false, Some("TASKMASTER_TASKS_MISSING".to_owned())),
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
    detect_with_env(
        rt,
        input,
        std::env::var_os("PATH").as_deref(),
        VERSION_TIMEOUT,
    )
    .await
}

async fn detect_with_env(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    path_env: Option<&std::ffi::OsStr>,
    probe_timeout: std::time::Duration,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let integration_type = input
        .get("integrationType")
        .and_then(Value::as_str)
        .unwrap_or("taskmaster");
    if integration_type != "taskmaster" {
        return Err(
            invalid_arg(format!("unsupported integrationType `{integration_type}`")).into(),
        );
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
                return Err(
                    invalid_arg(format!("project `{project_id}` is not registered")).into(),
                );
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
            Ok(workspace) => {
                let ws = workspace.workspace;
                tokio::task::spawn_blocking(move || project_taskmaster_state(&ws))
                    .await
                    .unwrap_or_else(|_| (false, Some("PROJECT_ROOT_UNRESOLVED".to_owned())))
            }
            Err(_) => (false, Some("PROJECT_ROOT_UNRESOLVED".to_owned())),
        };
        projects.push(json!({
            "projectId": project_id,
            "initialized": initialized,
            "reasonCode": reason_code,
        }));
    }

    let (installed, version, runtime) = match find_in_path_env(TASKMASTER_CLI, path_env) {
        FindOutcome::NotFound => (
            false,
            None,
            json!({
                "state": "not_installed",
                "available": false,
                "reasonCode": "TASKMASTER_NOT_INSTALLED",
                "generation": Value::Null,
            }),
        ),
        FindOutcome::PermissionDenied => (
            false,
            None,
            json!({
                "state": "permission_denied",
                "available": false,
                "reasonCode": "TASKMASTER_PERMISSION_DENIED",
                "generation": Value::Null,
            }),
        ),
        FindOutcome::Found(bin) => match probe_version_with(&bin, probe_timeout).await {
            ProbeOutcome::Ready(version) => (
                true,
                version,
                json!({
                    "state": "ready",
                    "available": true,
                    "reasonCode": Value::Null,
                    "generation": Value::Null,
                }),
            ),
            ProbeOutcome::PermissionDenied => (
                true,
                None,
                json!({
                    "state": "permission_denied",
                    "available": false,
                    "reasonCode": "TASKMASTER_PERMISSION_DENIED",
                    "generation": Value::Null,
                }),
            ),
            ProbeOutcome::Timeout => (
                true,
                None,
                json!({
                    "state": "unavailable",
                    "available": false,
                    "reasonCode": "TASKMASTER_VERSION_TIMEOUT",
                    "generation": Value::Null,
                }),
            ),
            ProbeOutcome::Failed => (
                true,
                None,
                json!({
                    "state": "unavailable",
                    "available": false,
                    "reasonCode": "TASKMASTER_PROBE_FAILED",
                    "generation": Value::Null,
                }),
            ),
        },
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
            assert_eq!(
                detected["runtime"]["reasonCode"],
                "TASKMASTER_NOT_INSTALLED"
            );
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
        std::fs::create_dir_all(
            workspace_root
                .join("proj-a")
                .join(".taskmaster")
                .join("tasks"),
        )
        .unwrap();
        std::fs::write(
            workspace_root
                .join("proj-a")
                .join(".taskmaster")
                .join("tasks")
                .join("tasks.json"),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_version_timeout_kills_process_group_and_detect_classifies_failures() {
        use std::os::unix::fs::PermissionsExt;

        let fx = fixture("integrations-probe").await;
        let tmp = &fx._tmp.0;

        // 1. Hung wrapper that spawns a descendant sleep in the same process group.
        let hang_dir = tmp.join("bin-hang");
        std::fs::create_dir_all(&hang_dir).unwrap();
        let pid_file = tmp.join("descendant.pid");
        let hang_bin = hang_dir.join(TASKMASTER_CLI);
        std::fs::write(
            &hang_bin,
            format!(
                "#!/bin/sh\nsleep 30 >/dev/null 2>&1 &\necho $! > \"{}\"\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hang_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (_, res) = detect_with_env(
            fx.rt.clone(),
            json!({}),
            Some(hang_dir.as_os_str()),
            std::time::Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert_eq!(res["runtime"]["available"], false);
        assert_eq!(res["runtime"]["reasonCode"], "TASKMASTER_VERSION_TIMEOUT");

        // Verify the descendant process was killed with the process group.
        if let Ok(raw_pid) = std::fs::read_to_string(&pid_file)
            && let Ok(pid_num) = raw_pid.trim().parse::<i32>()
            && let Some(pid) = rustix::process::Pid::from_raw(pid_num)
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if rustix::process::test_kill_process(pid).is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert!(
                rustix::process::test_kill_process(pid).is_err(),
                "descendant pid {pid_num} leaked after probe_version timeout"
            );
        }

        // 2. Non-zero exit must NOT report `ready`.
        let fail_dir = tmp.join("bin-fail");
        std::fs::create_dir_all(&fail_dir).unwrap();
        let fail_bin = fail_dir.join(TASKMASTER_CLI);
        std::fs::write(&fail_bin, "#!/bin/sh\necho broken >&2\nexit 2\n").unwrap();
        std::fs::set_permissions(&fail_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_, res) = detect_with_env(
            fx.rt.clone(),
            json!({}),
            Some(fail_dir.as_os_str()),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(res["runtime"]["available"], false);
        assert_eq!(res["runtime"]["reasonCode"], "TASKMASTER_PROBE_FAILED");

        // 3. Working executable outputs trimmed version and reports ready.
        let ok_dir = tmp.join("bin-ok");
        std::fs::create_dir_all(&ok_dir).unwrap();
        let ok_bin = ok_dir.join(TASKMASTER_CLI);
        std::fs::write(&ok_bin, "#!/bin/sh\necho 0.99.1\n").unwrap();
        std::fs::set_permissions(&ok_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_, res) = detect_with_env(
            fx.rt.clone(),
            json!({}),
            Some(ok_dir.as_os_str()),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(res["installed"], true);
        assert_eq!(res["version"], "0.99.1");
        assert_eq!(res["runtime"]["state"], "ready");
        assert_eq!(res["runtime"]["available"], true);

        // 4. PermissionDenied PATH dir reports permission_denied cleanly.
        if rustix::process::geteuid().as_raw() != 0 {
            let deny_dir = tmp.join("bin-deny");
            std::fs::create_dir_all(&deny_dir).unwrap();
            let deny_bin = deny_dir.join(TASKMASTER_CLI);
            std::fs::write(&deny_bin, "#!/bin/sh\necho ok\n").unwrap();
            std::fs::set_permissions(&deny_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            let (_, res) = detect_with_env(
                fx.rt.clone(),
                json!({}),
                Some(deny_dir.as_os_str()),
                std::time::Duration::from_secs(2),
            )
            .await
            .unwrap();
            let _ = std::fs::set_permissions(&deny_dir, std::fs::Permissions::from_mode(0o755));
            assert_eq!(res["installed"], false);
            assert_eq!(res["runtime"]["state"], "permission_denied");
            assert_eq!(res["runtime"]["available"], false);
            assert_eq!(res["runtime"]["reasonCode"], "TASKMASTER_PERMISSION_DENIED");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detect_project_distinguishes_permission_denied_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fx = fixture("integrations-perm-sym").await;
        let store = fx.rt.store();
        let instance_id = store.meta().instance_id.clone();
        let workspace_root = fx._tmp.0.join("ws");
        std::fs::create_dir_all(&workspace_root).unwrap();
        store
            .declare_authorized_root("root-perm", &workspace_root)
            .unwrap();

        let outside = fx._tmp.0.join("outside-tm");
        std::fs::create_dir_all(outside.join("tasks")).unwrap();
        std::fs::write(outside.join("tasks").join("tasks.json"), "{}").unwrap();

        // Symlink .taskmaster pointing outside workspace.
        let p_sym_dir = workspace_root.join("proj-sym-dir");
        std::fs::create_dir_all(&p_sym_dir).unwrap();
        symlink(&outside, p_sym_dir.join(".taskmaster")).unwrap();

        // Symlink tasks.json pointing outside workspace.
        let p_sym_file = workspace_root.join("proj-sym-file");
        std::fs::create_dir_all(p_sym_file.join(".taskmaster").join("tasks")).unwrap();
        symlink(
            outside.join("tasks").join("tasks.json"),
            p_sym_file
                .join(".taskmaster")
                .join("tasks")
                .join("tasks.json"),
        )
        .unwrap();

        // chmod 000 tasks.json and chmod 000 .taskmaster.
        let p_noperm_file = workspace_root.join("proj-noperm-file");
        std::fs::create_dir_all(p_noperm_file.join(".taskmaster").join("tasks")).unwrap();
        let noperm_tasks = p_noperm_file
            .join(".taskmaster")
            .join("tasks")
            .join("tasks.json");
        std::fs::write(&noperm_tasks, "{}").unwrap();
        std::fs::set_permissions(&noperm_tasks, std::fs::Permissions::from_mode(0o000)).unwrap();

        let p_noperm_dir = workspace_root.join("proj-noperm-dir");
        std::fs::create_dir_all(p_noperm_dir.join(".taskmaster")).unwrap();
        std::fs::set_permissions(
            p_noperm_dir.join(".taskmaster"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        for id in [
            "proj-sym-dir",
            "proj-sym-file",
            "proj-noperm-file",
            "proj-noperm-dir",
        ] {
            fx.rt
                .dispatch(
                    "project.register",
                    json!({
                        "instanceId": instance_id,
                        "projectId": id,
                        "name": id,
                        "environmentId": "local",
                        "root": {"instanceId": instance_id, "rootId": "root-perm", "relativePath": id}
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

        // Restore permissions so TempDir cleanup always succeeds.
        let _ = std::fs::set_permissions(&noperm_tasks, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::set_permissions(
            p_noperm_dir.join(".taskmaster"),
            std::fs::Permissions::from_mode(0o755),
        );

        let projects = detected["projects"].as_array().unwrap();
        let by_id = |id: &str| {
            projects
                .iter()
                .find(|p| p["projectId"] == json!(id))
                .unwrap()
        };

        assert_eq!(by_id("proj-sym-dir")["initialized"], false);
        assert_eq!(
            by_id("proj-sym-dir")["reasonCode"],
            "TASKMASTER_DIR_INVALID"
        );
        assert_eq!(by_id("proj-sym-file")["initialized"], false);
        assert_eq!(
            by_id("proj-sym-file")["reasonCode"],
            "TASKMASTER_TASKS_INVALID"
        );
        if rustix::process::geteuid().as_raw() != 0 {
            assert_eq!(by_id("proj-noperm-file")["initialized"], false);
            assert_eq!(
                by_id("proj-noperm-file")["reasonCode"],
                "TASKMASTER_PERMISSION_DENIED"
            );
            assert_eq!(by_id("proj-noperm-dir")["initialized"], false);
            assert_eq!(
                by_id("proj-noperm-dir")["reasonCode"],
                "TASKMASTER_PERMISSION_DENIED"
            );
        }
    }
}
