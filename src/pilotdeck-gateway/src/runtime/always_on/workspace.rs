//! Port of TS `src/always-on/workspace/` — `GitWorktreeProvider`,
//! `SnapshotCopyProvider` and `WorkspaceProviderRegistry`: the two isolation
//! strategies an Always-On run executes in, picked by priority
//! (git-worktree first, snapshot-copy as the fallback).
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use pilotdeck_protocol::{JsObject, JsValue};

use super::AlwaysOnError;

/// `WorkspaceHandle` (protocol/types.ts:68-74).
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceHandle {
    pub run_id: String,
    pub project_key: String,
    pub strategy: String,
    pub cwd: String,
    /// String-valued, in the provider's insertion order.
    pub metadata: Vec<(String, String)>,
}

impl WorkspaceHandle {
    pub(crate) fn metadata_object(&self) -> JsObject {
        let mut out = JsObject::new();
        for (key, value) in &self.metadata {
            out.insert(key.as_str().into(), JsValue::from(value.as_str()));
        }
        out
    }
}

/// `onWorktreeCreated(runId, cwd)` (AlwaysOnRuntime.ts:70): the host
/// broadcasts `worktree_created`.
pub(crate) type WorktreeCreatedHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

struct CommandResult {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `runGit` / `runCommand`: stdin ignored, output collected, a spawn
/// failure is exit code -1 with the error message as stderr.
async fn run(bin: &str, args: &[&str]) -> CommandResult {
    match tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) => CommandResult {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => CommandResult {
            code: -1,
            stdout: String::new(),
            stderr: error.to_string(),
        },
    }
}

fn expect_ok(result: &CommandResult, label: &str) -> Result<(), AlwaysOnError> {
    if result.code != 0 {
        let detail = if result.stderr.is_empty() {
            &result.stdout
        } else {
            &result.stderr
        };
        return Err(AlwaysOnError::new(
            "workspace_prepare_failed",
            format!("{label} failed: {detail}"),
        ));
    }
    Ok(())
}

/// The registry's providers (WorkspaceProvider.ts), dispatched by id.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strategy {
    GitWorktree,
    SnapshotCopy,
}

impl Strategy {
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        match id {
            "git-worktree" => Some(Self::GitWorktree),
            "snapshot-copy" => Some(Self::SnapshotCopy),
            _ => None,
        }
    }
}

/// `WorkspaceProviderRegistry` with the runtime's two default providers
/// (AlwaysOnRuntime.ts:431-447): git worktrees under `worktreesDir`, CoW
/// snapshots under `snapshotsDir`.
#[derive(Clone)]
pub(crate) struct WorkspaceRegistry {
    pub worktrees_dir: PathBuf,
    pub snapshots_dir: PathBuf,
    pub on_worktree_created: Option<WorktreeCreatedHook>,
}

const SNAPSHOT_IGNORES: [&str; 5] = [
    ".git",
    "node_modules",
    "dist",
    ".pilotdeck",
    ".pilotdeck-always-on",
];

/// `DEFAULT_SNAPSHOT_MAX_BYTES` (parseAlwaysOnConfig.ts:35).
const SNAPSHOT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

impl WorkspaceRegistry {
    /// `resolve` (WorkspaceProviderRegistry.ts:22-36): the first applicable
    /// provider in priority order.
    pub(crate) async fn resolve(&self, project_root: &str) -> Result<Strategy, AlwaysOnError> {
        if git_applicable(project_root).await {
            return Ok(Strategy::GitWorktree);
        }
        if std::fs::metadata(project_root).is_ok_and(|meta| meta.is_dir()) {
            return Ok(Strategy::SnapshotCopy);
        }
        Err(AlwaysOnError::new(
            "workspace_unavailable",
            format!("no workspace provider applicable for {project_root}."),
        ))
    }

    /// `prepare` (WorkspaceProviderRegistry.ts:38-45).
    pub(crate) async fn prepare(
        &self,
        project_root: &str,
        run_id: &str,
        plan_title: &str,
    ) -> Result<WorkspaceHandle, AlwaysOnError> {
        let strategy = self.resolve(project_root).await?;
        self.prepare_with(strategy, project_root, run_id, plan_title)
            .await
    }

    pub(crate) async fn prepare_with(
        &self,
        strategy: Strategy,
        project_root: &str,
        run_id: &str,
        plan_title: &str,
    ) -> Result<WorkspaceHandle, AlwaysOnError> {
        match strategy {
            Strategy::GitWorktree => {
                self.prepare_worktree(project_root, run_id, plan_title)
                    .await
            }
            Strategy::SnapshotCopy => self.prepare_snapshot(project_root, run_id).await,
        }
    }

    /// `GitWorktreeProvider.prepare` (GitWorktreeProvider.ts:32-73).
    async fn prepare_worktree(
        &self,
        project_root: &str,
        run_id: &str,
        plan_title: &str,
    ) -> Result<WorkspaceHandle, AlwaysOnError> {
        let top = run("git", &["-C", project_root, "rev-parse", "--show-toplevel"]).await;
        expect_ok(&top, "git rev-parse --show-toplevel")?;
        let repo_root = top.stdout.trim().to_string();
        checkpoint_dirty_worktree(&repo_root, plan_title).await?;
        let branch = run(
            "git",
            &["-C", &repo_root, "rev-parse", "--abbrev-ref", "HEAD"],
        )
        .await;
        expect_ok(&branch, "git rev-parse --abbrev-ref HEAD")?;
        let base_branch = branch.stdout.trim().to_string();
        let commit = run("git", &["-C", &repo_root, "rev-parse", "HEAD"]).await;
        expect_ok(&commit, "git rev-parse HEAD")?;
        let base_commit = commit.stdout.trim().to_string();
        let worktree_path = crate::metadata::resolve(&self.worktrees_dir, run_id)
            .to_string_lossy()
            .into_owned();
        let branch_name = format!("always-on/{run_id}");
        let add = run(
            "git",
            &[
                "-C",
                &repo_root,
                "worktree",
                "add",
                "-b",
                &branch_name,
                &worktree_path,
                &base_commit,
            ],
        )
        .await;
        if add.code != 0 {
            let detail = if add.stderr.is_empty() {
                add.stdout
            } else {
                add.stderr
            };
            return Err(AlwaysOnError::new(
                "workspace_prepare_failed",
                format!("git worktree add failed: {detail}"),
            ));
        }
        if let Some(hook) = &self.on_worktree_created {
            hook(run_id, &worktree_path);
        }
        Ok(WorkspaceHandle {
            run_id: run_id.into(),
            project_key: project_root.into(),
            strategy: "git-worktree".into(),
            cwd: worktree_path,
            metadata: vec![
                ("repoRoot".into(), repo_root),
                ("baseBranch".into(), base_branch),
                ("baseCommit".into(), base_commit),
                ("branchName".into(), branch_name),
            ],
        })
    }

    /// `SnapshotCopyProvider.prepare` (SnapshotCopyProvider.ts:41-65).
    async fn prepare_snapshot(
        &self,
        project_root: &str,
        run_id: &str,
    ) -> Result<WorkspaceHandle, AlwaysOnError> {
        let target = crate::metadata::resolve(&self.snapshots_dir, run_id);
        let size = estimate_size(project_root).await;
        if size > SNAPSHOT_MAX_BYTES {
            return Err(AlwaysOnError::new(
                "workspace_prepare_failed",
                format!("snapshot source size {size} exceeds maxBytes {SNAPSHOT_MAX_BYTES}."),
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AlwaysOnError::new("internal", error.to_string()))?;
        }
        let target_text = target.to_string_lossy().into_owned();
        let strategy = copy_tree(project_root, &target)
            .await
            .map_err(|error| AlwaysOnError::new("internal", error.to_string()))?;
        Ok(WorkspaceHandle {
            run_id: run_id.into(),
            project_key: project_root.into(),
            strategy: "snapshot-copy".into(),
            cwd: target_text,
            metadata: vec![
                ("copyStrategy".into(), strategy.into()),
                ("baseSize".into(), size.to_string()),
            ],
        })
    }
}

/// `GitWorktreeProvider.isApplicable` (GitWorktreeProvider.ts:24-30).
async fn git_applicable(project_root: &str) -> bool {
    run("git", &["-C", project_root, "rev-parse", "--show-toplevel"])
        .await
        .code
        == 0
        && run("git", &["-C", project_root, "rev-parse", "HEAD"])
            .await
            .code
            == 0
}

/// `checkpointDirtyWorktree` (GitWorktreeProvider.ts:112-139).
async fn checkpoint_dirty_worktree(repo_root: &str, plan_title: &str) -> Result<(), AlwaysOnError> {
    let status = run("git", &["-C", repo_root, "status", "--porcelain"]).await;
    expect_ok(&status, "git status --porcelain")?;
    if status.stdout.trim().is_empty() {
        return Ok(());
    }
    let add = run("git", &["-C", repo_root, "add", "-A"]).await;
    expect_ok(&add, "git add -A")?;
    let normalized = plan_title.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = if normalized.is_empty() {
        "untitled plan".to_string()
    } else {
        normalized
    };
    let message = format!("chore(always-on): checkpoint before executing {normalized}");
    let commit = run("git", &["-C", repo_root, "commit", "-m", &message]).await;
    expect_ok(&commit, "git commit")?;
    let clean = run("git", &["-C", repo_root, "status", "--porcelain"]).await;
    expect_ok(&clean, "git status --porcelain")?;
    if !clean.stdout.trim().is_empty() {
        return Err(AlwaysOnError::new(
            "workspace_prepare_failed",
            "git worktree remained dirty after the Always-On checkpoint commit.",
        ));
    }
    Ok(())
}

/// `estimateSize` (SnapshotCopyProvider.ts:133-147): `du -sk`, 0 on failure.
async fn estimate_size(root: &str) -> u64 {
    let result = run("du", &["-sk", root]).await;
    if result.code != 0 {
        return 0;
    }
    result
        .stdout
        .split_whitespace()
        .next()
        .and_then(|kb| kb.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// `copy` (SnapshotCopyProvider.ts:80-101): the platform CoW copy with the
/// ignored entries pruned, else a filtered recursive copy.
async fn copy_tree(source: &str, target: &Path) -> std::io::Result<&'static str> {
    let target_text = target.to_string_lossy().into_owned();
    let cow: Option<(&[&str], &'static str)> = if cfg!(target_os = "macos") {
        Some((&["-c", "-R"], "clonefile"))
    } else if cfg!(target_os = "linux") {
        Some((&["--reflink=auto", "-R"], "reflink"))
    } else {
        None
    };
    if let Some((flags, label)) = cow {
        let mut args: Vec<&str> = flags.to_vec();
        args.push(source);
        args.push(&target_text);
        if run("cp", &args).await.code == 0 && target.exists() {
            for entry in SNAPSHOT_IGNORES {
                let path = target.join(entry);
                let _ = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            }
            return Ok(label);
        }
    }
    copy_filtered(Path::new(source), target, true)?;
    Ok("fs.cp")
}

fn copy_filtered(source: &Path, target: &Path, top: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if top && SNAPSHOT_IGNORES.iter().any(|ignored| name == *ignored) {
            continue;
        }
        let from = entry.path();
        let to = target.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_filtered(&from, &to, false)?;
        } else if kind.is_symlink() {
            let link = std::fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, &to)?;
            #[cfg(not(unix))]
            let _ = link;
        } else {
            crate::node_io::copy_file(&from, &to)?;
        }
    }
    Ok(())
}
